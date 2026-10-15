//! The interactive `archix init`: a step-by-step wizard you can move back and forth through before anything is written.

use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

use super::{Answers, BootFacts, FALLBACK, Facts, Home, KEEP, KEEP_DAYS, MISSING_VERSIONS, secret_note, some_of};
use crate::commands::count;
use archix::capture::Dotfile;

const ACCENT: Color = Color::Cyan;
const GOOD: Color = Color::Green;
const WARN: Color = Color::Yellow;
const QUIET: Color = Color::DarkGray;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Step {
    Welcome,
    Config,
    Packages,
    System,
    Files,
    Boot,
    Generations,
    Dotfiles,
    Review,
}

const STEPS: [Step; 9] = [
    Step::Welcome,
    Step::Config,
    Step::Packages,
    Step::System,
    Step::Files,
    Step::Boot,
    Step::Generations,
    Step::Dotfiles,
    Step::Review,
];

impl Step {
    fn title(self) -> &'static str {
        match self {
            Step::Welcome => "Welcome",
            Step::Config => "Config",
            Step::Packages => "Packages",
            Step::System => "System",
            Step::Files => "Config files",
            Step::Boot => "Boot",
            Step::Generations => "Generations",
            Step::Dotfiles => "Dotfiles",
            Step::Review => "Review",
        }
    }

    fn blurb(self) -> &'static str {
        match self {
            Step::Welcome => "Describe this machine as a config, exactly as it is now.",
            Step::Config => "Where the config lives.",
            Step::Packages => "What's installed, and what happens to what isn't declared.",
            Step::System => "Settings, accounts and services taken over as they are.",
            Step::Files => "Edited and hand-written files under /etc that archix can manage.",
            Step::Boot => "Boot entries per generation, and snapshots before each switch.",
            Step::Generations => "How many old generations gc keeps around.",
            Step::Dotfiles => "Optional: bring dotfiles into the config as live links.",
            Step::Review => "Everything init will write. Nothing has changed yet.",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Control {
    Location,
    Git,
    Fallback,
    Orphans,
    Strict,
    AurReview,
    MissingVersions,
    Files,
    Snapshots,
    Boot,
    Keep,
    KeepDays,
    DotUser,
    Dotfiles,
    Settings,
    Users,
    Groups,
    Services,
}

/// One focusable line: a choice's option, a toggle, a text field, a checklist item or a button.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Row {
    Pick(Control, usize),
    Flip(Control),
    Edit(Control),
    Item(Control, usize),
    Next,
    Back,
}

impl Row {
    fn control(self) -> Option<Control> {
        match self {
            Row::Pick(c, _) | Row::Flip(c) | Row::Edit(c) | Row::Item(c, _) => Some(c),
            Row::Next | Row::Back => None,
        }
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum Flow {
    Stay,
    Done,
    Quit,
}

const ORPHANS: [&str; 2] = [
    "keep them (listed in ignored_packages)",
    "let the first switch remove them",
];
const STRICT: [&str; 2] = [
    "switch offers to declare it, and removes it if you say no (recommended)",
    "switch leaves it installed and only lists it",
];

pub(super) struct App<'a> {
    facts: &'a Facts,
    host: &'a str,
    pub(super) answers: Answers,
    step: usize,
    seen: usize,
    focus: usize,
    /// The value a text field had before editing started, while it's being edited.
    editing: Option<String>,
    scroll: u16,
    error: Option<String>,
}

/// Runs the wizard in the terminal; None when it's quit.
pub(super) fn run(facts: &Facts, host: &str, answers: Answers) -> Result<Option<Answers>> {
    let mut terminal = ratatui::init();
    let mut app = App::new(facts, host, answers);
    let outcome = loop {
        if let Err(e) = terminal.draw(|frame| app.draw(frame)) {
            break Err(e.into());
        }
        match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match app.handle(key) {
                Flow::Stay => {}
                Flow::Done => break Ok(Some(app.answers)),
                Flow::Quit => break Ok(None),
            },
            Ok(_) => {}
            Err(e) => break Err(e.into()),
        }
    };
    ratatui::restore();
    outcome
}

impl<'a> App<'a> {
    pub(super) fn new(facts: &'a Facts, host: &'a str, answers: Answers) -> App<'a> {
        let mut app = App {
            facts,
            host,
            answers,
            step: 0,
            seen: 0,
            focus: 0,
            editing: None,
            scroll: 0,
            error: None,
        };
        app.go(0);
        app
    }

    fn current(&self) -> Step {
        STEPS[self.step]
    }

    fn users(&self) -> Vec<&String> {
        self.facts.dotfiles.keys().collect()
    }

    fn home(&self) -> Option<&Home> {
        self.facts.dotfiles.values().nth(self.answers.dotfiles_user)
    }

    fn dotfile_candidates(&self) -> &[Dotfile] {
        self.home().map_or(&[], |home| home.offered.as_slice())
    }

    /// A checklist's items as (name, detail) pairs.
    fn items(&self, control: Control) -> Vec<(String, String)> {
        let facts = self.facts;
        match control {
            Control::Files => facts
                .files
                .iter()
                .map(|file| {
                    let why = if file.edited {
                        "changed from its package's version"
                    } else {
                        "not from any package"
                    };
                    let tag = if file.recommended { "recommended · " } else { "" };
                    (file.path.clone(), format!("{tag}{why}"))
                })
                .collect(),
            Control::Dotfiles => self
                .dotfile_candidates()
                .iter()
                .map(|d| {
                    (
                        d.name.clone(),
                        if d.recommended {
                            "recommended".into()
                        } else {
                            String::new()
                        },
                    )
                })
                .collect(),
            Control::Settings => facts
                .settings_summary
                .iter()
                .map(|s| (s.clone(), String::new()))
                .collect(),
            Control::Users => facts
                .accounts
                .users
                .iter()
                .map(|(name, user)| {
                    let groups = if user.groups.is_empty() {
                        String::new()
                    } else {
                        format!("  groups {}", user.groups.join(", "))
                    };
                    (name.clone(), format!("uid {}  {}{groups}", user.uid, user.shell))
                })
                .collect(),
            Control::Groups => facts
                .accounts
                .groups
                .iter()
                .map(|(name, gid)| (name.clone(), format!("gid {gid}")))
                .collect(),
            Control::Services => facts
                .services
                .iter()
                .map(|(unit, on)| (unit.clone(), if *on { "enabled".into() } else { "disabled".into() }))
                .collect(),
            _ => Vec::new(),
        }
    }

    fn is_list(control: Control) -> bool {
        matches!(
            control,
            Control::Files
                | Control::Dotfiles
                | Control::Settings
                | Control::Users
                | Control::Groups
                | Control::Services
        )
    }

    fn checked(&self, control: Control, i: usize) -> bool {
        let answers = &self.answers;
        let name = || {
            self.items(control)
                .get(i)
                .map(|(name, _)| name.clone())
                .unwrap_or_default()
        };
        match control {
            Control::Files => answers.files.contains(&i),
            Control::Settings => answers.settings.contains(&i),
            Control::Dotfiles => answers.dotfiles.contains(&name()),
            Control::Users => answers.users.contains(&name()),
            Control::Groups => answers.groups.contains(&name()),
            Control::Services => answers.services.contains(&name()),
            _ => false,
        }
    }

    fn set_checked(&mut self, control: Control, i: usize, on: bool) {
        let name = self
            .items(control)
            .get(i)
            .map(|(name, _)| name.clone())
            .unwrap_or_default();
        let answers = &mut self.answers;
        fn set<T: Ord>(set: &mut std::collections::BTreeSet<T>, value: T, on: bool) {
            if on {
                set.insert(value);
            } else {
                set.remove(&value);
            }
        }
        match control {
            Control::Files => set(&mut answers.files, i, on),
            Control::Settings => set(&mut answers.settings, i, on),
            Control::Dotfiles => set(&mut answers.dotfiles, name, on),
            Control::Users => set(&mut answers.users, name, on),
            Control::Groups => set(&mut answers.groups, name, on),
            Control::Services => set(&mut answers.services, name, on),
            _ => {}
        }
    }

    fn controls(&self) -> Vec<Control> {
        let facts = self.facts;
        let mut controls = Vec::new();
        match self.current() {
            Step::Config => {
                controls.push(Control::Location);
                if facts.git {
                    controls.push(Control::Git);
                }
            }
            Step::Packages => {
                if !facts.custom.is_empty() {
                    controls.push(Control::Fallback);
                }
                if !facts.orphans.is_empty() {
                    controls.push(Control::Orphans);
                }
                controls.push(Control::Strict);
                if !facts.aur_packages.is_empty() {
                    controls.extend([Control::AurReview, Control::MissingVersions]);
                }
            }
            Step::System => {
                if !facts.settings_summary.is_empty() {
                    controls.push(Control::Settings);
                }
                if !facts.accounts.users.is_empty() {
                    controls.push(Control::Users);
                }
                if !facts.accounts.groups.is_empty() {
                    controls.push(Control::Groups);
                }
                if !facts.services.is_empty() {
                    controls.push(Control::Services);
                }
            }
            Step::Files if !facts.files.is_empty() => controls.push(Control::Files),
            Step::Boot => {
                if facts.btrfs {
                    controls.push(Control::Snapshots);
                }
                if matches!(facts.boot, BootFacts::Ready { .. }) {
                    controls.push(Control::Boot);
                }
            }
            Step::Generations => controls.extend([Control::Keep, Control::KeepDays]),
            Step::Dotfiles => {
                if self.users().len() > 1 {
                    controls.push(Control::DotUser);
                }
                if !self.dotfile_candidates().is_empty() {
                    controls.push(Control::Dotfiles);
                }
            }
            _ => {}
        }
        controls
    }

    fn options(&self, control: Control) -> Vec<String> {
        match control {
            Control::Fallback => FALLBACK.iter().map(|(_, label)| label.to_string()).collect(),
            Control::MissingVersions => MISSING_VERSIONS.iter().map(|(_, label)| label.to_string()).collect(),
            Control::Orphans => ORPHANS.map(str::to_string).to_vec(),
            Control::Strict => STRICT.map(str::to_string).to_vec(),
            Control::DotUser => self.users().into_iter().cloned().collect(),
            _ => Vec::new(),
        }
    }

    fn picked(&self, control: Control) -> usize {
        let answers = &self.answers;
        match control {
            Control::Fallback => answers.fallback,
            Control::MissingVersions => answers.missing_versions,
            Control::Orphans => usize::from(!answers.keep_orphans),
            Control::Strict => usize::from(!answers.strict),
            Control::DotUser => answers.dotfiles_user,
            _ => 0,
        }
    }

    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for control in self.controls() {
            match control {
                Control::Location | Control::Keep | Control::KeepDays => rows.push(Row::Edit(control)),
                Control::Git | Control::AurReview | Control::Snapshots | Control::Boot => rows.push(Row::Flip(control)),
                c if Self::is_list(c) => rows.extend((0..self.items(c).len()).map(|i| Row::Item(control, i))),
                _ => rows.extend((0..self.options(control).len()).map(|i| Row::Pick(control, i))),
            }
        }
        rows.push(Row::Next);
        if self.step > 0 {
            rows.push(Row::Back);
        }
        rows
    }

    fn focused(&self) -> Row {
        let rows = self.rows();
        rows[self.focus.min(rows.len() - 1)]
    }

    fn only_buttons(&self) -> bool {
        self.controls().is_empty()
    }

    fn text(&mut self, control: Control) -> Option<&mut String> {
        match control {
            Control::Location => Some(&mut self.answers.location),
            Control::Keep => Some(&mut self.answers.keep),
            Control::KeepDays => Some(&mut self.answers.keep_days),
            _ => None,
        }
    }

    pub(super) fn handle(&mut self, key: KeyEvent) -> Flow {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Flow::Quit;
        }
        self.error = None;
        if self.editing.is_some() {
            self.edit(key);
            return Flow::Stay;
        }
        let row = self.focused();
        let in_list = matches!(row, Row::Item(..));
        match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => return self.activate(row),
            KeyCode::Esc => return Flow::Quit,
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab if self.only_buttons() && key.code != KeyCode::Tab => {
                self.scroll = self.scroll.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') if self.only_buttons() => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => self.move_focus(1),
            KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => self.move_focus(-1),
            KeyCode::Left | KeyCode::Char('h') => self.back(),
            KeyCode::Right | KeyCode::Char('l') if self.current() == Step::Review => self.focus_row(Row::Next),
            KeyCode::Right | KeyCode::Char('l') => return self.forward(),
            KeyCode::PageDown if self.only_buttons() => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageUp if self.only_buttons() => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.move_focus(10),
            KeyCode::PageUp => self.move_focus(-10),
            KeyCode::Home => self.focus = 0,
            KeyCode::End => self.focus_row(Row::Next),
            KeyCode::Char('a') if in_list => self.select(|_, _, _| true),
            KeyCode::Char('n') if in_list => self.select(|_, _, _| false),
            KeyCode::Char('r') if in_list => self.select(|app, control, i| app.recommended(control, i)),
            KeyCode::Char('q') => return Flow::Quit,
            _ => {}
        }
        Flow::Stay
    }

    /// Keys while a text field is being edited: they type, enter keeps the value and esc puts the old one back.
    fn edit(&mut self, key: KeyEvent) {
        let Some(control) = self.focused().control() else {
            return;
        };
        match key.code {
            KeyCode::Enter => self.editing = None,
            KeyCode::Esc => {
                if let Some(before) = self.editing.take()
                    && let Some(text) = self.text(control)
                {
                    *text = before;
                }
            }
            KeyCode::Backspace => {
                if let Some(text) = self.text(control) {
                    text.pop();
                }
            }
            KeyCode::Char(c) => {
                if let Some(text) = self.text(control) {
                    text.push(c);
                }
            }
            _ => {}
        }
    }

    fn activate(&mut self, row: Row) -> Flow {
        let answers = &mut self.answers;
        match row {
            Row::Next => return self.forward(),
            Row::Back => self.back(),
            Row::Pick(control, i) => match control {
                Control::Fallback => answers.fallback = i,
                Control::MissingVersions => answers.missing_versions = i,
                Control::Orphans => answers.keep_orphans = i == 0,
                Control::Strict => answers.strict = i == 0,
                Control::DotUser if answers.dotfiles_user != i => {
                    answers.dotfiles_user = i;
                    answers.dotfiles = self
                        .facts
                        .dotfiles
                        .values()
                        .nth(i)
                        .map(|home| {
                            home.offered
                                .iter()
                                .filter(|d| d.recommended)
                                .map(|d| d.name.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                }
                _ => {}
            },
            Row::Flip(control) => match control {
                Control::Git => answers.git = !answers.git,
                Control::AurReview => answers.aur_review = !answers.aur_review,
                Control::Snapshots => answers.snapshots = !answers.snapshots,
                Control::Boot => answers.boot = !answers.boot,
                _ => {}
            },
            Row::Edit(control) => {
                let before = self.text(control).map(|text| text.clone());
                self.editing = before;
            }
            Row::Item(control, i) => {
                let on = !self.checked(control, i);
                self.set_checked(control, i, on);
            }
        }
        Flow::Stay
    }

    fn go(&mut self, step: usize) {
        self.step = step;
        self.seen = self.seen.max(step);
        self.editing = None;
        self.scroll = 0;
        self.focus_row(Row::Next);
    }

    fn focus_row(&mut self, target: Row) {
        if let Some(i) = self.rows().iter().position(|row| *row == target) {
            self.focus = i;
        }
    }

    fn forward(&mut self) -> Flow {
        if let Some(problem) = self.problem() {
            self.error = Some(problem);
            return Flow::Stay;
        }
        if self.current() == Step::Review {
            return match self.answers.problem() {
                Some(problem) => {
                    self.error = Some(problem);
                    Flow::Stay
                }
                None => Flow::Done,
            };
        }
        self.go(self.step + 1);
        Flow::Stay
    }

    /// What's wrong with this step's answers, so the wizard doesn't move on with them.
    fn problem(&self) -> Option<String> {
        let answers = &self.answers;
        match self.current() {
            Step::Config
                if !answers.location.trim().is_empty()
                    && !std::path::Path::new(answers.location.trim()).is_absolute() =>
            {
                Some("the config directory must be an absolute path".into())
            }
            Step::Generations => [&answers.keep, &answers.keep_days]
                .into_iter()
                .find(|value| !value.trim().is_empty() && value.trim().parse::<u32>().is_err())
                .map(|value| format!("{value:?} isn't a number")),
            _ => None,
        }
    }

    fn back(&mut self) {
        if self.step > 0 {
            self.go(self.step - 1);
        }
    }

    fn move_focus(&mut self, delta: isize) {
        let last = self.rows().len() as isize - 1;
        self.focus = (self.focus as isize + delta).clamp(0, last) as usize;
    }

    /// Ticks the focused checklist's items `keep` says yes to and unticks the rest.
    fn select(&mut self, keep: impl Fn(&Self, Control, usize) -> bool) {
        let Some(control) = self.focused().control().filter(|c| Self::is_list(*c)) else {
            return;
        };
        for i in 0..self.items(control).len() {
            let on = keep(self, control, i);
            self.set_checked(control, i, on);
        }
    }

    fn recommended(&self, control: Control, i: usize) -> bool {
        match control {
            Control::Files => self.facts.files[i].recommended,
            Control::Dotfiles => self.dotfile_candidates()[i].recommended,
            _ => true,
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame) {
        let [header, body, footer] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(6), Constraint::Length(2)]).areas(frame.area());
        self.draw_header(frame, header);
        let [sidebar, main] = Layout::horizontal([Constraint::Length(22), Constraint::Min(30)]).areas(body);
        self.draw_sidebar(frame, sidebar);
        self.draw_main(frame, main);
        self.draw_footer(frame, footer);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let [left, right] = Layout::horizontal([Constraint::Min(20), Constraint::Length(36)]).areas(area);
        let title = Line::from(vec![
            Span::styled(" archix", Style::new().fg(ACCENT).bold()),
            Span::styled(" init", Style::new().bold()),
        ]);
        frame.render_widget(Paragraph::new(title), left);
        let where_ = Line::from(vec![
            Span::styled(format!("host {}", self.host), Style::new().fg(QUIET)),
            Span::styled(
                format!("  ·  step {} of {} ", self.step + 1, STEPS.len()),
                Style::new().fg(QUIET),
            ),
        ])
        .right_aligned();
        frame.render_widget(Paragraph::new(where_), right);
    }

    fn draw_sidebar(&self, frame: &mut Frame, area: Rect) {
        let lines: Vec<Line> = STEPS
            .iter()
            .enumerate()
            .map(|(i, step)| {
                let (mark, style) = if i == self.step {
                    ("›", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))
                } else if i <= self.seen {
                    ("✓", Style::new().fg(GOOD))
                } else {
                    (" ", Style::new().fg(QUIET))
                };
                let text_style = if i == self.step {
                    style
                } else if i <= self.seen {
                    Style::new()
                } else {
                    Style::new().fg(QUIET)
                };
                Line::from(vec![
                    Span::styled(format!(" {mark} "), style),
                    Span::styled(step.title(), text_style),
                ])
            })
            .collect();
        let block = Block::new()
            .borders(Borders::RIGHT)
            .border_style(Style::new().fg(QUIET))
            .padding(Padding::vertical(1));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_main(&mut self, frame: &mut Frame, area: Rect) {
        let step = self.current();
        let mut lines = vec![
            Line::styled(step.title(), Style::new().fg(ACCENT).bold()),
            Line::styled(step.blurb(), Style::new().fg(QUIET)),
            Line::default(),
        ];
        let mut cursor_line = None;
        self.content(&mut lines, &mut cursor_line);
        let [content, buttons] = Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(area);
        let mut button_line = Vec::new();
        self.buttons(&mut button_line, &mut None);
        frame.render_widget(
            Paragraph::new(button_line).block(Block::new().padding(Padding::new(3, 2, 0, 0))),
            buttons,
        );
        let area = content;
        let block = Block::new().padding(Padding::new(3, 2, 1, 0));
        let height = block.inner(area).height;
        if let Some(line) = cursor_line.filter(|_| !self.only_buttons()) {
            let line = line as u16;
            if line < self.scroll {
                self.scroll = line.saturating_sub(1);
            } else if line >= self.scroll + height {
                self.scroll = line + 2 - height.min(line + 2);
            }
        }
        let max = (lines.len() as u16).saturating_sub(height);
        self.scroll = self.scroll.min(max);
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            area,
        );
    }

    fn buttons(&self, lines: &mut Vec<Line<'static>>, cursor_line: &mut Option<usize>) {
        let focused = self.focused();
        if matches!(focused, Row::Next | Row::Back) {
            *cursor_line = Some(lines.len());
        }
        let button = |label: String, row: Row, primary: bool| {
            let style = match (focused == row, primary) {
                (true, true) => Style::new().fg(Color::Black).bg(Color::White).bold(),
                (false, true) => Style::new().fg(Color::Black).bg(ACCENT).bold(),
                (true, false) => Style::new().fg(Color::Black).bg(Color::Gray).bold(),
                (false, false) => Style::new().fg(QUIET),
            };
            Span::styled(format!(" {label} "), style)
        };
        let next = if self.current() == Step::Review {
            "Write the config ›".to_string()
        } else {
            format!("Next: {} ›", STEPS[self.step + 1].title())
        };
        let mut spans = Vec::new();
        if self.step > 0 {
            spans.push(button("‹ Back".into(), Row::Back, false));
            spans.push(Span::raw("   "));
        }
        spans.push(button(next, Row::Next, true));
        lines.push(Line::from(spans));
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let key = |k: &'static str| Span::styled(k, Style::new().bold());
        let gap = || Span::styled("  ·  ", Style::new().fg(QUIET));
        let label = |t: &'static str| Span::styled(t, Style::new().fg(QUIET));
        let mut hints = vec![Span::raw(" ")];
        if self.editing.is_some() {
            hints.extend([
                key("type"),
                label(" to edit"),
                gap(),
                key("enter"),
                label(" done"),
                gap(),
                key("esc"),
                label(" cancel"),
            ]);
        } else {
            let row = self.focused();
            let action = match row {
                Row::Next if self.current() == Step::Review => " write the config",
                Row::Next => " next step",
                Row::Back => " previous step",
                Row::Edit(_) => " edit",
                Row::Pick(..) => " choose",
                Row::Flip(_) | Row::Item(..) => " switch on/off",
            };
            let moves = if self.only_buttons() { " scroll" } else { " move" };
            hints.extend([key("↑↓"), label(moves), gap(), key("enter"), label(action), gap()]);
            if matches!(row, Row::Item(..)) {
                hints.extend([key("a/n"), label(" all/none"), gap()]);
            }
            hints.extend([key("←→"), label(" step"), gap(), key("esc"), label(" quit")]);
        }
        let first = match &self.error {
            Some(error) => Line::styled(format!(" ! {error}"), Style::new().fg(WARN).bold()),
            None => Line::default(),
        };
        frame.render_widget(Paragraph::new(vec![first, Line::from(hints)]), area);
    }

    fn content(&self, lines: &mut Vec<Line<'static>>, cursor_line: &mut Option<usize>) {
        let facts = self.facts;
        let answers = &self.answers;
        let done = |text: String| Line::from(vec![Span::styled("✓ ", Style::new().fg(GOOD).bold()), Span::raw(text)]);
        let note = |text: String| Line::styled(format!("· {text}"), Style::new().fg(QUIET));
        let warn = |text: String| Line::from(vec![Span::styled("! ", Style::new().fg(WARN).bold()), Span::raw(text)]);
        match self.current() {
            Step::Welcome => {
                lines.push(Line::raw(
                    "archix reads this machine and writes a config that describes it as it is, \
                     plus a lock of the exact versions installed now.",
                ));
                lines.push(Line::default());
                lines.push(done(format!(
                    "{} to declare, {}",
                    count(facts.packages.len(), "package", "packages"),
                    count(facts.aur_packages.len(), "from the AUR", "from the AUR")
                )));
                lines.push(done(count(
                    facts.files.len(),
                    "config file found",
                    "config files found",
                )));
                lines.push(done(count(facts.accounts.users.len(), "user account", "user accounts")));
                if !facts.flatpak_apps.is_empty() {
                    lines.push(done(count(facts.flatpak_apps.len(), "Flatpak app", "Flatpak apps")));
                }
                lines.push(Line::default());
                lines.push(note(
                    "Nothing on the system changes. Every step can be revisited before anything is written.".into(),
                ));
            }
            Step::Config => {
                lines.push(note(format!(
                    "Any directory works, such as a clone of your own repo; {} then links to it.",
                    facts.default_dir.display()
                )));
                lines.push(Line::default());
                for control in self.controls() {
                    self.control(control, lines, cursor_line);
                }
            }
            Step::Packages => {
                lines.push(done(format!("repos {}", facts.official.join(", "))));
                if !facts.custom.is_empty() {
                    let names: Vec<String> = facts.custom.iter().map(|(n, _)| n.clone()).collect();
                    lines.push(done(format!("third-party repos {}", names.join(", "))));
                }
                lines.push(done(format!(
                    "{} declared, {}",
                    count(facts.packages.len(), "package", "packages"),
                    count(facts.aur_packages.len(), "from the AUR", "from the AUR")
                )));
                if !facts.flatpak_apps.is_empty() {
                    let ids: Vec<String> = facts.flatpak_apps.keys().cloned().collect();
                    lines.push(done(format!(
                        "{}, locked to their installed commits: {}",
                        count(ids.len(), "Flatpak app", "Flatpak apps"),
                        some_of(&ids, 4)
                    )));
                }
                if !facts.unknown.is_empty() {
                    lines.push(warn(format!(
                        "not in any repo or the AUR, so left alone: {}",
                        some_of(&facts.unknown, 6)
                    )));
                }
                if !facts.flatpak_gone.is_empty() {
                    lines.push(warn(format!(
                        "their remote is gone, so archix leaves these Flatpak apps alone: {}",
                        some_of(&facts.flatpak_gone, 4)
                    )));
                }
                if !facts.orphans.is_empty() {
                    lines.push(note(format!(
                        "{} that nothing needs any more: {}",
                        count(facts.orphans.len(), "dependency", "dependencies"),
                        some_of(&facts.orphans, 6)
                    )));
                }
                lines.push(note(
                    "Everything installed now is declared, so the first switch removes nothing.".into(),
                ));
                lines.push(Line::default());
                for control in self.controls() {
                    self.control(control, lines, cursor_line);
                }
            }
            Step::System => {
                lines.push(note(
                    "Everything here is taken over as it is; untick what archix should leave alone.".into(),
                ));
                lines.push(note("Passwords stay where they are; archix never manages them.".into()));
                lines.push(Line::default());
                if facts.settings_summary.is_empty() {
                    lines.push(note(
                        "no hostname, timezone, locale or keymap archix can take over exactly".into(),
                    ));
                    lines.push(Line::default());
                }
                for control in self.controls() {
                    self.control(control, lines, cursor_line);
                }
                if facts.accounts.users.is_empty() {
                    lines.push(note("no users in the regular uid range".into()));
                }
                if facts.services.is_empty() {
                    lines.push(note("no services differ from their preset".into()));
                }
            }
            Step::Files => {
                if facts.secrets > 0 {
                    lines.push(note(secret_note(facts.secrets, "file")));
                }
                if facts.files.is_empty() {
                    lines.push(note("no edited or hand-written config files found".into()));
                } else {
                    lines.push(note(format!(
                        "{} of {} selected; recommended ones start ticked, r resets to them",
                        answers.files.len(),
                        facts.files.len()
                    )));
                    lines.push(Line::default());
                    self.control(Control::Files, lines, cursor_line);
                }
            }
            Step::Boot => {
                if facts.btrfs {
                    self.control(Control::Snapshots, lines, cursor_line);
                } else {
                    lines.push(note(
                        "/ isn't btrfs, so no snapshots; packages, config and boot entries still roll back".into(),
                    ));
                    lines.push(Line::default());
                }
                match &facts.boot {
                    BootFacts::NoLoader => {
                        lines.push(note("no systemd-boot found, so archix leaves booting alone".into()));
                    }
                    BootFacts::NoRoot { esp } => lines.push(warn(format!(
                        "systemd-boot is on {esp}, but the kernel command line has no root=, so archix leaves booting alone"
                    ))),
                    BootFacts::Ready {
                        esp, kernel, params, ..
                    } => {
                        lines.push(done(format!("systemd-boot on {esp}, kernel {kernel}")));
                        lines.push(note(format!("kernel command line: {}", params.join(" "))));
                        lines.push(Line::default());
                        self.control(Control::Boot, lines, cursor_line);
                    }
                }
            }
            Step::Generations => {
                lines.push(note(
                    "gc keeps the newest generations, plus any younger than the age below.".into(),
                ));
                lines.push(Line::default());
                for control in self.controls() {
                    self.control(control, lines, cursor_line);
                }
            }
            Step::Dotfiles => {
                if self.users().is_empty() {
                    lines.push(note("no user accounts, so there are no dotfiles to bring in".into()));
                } else {
                    lines.push(note(
                        "Chosen ones are copied into the config and linked back, so editing them edits the config."
                            .into(),
                    ));
                    lines.push(note(
                        "The first switch backs up the originals. Recommended ones start ticked; r resets to them."
                            .into(),
                    ));
                    if let Some(home) = self.home().filter(|home| home.secrets > 0) {
                        lines.push(note(secret_note(home.secrets, "entry")));
                    }
                    lines.push(Line::default());
                    for control in self.controls() {
                        self.control(control, lines, cursor_line);
                    }
                    if self.dotfile_candidates().is_empty() {
                        lines.push(note("no dotfiles found in that home".into()));
                    }
                }
            }
            Step::Review => self.review(lines),
        }
    }

    fn review(&self, lines: &mut Vec<Line<'static>>) {
        let facts = self.facts;
        let answers = &self.answers;
        let row = |label: &str, value: String| {
            Line::from(vec![
                Span::styled(format!("{label:<15}"), Style::new().fg(QUIET)),
                Span::raw(value),
            ])
        };
        let git = if answers.git { ", a git repository" } else { "" };
        lines.push(row(
            "Config",
            format!("{}{git}", answers.location(&facts.default_dir).display()),
        ));
        lines.push(row(
            "Packages",
            format!(
                "{} · repos {}",
                count(facts.packages.len(), "package", "packages"),
                facts.official.join(", ")
            ),
        ));
        lines.push(row(
            "Undeclared",
            if answers.strict {
                "switch offers to declare them, else removes them".into()
            } else {
                "switch only lists them".into()
            },
        ));
        if !facts.orphans.is_empty() {
            lines.push(row(
                "Unused deps",
                if answers.keep_orphans {
                    format!("{} kept in ignored_packages", facts.orphans.len())
                } else {
                    format!("{} removed by the first switch", facts.orphans.len())
                },
            ));
        }
        if !facts.flatpak_apps.is_empty() {
            lines.push(row("Flatpak", count(facts.flatpak_apps.len(), "app", "apps")));
        }
        let users: Vec<String> = answers.users.iter().cloned().collect();
        lines.push(row(
            "Accounts",
            if users.is_empty() {
                "none".into()
            } else {
                users.join(", ")
            },
        ));
        lines.push(row(
            "Services",
            format!("{} of {}", answers.services.len(), facts.services.len()),
        ));
        lines.push(row("Config files", count(answers.files.len(), "file", "files")));
        let boot = match (&facts.boot, answers.boot) {
            (BootFacts::Ready { esp, .. }, true) => format!("an entry per generation on {esp}"),
            _ => "left alone".into(),
        };
        lines.push(row("Boot", boot));
        if facts.btrfs {
            lines.push(row(
                "Snapshots",
                if answers.snapshots {
                    "before every switch"
                } else {
                    "off"
                }
                .into(),
            ));
        }
        lines.push(row(
            "Generations",
            format!(
                "keep {} and any younger than {} days",
                answers.keep(),
                answers.keep_days()
            ),
        ));
        lines.push(row(
            "Dotfiles",
            if answers.dotfiles.is_empty() {
                "none".into()
            } else {
                let user = self
                    .users()
                    .get(answers.dotfiles_user)
                    .map_or(String::new(), |u| (*u).clone());
                format!("{} of {user}'s", answers.dotfiles.len())
            },
        ));
    }

    fn control(&self, control: Control, lines: &mut Vec<Line<'static>>, cursor_line: &mut Option<usize>) {
        let focused = self.focused();
        let answers = &self.answers;
        let active = focused.control() == Some(control);
        let heading = |text: &str| {
            let style = if active {
                Style::new().fg(ACCENT).bold()
            } else {
                Style::new().bold()
            };
            Line::styled(format!("  {text}"), style)
        };
        let pointer = |row: Row, cursor_line: &mut Option<usize>, at: usize| {
            if focused == row {
                *cursor_line = Some(at);
                Span::styled("  › ", Style::new().fg(ACCENT).bold())
            } else {
                Span::raw("    ")
            }
        };
        let label_style = |row: Row| {
            if focused == row {
                Style::new().fg(ACCENT).bold()
            } else {
                Style::new()
            }
        };
        match control {
            Control::Location | Control::Keep | Control::KeepDays => {
                let (title, value, placeholder) = match control {
                    Control::Location => (
                        "Where should the config live?",
                        &answers.location,
                        self.facts.default_dir.display().to_string(),
                    ),
                    Control::Keep => ("Generations gc always keeps", &answers.keep, KEEP.to_string()),
                    _ => (
                        "…and any younger than this many days",
                        &answers.keep_days,
                        KEEP_DAYS.to_string(),
                    ),
                };
                const LABEL: usize = 38;
                const FIELD: usize = 24;
                let row = Row::Edit(control);
                let editing = focused == row && self.editing.is_some();
                let pointer = pointer(row, cursor_line, lines.len());
                let mut spans = vec![pointer, Span::styled(format!("{title:<LABEL$}"), label_style(row))];
                let typed = if editing { format!("{value}▏") } else { value.clone() };
                let underline = Modifier::UNDERLINED;
                if value.is_empty() {
                    let caret = if editing { "▏" } else { "" };
                    let pad = FIELD.saturating_sub(caret.chars().count() + placeholder.chars().count());
                    spans.push(Span::styled(caret, Style::new().fg(ACCENT).add_modifier(underline)));
                    spans.push(Span::styled(
                        placeholder,
                        Style::new().fg(QUIET).add_modifier(underline),
                    ));
                    spans.push(Span::styled(" ".repeat(pad), Style::new().add_modifier(underline)));
                } else {
                    let style = if editing {
                        Style::new().fg(ACCENT)
                    } else {
                        label_style(row)
                    };
                    let width = FIELD.max(typed.chars().count() + 1);
                    spans.push(Span::styled(format!("{typed:<width$}"), style.add_modifier(underline)));
                }
                if focused == row && !editing {
                    spans.push(Span::styled("  enter to edit", Style::new().fg(QUIET)));
                }
                lines.push(Line::from(spans));
                if control != Control::Keep {
                    lines.push(Line::default());
                }
            }
            Control::Git | Control::AurReview | Control::Snapshots | Control::Boot => {
                let (title, on) = match control {
                    Control::Git => ("Make it a git repository", answers.git),
                    Control::AurReview => (
                        "Show each AUR build recipe and ask before trusting it",
                        answers.aur_review,
                    ),
                    Control::Snapshots => (
                        "Snapshot / before every switch, as an extra recovery point",
                        answers.snapshots,
                    ),
                    _ => ("Write a boot entry for each generation", answers.boot),
                };
                let row = Row::Flip(control);
                let mark = if on { "[x] " } else { "[ ] " };
                let mark_style = if on {
                    Style::new().fg(GOOD)
                } else {
                    Style::new().fg(QUIET)
                };
                let pointer = pointer(row, cursor_line, lines.len());
                lines.push(Line::from(vec![
                    pointer,
                    Span::styled(mark, mark_style),
                    Span::styled(title, label_style(row)),
                ]));
                lines.push(Line::default());
            }
            c if Self::is_list(c) => {
                let title = match control {
                    Control::Files => "Config files archix manages",
                    Control::Dotfiles => "Dotfiles to bring in",
                    Control::Settings => "Settings",
                    Control::Users => "Users",
                    Control::Groups => "Groups",
                    _ => "Services that differ from their preset",
                };
                lines.push(heading(title));
                let items: Vec<(String, bool, String)> = self
                    .items(control)
                    .into_iter()
                    .enumerate()
                    .map(|(i, (name, detail))| (name, self.checked(control, i), detail))
                    .collect();
                for (i, (name, checked, detail)) in items.into_iter().enumerate() {
                    let row = Row::Item(control, i);
                    let pointer = pointer(row, cursor_line, lines.len());
                    let (mark, mark_style) = if checked {
                        ("[x] ", Style::new().fg(GOOD))
                    } else {
                        ("[ ] ", Style::new().fg(QUIET))
                    };
                    let mut spans = vec![
                        pointer,
                        Span::styled(mark, mark_style),
                        Span::styled(name, label_style(row)),
                    ];
                    if !detail.is_empty() {
                        spans.push(Span::styled(format!("  {detail}"), Style::new().fg(QUIET)));
                    }
                    lines.push(Line::from(spans));
                }
                lines.push(Line::default());
            }
            _ => {
                let title = match control {
                    Control::Fallback => "If a locked package disappears from a third-party repo",
                    Control::Orphans => "Dependencies nothing needs any more",
                    Control::Strict => "Later, when you install something with plain pacman or yay",
                    Control::MissingVersions => "If a locked AUR version can't be built any more",
                    _ => "Whose dotfiles",
                };
                lines.push(heading(title));
                let picked = self.picked(control);
                for (i, option) in self.options(control).into_iter().enumerate() {
                    let row = Row::Pick(control, i);
                    let pointer = pointer(row, cursor_line, lines.len());
                    let (mark, mark_style) = if i == picked {
                        ("● ", Style::new().fg(ACCENT))
                    } else {
                        ("○ ", Style::new().fg(QUIET))
                    };
                    let text_style = if focused == row {
                        Style::new().fg(ACCENT).bold()
                    } else if i == picked {
                        Style::new()
                    } else {
                        Style::new().fg(QUIET)
                    };
                    lines.push(Line::from(vec![
                        pointer,
                        Span::styled(mark, mark_style),
                        Span::styled(option, text_style),
                    ]));
                }
                lines.push(Line::default());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::commands::init::Machine;
    use archix::capture::{Account, Accounts, FoundFile};

    fn facts() -> Facts {
        let file = |path: &str, recommended: bool| FoundFile {
            path: path.into(),
            mode: 0o644,
            owner: "root".into(),
            group: "root".into(),
            recommended,
            edited: true,
        };
        let me = Account {
            uid: 1000,
            group: None,
            groups: vec!["wheel".into()],
            shell: "/usr/bin/zsh".into(),
            home: None,
            description: String::new(),
        };
        Facts {
            machine: Machine {
                local: BTreeMap::new(),
                details: BTreeMap::new(),
                foreign: Default::default(),
            },
            default_dir: PathBuf::from("/etc/archix"),
            git: true,
            official: vec!["core".into(), "extra".into()],
            custom: Vec::new(),
            packages: vec!["base".into(), "yay-bin".into()],
            aur_packages: vec!["yay-bin".into()],
            unknown: Vec::new(),
            orphans: vec!["old-dep".into()],
            settings: vec![("hostname".into(), archix::luagen::Lua::str("box"))],
            settings_summary: vec!["hostname box".into()],
            accounts: Accounts {
                users: BTreeMap::from([("me".to_string(), me)]),
                groups: BTreeMap::new(),
            },
            services: BTreeMap::from([("sshd.service".to_string(), true)]),
            files: vec![file("/etc/fstab", true), file("/etc/shells", false)],
            secrets: 2,
            btrfs: true,
            boot: BootFacts::Ready {
                esp: "/boot".into(),
                kernel: "linux".into(),
                microcode: Vec::new(),
                params: vec!["root=UUID=x".into(), "rw".into()],
            },
            flatpak_apps: BTreeMap::new(),
            flatpak_remotes: BTreeMap::new(),
            flatpak_gone: Vec::new(),
            dotfiles: BTreeMap::from([(
                "me".to_string(),
                Home {
                    dir: PathBuf::from("/home/me"),
                    offered: vec![
                        Dotfile {
                            name: ".config/kdeglobals".into(),
                            recommended: false,
                        },
                        Dotfile {
                            name: ".config/nvim".into(),
                            recommended: true,
                        },
                    ],
                    secrets: 1,
                },
            )]),
        }
    }

    fn press(app: &mut App, code: KeyCode) -> Flow {
        app.handle(KeyEvent::from(code))
    }

    /// Moves focus to the Next button and presses it.
    fn next(app: &mut App) -> Flow {
        press(app, KeyCode::End);
        press(app, KeyCode::Enter)
    }

    fn render(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 40)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn enter_picks_options_and_the_buttons_move_between_steps() {
        let facts = facts();
        let mut app = App::new(&facts, "box", Answers::recommended(&facts));
        assert_eq!(app.focused(), Row::Next, "a step without fields starts on Next");
        press(&mut app, KeyCode::Enter);
        next(&mut app);
        assert_eq!(app.current(), Step::Packages);
        assert_eq!(app.focused(), Row::Next, "every step opens on Next");
        press(&mut app, KeyCode::Home);
        assert_eq!(app.focused(), Row::Pick(Control::Orphans, 0));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.focused(), Row::Pick(Control::Strict, 1));
        assert!(app.answers.strict, "moving onto an option doesn't pick it");
        press(&mut app, KeyCode::Enter);
        assert!(!app.answers.strict);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.focused(), Row::Flip(Control::AurReview));
        press(&mut app, KeyCode::Enter);
        assert!(!app.answers.aur_review, "enter switches a toggle");

        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.focused(), Row::Back);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current(), Step::Config, "the Back button goes back");
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(app.current(), Step::Config, "number keys don't jump");
        press(&mut app, KeyCode::Right);
        assert_eq!(app.current(), Step::Packages, "→ is the next step");
        assert!(!app.answers.strict && !app.answers.aur_review, "answers survive");
        press(&mut app, KeyCode::Left);
        assert_eq!(app.current(), Step::Config, "← is the previous step");
        press(&mut app, KeyCode::Right);
        let screen = render(&mut app);
        assert!(screen.contains("‹ Back     Next: System ›"), "{screen}");
    }

    #[test]
    fn defaults_are_placeholders_git_and_recommended_dotfiles() {
        let facts = facts();
        let answers = Answers::recommended(&facts);
        assert!(answers.git, "git is on when it's installed");
        assert_eq!(answers.dotfiles, [".config/nvim".to_string()].into());
        assert_eq!((answers.keep(), answers.keep_days()), (10, 30));
        let mut app = App::new(&facts, "box", answers);
        for _ in 0..6 {
            press(&mut app, KeyCode::Right);
        }
        assert_eq!(app.current(), Step::Generations);
        let screen = render(&mut app);
        assert!(
            screen.contains("Generations gc always keeps") && screen.contains(" 10 ") && screen.contains(" 30 "),
            "{screen}"
        );
        assert!(app.answers.keep.is_empty(), "the 10 is a placeholder, not a value");
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('5'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.answers.keep(), 5);
    }

    #[test]
    fn the_system_step_can_leave_things_out() {
        let facts = facts();
        let mut app = App::new(&facts, "box", Answers::recommended(&facts));
        for _ in 0..3 {
            press(&mut app, KeyCode::Right);
        }
        assert_eq!(app.current(), Step::System);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.focused(), Row::Item(Control::Settings, 0));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.focused(), Row::Item(Control::Services, 0));
        press(&mut app, KeyCode::Enter);
        assert!(app.answers.services.is_empty());
        let config = crate::commands::init::build(&facts, &app.answers, std::path::Path::new("/cfg"), "box");
        assert!(
            config.fields.iter().all(|(key, _)| key != "services"),
            "unticked services aren't declared"
        );
        assert!(config.fields.iter().any(|(key, _)| key == "users"));
    }

    #[test]
    fn enter_ticks_checklist_items() {
        let facts = facts();
        let mut app = App::new(&facts, "box", Answers::recommended(&facts));
        for _ in 0..4 {
            press(&mut app, KeyCode::Right);
        }
        assert_eq!(app.current(), Step::Files);
        assert_eq!(app.answers.files, [0].into());
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Enter);
        assert!(app.answers.files.is_empty());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.answers.files, [1].into());
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.answers.files, [0, 1].into());
        press(&mut app, KeyCode::Char('r'));
        assert_eq!(app.answers.files, [0].into());
        let screen = render(&mut app);
        assert!(screen.contains("[x] /etc/fstab  recommended"), "{screen}");
        assert!(screen.contains("› [ ] /etc/shells"), "{screen}");
    }

    #[test]
    fn enter_edits_a_field_and_esc_cancels() {
        let facts = facts();
        let mut app = App::new(&facts, "box", Answers::recommended(&facts));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.focused(), Row::Edit(Control::Location));
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.answers.location, "", "keys aren't typed until editing starts");
        press(&mut app, KeyCode::Enter);
        for _ in 0.."/etc/archix".len() {
            press(&mut app, KeyCode::Backspace);
        }
        for c in "rel q1".chars() {
            assert_eq!(press(&mut app, KeyCode::Char(c)), Flow::Stay);
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.answers.location, "rel q1", "while editing, q and digits are text");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.answers.location, "rel q1", "esc puts the old value back");
        assert_eq!(app.current(), Step::Config, "esc while editing doesn't leave the step");
        next(&mut app);
        assert_eq!(app.current(), Step::Config);
        assert!(render(&mut app).contains("must be an absolute path"));
    }

    #[test]
    fn every_step_renders_and_the_last_button_writes() {
        let facts = facts();
        let mut app = App::new(&facts, "box", Answers::recommended(&facts));
        for step in STEPS {
            assert_eq!(app.current(), step);
            let screen = render(&mut app);
            assert!(screen.contains(step.blurb()), "{screen}");
            if step == Step::Review {
                assert!(screen.contains("Write the config ›"), "{screen}");
                assert_eq!(press(&mut app, KeyCode::Right), Flow::Stay, "→ never writes the config");
                assert_eq!(app.focused(), Row::Next);
                assert_eq!(next(&mut app), Flow::Done);
            } else {
                next(&mut app);
            }
        }
        assert_eq!(press(&mut app, KeyCode::Char('q')), Flow::Quit);
        assert_eq!(press(&mut app, KeyCode::Esc), Flow::Quit, "esc quits outside editing");
    }
}
