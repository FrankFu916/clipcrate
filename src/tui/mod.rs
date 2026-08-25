//! Full-screen fuzzy-search picker built with ratatui.
//!
//! Returns the chosen entry id (`Some`) or `None` when dismissed.
//! Keys: type to search · ↑/↓ select · Enter pick · Esc cancel ·
//! Ctrl+P pin · Del/Ctrl+D delete · PgUp/PgDn jump.

use crate::store::Store;
use anyhow::Result;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};

pub struct Picker<'a> {
    store: &'a mut Store,
    query: String,
    /// Indices into store.entries (which is oldest-first), newest first.
    view: Vec<usize>,
    selected: ListState,
    matcher: SkimMatcherV2,
}

impl<'a> Picker<'a> {
    pub fn new(store: &'a mut Store) -> Picker<'a> {
        let mut p = Picker {
            store,
            query: String::new(),
            view: Vec::new(),
            selected: ListState::default(),
            matcher: SkimMatcherV2::default().ignore_case(),
        };
        p.refilter();
        p
    }

    fn refilter(&mut self) {
        let entries = &self.store.entries;
        let mut idx: Vec<(usize, i64)> = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            // Pinned entries always float to the top of the unfiltered view.
            let base = if e.pinned { i64::MIN / 2 } else { 0 };
            if self.query.is_empty() {
                idx.push((i, base - i as i64));
            } else if let Some(score) = self.matcher.fuzzy_match(&e.text, &self.query) {
                idx.push((i, base - score));
            }
        }
        idx.sort_by_key(|(_, k)| *k);
        self.view = idx.into_iter().map(|(i, _)| i).collect();
        let len = self.view.len();
        let cur = self
            .selected
            .selected()
            .unwrap_or(0)
            .min(len.saturating_sub(1));
        self.selected
            .select(if len == 0 { None } else { Some(cur) });
    }

    fn move_selection(&mut self, delta: isize) {
        let len = self.view.len();
        if len == 0 {
            return;
        }
        let cur = self.selected.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, len as isize - 1);
        self.selected.select(Some(next as usize));
    }

    /// Mutate the selected entry then keep the view coherent.
    fn with_selected<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Store, u64),
    {
        if let Some(pos) = self.selected.selected() {
            if let Some(&idx) = self.view.get(pos) {
                let id = self.store.entries[idx].id;
                f(self.store, id);
                self.refilter();
            }
        }
    }

    fn run_inner(
        &mut self,
        term: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    ) -> Result<Option<u64>> {
        loop {
            term.draw(|f| self.render(f))?;
            if !event::poll(std::time::Duration::from_millis(200))? {
                continue;
            }
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(None)
                    }
                    KeyCode::Enter => {
                        return Ok(self.selected.selected().and_then(|pos| {
                            self.view.get(pos).map(|&i| self.store.entries[i].id)
                        }));
                    }
                    KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.with_selected(|s, id| {
                            s.toggle_pin(id);
                        });
                        self.store.rewrite()?;
                    }
                    KeyCode::Delete => {
                        self.with_selected(|s, id| {
                            s.delete(id);
                        });
                        self.store.rewrite()?;
                    }
                    KeyCode::Char(c) => {
                        self.query.push(c);
                        self.refilter();
                    }
                    KeyCode::Backspace => {
                        self.query.pop();
                        self.refilter();
                    }
                    KeyCode::Up => self.move_selection(-1),
                    KeyCode::Down => self.move_selection(1),
                    KeyCode::PageUp => self.move_selection(-10),
                    KeyCode::PageDown => self.move_selection(10),
                    KeyCode::Home => self.move_selection(isize::MIN / 2),
                    KeyCode::End => self.move_selection(isize::MAX / 2),
                    _ => {}
                }
            }
        }
    }

    fn render(&mut self, f: &mut Frame) {
        let now = crate::entry::now_ms();
        let preview_lines =
            crate::config::Config::load(&crate::config::Config::data_dir().join("config.toml"))
                .map(|c| c.preview_lines)
                .unwrap_or(8);

        let outer = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(preview_lines as u16 + 2),
        ])
        .split(f.area());

        f.render_widget(
            Paragraph::new(format!("❯ {}", self.query))
                .style(Style::default().fg(Color::Cyan))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" clipcrate search "),
                ),
            outer[0],
        );

        let now_ms_val = now;
        let items: Vec<ListItem> = self
            .view
            .iter()
            .enumerate()
            .map(|(row, &idx)| {
                let e = &self.store.entries[idx];
                let pin = if e.pinned { "📌 " } else { "  " };
                let head = e.preview(80);
                let multi = if e.is_multiline() { " ⏎" } else { "" };
                let style = if row == self.selected.selected().unwrap_or(usize::MAX) {
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::raw(pin),
                    Span::styled(head, style),
                    Span::styled(multi.to_string(), Style::default().dim()),
                    Span::styled(
                        format!("  {} · {}", e.age(now_ms_val), e.human_size()),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" history "))
            .highlight_style(Style::default().add_modifier(Modifier::BOLD))
            .highlight_symbol("▸ ");
        f.render_stateful_widget(list, outer[1], &mut self.selected);

        // Preview pane for the current selection.
        let preview_title;
        let body: Vec<Line>;
        if let Some(pos) = self.selected.selected() {
            if let Some(&idx) = self.view.get(pos) {
                let e = &self.store.entries[idx];
                preview_title = format!(" preview · #{} · {} ", e.id, e.human_size());
                body = match e.kind {
                    crate::entry::Kind::Image => vec![Line::from("[image payload]".dim())],
                    crate::entry::Kind::Text => e
                        .text
                        .lines()
                        .take(preview_lines)
                        .map(Line::from)
                        .chain(std::iter::once(Line::from("…".dim())))
                        .take(
                            preview_lines
                                + if e.text.lines().count() > preview_lines {
                                    1
                                } else {
                                    0
                                },
                        )
                        .collect(),
                };
            } else {
                preview_title = " preview ".into();
                body = vec![];
            }
        } else {
            preview_title = " preview ".into();
            body = vec![Line::from("(no matches)".dim())];
        }
        f.render_widget(
            Paragraph::new(body).block(Block::default().borders(Borders::ALL).title(preview_title)),
            outer[2],
        );
    }
}

/// Public entry point: sets up the alternate screen and runs the picker.
/// The terminal is restored even on error.
pub fn run_picker(store: &mut Store) -> Result<Option<u64>> {
    use ratatui::crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    ratatui::crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let res = Picker::new(store).run_inner(&mut term);

    disable_raw_mode()?;
    ratatui::crossterm::execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res
}
