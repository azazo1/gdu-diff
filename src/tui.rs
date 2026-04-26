use std::io;
use std::time::Duration;

use anyhow::Result;
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

use crate::analysis::{Analysis, RowData, SizeMetric, SortMode};

pub struct App {
    analysis: Analysis,
    metric: SizeMetric,
    sort: SortMode,
    include_files: bool,
    current_path: String,
    rows: Vec<RowData>,
    table_state: TableState,
    should_quit: bool,
}

impl App {
    pub fn new(analysis: Analysis, metric: SizeMetric, include_files: bool) -> Result<Self> {
        let mut app = Self {
            analysis,
            metric,
            sort: SortMode::LatestSize,
            include_files,
            current_path: String::new(),
            rows: Vec::new(),
            table_state: TableState::default(),
            should_quit: false,
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
            item_lines: vec![
                stat_line_spans(
                    "Name",
                    styled_entry_name_spans(&selected.name, selected.kind),
                ),
                stat_line_spans(
                    "Path",
                    styled_path_spans(&self.analysis.display_path(&selected.path), selected.kind),
                ),
                stat_line(
                    "Type",
                    selected.kind.label(),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
            ],
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

        let chunks = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(11),
            Constraint::Length(3),
        ])
        .split(area);

        let header = Paragraph::new(vec![
            Line::from(format!(
                "gdu-diff  root: {}  snapshots: {}  range: {}",
                self.analysis.current_root_name(),
                self.analysis.snapshot_count(),
                self.analysis.snapshot_range_label()
            )),
            Line::from(format!(
                "path: {}",
                self.analysis.display_path(&self.current_path)
            )),
            Line::from(format!(
                "metric: {}  sort: {}  visible: {}",
                self.metric.label(),
                self.sort.label(),
                if self.include_files {
                    "dirs + files"
                } else {
                    "dirs only"
                }
            )),
        ])
        .block(Block::default().borders(Borders::ALL).title("Overview"));
        frame.render_widget(header, chunks[0]);

        let header_row = Row::new(vec![
            Cell::from("Type"),
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

        let rows = self.rows.iter().map(|row| {
            let delta_style = delta_style(row.delta);
            let share_style = share_style(row.local_share_delta);
            Row::new(vec![
                Cell::from(row.kind.short()),
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

        self.render_selected_panel(frame, chunks[2])?;

        let help = Paragraph::new(Line::from(
            "j/k or arrows move  l/enter open  h/backspace up  s size-sort  d delta-sort  p share-sort  n name-sort  a metric  f files  q quit",
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
        let left = Layout::vertical([Constraint::Length(5), Constraint::Min(4)]).split(columns[0]);
        let right = Layout::vertical([Constraint::Length(4), Constraint::Min(5)]).split(columns[1]);

        let item = Paragraph::new(detail.item_lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Item"));
        frame.render_widget(item, left[0]);

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
    item_lines: Vec<Line<'static>>,
    size_lines: Vec<Line<'static>>,
    share_lines: Vec<Line<'static>>,
    timeline_lines: Vec<Line<'static>>,
}

impl SelectedDetail {
    fn empty() -> Self {
        Self {
            is_empty: true,
            item_lines: Vec::new(),
            size_lines: Vec::new(),
            share_lines: Vec::new(),
            timeline_lines: Vec::new(),
        }
    }
}

fn stat_line(
    label: impl Into<String>,
    value: impl Into<String>,
    value_style: Style,
) -> Line<'static> {
    stat_line_spans(label, vec![Span::styled(value.into(), value_style)])
}

fn stat_line_spans(label: impl Into<String>, value_spans: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![Span::styled(
        format!("{:<8}", label.into()),
        Style::default().fg(Color::DarkGray),
    )];
    spans.extend(value_spans);
    Line::from(spans)
}

fn share_line(label: &str, baseline: f64, latest: f64, delta: f64) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<6}"), Style::default().fg(Color::DarkGray)),
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

fn styled_entry_name_spans(name: &str, kind: crate::analysis::EntryKind) -> Vec<Span<'static>> {
    let mut spans = vec![Span::styled(
        name.to_string(),
        entry_name_style(name, kind).add_modifier(Modifier::BOLD),
    )];
    if matches!(
        kind,
        crate::analysis::EntryKind::Dir | crate::analysis::EntryKind::Mixed
    ) {
        spans.push(Span::styled("/", directory_style()));
    }
    spans
}

fn styled_path_spans(path: &str, final_kind: crate::analysis::EntryKind) -> Vec<Span<'static>> {
    if path == "/" {
        return vec![Span::styled("/", directory_style())];
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
        spans.push(Span::styled(
            (*component).to_string(),
            entry_name_style(component, kind).add_modifier(Modifier::BOLD),
        ));

        if !is_last {
            spans.push(Span::styled("/", separator_style()));
        } else if matches!(
            kind,
            crate::analysis::EntryKind::Dir | crate::analysis::EntryKind::Mixed
        ) {
            spans.push(Span::styled("/", directory_style()));
        }
    }

    spans
}

fn entry_name_style(name: &str, kind: crate::analysis::EntryKind) -> Style {
    if matches!(
        kind,
        crate::analysis::EntryKind::Dir | crate::analysis::EntryKind::Mixed
    ) {
        return directory_style();
    }

    if name.starts_with('.') {
        return Style::default().fg(Color::Rgb(198, 198, 198));
    }

    match extension(name) {
        Some("json" | "jsonl" | "json5") => Style::default().fg(Color::Rgb(215, 0, 255)),
        Some("toml" | "yaml" | "yml") => Style::default().fg(Color::Rgb(255, 176, 0)),
        Some("rs" | "go" | "py" | "sh" | "bash" | "zsh") => {
            Style::default().fg(Color::Rgb(107, 214, 114))
        }
        Some("md" | "txt") => Style::default().fg(Color::Rgb(224, 224, 224)),
        _ => Style::default().fg(Color::Rgb(235, 235, 235)),
    }
}

fn directory_style() -> Style {
    Style::default().fg(Color::Rgb(36, 148, 255))
}

fn separator_style() -> Style {
    Style::default().fg(Color::Rgb(214, 214, 214))
}

fn extension(name: &str) -> Option<&str> {
    let (_, extension) = name.rsplit_once('.')?;
    (!extension.is_empty()).then_some(extension)
}
