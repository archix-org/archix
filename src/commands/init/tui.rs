//! The interactive `archix init`: a step-by-step wizard you can move back and forth through before anything is written.

use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

use super::{Answers, BootFacts, FALLBACK, Facts, MISSING_VERSIONS, some_of};
use crate::commands::count;

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
    cursor: usize,
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
        App {
            facts,
            host,
            answers,
            step: 0,
            seen: 0,
            focus: 0,
            cursor: 0,
            scroll: 0,
            error: None,
        }
    }

    fn current(&self) -> Step {
        STEPS[self.step]
    }

    fn users(&self) -> Vec<&String> {
        self.facts.dotfiles.keys().collect()
    }

    fn dotfile_candidates(&self) -> &[String] {
        self.facts
            .dotfiles
            .values()
            .nth(self.answers.dotfiles_user)
            .map_or(&[], |(_, candidates)| candidates.as_slice())
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

    fn focused(&self) -> Option<Control> {
        self.controls().get(self.focus).copied()
    }

    fn list_len(&self, control: Control) -> usize {
        match control {
            Control::Files => self.facts.files.len(),
            Control::Dotfiles => self.dotfile_candidates().len(),
            _ => 0,
        }
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
        let focused = self.focused();
        if let Some(control) = focused
            && let Some(text) = self.text(control)
        {
            match key.code {
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    text.push(c);
                    return Flow::Stay;
                }
                KeyCode::Backspace => {
                    text.pop();
                    return Flow::Stay;
                }
                _ => {}
            }
        }
        let in_list = focused.is_some_and(|c| self.list_len(c) > 0);
        match key.code {
            KeyCode::Enter => return self.forward(),
            KeyCode::Esc | KeyCode::BackTab => self.back(),
            KeyCode::Tab => self.move_focus(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if in_list && self.cursor + 1 < self.list_len(focused.unwrap()) {
                    self.cursor += 1;
                } else if self.controls().is_empty() {
                    self.scroll = self.scroll.saturating_add(1);
                } else {
                    self.move_focus(1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if in_list && self.cursor > 0 {
                    self.cursor -= 1;
                } else if self.controls().is_empty() {
                    self.scroll = self.scroll.saturating_sub(1);
                } else {
                    self.move_focus(-1);
                }
            }
            KeyCode::PageDown if in_list => {
                self.cursor = (self.cursor + 10).min(self.list_len(focused.unwrap()) - 1);
            }
            KeyCode::PageUp if in_list => self.cursor = self.cursor.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Left | KeyCode::Char('h') => self.change(-1),
            KeyCode::Right | KeyCode::Char('l') => self.change(1),
            KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('a') if in_list => self.select(|_, _| true),
            KeyCode::Char('n') if in_list => self.select(|_, _| false),
            KeyCode::Char('r') if focused == Some(Control::Files) => {
                self.select(|facts, i| facts.files[i].recommended);
            }
            KeyCode::Char('q') => return Flow::Quit,
            KeyCode::Char(digit @ '1'..='9') => {
                let target = digit as usize - '1' as usize;
                if target < STEPS.len() {
                    self.go(target);
                }
            }
            _ => {}
        }
        Flow::Stay
    }

    fn go(&mut self, step: usize) {
        self.step = step;
        self.seen = self.seen.max(step);
        self.focus = 0;
        self.cursor = 0;
        self.scroll = 0;
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
            Step::Config if !std::path::Path::new(answers.location.trim()).is_absolute() => {
                Some("the config directory must be an absolute path".into())
            }
            Step::Generations => [&answers.keep, &answers.keep_days]
                .into_iter()
                .find(|value| value.trim().parse::<u32>().is_err())
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
        let len = self.controls().len();
        if len == 0 {
            return;
        }
        self.focus = (self.focus as isize + delta).rem_euclid(len as isize) as usize;
        self.cursor = 0;
    }

    fn change(&mut self, delta: isize) {
        let cycle = |value: usize, len: usize| (value as isize + delta).rem_euclid(len as isize) as usize;
        let focused = self.focused();
        let users = self.facts.dotfiles.len();
        let answers = &mut self.answers;
        match focused {
            Some(Control::Fallback) => answers.fallback = cycle(answers.fallback, FALLBACK.len()),
            Some(Control::MissingVersions) => {
                answers.missing_versions = cycle(answers.missing_versions, MISSING_VERSIONS.len());
            }
            Some(Control::Orphans) => answers.keep_orphans = !answers.keep_orphans,
            Some(Control::Strict) => answers.strict = !answers.strict,
            Some(Control::Git) => answers.git = !answers.git,
            Some(Control::AurReview) => answers.aur_review = !answers.aur_review,
            Some(Control::Snapshots) => answers.snapshots = !answers.snapshots,
            Some(Control::Boot) => answers.boot = !answers.boot,
            Some(Control::DotUser) => {
                answers.dotfiles_user = cycle(answers.dotfiles_user, users);
                answers.dotfiles.clear();
            }
            _ => {}
        }
    }

    fn toggle(&mut self) {
        match self.focused() {
            Some(Control::Files) => {
                if !self.answers.files.remove(&self.cursor) {
                    self.answers.files.insert(self.cursor);
                }
            }
            Some(Control::Dotfiles) => {
                if let Some(name) = self.dotfile_candidates().get(self.cursor).cloned()
                    && !self.answers.dotfiles.remove(&name)
                {
                    self.answers.dotfiles.insert(name);
                }
            }
            _ => self.change(1),
        }
    }

    fn select(&mut self, keep: impl Fn(&Facts, usize) -> bool) {
        match self.focused() {
            Some(Control::Files) => {
                self.answers.files = (0..self.facts.files.len()).filter(|i| keep(self.facts, *i)).collect();
            }
            Some(Control::Dotfiles) => {
                let chosen = self
                    .dotfile_candidates()
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| keep(self.facts, *i))
                    .map(|(_, name)| name.clone())
                    .collect();
                self.answers.dotfiles = chosen;
            }
            _ => {}
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
                    Span::styled(format!("{} ", i + 1), Style::new().fg(QUIET)),
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
        let block = Block::new().padding(Padding::new(3, 2, 1, 0));
        let inner = block.inner(area);
        let height = inner.height;
        if let Some(line) = cursor_line {
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

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let key = |k: &'static str| Span::styled(k, Style::new().bold());
        let gap = || Span::styled("  ·  ", Style::new().fg(QUIET));
        let label = |t: &'static str| Span::styled(t, Style::new().fg(QUIET));
        let mut hints = vec![Span::raw(" ")];
        if self.current() == Step::Review {
            hints.extend([key("enter"), label(" write the config"), gap()]);
        } else {
            hints.extend([key("enter"), label(" next"), gap()]);
        }
        hints.extend([key("esc"), label(" back"), gap()]);
        match self.focused() {
            Some(Control::Files | Control::Dotfiles) => hints.extend([
                key("↑↓"),
                label(" move"),
                gap(),
                key("space"),
                label(" toggle"),
                gap(),
                key("a/n"),
                label(" all/none"),
                gap(),
            ]),
            Some(Control::Location | Control::Keep | Control::KeepDays) => {
                hints.extend([
                    key("type"),
                    label(" to edit"),
                    gap(),
                    key("tab"),
                    label(" next field"),
                    gap(),
                ]);
            }
            Some(_) => hints.extend([key("↑↓"), label(" field"), gap(), key("←→"), label(" choose"), gap()]),
            None => hints.extend([key("↑↓"), label(" scroll"), gap()]),
        }
        hints.extend([key("1-9"), label(" jump"), gap(), key("q"), label(" quit")]);
        let mut lines = vec![Line::from(hints)];
        if let Some(error) = &self.error {
            lines.insert(0, Line::styled(format!(" ! {error}"), Style::new().fg(WARN).bold()));
        } else {
            lines.insert(0, Line::default());
        }
        frame.render_widget(Paragraph::new(lines), area);
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
                if facts.settings_summary.is_empty() {
                    lines.push(note(
                        "no hostname, timezone, locale or keymap archix can take over exactly".into(),
                    ));
                } else {
                    lines.push(done(facts.settings_summary.join(", ")));
                }
                lines.push(Line::default());
                lines.push(Line::styled("Users", Style::new().bold()));
                for (name, user) in &facts.accounts.users {
                    let groups = if user.groups.is_empty() {
                        String::new()
                    } else {
                        format!("  groups {}", user.groups.join(", "))
                    };
                    lines.push(Line::from(vec![
                        Span::raw(format!("  {name}")),
                        Span::styled(
                            format!("  uid {}  {}{groups}", user.uid, user.shell),
                            Style::new().fg(QUIET),
                        ),
                    ]));
                }
                if facts.accounts.users.is_empty() {
                    lines.push(note("none in the regular uid range".into()));
                }
                lines.push(note("passwords stay where they are; archix never manages them".into()));
                lines.push(Line::default());
                lines.push(Line::styled(
                    "Services that differ from their preset",
                    Style::new().bold(),
                ));
                for (unit, on) in &facts.services {
                    let (mark, color) = if *on { ("on ", GOOD) } else { ("off", WARN) };
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {mark} "), Style::new().fg(color)),
                        Span::raw(unit.clone()),
                    ]));
                }
                if facts.services.is_empty() {
                    lines.push(note("none".into()));
                }
            }
            Step::Files => {
                if facts.secrets > 0 {
                    lines.push(note(format!(
                        "{} that hold secrets stay out of the config and aren't listed",
                        count(facts.secrets, "file", "files")
                    )));
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
                }
                lines.push(Line::default());
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
                        "The first switch backs up the originals. Nothing is ticked by default.".into(),
                    ));
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
        lines.push(row("Config", format!("{}{git}", answers.location.trim())));
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
        let users: Vec<String> = facts.accounts.users.keys().cloned().collect();
        lines.push(row(
            "Accounts",
            if users.is_empty() {
                "none".into()
            } else {
                users.join(", ")
            },
        ));
        lines.push(row("Services", count(facts.services.len(), "unit", "units")));
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
                answers.keep.trim(),
                answers.keep_days.trim()
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
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::raw("Press "),
            Span::styled("enter", Style::new().fg(ACCENT).bold()),
            Span::raw(" to write the config and lock it to what's installed. "),
            Span::styled("esc", Style::new().fg(ACCENT).bold()),
            Span::raw(" goes back."),
        ]));
    }

    fn control(&self, control: Control, lines: &mut Vec<Line<'static>>, cursor_line: &mut Option<usize>) {
        let focused = self.focused() == Some(control);
        let answers = &self.answers;
        let label_style = if focused {
            Style::new().fg(ACCENT).bold()
        } else {
            Style::new().bold()
        };
        let bar = if focused { "▌ " } else { "  " };
        let heading = |text: &str| {
            Line::from(vec![
                Span::styled(bar, Style::new().fg(ACCENT)),
                Span::styled(text.to_string(), label_style),
            ])
        };
        let choice = |lines: &mut Vec<Line<'static>>, title: &str, options: &[&str], selected: usize| {
            lines.push(heading(title));
            for (i, option) in options.iter().enumerate() {
                let (mark, style) = if i == selected {
                    ("●", if focused { Style::new().fg(ACCENT) } else { Style::new() })
                } else {
                    ("○", Style::new().fg(QUIET))
                };
                lines.push(Line::styled(format!("    {mark} {option}"), style));
            }
            lines.push(Line::default());
        };
        let toggle = |lines: &mut Vec<Line<'static>>, title: &str, on: bool| {
            let mark = if on { "[x]" } else { "[ ]" };
            let mark_style = if on {
                Style::new().fg(GOOD)
            } else {
                Style::new().fg(QUIET)
            };
            lines.push(Line::from(vec![
                Span::styled(bar, Style::new().fg(ACCENT)),
                Span::styled(format!("{mark} "), mark_style),
                Span::styled(title.to_string(), label_style),
            ]));
            lines.push(Line::default());
        };
        let text = |lines: &mut Vec<Line<'static>>, title: &str, value: &str| {
            lines.push(heading(title));
            let caret = if focused { "▏" } else { "" };
            let style = if focused { Style::new().fg(ACCENT) } else { Style::new() };
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(format!("{value}{caret}"), style.add_modifier(Modifier::UNDERLINED)),
            ]));
            lines.push(Line::default());
        };
        match control {
            Control::Location => text(lines, "Where should the config live?", &answers.location),
            Control::Git => toggle(lines, "Make it a git repository", answers.git),
            Control::Fallback => {
                let labels = FALLBACK.map(|(_, label)| label);
                choice(
                    lines,
                    "If a locked package disappears from a third-party repo",
                    &labels,
                    answers.fallback,
                );
            }
            Control::Orphans => choice(
                lines,
                "Dependencies nothing needs any more",
                &ORPHANS,
                usize::from(!answers.keep_orphans),
            ),
            Control::Strict => choice(
                lines,
                "Later, when you install something with plain pacman or yay",
                &STRICT,
                usize::from(!answers.strict),
            ),
            Control::AurReview => toggle(
                lines,
                "Show each AUR build recipe and ask before trusting it",
                answers.aur_review,
            ),
            Control::MissingVersions => {
                let labels = MISSING_VERSIONS.map(|(_, label)| label);
                choice(
                    lines,
                    "If a locked AUR version can't be built any more",
                    &labels,
                    answers.missing_versions,
                );
            }
            Control::Snapshots => toggle(
                lines,
                "Snapshot / before every switch, as an extra recovery point",
                answers.snapshots,
            ),
            Control::Boot => toggle(lines, "Write a boot entry for each generation", answers.boot),
            Control::Keep => text(lines, "Generations gc always keeps", &answers.keep),
            Control::KeepDays => text(lines, "…and any younger than this many days", &answers.keep_days),
            Control::DotUser => {
                let users: Vec<String> = self.users().into_iter().cloned().collect();
                let labels: Vec<&str> = users.iter().map(String::as_str).collect();
                choice(lines, "Whose dotfiles", &labels, answers.dotfiles_user);
            }
            Control::Files => {
                lines.push(heading("Config files archix manages"));
                for (i, file) in self.facts.files.iter().enumerate() {
                    let checked = answers.files.contains(&i);
                    let why = if file.edited {
                        "changed from its package's version"
                    } else {
                        "not from any package"
                    };
                    let tag = if file.recommended { "recommended · " } else { "" };
                    self.item(
                        lines,
                        cursor_line,
                        focused && i == self.cursor,
                        checked,
                        &file.path,
                        &format!("{tag}{why}"),
                    );
                }
                lines.push(Line::default());
            }
            Control::Dotfiles => {
                lines.push(heading("Dotfiles to bring in"));
                for (i, name) in self.dotfile_candidates().iter().enumerate() {
                    let checked = answers.dotfiles.contains(name);
                    self.item(lines, cursor_line, focused && i == self.cursor, checked, name, "");
                }
                lines.push(Line::default());
            }
        }
    }

    fn item(
        &self,
        lines: &mut Vec<Line<'static>>,
        cursor_line: &mut Option<usize>,
        under_cursor: bool,
        checked: bool,
        name: &str,
        detail: &str,
    ) {
        if under_cursor {
            *cursor_line = Some(lines.len());
        }
        let mark = if checked { "[x]" } else { "[ ]" };
        let pointer = if under_cursor { "  › " } else { "    " };
        let mark_style = if checked {
            Style::new().fg(GOOD)
        } else {
            Style::new().fg(QUIET)
        };
        let name_style = if under_cursor {
            Style::new().fg(ACCENT).bold()
        } else {
            Style::new()
        };
        lines.push(Line::from(vec![
            Span::styled(pointer, Style::new().fg(ACCENT)),
            Span::styled(format!("{mark} "), mark_style),
            Span::styled(name.to_string(), name_style),
            Span::styled(
                if detail.is_empty() {
                    String::new()
                } else {
                    format!("  {detail}")
                },
                Style::new().fg(QUIET),
            ),
        ]));
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
            settings: Vec::new(),
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
                (PathBuf::from("/home/me"), vec![".config/nvim".into(), ".zshrc".into()]),
            )]),
        }
    }

    fn press(app: &mut App, code: KeyCode) -> Flow {
        app.handle(KeyEvent::from(code))
    }

    fn render(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 34)).unwrap();
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
    fn answers_survive_going_back_and_forth() {
        let facts = facts();
        let mut app = App::new(&facts, "box", Answers::recommended(&facts));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current(), Step::Packages);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.focused(), Some(Control::Strict));
        press(&mut app, KeyCode::Right);
        assert!(!app.answers.strict);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current(), Step::System);
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.current(), Step::Welcome);
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(app.current(), Step::Packages);
        assert!(!app.answers.strict, "the answer is still there");
    }

    #[test]
    fn the_file_checklist_toggles_and_resets() {
        let facts = facts();
        let mut app = App::new(&facts, "box", Answers::recommended(&facts));
        press(&mut app, KeyCode::Char('5'));
        assert_eq!(app.answers.files, [0].into());
        press(&mut app, KeyCode::Char(' '));
        assert!(app.answers.files.is_empty());
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
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
    fn text_fields_are_checked_before_moving_on() {
        let facts = facts();
        let mut app = App::new(&facts, "box", Answers::recommended(&facts));
        press(&mut app, KeyCode::Enter);
        for _ in 0.."/etc/archix".len() {
            press(&mut app, KeyCode::Backspace);
        }
        for c in "relative".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert_eq!(
            app.answers.location, "relative",
            "letters edit the field instead of acting as keys"
        );
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.current(), Step::Config);
        assert!(render(&mut app).contains("must be an absolute path"));
    }

    #[test]
    fn every_step_renders_and_review_finishes() {
        let facts = facts();
        let mut app = App::new(&facts, "box", Answers::recommended(&facts));
        for step in STEPS {
            assert_eq!(app.current(), step);
            let screen = render(&mut app);
            assert!(screen.contains(step.blurb()), "{screen}");
            if step == Step::Review {
                assert!(screen.contains("1 file"), "{screen}");
                assert_eq!(press(&mut app, KeyCode::Enter), Flow::Done);
            } else {
                press(&mut app, KeyCode::Enter);
            }
        }
        assert_eq!(press(&mut app, KeyCode::Char('q')), Flow::Quit);
    }
}
