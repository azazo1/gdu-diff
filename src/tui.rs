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
use ratatui::layout::{Constraint, Layout, Margin};
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

    fn selected_detail(&self) -> Result<Vec<Line<'static>>> {
        let Some(selected) = self.selected_row() else {
            return Ok(vec![Line::from("No entries in this directory.")]);
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

        Ok(vec![
            Line::from(format!(
                "Selected: {} ({})",
                self.analysis.display_path(&selected.path),
                selected.kind.label()
            )),
            Line::from(format!(
                "Size: {} -> {} ({})",
                format_size(selected.baseline_size),
                format_size(selected.latest_size),
                format_signed_size(selected.delta)
            )),
            Line::from(format!(
                "Share in parent: {:.1}% -> {:.1}% ({})",
                selected.baseline_local_share * 100.0,
                selected.latest_local_share * 100.0,
                format_share_delta(selected.local_share_delta)
            )),
            Line::from(format!(
                "Share in root: {:.1}% -> {:.1}% ({})",
                selected.baseline_root_share() * 100.0,
                selected.latest_root_share() * 100.0,
                format_share_delta(selected.root_share_delta())
            )),
            Line::from(format!("Labels: {timeline_labels}")),
            Line::from(format!("Sizes : {timeline_sizes}")),
            Line::from(format!("Share : {timeline_shares}")),
        ])
    }

    fn render(&mut self, frame: &mut ratatui::Frame) -> Result<()> {
        let area = frame.area();
        frame.render_widget(Clear, area);

        let chunks = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(8),
            Constraint::Length(2),
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
                Cell::from(row.name.clone()),
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

        let detail = Paragraph::new(self.selected_detail()?)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title("Selected"));
        frame.render_widget(detail, chunks[2]);

        let help = Paragraph::new(Line::from(
            "j/k or arrows move  l/enter open  h/backspace up  s size-sort  d delta-sort  p share-sort  n name-sort  a metric  f files  q quit",
        ))
        .block(Block::default().borders(Borders::ALL).title("Keys"));
        frame.render_widget(help, chunks[3].inner(Margin::new(0, 0)));

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
