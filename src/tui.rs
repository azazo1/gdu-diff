use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clipboard_rs::{Clipboard, ClipboardContext};
use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table, TableState, Wrap,
};
use tempfile::tempdir;
use tokio::sync::mpsc;
use tokio::time::interval;

use crate::analysis::{Analysis, ChangeKind, RowData, SizeMetric, SortMode};
use crate::gdu::{SnapshotTree, export_snapshot_with_progress};

const REFRESH_DRAW_THROTTLE: Duration = Duration::from_millis(80);
const REFRESH_PROGRESS_TICK: Duration = Duration::from_millis(16);
const REFRESH_PROGRESS_CAP: f64 = 0.95;
const REFRESH_PROGRESS_SETTLE_TIME: Duration = Duration::from_secs(8);

pub struct App {
    analysis: Analysis,
    metric: SizeMetric,
    sort: SortMode,
    include_files: bool,
    refresh_state: Option<RefreshState>,
    current_path: String,
    rows: Vec<RowData>,
    marked_paths: BTreeSet<String>,
    table_state: TableState,
    page_step: usize,
    show_help: bool,
    should_quit: bool,
    status_message: Option<StatusMessage>,
}

#[derive(Clone, Debug)]
pub struct LoadingStep {
    action: String,
    weight: f64,
    progress: f64,
}

impl LoadingStep {
    pub fn new(action: impl Into<String>, weight: f64) -> Self {
        Self {
            action: action.into(),
            weight: if weight.is_finite() && weight > 0.0 {
                weight
            } else {
                1.0
            },
            progress: 0.0,
        }
    }
}

pub struct LoadingState {
    title: String,
    steps: Vec<LoadingStep>,
    current_step: usize,
    detail: String,
    started_at: Instant,
}

impl LoadingState {
    pub fn new(title: impl Into<String>, mut steps: Vec<LoadingStep>) -> Self {
        if steps.is_empty() {
            steps.push(LoadingStep::new("Work", 1.0));
        }
        Self {
            title: title.into(),
            steps,
            current_step: 1,
            detail: String::new(),
            started_at: Instant::now(),
        }
    }

    pub fn set_step(&mut self, step: usize, action: impl Into<String>, detail: impl Into<String>) {
        let next_step = step.clamp(1, self.steps.len());
        if next_step > self.current_step {
            self.steps[self.current_step - 1].progress = 1.0;
        }
        self.current_step = next_step;
        let current = &mut self.steps[self.current_step - 1];
        current.action = action.into();
        current.progress = 0.0;
        self.detail = detail.into();
    }

    pub fn set_step_progress(&mut self, progress: f64) {
        self.steps[self.current_step - 1].progress = progress.clamp(0.0, 1.0);
    }

    pub fn set_detail(&mut self, detail: impl Into<String>) {
        self.detail = detail.into();
    }

    pub fn total_steps(&self) -> usize {
        self.steps.len()
    }

    fn current_action(&self) -> &str {
        self.steps
            .get(self.current_step.saturating_sub(1))
            .map(|step| step.action.as_str())
            .unwrap_or("")
    }

    fn progress_ratio(&self) -> f64 {
        let total_weight = self.steps.iter().map(|step| step.weight).sum::<f64>();
        if total_weight <= 0.0 {
            return 0.0;
        }
        let completed_weight = self
            .steps
            .iter()
            .map(|step| step.weight * step.progress.clamp(0.0, 1.0))
            .sum::<f64>();
        (completed_weight / total_weight).clamp(0.0, 1.0)
    }
}

pub struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    active: bool,
    last_loading_draw: Option<Instant>,
}

impl TerminalSession {
    pub fn start() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;
        Ok(Self {
            terminal,
            active: true,
            last_loading_draw: None,
        })
    }

    pub fn draw_loading(&mut self, loading: &LoadingState) -> Result<()> {
        self.terminal.draw(|frame| render_loading(frame, loading))?;
        self.last_loading_draw = Some(Instant::now());
        Ok(())
    }

    pub fn draw_loading_throttled(
        &mut self,
        loading: &LoadingState,
        interval: Duration,
    ) -> Result<()> {
        if self
            .last_loading_draw
            .is_some_and(|last| last.elapsed() < interval)
        {
            return Ok(());
        }
        self.terminal.draw(|frame| render_loading(frame, loading))?;
        self.last_loading_draw = Some(Instant::now());
        Ok(())
    }

    pub async fn run_app(&mut self, app: &mut App) -> Result<()> {
        run_loop(&mut self.terminal, app).await
    }

    fn restore(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        self.active = false;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

impl App {
    pub fn new(analysis: Analysis, metric: SizeMetric, include_files: bool) -> Result<Self> {
        Self::new_with_refresh(analysis, metric, include_files, None)
    }

    pub fn new_with_refresh(
        analysis: Analysis,
        metric: SizeMetric,
        include_files: bool,
        refresh_state: Option<RefreshState>,
    ) -> Result<Self> {
        let mut app = Self {
            analysis,
            metric,
            sort: SortMode::Delta,
            include_files,
            refresh_state,
            current_path: String::new(),
            rows: Vec::new(),
            marked_paths: BTreeSet::new(),
            table_state: TableState::default(),
            page_step: 1,
            show_help: false,
            should_quit: false,
            status_message: None,
        };
        app.refresh_rows()?;
        Ok(app)
    }

    fn refresh_rows(&mut self) -> Result<()> {
        self.refresh_rows_with_selection(None)
    }

    fn refresh_rows_with_selection(&mut self, preferred_path: Option<String>) -> Result<()> {
        let selected_path =
            preferred_path.or_else(|| self.selected_row().map(|row| row.path.clone()));
        self.rows = self.analysis.children_of(
            &self.current_path,
            self.include_files,
            self.metric,
            self.sort,
        )?;

        let selection = selected_path
            .and_then(|path| self.rows.iter().position(|row| row.path == path))
            .or_else(|| (!self.rows.is_empty()).then_some(0));
        self.table_state.select(selection);
        Ok(())
    }

    fn selected_row(&self) -> Option<&RowData> {
        self.table_state
            .selected()
            .and_then(|index| self.rows.get(index))
    }

    fn on_key(&mut self, code: KeyCode) -> Result<AppAction> {
        if self.show_help {
            match code {
                KeyCode::Esc | KeyCode::Char('?') => self.show_help = false,
                KeyCode::Char('q') => self.should_quit = true,
                _ => {}
            }
            return Ok(AppAction::None);
        }

        match code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Esc => self.clear_marked_with_status(),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp | KeyCode::Char(',') => self.move_selection_page(-1),
            KeyCode::PageDown | KeyCode::Char('.') => self.move_selection_page(1),
            KeyCode::Char('g') => self.move_selection_to_start(),
            KeyCode::Char('G') => self.move_selection_to_end(),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => self.enter_selected()?,
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.go_parent()?,
            KeyCode::Char('a') => {
                self.clear_marked_silently();
                self.metric = match self.metric {
                    SizeMetric::Disk => SizeMetric::Apparent,
                    SizeMetric::Apparent => SizeMetric::Disk,
                };
                self.refresh_rows()?;
            }
            KeyCode::Char('f') => {
                self.clear_marked_silently();
                self.include_files = !self.include_files;
                self.refresh_rows()?;
            }
            KeyCode::Char('n') => {
                self.sort = SortMode::Name;
                self.refresh_rows()?;
            }
            KeyCode::Char('s') => {
                self.sort = SortMode::LatestSize;
                self.refresh_rows()?;
            }
            KeyCode::Char('d') => {
                self.sort = SortMode::Delta;
                self.refresh_rows()?;
            }
            KeyCode::Char('p') => {
                self.sort = SortMode::ShareDelta;
                self.refresh_rows()?;
            }
            KeyCode::Char(' ') => self.toggle_mark_selected_and_advance(),
            KeyCode::Char('c') => return Ok(AppAction::Copy(self.copy_relative_path())),
            KeyCode::Char('C') => return Ok(AppAction::Copy(self.copy_absolute_path())),
            KeyCode::Char('b') => return Ok(AppAction::OpenShell(self.current_directory_path())),
            KeyCode::Char('r') if self.refresh_state.is_some() => {
                return Ok(AppAction::RefreshCurrentSubtree)
            }
            KeyCode::Char('?') => self.show_help = true,
            _ => {}
        }
        Ok(AppAction::None)
    }

    fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            self.table_state.select(None);
            return;
        }
        let current = self.table_state.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.rows.len() as isize - 1) as usize;
        self.table_state.select(Some(next));
    }

    fn move_selection_page(&mut self, direction: isize) {
        let step = self.page_step.max(1) as isize;
        self.move_selection(step * direction);
    }

    fn move_selection_to_start(&mut self) {
        if self.rows.is_empty() {
            self.table_state.select(None);
            return;
        }
        self.table_state.select(Some(0));
    }

    fn move_selection_to_end(&mut self) {
        if self.rows.is_empty() {
            self.table_state.select(None);
            return;
        }
        self.table_state.select(Some(self.rows.len() - 1));
    }

    fn enter_selected(&mut self) -> Result<()> {
        let Some(path) = self
            .selected_row()
            .filter(|row| row.has_children())
            .map(|row| row.path.clone())
        else {
            return Ok(());
        };
        self.clear_marked_silently();
        self.current_path = path;
        self.refresh_rows()
    }

    fn go_parent(&mut self) -> Result<()> {
        let child_path = self.current_path.clone();
        let Some(parent) = Analysis::parent_path(&self.current_path) else {
            return Ok(());
        };
        self.clear_marked_silently();
        self.current_path = parent;
        self.refresh_rows_with_selection(Some(child_path))
    }

    fn copy_relative_path(&self) -> CopyAction {
        let target = self.copy_target();
        CopyAction {
            value: target.relative_path,
            label: String::from("relative path"),
        }
    }

    fn copy_absolute_path(&self) -> CopyAction {
        let target = self.copy_target();
        CopyAction {
            value: target.absolute_path,
            label: String::from("absolute path"),
        }
    }

    fn copy_target(&self) -> CopyTarget {
        if let Some(row) = self.selected_row() {
            let relative_path = if row.path.is_empty() {
                String::from(".")
            } else {
                row.path.clone()
            };
            return CopyTarget {
                relative_path,
                absolute_path: self.analysis.display_path(&row.path),
            };
        }

        let relative_path = if self.current_path.is_empty() {
            String::from(".")
        } else {
            self.current_path.clone()
        };
        CopyTarget {
            relative_path,
            absolute_path: self.analysis.display_path(&self.current_path),
        }
    }

    fn current_directory_path(&self) -> PathBuf {
        PathBuf::from(self.analysis.display_path(&self.current_path))
    }

    fn current_refresh_target_path(&self) -> String {
        self.selected_row()
            .filter(|row| row.has_children())
            .map(|row| row.path.clone())
            .unwrap_or_else(|| self.current_path.clone())
    }

    fn apply_refreshed_current_snapshot(
        &mut self,
        analysis: Analysis,
        current: SnapshotTree,
    ) -> Result<()> {
        let Some(refresh_state) = &mut self.refresh_state else {
            bail!("current refresh is not available in this mode");
        };
        refresh_state.current = current;

        let selected_path = self.selected_row().map(|row| row.path.clone());
        let current_path = self.current_path.clone();
        self.analysis = analysis;

        if !self.path_exists(&current_path) {
            self.current_path = nearest_existing_parent(&self.analysis, &current_path);
        }

        let visible_paths = self
            .marked_paths
            .iter()
            .filter(|path| self.path_exists(path))
            .cloned()
            .collect::<BTreeSet<_>>();
        self.marked_paths = visible_paths;
        self.refresh_rows_with_selection(selected_path)?;
        Ok(())
    }

    fn path_exists(&self, path: &str) -> bool {
        self.analysis.row_for_path(path, self.metric).is_ok()
    }

    fn set_status(&mut self, text: String, kind: StatusKind) {
        self.status_message = Some(StatusMessage { text, kind });
    }

    fn toggle_mark_selected_and_advance(&mut self) {
        let Some((path, name)) = self
            .selected_row()
            .map(|row| (row.path.clone(), row.name.clone()))
        else {
            return;
        };

        if self.marked_paths.insert(path.clone()) {
            self.set_status(format!("Marked {name}"), StatusKind::Info);
        } else {
            self.marked_paths.remove(&path);
            self.set_status(format!("Unmarked {name}"), StatusKind::Info);
        }

        self.move_selection(1);
    }

    fn clear_marked_silently(&mut self) {
        self.marked_paths.clear();
    }

    fn clear_marked_with_status(&mut self) {
        let count = self.marked_paths.len();
        self.marked_paths.clear();
        if count > 0 {
            self.set_status(format!("Cleared {count} marked entries"), StatusKind::Info);
        }
    }

    fn marked_summary(&self) -> Option<MarkedSummary> {
        let mut summary = MarkedSummary::default();

        for row in self
            .rows
            .iter()
            .filter(|row| self.marked_paths.contains(&row.path))
        {
            summary.count += 1;
            summary.baseline_size += row.baseline_size;
            summary.latest_size += row.latest_size;
            summary.delta += row.delta;
            summary.baseline_local_share += row.baseline_local_share;
            summary.latest_local_share += row.latest_local_share;
            summary.baseline_root_share += row.baseline_root_share();
            summary.latest_root_share += row.latest_root_share();

            match row.change_kind {
                ChangeKind::Added => summary.added += 1,
                ChangeKind::Removed => summary.removed += 1,
                ChangeKind::Changed => summary.changed += 1,
                ChangeKind::Unchanged => summary.unchanged += 1,
            }
        }

        (summary.count > 0).then_some(summary)
    }

    fn selected_detail(&self) -> Result<SelectedDetail> {
        let Some(selected) = self.selected_row() else {
            return Ok(SelectedDetail::empty());
        };

        let timeline_labels = compact_timeline(
            selected
                .timeline
                .iter()
                .map(|point| point.label.as_str())
                .collect::<Vec<_>>(),
        );
        let timeline_sizes = compact_timeline(
            selected
                .timeline
                .iter()
                .map(|point| format_size(point.size))
                .collect::<Vec<_>>(),
        );
        let timeline_shares = compact_timeline(
            selected
                .timeline
                .iter()
                .map(|point| format!("{:.1}%", point.local_share * 100.0))
                .collect::<Vec<_>>(),
        );

        Ok(SelectedDetail {
            is_empty: false,
            name_spans: styled_entry_name_spans(&selected.name, selected.kind),
            path_spans: styled_path_spans(
                &self.analysis.display_path(&selected.path),
                selected.kind,
            ),
            kind_label: selected.kind.label().to_string(),
            change_label: selected.change_kind.label().to_string(),
            change_kind: selected.change_kind,
            size_lines: vec![
                stat_line(
                    "Baseline",
                    format_size(selected.baseline_size),
                    Style::default().fg(Color::DarkGray),
                ),
                stat_line(
                    "Latest",
                    format_size(selected.latest_size),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                stat_line(
                    "Delta",
                    format_signed_size(selected.delta),
                    delta_style(selected.delta).add_modifier(Modifier::BOLD),
                ),
            ],
            share_lines: vec![
                share_line(
                    "Parent",
                    selected.baseline_local_share,
                    selected.latest_local_share,
                    selected.local_share_delta,
                ),
                share_line(
                    "Root",
                    selected.baseline_root_share(),
                    selected.latest_root_share(),
                    selected.root_share_delta(),
                ),
            ],
            timeline_lines: vec![
                stat_line(
                    "Labels",
                    timeline_labels,
                    Style::default().fg(Color::DarkGray),
                ),
                stat_line("Sizes", timeline_sizes, Style::default().fg(Color::Cyan)),
                stat_line(
                    "Parent",
                    timeline_shares,
                    Style::default().fg(Color::Yellow),
                ),
            ],
        })
    }

    fn render(&mut self, frame: &mut ratatui::Frame) -> Result<()> {
        let area = frame.area();
        frame.render_widget(Clear, area);
        let current_dir = self
            .analysis
            .row_for_path(&self.current_path, self.metric)?;

        let show_selected_panel = should_show_selected_panel(area);
        let chunks = if show_selected_panel {
            Layout::vertical([
                Constraint::Length(6),
                Constraint::Min(6),
                Constraint::Length(12),
                Constraint::Length(3),
            ])
            .split(area)
        } else {
            Layout::vertical([
                Constraint::Length(6),
                Constraint::Min(6),
                Constraint::Length(3),
            ])
            .split(area)
        };
        self.page_step = children_page_step(chunks[1]);

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "gdu-diff  ",
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("root ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.analysis.current_root_name().to_string(),
                    directory_style().add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled("snapshots ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.analysis.snapshot_count().to_string(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled("range ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.analysis.snapshot_range_label(),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw("  "),
                Span::styled("metric ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    self.metric.label(),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            stat_line_spans("Path", {
                let mut spans = styled_path_spans(
                    &self.analysis.display_path(&self.current_path),
                    crate::analysis::EntryKind::Dir,
                );
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    "visible ",
                    Style::default().fg(Color::DarkGray),
                ));
                spans.push(Span::styled(
                    if self.include_files {
                        "dirs + files"
                    } else {
                        "dirs only"
                    },
                    Style::default().fg(Color::Rgb(210, 210, 210)),
                ));
                spans.push(Span::raw("  "));
                spans.push(Span::styled("sort ", Style::default().fg(Color::DarkGray)));
                spans.push(Span::styled(
                    self.sort.label(),
                    Style::default().fg(Color::Rgb(120, 196, 255)),
                ));
                spans
            }),
            current_dir_summary_line(&current_dir),
            status_line(self.status_message.as_ref()),
        ])
        .block(Block::default().borders(Borders::ALL).title("Overview"));
        frame.render_widget(header, chunks[0]);

        let header_row = Row::new(vec![
            Cell::from("M"),
            Cell::from("Type"),
            Cell::from("Change"),
            Cell::from("Name"),
            Cell::from("Latest"),
            Cell::from("Delta"),
            Cell::from("Share"),
            Cell::from("ShareD"),
        ])
        .style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );

        if self.rows.is_empty() {
            let empty_children = Paragraph::new(children_empty_lines(self.include_files))
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("Children"));
            frame.render_widget(empty_children, chunks[1]);
        } else {
            let rows = self.rows.iter().map(|row| {
                let delta_style = delta_style(row.delta);
                let share_style = share_style(row.local_share_delta);
                let mark = if self.marked_paths.contains(&row.path) {
                    Span::styled(
                        "*",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw("")
                };
                Row::new(vec![
                    Cell::from(mark),
                    Cell::from(row.kind.short()),
                    Cell::from(Span::styled(
                        row.change_kind.short(),
                        change_kind_style(row.change_kind).add_modifier(Modifier::BOLD),
                    )),
                    Cell::from(Line::from(styled_entry_name_spans(&row.name, row.kind))),
                    Cell::from(format_size(row.latest_size)),
                    Cell::from(Span::styled(format_signed_size(row.delta), delta_style)),
                    Cell::from(format!("{:.1}%", row.latest_local_share * 100.0)),
                    Cell::from(Span::styled(
                        format_share_delta(row.local_share_delta),
                        share_style,
                    )),
                ])
            });

            let table = Table::new(
                rows,
                [
                    Constraint::Length(3),
                    Constraint::Length(6),
                    Constraint::Length(3),
                    Constraint::Min(24),
                    Constraint::Length(12),
                    Constraint::Length(12),
                    Constraint::Length(8),
                    Constraint::Length(8),
                ],
            )
            .header(header_row)
            .block(Block::default().borders(Borders::ALL).title("Children"))
            .row_highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(table, chunks[1], &mut self.table_state);
        }

        if show_selected_panel {
            self.render_selected_panel(frame, chunks[2])?;
        }

        let help = Paragraph::new(Line::from(
            "j/k move  space mark  g/G ends  l open  h up  ? help  q quit",
        ))
        .block(Block::default().borders(Borders::ALL).title("Keys"));
        frame.render_widget(help, chunks[if show_selected_panel { 3 } else { 2 }]);

        if self.show_help {
            self.render_help_overlay(frame);
        }

        Ok(())
    }

    fn render_selected_panel(&self, frame: &mut ratatui::Frame, area: Rect) -> Result<()> {
        let block = Block::default().borders(Borders::ALL).title("Selected");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let detail = self.selected_detail()?;
        if detail.is_empty {
            let empty = Paragraph::new(Line::from(Span::styled(
                "No entries in this directory.",
                Style::default().fg(Color::DarkGray),
            )))
            .block(Block::default().borders(Borders::ALL).title("Item"));
            frame.render_widget(empty, inner);
            return Ok(());
        }

        let columns = Layout::horizontal([
            Constraint::Percentage(28),
            Constraint::Percentage(32),
            Constraint::Percentage(40),
        ])
        .split(inner);
        let center =
            Layout::vertical([Constraint::Length(6), Constraint::Min(4)]).split(columns[1]);
        let right = Layout::vertical([Constraint::Length(4), Constraint::Min(5)]).split(columns[2]);

        self.render_marked_panel(frame, columns[0]);
        self.render_selected_item_panel(frame, center[0], &detail);

        let size = Paragraph::new(detail.size_lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Size"));
        frame.render_widget(size, center[1]);

        let share = Paragraph::new(detail.share_lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Share"));
        frame.render_widget(share, right[0]);

        let timeline = Paragraph::new(detail.timeline_lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Timeline"));
        frame.render_widget(timeline, right[1]);

        Ok(())
    }

    fn render_selected_item_panel(
        &self,
        frame: &mut ratatui::Frame,
        area: Rect,
        detail: &SelectedDetail,
    ) {
        let block = Block::default().borders(Borders::ALL).title("Item");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

        render_key_value_row(
            frame,
            rows[0],
            "Name",
            Line::from(detail.name_spans.clone()),
            0,
        );
        render_key_value_row(
            frame,
            rows[1],
            "Path",
            Line::from(detail.path_spans.clone()),
            marquee_offset(
                spans_width(&detail.path_spans),
                value_area_width(rows[1], DETAIL_LABEL_WIDTH),
            ),
        );
        render_key_value_row(
            frame,
            rows[2],
            "Type",
            Line::from(Span::styled(
                detail.kind_label.clone(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )),
            0,
        );
        render_key_value_row(
            frame,
            rows[3],
            "Change",
            Line::from(Span::styled(
                detail.change_label.clone(),
                change_kind_style(detail.change_kind).add_modifier(Modifier::BOLD),
            )),
            0,
        );
    }

    fn render_help_overlay(&self, frame: &mut ratatui::Frame) {
        let area = centered_rect(frame.area(), 78, 74);
        let lines = vec![
            Line::from(Span::styled(
                "Navigation",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("j/k or arrows move, ,/. page, g/G jump to first/last"),
            Line::from("l or Enter opens a directory, h or Backspace goes to parent"),
            Line::from(""),
            Line::from(Span::styled(
                "View",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("d delta sort (default), s size sort, p share sort, n name sort"),
            Line::from("a toggles disk/apparent, f toggles files"),
            Line::from(""),
            Line::from(Span::styled(
                "Actions",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("Space toggles the current row in the marked set, then moves down"),
            Line::from("c copies relative path, C copies absolute path"),
            Line::from("b opens a shell in the current view directory"),
            Line::from("r rescans the selected directory, or the current view when nothing is selected"),
            Line::from("Esc clears marked rows, or closes this help"),
            Line::from("Entering or leaving a directory also clears the marked set"),
            Line::from("q quits"),
        ];
        let help = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Help"));
        frame.render_widget(Clear, area);
        frame.render_widget(help, area);
    }

    fn render_marked_panel(&self, frame: &mut ratatui::Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title("Marked");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(summary) = self.marked_summary() else {
            let lines = vec![
                Line::from(Span::styled(
                    "No marked entries.",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "Press Space to mark the current row.",
                    Style::default().fg(Color::Rgb(170, 170, 170)),
                )),
            ];
            let panel = Paragraph::new(lines).wrap(Wrap { trim: false });
            frame.render_widget(panel, inner);
            return;
        };

        let panel = Paragraph::new(vec![
            stat_line(
                "Items",
                summary.count.to_string(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            marked_counts_line(&summary),
            stat_line(
                "Baseline",
                format_size(summary.baseline_size),
                Style::default().fg(Color::DarkGray),
            ),
            stat_line(
                "Latest",
                format_size(summary.latest_size),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            stat_line(
                "Delta",
                format_signed_size(summary.delta),
                delta_style(summary.delta).add_modifier(Modifier::BOLD),
            ),
            share_line(
                "Parent",
                summary.baseline_local_share,
                summary.latest_local_share,
                summary.latest_local_share - summary.baseline_local_share,
            ),
            share_line(
                "Root",
                summary.baseline_root_share,
                summary.latest_root_share,
                summary.latest_root_share - summary.baseline_root_share,
            ),
        ])
        .wrap(Wrap { trim: false });
        frame.render_widget(panel, inner);
    }
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut redraw = interval(Duration::from_millis(250));

    loop {
        terminal.draw(|frame| {
            let _ = app.render(frame);
        })?;

        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        match app.on_key(key.code)? {
                            AppAction::None => {}
                            AppAction::Copy(copy) => copy_to_clipboard(app, copy).await,
                            AppAction::OpenShell(path) => open_shell(terminal, app, &path).await?,
                            AppAction::RefreshCurrentSubtree => {
                                if let Err(error) = refresh_current_subtree(terminal, app).await {
                                    app.set_status(
                                        format!("Refresh failed: {error:#}"),
                                        StatusKind::Error,
                                    );
                                }
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error).context("failed to read terminal event"),
                    None => return Ok(()),
                }
            }
            _ = redraw.tick() => {}
        }
    }
}

enum RefreshScanUpdate {
    Detail(String),
    Finished(Result<()>),
}

async fn refresh_current_subtree(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let refresh_path = app.current_refresh_target_path();
    let refresh_display_path = app.analysis.display_path(&refresh_path);
    let refresh_target = {
        let refresh_state = app
            .refresh_state
            .as_ref()
            .context("current refresh is not available in this mode")?;
        absolute_refresh_target(&refresh_state.target_dir, &refresh_path)
    };

    let mut loading = LoadingState::new(
        "gdu-diff",
        vec![
            LoadingStep::new("Scan current directory", 8.0),
            LoadingStep::new("Load refreshed snapshot", 4.0),
            LoadingStep::new("Rebuild analysis", 5.0),
            LoadingStep::new("Restore view", 1.0),
        ],
    );
    loading.set_step(
        1,
        String::from("Scan current directory"),
        format!("Launching gdu-go for {}", refresh_target.display()),
    );
    draw_loading_in_terminal(terminal, &loading)?;

    let temp_dir = tempdir().context("failed to create temporary directory")?;
    let output = temp_dir.path().join("current.json");
    export_snapshot_with_fake_progress_in_terminal(
        &refresh_target,
        &output,
        terminal,
        &mut loading,
        format!("Launching gdu-go for {}", refresh_target.display()),
    )
    .await?;

    loading.set_step(
        2,
        String::from("Load refreshed snapshot"),
        String::from("Parsing generated JSON"),
    );
    draw_loading_in_terminal(terminal, &loading)?;
    let refreshed_subtree = SnapshotTree::load_with_label(output, String::from("current")).await?;
    loading.set_step_progress(1.0);
    draw_loading_in_terminal(terminal, &loading)?;

    loading.set_step(
        3,
        String::from("Rebuild analysis"),
        format!("Refreshing {}", refresh_display_path),
    );
    draw_loading_in_terminal(terminal, &loading)?;
    let mut last_loading_draw = Some(Instant::now());

    let (baseline, mut next_current) = {
        let refresh_state = app
            .refresh_state
            .as_ref()
            .context("current refresh is not available in this mode")?;
        (refresh_state.baseline.clone(), refresh_state.current.clone())
    };
    next_current.replace_subtree(&refresh_path, refreshed_subtree)?;

    let analysis = Analysis::new_with_progress(vec![baseline, next_current.clone()], |progress| {
        loading.set_step_progress(progress.overall_progress());
        loading.set_detail(format!(
            "Indexing snapshot {}/{}: {} ({:.0}%)\nCurrent path: {}",
            progress.snapshot_index,
            progress.snapshot_total,
            progress.snapshot_label,
            progress.snapshot_progress * 100.0,
            progress.current_path
        ));
        let _ = draw_loading_throttled_in_terminal(
            terminal,
            &loading,
            REFRESH_DRAW_THROTTLE,
            &mut last_loading_draw,
        );
    })?;
    loading.set_step_progress(1.0);
    draw_loading_in_terminal(terminal, &loading)?;

    loading.set_step(
        4,
        String::from("Restore view"),
        format!("Restoring {}", app.analysis.display_path(&app.current_path)),
    );
    draw_loading_in_terminal(terminal, &loading)?;
    app.apply_refreshed_current_snapshot(analysis, next_current)?;
    loading.set_step_progress(1.0);
    draw_loading_in_terminal(terminal, &loading)?;

    app.set_status(format!("Refreshed {refresh_display_path}"), StatusKind::Info);
    Ok(())
}

async fn copy_to_clipboard(app: &mut App, copy: CopyAction) {
    let value = copy.value;
    let label = copy.label;
    let result = tokio::task::spawn_blocking(move || {
        ClipboardContext::new()
            .and_then(|clipboard| clipboard.set_text(value.clone()))
            .map_err(|error| error.to_string())?;
        Ok::<String, String>(value)
    })
    .await;

    match result {
        Ok(Ok(value)) => {
            app.set_status(format!("Copied {label}: {value}"), StatusKind::Info);
        }
        Ok(Err(error)) => {
            app.set_status(
                format!("Failed to copy {label}: {error}"),
                StatusKind::Error,
            );
        }
        Err(error) => {
            app.set_status(
                format!("Failed to copy {label}: {error}"),
                StatusKind::Error,
            );
        }
    }
}

async fn open_shell(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    path: &Path,
) -> Result<()> {
    suspend_terminal(terminal)?;
    let path = path.to_path_buf();
    let display_path = path.clone();
    let shell_result = tokio::task::spawn_blocking(move || spawn_shell(&path))
        .await
        .context("shell task panicked")?;
    let resume_result = resume_terminal(terminal);

    match (shell_result, resume_result) {
        (Ok(()), Ok(())) => {
            app.set_status(
                format!("Returned from shell: {}", display_path.display()),
                StatusKind::Info,
            );
            Ok(())
        }
        (Err(shell_error), Ok(())) => {
            app.set_status(
                format!("Failed to open shell: {shell_error}"),
                StatusKind::Error,
            );
            Ok(())
        }
        (_, Err(resume_error)) => Err(resume_error),
    }
}

fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    // Reset ratatui's diff state because the shell session replaced the screen contents.
    terminal.clear()?;
    terminal.hide_cursor()?;
    Ok(())
}

fn spawn_shell(path: &Path) -> Result<()> {
    let shell = if cfg!(windows) {
        std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd".into())
    } else {
        std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into())
    };
    let status = Command::new(&shell)
        .current_dir(path)
        .status()
        .with_context(|| format!("failed to start shell in {}", path.display()))?;
    if status.success() {
        return Ok(());
    }
    bail!("shell exited with status {status}");
}

fn format_size(size: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_signed_size(delta: i64) -> String {
    let magnitude = delta.unsigned_abs();
    if delta >= 0 {
        format!("+{}", format_size(magnitude))
    } else {
        format!("-{}", format_size(magnitude))
    }
}

fn format_share_delta(delta: f64) -> String {
    format!("{:+.1}pp", delta * 100.0)
}

fn render_loading(frame: &mut ratatui::Frame, loading: &LoadingState) {
    let area = frame.area();
    frame.render_widget(Clear, area);

    let block = Block::default().borders(Borders::ALL).title("Loading");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let steps_height = loading.total_steps().min(6) as u16 + 2;
    let sections = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(5),
        Constraint::Length(steps_height),
        Constraint::Length(2),
    ])
    .split(inner);

    let title = Paragraph::new(Line::from(vec![
        Span::styled(
            &loading.title,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("step {}/{}", loading.current_step, loading.total_steps()),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    frame.render_widget(title, sections[0]);

    let ratio = loading.progress_ratio();
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title("Progress"))
        .gauge_style(Style::default().fg(Color::Cyan))
        .ratio(ratio)
        .label(format!("{:.0}%", ratio * 100.0));
    frame.render_widget(gauge, sections[1]);

    let action = Paragraph::new(Line::from(vec![
        Span::styled("Action ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            loading.current_action(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(action, sections[2]);

    let detail = Paragraph::new(loading.detail.as_str())
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title("Status"));
    frame.render_widget(detail, sections[3]);

    let step_bar_width = usize::from(sections[4].width.saturating_sub(28)).clamp(8, 28);
    let step_lines = loading
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let is_current = loading.current_step == index + 1;
            let is_completed = step.progress >= 1.0;
            let (marker, marker_style, action_style) = if is_completed {
                (
                    "[x]",
                    Style::default().fg(Color::Green),
                    Style::default().fg(Color::Green),
                )
            } else if is_current {
                (
                    "[>]",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                (
                    "[ ]",
                    Style::default().fg(Color::DarkGray),
                    Style::default().fg(Color::DarkGray),
                )
            };
            Line::from(vec![
                Span::styled(format!("{marker} {:>2} ", index + 1), marker_style),
                Span::styled(
                    format!("[{}]", mini_progress_bar(step.progress, step_bar_width)),
                    marker_style,
                ),
                Span::raw(" "),
                Span::styled(format!("{:>3.0}%", step.progress * 100.0), marker_style),
                Span::raw(" "),
                Span::styled(step.action.as_str(), action_style),
            ])
        })
        .collect::<Vec<_>>();
    let completed = Paragraph::new(step_lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title("Steps"));
    frame.render_widget(completed, sections[4]);

    let footer = Paragraph::new(Line::from(vec![
        Span::styled("Elapsed ", Style::default().fg(Color::DarkGray)),
        Span::raw(format_duration(loading.started_at.elapsed())),
    ]));
    frame.render_widget(footer, sections[5]);
}

fn delta_style(delta: i64) -> Style {
    if delta > 0 {
        Style::default().fg(Color::Red)
    } else if delta < 0 {
        Style::default().fg(Color::Green)
    } else {
        Style::default()
    }
}

fn mini_progress_bar(progress: f64, width: usize) -> String {
    let width = width.max(1);
    let filled = ((progress.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    let mut bar = String::with_capacity(width);
    bar.push_str(&"#".repeat(filled));
    bar.push_str(&"-".repeat(width - filled));
    bar
}

fn draw_loading_in_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    loading: &LoadingState,
) -> Result<()> {
    terminal.draw(|frame| render_loading(frame, loading))?;
    Ok(())
}

fn draw_loading_throttled_in_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    loading: &LoadingState,
    throttle: Duration,
    last_draw: &mut Option<Instant>,
) -> Result<()> {
    if last_draw.is_some_and(|last| last.elapsed() < throttle) {
        return Ok(());
    }
    terminal.draw(|frame| render_loading(frame, loading))?;
    *last_draw = Some(Instant::now());
    Ok(())
}

async fn export_snapshot_with_fake_progress_in_terminal(
    target: &Path,
    output: &Path,
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    loading: &mut LoadingState,
    detail: String,
) -> Result<()> {
    let target = target.to_path_buf();
    let output = output.to_path_buf();
    let display_target = target.display().to_string();
    let (sender, mut receiver) = mpsc::unbounded_channel();

    loading.set_step_progress(0.0);
    loading.set_detail(detail);
    draw_loading_in_terminal(terminal, loading)?;

    let task = tokio::spawn(async move {
        let progress_sender = sender.clone();
        let result = export_snapshot_with_progress(&target, &output, |progress| {
            let _ = progress_sender.send(RefreshScanUpdate::Detail(progress.to_string()));
        })
        .await;
        let _ = sender.send(RefreshScanUpdate::Finished(result));
    });

    let started_at = Instant::now();
    let mut ticker = interval(REFRESH_PROGRESS_TICK);
    let mut last_draw = Some(Instant::now());
    loop {
        tokio::select! {
            maybe_update = receiver.recv() => {
                match maybe_update {
                    Some(RefreshScanUpdate::Detail(detail)) => loading.set_detail(detail),
                    Some(RefreshScanUpdate::Finished(result)) => {
                        loading.set_step_progress(1.0);
                        draw_loading_in_terminal(terminal, loading)?;
                        task.await.context("background gdu export task panicked")?;
                        return result;
                    }
                    None => {
                        task.await.context("background gdu export task panicked")?;
                        bail!("background gdu export exited unexpectedly while scanning {display_target}");
                    }
                }
            }
            _ = ticker.tick() => {
                loading.set_step_progress(refresh_fake_progress(started_at.elapsed()));
                let _ = draw_loading_throttled_in_terminal(
                    terminal,
                    loading,
                    REFRESH_DRAW_THROTTLE,
                    &mut last_draw,
                );
            }
        }
    }
}

fn refresh_fake_progress(elapsed: Duration) -> f64 {
    let ratio =
        (elapsed.as_secs_f64() / REFRESH_PROGRESS_SETTLE_TIME.as_secs_f64()).clamp(0.0, 1.0);
    let eased_ratio = 1.0 - (1.0 - ratio).powi(2);
    (REFRESH_PROGRESS_CAP * eased_ratio).clamp(0.0, REFRESH_PROGRESS_CAP)
}

fn absolute_refresh_target(root: &Path, relative_path: &str) -> PathBuf {
    if relative_path.is_empty() {
        return root.to_path_buf();
    }
    root.join(relative_path)
}

fn nearest_existing_parent(analysis: &Analysis, path: &str) -> String {
    let mut current = path.to_string();
    loop {
        if analysis.row_for_path(&current, SizeMetric::Disk).is_ok() {
            return current;
        }

        let Some(parent) = Analysis::parent_path(&current) else {
            return String::new();
        };
        current = parent;
    }
}

fn share_style(delta: f64) -> Style {
    if delta > 0.0 {
        Style::default().fg(Color::Red)
    } else if delta < 0.0 {
        Style::default().fg(Color::Green)
    } else {
        Style::default()
    }
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

fn change_kind_style(change_kind: ChangeKind) -> Style {
    match change_kind {
        ChangeKind::Added => Style::default().fg(Color::Green),
        ChangeKind::Removed => Style::default().fg(Color::Red),
        ChangeKind::Changed => Style::default().fg(Color::Yellow),
        ChangeKind::Unchanged => Style::default().fg(Color::DarkGray),
    }
}

fn compact_timeline<I>(items: I) -> String
where
    I: IntoIterator,
    I::Item: ToString,
{
    let values = items
        .into_iter()
        .map(|item| item.to_string())
        .collect::<Vec<_>>();
    if values.len() <= 6 {
        return values.join(" | ");
    }
    let mut compact = Vec::with_capacity(6);
    compact.extend(values.iter().take(3).cloned());
    compact.push(String::from("..."));
    compact.extend(values.iter().rev().take(2).cloned().rev());
    compact.join(" | ")
}

struct SelectedDetail {
    is_empty: bool,
    name_spans: Vec<Span<'static>>,
    path_spans: Vec<Span<'static>>,
    kind_label: String,
    change_label: String,
    change_kind: ChangeKind,
    size_lines: Vec<Line<'static>>,
    share_lines: Vec<Line<'static>>,
    timeline_lines: Vec<Line<'static>>,
}

#[derive(Default)]
struct MarkedSummary {
    count: usize,
    added: usize,
    removed: usize,
    changed: usize,
    unchanged: usize,
    baseline_size: u64,
    latest_size: u64,
    delta: i64,
    baseline_local_share: f64,
    latest_local_share: f64,
    baseline_root_share: f64,
    latest_root_share: f64,
}

enum AppAction {
    None,
    Copy(CopyAction),
    OpenShell(PathBuf),
    RefreshCurrentSubtree,
}

#[derive(Clone)]
pub struct RefreshState {
    baseline: SnapshotTree,
    current: SnapshotTree,
    target_dir: PathBuf,
}

impl RefreshState {
    pub fn new(baseline: SnapshotTree, current: SnapshotTree, target_dir: PathBuf) -> Self {
        Self {
            baseline,
            current,
            target_dir,
        }
    }
}

impl SelectedDetail {
    fn empty() -> Self {
        Self {
            is_empty: true,
            name_spans: Vec::new(),
            path_spans: Vec::new(),
            kind_label: String::new(),
            change_label: String::new(),
            change_kind: ChangeKind::Unchanged,
            size_lines: Vec::new(),
            share_lines: Vec::new(),
            timeline_lines: Vec::new(),
        }
    }
}

const DETAIL_LABEL_WIDTH: u16 = 10;

fn stat_line(
    label: impl Into<String>,
    value: impl Into<String>,
    value_style: Style,
) -> Line<'static> {
    stat_line_spans(label, vec![Span::styled(value.into(), value_style)])
}

fn stat_line_spans(label: impl Into<String>, value_spans: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!(
            "{:<DETAIL_LABEL_WIDTH$}",
            label.into(),
            DETAIL_LABEL_WIDTH = DETAIL_LABEL_WIDTH as usize
        ),
        Style::default().fg(Color::DarkGray),
    )];
    spans.extend(value_spans);
    Line::from(spans)
}

fn render_key_value_row(
    frame: &mut ratatui::Frame,
    area: Rect,
    label: &str,
    value: Line<'static>,
    scroll_x: u16,
) {
    let parts = Layout::horizontal([Constraint::Length(DETAIL_LABEL_WIDTH), Constraint::Min(1)])
        .split(area);
    let label = Paragraph::new(Line::from(Span::styled(
        format!(
            "{label:<DETAIL_LABEL_WIDTH$}",
            DETAIL_LABEL_WIDTH = DETAIL_LABEL_WIDTH as usize
        ),
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(label, parts[0]);
    let value = Paragraph::new(value).scroll((0, scroll_x));
    frame.render_widget(value, parts[1]);
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

fn value_area_width(area: Rect, label_width: u16) -> usize {
    usize::from(area.width.saturating_sub(label_width))
}

fn marquee_offset(content_width: usize, viewport_width: usize) -> u16 {
    if viewport_width == 0 || content_width <= viewport_width {
        return 0;
    }

    let overflow = content_width - viewport_width;
    let pause = 4usize;
    let cycle = pause + overflow + pause + overflow;
    let tick = marquee_tick() % cycle;

    let offset = if tick < pause {
        0
    } else if tick < pause + overflow {
        tick - pause
    } else if tick < pause + overflow + pause {
        overflow
    } else {
        overflow - (tick - pause - overflow - pause)
    };

    offset.min(u16::MAX as usize) as u16
}

fn marquee_tick() -> usize {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.as_millis() / 250) as usize)
        .unwrap_or_default()
}

fn share_line(label: &str, baseline: f64, latest: f64, delta: f64) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<8}"), Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(
                "{baseline:.1}% -> {latest:.1}%  ",
                baseline = baseline * 100.0,
                latest = latest * 100.0
            ),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format_share_delta(delta),
            share_style(delta).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn marked_counts_line(summary: &MarkedSummary) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(
                "{:<DETAIL_LABEL_WIDTH$}",
                "Counts",
                DETAIL_LABEL_WIDTH = DETAIL_LABEL_WIDTH as usize
            ),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("+{}  ", summary.added),
            change_kind_style(ChangeKind::Added).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("-{}  ", summary.removed),
            change_kind_style(ChangeKind::Removed).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("~{}  ", summary.changed),
            change_kind_style(ChangeKind::Changed).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("={}", summary.unchanged),
            change_kind_style(ChangeKind::Unchanged).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn current_dir_summary_line(row: &RowData) -> Line<'static> {
    let mut spans = vec![
        Span::styled("Current ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format_size(row.baseline_size),
            Style::default().fg(Color::Rgb(174, 174, 174)),
        ),
        Span::styled(" -> ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format_size(row.latest_size),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format_signed_size(row.delta),
            delta_style(row.delta).add_modifier(Modifier::BOLD),
        ),
    ];

    if !row.path.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("P ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!(
                "{:.1}% -> {:.1}% ",
                row.baseline_local_share * 100.0,
                row.latest_local_share * 100.0
            ),
            Style::default().fg(Color::White),
        ));
        spans.push(Span::styled(
            format_share_delta(row.local_share_delta),
            share_style(row.local_share_delta).add_modifier(Modifier::BOLD),
        ));
    }

    spans.push(Span::raw("  "));
    spans.push(Span::styled("R ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(
        format!(
            "{:.1}% -> {:.1}% ",
            row.baseline_root_share() * 100.0,
            row.latest_root_share() * 100.0
        ),
        Style::default().fg(Color::White),
    ));
    spans.push(Span::styled(
        format_share_delta(row.root_share_delta()),
        share_style(row.root_share_delta()).add_modifier(Modifier::BOLD),
    ));

    Line::from(spans)
}

fn status_line(status: Option<&StatusMessage>) -> Line<'static> {
    let Some(status) = status else {
        return Line::from(vec![
            Span::styled("Status  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Ready", Style::default().fg(Color::Rgb(170, 170, 170))),
        ]);
    };

    Line::from(vec![
        Span::styled("Status  ", Style::default().fg(Color::DarkGray)),
        Span::styled(status.text.clone(), status.kind.style()),
    ])
}

fn children_empty_lines(include_files: bool) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        "This directory is empty.",
        Style::default().fg(Color::DarkGray),
    ))];

    if !include_files {
        lines.push(Line::from(Span::styled(
            "If it only contains files, press f to show them.",
            Style::default().fg(Color::Rgb(170, 170, 170)),
        )));
    }

    lines
}

fn children_page_step(area: Rect) -> usize {
    usize::from(area.height.saturating_sub(3)).max(1)
}

fn should_show_selected_panel(area: Rect) -> bool {
    area.width >= 110 && area.height >= 28
}

fn centered_rect(area: Rect, width_percent: u16, height_percent: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - height_percent) / 2),
        Constraint::Percentage(height_percent),
        Constraint::Percentage((100 - height_percent) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - width_percent) / 2),
        Constraint::Percentage(width_percent),
        Constraint::Percentage((100 - width_percent) / 2),
    ])
    .split(vertical[1])[1]
}

fn styled_entry_name_spans(name: &str, kind: crate::analysis::EntryKind) -> Vec<Span<'static>> {
    match kind {
        crate::analysis::EntryKind::Dir | crate::analysis::EntryKind::Mixed => vec![
            Span::styled(
                name.to_string(),
                directory_style().add_modifier(Modifier::BOLD),
            ),
            Span::styled("/", separator_style()),
        ],
        crate::analysis::EntryKind::File => styled_file_name_spans(name),
    }
}

fn styled_path_spans(path: &str, final_kind: crate::analysis::EntryKind) -> Vec<Span<'static>> {
    if path == "/" {
        return vec![Span::styled("/", separator_style())];
    }

    let mut spans = Vec::new();
    let has_root = path.starts_with('/');
    let components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();

    if has_root {
        spans.push(Span::styled("/", separator_style()));
    }

    for (index, component) in components.iter().enumerate() {
        let is_last = index + 1 == components.len();
        let kind = if is_last {
            final_kind
        } else {
            crate::analysis::EntryKind::Dir
        };
        let mut component_spans = styled_entry_name_spans(component, kind);
        if !is_last
            && matches!(
                kind,
                crate::analysis::EntryKind::Dir | crate::analysis::EntryKind::Mixed
            )
        {
            component_spans.pop();
        }
        spans.extend(component_spans);

        if !is_last {
            spans.push(Span::styled("/", separator_style()));
        }
    }

    spans
}

fn styled_file_name_spans(name: &str) -> Vec<Span<'static>> {
    if name.starts_with('.') {
        return vec![Span::styled(
            name.to_string(),
            hidden_style().add_modifier(Modifier::BOLD),
        )];
    }

    let Some((stem, extension)) = name.rsplit_once('.') else {
        return vec![Span::styled(
            name.to_string(),
            regular_file_style().add_modifier(Modifier::BOLD),
        )];
    };

    if stem.is_empty() || extension.is_empty() {
        return vec![Span::styled(
            name.to_string(),
            regular_file_style().add_modifier(Modifier::BOLD),
        )];
    }

    vec![
        Span::styled(
            stem.to_string(),
            regular_file_style().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(".{extension}"),
            extension_style(extension).add_modifier(Modifier::BOLD),
        ),
    ]
}

fn directory_style() -> Style {
    Style::default().fg(Color::Rgb(36, 148, 255))
}

fn hidden_style() -> Style {
    Style::default().fg(Color::Rgb(198, 198, 198))
}

fn regular_file_style() -> Style {
    Style::default().fg(Color::Rgb(235, 235, 235))
}

fn separator_style() -> Style {
    Style::default().fg(Color::Rgb(214, 214, 214))
}

fn extension_style(extension: &str) -> Style {
    match extension {
        "json" | "jsonl" | "json5" => Style::default().fg(Color::Rgb(215, 0, 255)),
        "toml" | "yaml" | "yml" => Style::default().fg(Color::Rgb(255, 176, 0)),
        "rs" | "go" | "py" | "sh" | "bash" | "zsh" => {
            Style::default().fg(Color::Rgb(107, 214, 114))
        }
        "md" | "txt" => Style::default().fg(Color::Rgb(224, 224, 224)),
        _ => regular_file_style(),
    }
}

struct CopyTarget {
    relative_path: String,
    absolute_path: String,
}

struct CopyAction {
    value: String,
    label: String,
}

struct StatusMessage {
    text: String,
    kind: StatusKind,
}

enum StatusKind {
    Info,
    Error,
}

impl StatusKind {
    fn style(&self) -> Style {
        match self {
            Self::Info => Style::default().fg(Color::Rgb(170, 170, 170)),
            Self::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anyhow::Result;
    use crossterm::event::KeyCode;

    use crate::analysis::{Analysis, SizeMetric, SortMode};
    use crate::gdu::SnapshotTree;

    use ratatui::layout::Rect;

    use super::{App, AppAction, RefreshState, should_show_selected_panel};

    #[test]
    fn go_parent_reselects_the_directory_we_came_from() -> Result<()> {
        let first = SnapshotTree::from_json_str(
            "first".into(),
            PathBuf::from("first.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},[{"name":"a","mtime":1},{"name":"one.bin","asize":10,"dsize":10,"mtime":1}],{"name":"b.bin","asize":5,"dsize":5,"mtime":1}]]"#,
        )?;
        let second = SnapshotTree::from_json_str(
            "second".into(),
            PathBuf::from("second.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},[{"name":"a","mtime":1},{"name":"one.bin","asize":20,"dsize":20,"mtime":1}],{"name":"b.bin","asize":7,"dsize":7,"mtime":1}]]"#,
        )?;

        let analysis = Analysis::new(vec![first, second])?;
        let mut app = App::new(analysis, SizeMetric::Disk, true)?;
        app.sort = SortMode::Name;
        app.refresh_rows()?;
        let selected_index = app
            .rows
            .iter()
            .position(|row| row.path == "a")
            .expect("directory a should exist");
        app.table_state.select(Some(selected_index));

        app.enter_selected()?;
        app.go_parent()?;

        assert_eq!(app.current_path, "");
        assert_eq!(app.selected_row().map(|row| row.path.as_str()), Some("a"));
        Ok(())
    }

    #[test]
    fn space_marks_and_escape_clears_selection() -> Result<()> {
        let first = SnapshotTree::from_json_str(
            "first".into(),
            PathBuf::from("first.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},{"name":"a.bin","asize":10,"dsize":10,"mtime":1},{"name":"b.bin","asize":5,"dsize":5,"mtime":1}]]"#,
        )?;
        let second = SnapshotTree::from_json_str(
            "second".into(),
            PathBuf::from("second.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},{"name":"a.bin","asize":20,"dsize":20,"mtime":1},{"name":"b.bin","asize":7,"dsize":7,"mtime":1}]]"#,
        )?;

        let analysis = Analysis::new(vec![first, second])?;
        let mut app = App::new(analysis, SizeMetric::Disk, true)?;

        assert!(matches!(app.on_key(KeyCode::Char(' '))?, AppAction::None));
        assert_eq!(app.marked_paths.len(), 1);
        assert_eq!(
            app.selected_row().map(|row| row.path.as_str()),
            Some("b.bin")
        );

        assert!(matches!(app.on_key(KeyCode::Esc)?, AppAction::None));
        assert!(app.marked_paths.is_empty());
        Ok(())
    }

    #[test]
    fn space_advances_selection_when_unmarking() -> Result<()> {
        let first = SnapshotTree::from_json_str(
            "first".into(),
            PathBuf::from("first.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},{"name":"a.bin","asize":10,"dsize":10,"mtime":1},{"name":"b.bin","asize":5,"dsize":5,"mtime":1},{"name":"c.bin","asize":2,"dsize":2,"mtime":1}]]"#,
        )?;
        let second = SnapshotTree::from_json_str(
            "second".into(),
            PathBuf::from("second.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},{"name":"a.bin","asize":20,"dsize":20,"mtime":1},{"name":"b.bin","asize":7,"dsize":7,"mtime":1},{"name":"c.bin","asize":3,"dsize":3,"mtime":1}]]"#,
        )?;

        let analysis = Analysis::new(vec![first, second])?;
        let mut app = App::new(analysis, SizeMetric::Disk, true)?;
        app.sort = SortMode::Name;
        app.refresh_rows()?;
        app.table_state.select(Some(0));

        assert!(matches!(app.on_key(KeyCode::Char(' '))?, AppAction::None));
        assert_eq!(
            app.selected_row().map(|row| row.path.as_str()),
            Some("b.bin")
        );
        assert_eq!(app.marked_paths.len(), 1);

        app.table_state.select(Some(0));
        assert!(matches!(app.on_key(KeyCode::Char(' '))?, AppAction::None));
        assert!(app.marked_paths.is_empty());
        assert_eq!(
            app.selected_row().map(|row| row.path.as_str()),
            Some("b.bin")
        );

        Ok(())
    }

    #[test]
    fn entering_directory_clears_marked_selection() -> Result<()> {
        let first = SnapshotTree::from_json_str(
            "first".into(),
            PathBuf::from("first.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},[{"name":"a","mtime":1},{"name":"one.bin","asize":10,"dsize":10,"mtime":1}],{"name":"b.bin","asize":5,"dsize":5,"mtime":1}]]"#,
        )?;
        let second = SnapshotTree::from_json_str(
            "second".into(),
            PathBuf::from("second.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},[{"name":"a","mtime":1},{"name":"one.bin","asize":20,"dsize":20,"mtime":1}],{"name":"b.bin","asize":7,"dsize":7,"mtime":1}]]"#,
        )?;

        let analysis = Analysis::new(vec![first, second])?;
        let mut app = App::new(analysis, SizeMetric::Disk, true)?;
        app.sort = SortMode::Name;
        app.refresh_rows()?;
        let selected_index = app
            .rows
            .iter()
            .position(|row| row.path == "a")
            .expect("directory a should exist");
        app.table_state.select(Some(selected_index));

        let _ = app.on_key(KeyCode::Char(' '))?;
        assert_eq!(app.marked_paths.len(), 1);
        app.table_state.select(Some(selected_index));

        let _ = app.on_key(KeyCode::Enter)?;
        assert!(app.marked_paths.is_empty());
        Ok(())
    }

    #[test]
    fn changing_sort_keeps_marked_selection() -> Result<()> {
        let first = SnapshotTree::from_json_str(
            "first".into(),
            PathBuf::from("first.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},{"name":"b.bin","asize":10,"dsize":10,"mtime":1},{"name":"a.bin","asize":5,"dsize":5,"mtime":1}]]"#,
        )?;
        let second = SnapshotTree::from_json_str(
            "second".into(),
            PathBuf::from("second.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},{"name":"b.bin","asize":20,"dsize":20,"mtime":1},{"name":"a.bin","asize":7,"dsize":7,"mtime":1}]]"#,
        )?;

        let analysis = Analysis::new(vec![first, second])?;
        let mut app = App::new(analysis, SizeMetric::Disk, true)?;

        app.table_state.select(Some(0));
        let _ = app.on_key(KeyCode::Char(' '))?;
        app.table_state.select(Some(1));
        let _ = app.on_key(KeyCode::Char(' '))?;
        assert_eq!(app.marked_paths.len(), 2);

        let _ = app.on_key(KeyCode::Char('n'))?;
        assert_eq!(app.marked_paths.len(), 2);
        assert_eq!(app.marked_summary().map(|summary| summary.count), Some(2));

        Ok(())
    }

    #[test]
    fn refresh_target_prefers_selected_directory() -> Result<()> {
        let first = SnapshotTree::from_json_str(
            "first".into(),
            PathBuf::from("first.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},[{"name":"a","mtime":1},{"name":"one.bin","asize":10,"dsize":10,"mtime":1}],{"name":"b.bin","asize":5,"dsize":5,"mtime":1}]]"#,
        )?;
        let second = SnapshotTree::from_json_str(
            "current".into(),
            PathBuf::from("second.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},[{"name":"a","mtime":1},{"name":"one.bin","asize":20,"dsize":20,"mtime":1}],{"name":"b.bin","asize":7,"dsize":7,"mtime":1}]]"#,
        )?;

        let analysis = Analysis::new(vec![first.clone(), second.clone()])?;
        let mut app = App::new_with_refresh(
            analysis,
            SizeMetric::Disk,
            true,
            Some(RefreshState::new(first, second, PathBuf::from("/root"))),
        )?;
        app.sort = SortMode::Name;
        app.refresh_rows()?;
        let selected_index = app
            .rows
            .iter()
            .position(|row| row.path == "a")
            .expect("directory a should exist");
        app.table_state.select(Some(selected_index));

        assert_eq!(app.current_refresh_target_path(), "a");
        Ok(())
    }

    #[test]
    fn applying_refreshed_snapshot_updates_rows_and_keeps_selection() -> Result<()> {
        let baseline = SnapshotTree::from_json_str(
            "baseline".into(),
            PathBuf::from("baseline.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},[{"name":"a","mtime":1},{"name":"old.bin","asize":10,"dsize":10,"mtime":1}],{"name":"b.bin","asize":5,"dsize":5,"mtime":1}]]"#,
        )?;
        let current = SnapshotTree::from_json_str(
            "current".into(),
            PathBuf::from("current.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},[{"name":"a","mtime":1},{"name":"old.bin","asize":12,"dsize":12,"mtime":1}],{"name":"b.bin","asize":7,"dsize":7,"mtime":1}]]"#,
        )?;
        let refreshed = SnapshotTree::from_json_str(
            "current".into(),
            PathBuf::from("refresh.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":30},[{"name":"/root","mtime":1},[{"name":"a","mtime":1},{"name":"new.bin","asize":40,"dsize":40,"mtime":1}],{"name":"b.bin","asize":7,"dsize":7,"mtime":1}]]"#,
        )?;

        let analysis = Analysis::new(vec![baseline.clone(), current.clone()])?;
        let mut app = App::new_with_refresh(
            analysis,
            SizeMetric::Disk,
            true,
            Some(RefreshState::new(
                baseline.clone(),
                current.clone(),
                PathBuf::from("/root"),
            )),
        )?;
        app.sort = SortMode::Name;
        app.refresh_rows()?;
        let selected_index = app
            .rows
            .iter()
            .position(|row| row.path == "a")
            .expect("directory a should exist");
        app.table_state.select(Some(selected_index));

        let updated_analysis = Analysis::new(vec![baseline, refreshed.clone()])?;
        app.apply_refreshed_current_snapshot(updated_analysis, refreshed)?;

        assert_eq!(app.selected_row().map(|row| row.path.as_str()), Some("a"));
        let refreshed_row = app.analysis.row_for_path("a", SizeMetric::Disk)?;
        assert_eq!(refreshed_row.latest_size, 40);
        Ok(())
    }

    #[test]
    fn hides_selected_panel_in_small_terminal() {
        assert!(!should_show_selected_panel(Rect::new(0, 0, 100, 27)));
        assert!(should_show_selected_panel(Rect::new(0, 0, 110, 28)));
    }
}
