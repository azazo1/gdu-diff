use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use clipboard_rs::{Clipboard, ClipboardContext};
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

use crate::analysis::{Analysis, ChangeKind, RowData, SizeMetric, SortMode};

pub struct App {
    analysis: Analysis,
    metric: SizeMetric,
    sort: SortMode,
    include_files: bool,
    current_path: String,
    rows: Vec<RowData>,
    table_state: TableState,
    page_step: usize,
    should_quit: bool,
    status_message: Option<StatusMessage>,
}

impl App {
    pub fn new(analysis: Analysis, metric: SizeMetric, include_files: bool) -> Result<Self> {
        let mut app = Self {
            analysis,
            metric,
            sort: SortMode::Delta,
            include_files,
            current_path: String::new(),
            rows: Vec::new(),
            table_state: TableState::default(),
            page_step: 1,
            should_quit: false,
            status_message: None,
        };
        app.refresh_rows()?;
        Ok(app)
    }

    fn refresh_rows(&mut self) -> Result<()> {
        let selected_path = self.selected_row().map(|row| row.path.clone());
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

    fn on_key(&mut self, code: KeyCode) -> Result<()> {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp | KeyCode::Char(',') => self.move_selection_page(-1),
            KeyCode::PageDown | KeyCode::Char('.') => self.move_selection_page(1),
            KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => self.enter_selected()?,
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.go_parent()?,
            KeyCode::Char('a') => {
                self.metric = match self.metric {
                    SizeMetric::Disk => SizeMetric::Apparent,
                    SizeMetric::Apparent => SizeMetric::Disk,
                };
                self.refresh_rows()?;
            }
            KeyCode::Char('f') => {
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
            KeyCode::Char('c') => self.copy_relative_path(),
            KeyCode::Char('C') => self.copy_absolute_path(),
            _ => {}
        }
        Ok(())
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

    fn enter_selected(&mut self) -> Result<()> {
        let Some(path) = self
            .selected_row()
            .filter(|row| row.has_children())
            .map(|row| row.path.clone())
        else {
            return Ok(());
        };
        self.current_path = path;
        self.refresh_rows()
    }

    fn go_parent(&mut self) -> Result<()> {
        let Some(parent) = Analysis::parent_path(&self.current_path) else {
            return Ok(());
        };
        self.current_path = parent;
        self.refresh_rows()
    }

    fn copy_relative_path(&mut self) {
        let target = self.copy_target();
        self.copy_to_clipboard(target.relative_path.clone(), "relative path");
    }

    fn copy_absolute_path(&mut self) {
        let target = self.copy_target();
        self.copy_to_clipboard(target.absolute_path.clone(), "absolute path");
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

    fn copy_to_clipboard(&mut self, value: String, label: &str) {
        match ClipboardContext::new().and_then(|clipboard| clipboard.set_text(value.clone())) {
            Ok(()) => {
                self.status_message = Some(StatusMessage {
                    text: format!("Copied {label}: {value}"),
                    kind: StatusKind::Info,
                });
            }
            Err(error) => {
                self.status_message = Some(StatusMessage {
                    text: format!("Failed to copy {label}: {error}"),
                    kind: StatusKind::Error,
                });
            }
        }
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

        let chunks = Layout::vertical([
            Constraint::Length(6),
            Constraint::Min(6),
            Constraint::Length(12),
            Constraint::Length(3),
        ])
        .split(area);
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
                Row::new(vec![
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

        self.render_selected_panel(frame, chunks[2])?;

        let help = Paragraph::new(Line::from(
            "j/k or arrows move  ,/. page  l/enter open  h/backspace up  s size-sort  d delta-sort  p share-sort  n name-sort  a metric  f files  c rel-path  C abs-path  q quit",
        ))
        .block(Block::default().borders(Borders::ALL).title("Keys"));
        frame.render_widget(help, chunks[3]);

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

        let columns = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(inner);
        let left = Layout::vertical([Constraint::Length(6), Constraint::Min(4)]).split(columns[0]);
        let right = Layout::vertical([Constraint::Length(4), Constraint::Min(5)]).split(columns[1]);

        self.render_selected_item_panel(frame, left[0], &detail);

        let size = Paragraph::new(detail.size_lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Size"));
        frame.render_widget(size, left[1]);

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
}

pub fn run(mut app: App) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|frame| {
            let _ = app.render(frame);
        })?;

        if app.should_quit {
            return Ok(());
        }

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            app.on_key(key.code)?;
        }
    }
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

fn delta_style(delta: i64) -> Style {
    if delta > 0 {
        Style::default().fg(Color::Red)
    } else if delta < 0 {
        Style::default().fg(Color::Green)
    } else {
        Style::default()
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
