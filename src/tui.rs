use crate::cleaner::{clean_artifacts, CleanMode};
use crate::models::{ArtifactEntry, ArtifactKind, ScanStats};
use crate::scanner::scan_directory;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use humansize::{format_size, DECIMAL};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

// Industrial Precision matte dark charcoal palette
const BG_CHARCOAL: Color = Color::Rgb(11, 14, 20);
const BG_SURFACE: Color = Color::Rgb(17, 22, 31);
const BG_ROW_HIGHLIGHT: Color = Color::Rgb(28, 37, 52);
const BORDER_SUBTLE: Color = Color::Rgb(42, 52, 68);
const BORDER_ACTIVE: Color = Color::Rgb(56, 189, 248);
const BORDER_DANGER: Color = Color::Rgb(244, 63, 94);
const TEXT_PRIMARY: Color = Color::Rgb(226, 232, 240);
const TEXT_MUTED: Color = Color::Rgb(120, 136, 158);
const ACCENT_CYAN: Color = Color::Rgb(56, 189, 248);
const ACCENT_EMERALD: Color = Color::Rgb(16, 185, 129);
const ACCENT_AMBER: Color = Color::Rgb(245, 158, 11);
const ACCENT_ORANGE: Color = Color::Rgb(249, 115, 22);
const ACCENT_BLUE: Color = Color::Rgb(59, 130, 246);
const ACCENT_ROSE: Color = Color::Rgb(244, 63, 94);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortBy {
    Size,
    Age,
    Name,
}

impl SortBy {
    fn next(self) -> Self {
        match self {
            Self::Size => Self::Age,
            Self::Age => Self::Name,
            Self::Name => Self::Size,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Size => "SIZE ↓",
            Self::Age => "AGE ↓",
            Self::Name => "NAME ↑",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Filtering,
    ConfirmDelete,
}

struct App {
    root_path: PathBuf,
    entries: Vec<ArtifactEntry>,
    stats: ScanStats,
    reclaimed_bytes: u64,
    table_state: TableState,
    sort_by: SortBy,
    input_mode: InputMode,
    filter_query: String,
    status_message: String,
    should_quit: bool,
}

impl App {
    fn new(root_path: PathBuf) -> Self {
        let scan = scan_directory(&root_path);
        let mut table_state = TableState::default();
        if !scan.entries.is_empty() {
            table_state.select(Some(0));
        }

        let status_message = format!(
            "Scanned {} dirs in {} ms — press [z] to select all zombies (>30d), [/] to filter, [d] to delete",
            scan.stats.total_dirs, scan.stats.scan_duration_ms
        );

        Self {
            root_path: scan.stats.root_path.clone(),
            entries: scan.entries,
            stats: scan.stats,
            reclaimed_bytes: 0,
            table_state,
            sort_by: SortBy::Size,
            input_mode: InputMode::Normal,
            filter_query: String::new(),
            status_message,
            should_quit: false,
        }
    }

    /// Returns indices into `self.entries` that are not deleted and match `self.filter_query`.
    fn visible_indices(&self) -> Vec<usize> {
        let q = self.filter_query.trim().to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                if e.deleted {
                    return false;
                }
                if q.is_empty() {
                    return true;
                }
                e.project_name.to_lowercase().contains(&q)
                    || e.path.to_string_lossy().to_lowercase().contains(&q)
                    || e.kind.filter_tokens().contains(&q)
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    fn clamp_selection(&mut self) {
        let visible_len = self.visible_indices().len();
        if visible_len == 0 {
            self.table_state.select(None);
            return;
        }
        let next = match self.table_state.selected() {
            Some(i) if i < visible_len => i,
            Some(_) => visible_len - 1,
            None => 0,
        };
        self.table_state.select(Some(next));
    }

    fn move_down(&mut self) {
        let len = self.visible_indices().len();
        if len == 0 {
            self.table_state.select(None);
            return;
        }
        let next = match self.table_state.selected() {
            Some(i) if i + 1 < len => i + 1,
            _ => 0,
        };
        self.table_state.select(Some(next));
    }

    fn move_up(&mut self) {
        let len = self.visible_indices().len();
        if len == 0 {
            self.table_state.select(None);
            return;
        }
        let prev = match self.table_state.selected() {
            Some(0) | None => len - 1,
            Some(i) => i - 1,
        };
        self.table_state.select(Some(prev));
    }

    fn toggle_current(&mut self) {
        let visible = self.visible_indices();
        if let Some(row_idx) = self.table_state.selected() {
            if let Some(&entry_idx) = visible.get(row_idx) {
                self.entries[entry_idx].selected = !self.entries[entry_idx].selected;
            }
        }
    }

    fn select_all_zombies(&mut self) {
        let visible = self.visible_indices();
        let mut count = 0usize;
        let mut bytes = 0u64;
        for &idx in &visible {
            if self.entries[idx].is_zombie {
                self.entries[idx].selected = true;
                count += 1;
                bytes = bytes.saturating_add(self.entries[idx].size_bytes);
            }
        }
        self.status_message = if count > 0 {
            format!(
                "Auto-selected {} zombie artifacts ({}) untouched for >= 30 days",
                count,
                format_bytes(bytes)
            )
        } else {
            "No zombie artifacts (>= 30d) found in current view".to_string()
        };
    }

    fn toggle_select_all(&mut self) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return;
        }
        let all_selected = visible.iter().all(|&idx| self.entries[idx].selected);
        let new_state = !all_selected;
        for &idx in &visible {
            self.entries[idx].selected = new_state;
        }
        self.status_message = if new_state {
            format!("Selected all {} visible artifacts", visible.len())
        } else {
            "Cleared selection".to_string()
        };
    }

    fn cycle_sort(&mut self) {
        self.sort_by = self.sort_by.next();
        self.apply_sort();
        self.clamp_selection();
        self.status_message = format!("Sorted by {}", self.sort_by.label());
    }

    fn apply_sort(&mut self) {
        match self.sort_by {
            SortBy::Size => self.entries.sort_by(|a, b| {
                b.size_bytes
                    .cmp(&a.size_bytes)
                    .then_with(|| a.path.cmp(&b.path))
            }),
            SortBy::Age => self.entries.sort_by(|a, b| {
                b.days_since_modified
                    .cmp(&a.days_since_modified)
                    .then_with(|| b.size_bytes.cmp(&a.size_bytes))
            }),
            SortBy::Name => self.entries.sort_by(|a, b| {
                a.project_name
                    .to_lowercase()
                    .cmp(&b.project_name.to_lowercase())
                    .then_with(|| b.size_bytes.cmp(&a.size_bytes))
            }),
        }
    }

    fn rescan(&mut self) {
        let scan = scan_directory(&self.root_path);
        self.entries = scan.entries;
        self.stats = scan.stats;
        self.apply_sort();
        self.clamp_selection();
        self.status_message = format!(
            "Rescanned {} dirs ({}) in {} ms",
            self.stats.total_dirs,
            format_bytes(self.stats.total_bytes),
            self.stats.scan_duration_ms
        );
    }

    fn selected_summary(&self) -> (usize, u64) {
        let mut count = 0usize;
        let mut bytes = 0u64;
        for entry in &self.entries {
            if !entry.deleted && entry.selected {
                count += 1;
                bytes = bytes.saturating_add(entry.size_bytes);
            }
        }
        (count, bytes)
    }

    fn remaining_summary(&self) -> (usize, u64) {
        let mut count = 0usize;
        let mut bytes = 0u64;
        for entry in &self.entries {
            if !entry.deleted {
                count += 1;
                bytes = bytes.saturating_add(entry.size_bytes);
            }
        }
        (count, bytes)
    }

    fn execute_delete_selected(&mut self) {
        let targets: Vec<ArtifactEntry> = self
            .entries
            .iter()
            .filter(|e| !e.deleted && e.selected)
            .cloned()
            .collect();

        if targets.is_empty() {
            self.input_mode = InputMode::Normal;
            return;
        }

        let report = clean_artifacts(&targets, CleanMode::Permanent);
        let succeeded_paths: HashSet<PathBuf> = report
            .results
            .iter()
            .filter(|r| r.success)
            .map(|r| r.path.clone())
            .collect();

        for entry in &mut self.entries {
            if succeeded_paths.contains(&entry.path) {
                entry.deleted = true;
                entry.selected = false;
            }
        }

        self.reclaimed_bytes = self.reclaimed_bytes.saturating_add(report.reclaimed_bytes);
        self.input_mode = InputMode::Normal;
        self.clamp_selection();

        self.status_message = if report.failed_count == 0 {
            format!(
                "Reclaimed {} across {} dirs in {} ms",
                format_bytes(report.reclaimed_bytes),
                report.deleted_count,
                report.duration_ms
            )
        } else {
            format!(
                "Reclaimed {} ({} deleted, {} failed) in {} ms",
                format_bytes(report.reclaimed_bytes),
                report.deleted_count,
                report.failed_count,
                report.duration_ms
            )
        };
    }
}

/// Launches the full-screen interactive `ratatui` TUI on `root_path`.
pub fn run_tui(root_path: &Path) -> io::Result<()> {
    let mut app = App::new(root_path.to_path_buf());

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let run_result = event_loop(&mut terminal, &mut app);

    // Always restore terminal state cleanly, even on error.
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    run_result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    while !app.should_quit {
        terminal.draw(|frame| render_ui(frame, app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                // Ignore key release/repeat events for clean Windows + Unix behavior.
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                // Global Ctrl+C exit
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    app.should_quit = true;
                    continue;
                }

                match app.input_mode {
                    InputMode::Normal => handle_normal_key(app, key.code),
                    InputMode::Filtering => handle_filter_key(app, key.code),
                    InputMode::ConfirmDelete => handle_confirm_key(app, key.code),
                }
            }
        }
    }
    Ok(())
}

fn handle_normal_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('j') | KeyCode::Down => app.move_down(),
        KeyCode::Char('k') | KeyCode::Up => app.move_up(),
        KeyCode::Char(' ') => app.toggle_current(),
        KeyCode::Char('z') => app.select_all_zombies(),
        KeyCode::Char('a') => app.toggle_select_all(),
        KeyCode::Char('s') => app.cycle_sort(),
        KeyCode::Char('/') => {
            app.input_mode = InputMode::Filtering;
        }
        KeyCode::Char('d') => {
            let (count, _) = app.selected_summary();
            if count > 0 {
                app.input_mode = InputMode::ConfirmDelete;
            } else {
                app.status_message =
                    "Nothing selected — press [Space] to toggle or [z] to select zombies".to_string();
            }
        }
        KeyCode::Char('r') => app.rescan(),
        _ => {}
    }
}

fn handle_filter_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            app.filter_query.clear();
            app.input_mode = InputMode::Normal;
            app.clamp_selection();
        }
        KeyCode::Enter => {
            app.input_mode = InputMode::Normal;
            app.clamp_selection();
        }
        KeyCode::Backspace => {
            app.filter_query.pop();
            app.clamp_selection();
        }
        KeyCode::Char(c) => {
            app.filter_query.push(c);
            app.clamp_selection();
        }
        _ => {}
    }
}

fn handle_confirm_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            app.execute_delete_selected();
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            app.status_message = "Deletion cancelled".to_string();
        }
        _ => {}
    }
}

fn render_ui(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    // Paint matte dark charcoal background across the entire viewport.
    let bg_block = Block::default().style(Style::default().bg(BG_CHARCOAL));
    frame.render_widget(bg_block, area);

    let show_filter_bar = app.input_mode == InputMode::Filtering || !app.filter_query.is_empty();
    let chunks = if show_filter_bar {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Top header bar
                Constraint::Length(3), // Filter bar
                Constraint::Min(8),    // Main table
                Constraint::Length(3), // Footer keybindings & status
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // Top header bar
                Constraint::Length(0), // Hidden filter bar
                Constraint::Min(8),    // Main table
                Constraint::Length(3), // Footer keybindings & status
            ])
            .split(area)
    };

    render_header(frame, app, chunks[0]);
    if show_filter_bar {
        render_filter_bar(frame, app, chunks[1]);
    }
    render_table(frame, app, chunks[2]);
    render_footer(frame, app, chunks[3]);

    if app.input_mode == InputMode::ConfirmDelete {
        render_confirm_modal(frame, app, area);
    }
}

fn render_header(frame: &mut Frame, app: &App, area: Rect) {
    let (rem_dirs, rem_bytes) = app.remaining_summary();
    let (sel_dirs, sel_bytes) = app.selected_summary();
    let path_str = truncate_left(&app.root_path.display().to_string(), 32);

    let header_line = Line::from(vec![
        Span::styled(
            " DISKSWEEP v1.0 ",
            Style::default()
                .fg(BG_CHARCOAL)
                .bg(ACCENT_CYAN)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(BORDER_SUBTLE)),
        Span::styled("PATH ", Style::default().fg(TEXT_MUTED)),
        Span::styled(path_str, Style::default().fg(TEXT_PRIMARY)),
        Span::styled(" │ ", Style::default().fg(BORDER_SUBTLE)),
        Span::styled("FOUND ", Style::default().fg(TEXT_MUTED)),
        Span::styled(
            format!("{} across {} dirs", format_bytes(rem_bytes), rem_dirs),
            Style::default().fg(ACCENT_AMBER).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(BORDER_SUBTLE)),
        Span::styled("SELECTED ", Style::default().fg(TEXT_MUTED)),
        Span::styled(
            format!("{} ({})", format_bytes(sel_bytes), sel_dirs),
            Style::default().fg(ACCENT_ROSE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" │ ", Style::default().fg(BORDER_SUBTLE)),
        Span::styled("RECLAIMED ", Style::default().fg(TEXT_MUTED)),
        Span::styled(
            format_bytes(app.reclaimed_bytes),
            Style::default()
                .fg(ACCENT_EMERALD)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let header = Paragraph::new(header_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(BORDER_SUBTLE))
            .style(Style::default().bg(BG_SURFACE)),
    );

    frame.render_widget(header, area);
}

fn render_filter_bar(frame: &mut Frame, app: &App, area: Rect) {
    let is_active = app.input_mode == InputMode::Filtering;
    let border_color = if is_active {
        BORDER_ACTIVE
    } else {
        BORDER_SUBTLE
    };
    let cursor = if is_active { "█" } else { "" };

    let line = Line::from(vec![
        Span::styled(
            " / FILTER: ",
            Style::default().fg(ACCENT_CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{}{}", app.filter_query, cursor),
            Style::default().fg(TEXT_PRIMARY),
        ),
        Span::styled(
            "   (Enter to apply, Esc to clear — try: rust, node, py, next)",
            Style::default().fg(TEXT_MUTED),
        ),
    ]);

    let bar = Paragraph::new(line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(border_color))
            .style(Style::default().bg(BG_SURFACE)),
    );
    frame.render_widget(bar, area);
}

fn render_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let visible = app.visible_indices();
    let max_size = visible
        .iter()
        .map(|&idx| app.entries[idx].size_bytes)
        .max()
        .unwrap_or(1)
        .max(1);

    let header_cells = [
        "SEL",
        "SIZE & IMPACT",
        "ECOSYSTEM",
        "AGE",
        "PROJECT / PATH",
    ]
    .into_iter()
    .map(|h| {
        Cell::from(h).style(
            Style::default()
                .fg(TEXT_MUTED)
                .add_modifier(Modifier::BOLD),
        )
    });

    let header = Row::new(header_cells)
        .style(Style::default().bg(BG_SURFACE))
        .height(1)
        .bottom_margin(1);

    let rows: Vec<Row> = visible
        .iter()
        .map(|&idx| {
            let entry = &app.entries[idx];
            let checkbox = if entry.selected { "[✓]" } else { "[ ]" };
            let checkbox_style = if entry.selected {
                Style::default()
                    .fg(ACCENT_EMERALD)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT_MUTED)
            };

            let size_label = format!("{:>9}", format_bytes(entry.size_bytes));
            let bar = proportional_bar(entry.size_bytes, max_size, 8);
            let bar_color = if entry.size_bytes >= max_size / 2 {
                ACCENT_ROSE
            } else if entry.size_bytes >= max_size / 5 {
                ACCENT_AMBER
            } else {
                ACCENT_CYAN
            };

            let size_cell = Cell::from(Line::from(vec![
                Span::styled(size_label, Style::default().fg(TEXT_PRIMARY)),
                Span::raw(" "),
                Span::styled(bar, Style::default().fg(bar_color)),
            ]));

            let (pill_fg, pill_bg) = ecosystem_pill_colors(entry.kind);
            let eco_cell = Cell::from(Line::from(Span::styled(
                format!(" {:^6} ", entry.kind.pill_label()),
                Style::default()
                    .fg(pill_fg)
                    .bg(pill_bg)
                    .add_modifier(Modifier::BOLD),
            )));

            let age_str = if entry.is_zombie {
                format!("{:>4}d 🔴", entry.days_since_modified)
            } else {
                format!("{:>4}d 🟢", entry.days_since_modified)
            };
            let age_style = if entry.is_zombie {
                Style::default().fg(ACCENT_ROSE)
            } else {
                Style::default().fg(ACCENT_EMERALD)
            };

            let rel_path = entry
                .path
                .strip_prefix(&app.root_path)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| entry.path.display().to_string());

            let project_cell = Cell::from(Line::from(vec![
                Span::styled(
                    entry.project_name.clone(),
                    Style::default()
                        .fg(TEXT_PRIMARY)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ", Style::default()),
                Span::styled(rel_path, Style::default().fg(TEXT_MUTED)),
            ]));

            Row::new(vec![
                Cell::from(checkbox).style(checkbox_style),
                size_cell,
                eco_cell,
                Cell::from(age_str).style(age_style),
                project_cell,
            ])
            .height(1)
        })
        .collect();

    let title = format!(
        " ARTIFACTS ({} shown)  │  SORT: {} ",
        visible.len(),
        app.sort_by.label()
    );

    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(20),
            Constraint::Length(10),
            Constraint::Length(10),
            Constraint::Min(24),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .title(Span::styled(
                title,
                Style::default()
                    .fg(ACCENT_CYAN)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(BORDER_SUBTLE))
            .style(Style::default().bg(BG_CHARCOAL)),
    )
    .highlight_style(
        Style::default()
            .bg(BG_ROW_HIGHLIGHT)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▸ ");

    frame.render_stateful_widget(table, area, &mut app.table_state);
}

fn render_footer(frame: &mut Frame, app: &App, area: Rect) {
    let key_style = Style::default()
        .fg(ACCENT_CYAN)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(TEXT_MUTED);

    let line = Line::from(vec![
        Span::styled(" j/k", key_style),
        Span::styled(" Nav ", label_style),
        Span::styled("Space", key_style),
        Span::styled(" Toggle ", label_style),
        Span::styled("z", key_style),
        Span::styled(" Zombies ", label_style),
        Span::styled("a", key_style),
        Span::styled(" All ", label_style),
        Span::styled("s", key_style),
        Span::styled(" Sort ", label_style),
        Span::styled("/", key_style),
        Span::styled(" Filter ", label_style),
        Span::styled("d", Style::default().fg(ACCENT_ROSE).add_modifier(Modifier::BOLD)),
        Span::styled(" Delete ", label_style),
        Span::styled("r", key_style),
        Span::styled(" Rescan ", label_style),
        Span::styled("q", key_style),
        Span::styled(" Quit  │  ", label_style),
        Span::styled(&app.status_message, Style::default().fg(TEXT_PRIMARY)),
    ]);

    let footer = Paragraph::new(line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(BORDER_SUBTLE))
            .style(Style::default().bg(BG_SURFACE)),
    );

    frame.render_widget(footer, area);
}

fn render_confirm_modal(frame: &mut Frame, app: &App, viewport: Rect) {
    let (count, bytes) = app.selected_summary();
    let modal_area = centered_rect(56, 9, viewport);

    frame.render_widget(Clear, modal_area);

    let text = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Permanently delete ", Style::default().fg(TEXT_PRIMARY)),
            Span::styled(
                format!("{} directories", count),
                Style::default().fg(ACCENT_AMBER).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" and reclaim ", Style::default().fg(TEXT_PRIMARY)),
            Span::styled(
                format_bytes(bytes),
                Style::default()
                    .fg(ACCENT_EMERALD)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("?", Style::default().fg(TEXT_PRIMARY)),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "Source code is untouched. Safety guardrails re-verify every path.",
            Style::default().fg(TEXT_MUTED),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                " [y] CONFIRM WIPE ",
                Style::default()
                    .fg(BG_CHARCOAL)
                    .bg(ACCENT_ROSE)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("    "),
            Span::styled(
                " [n] CANCEL ",
                Style::default()
                    .fg(TEXT_PRIMARY)
                    .bg(BORDER_SUBTLE)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];

    let modal = Paragraph::new(text)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .title(Span::styled(
                    " CONFIRM DELETION ",
                    Style::default()
                        .fg(ACCENT_ROSE)
                        .add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_type(BorderType::Thick)
                .border_style(Style::default().fg(BORDER_DANGER))
                .style(Style::default().bg(BG_SURFACE)),
        );

    frame.render_widget(modal, modal_area);
}

fn ecosystem_pill_colors(kind: ArtifactKind) -> (Color, Color) {
    match kind {
        ArtifactKind::NodeModules => (BG_CHARCOAL, ACCENT_AMBER),
        ArtifactKind::RustTarget => (BG_CHARCOAL, ACCENT_ORANGE),
        ArtifactKind::PythonVenv => (BG_CHARCOAL, ACCENT_CYAN),
        ArtifactKind::DotNetBuild => (Color::White, ACCENT_BLUE),
        ArtifactKind::NextBuild => (BG_CHARCOAL, Color::Rgb(248, 250, 252)),
        ArtifactKind::GradleBuild
        | ArtifactKind::DistCache
        | ArtifactKind::MavenTarget
        | ArtifactKind::CmakeBuild => (BG_CHARCOAL, ACCENT_EMERALD),
    }
}

fn proportional_bar(bytes: u64, max_bytes: u64, width: usize) -> String {
    let ratio = (bytes as f64 / max_bytes.max(1) as f64).clamp(0.0, 1.0);
    let filled = ((ratio * width as f64).round() as usize).clamp(1, width);
    let empty = width.saturating_sub(filled);
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

fn format_bytes(bytes: u64) -> String {
    format_size(bytes, DECIMAL)
}

fn truncate_left(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let tail: String = chars[chars.len() - keep..].iter().collect();
    format!("…{}", tail)
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}
