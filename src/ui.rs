use crate::theme;
use crate::{
    backend,
    model::{self, Layout, Mode, Monitor},
    profiles,
    transaction::Transaction,
};
use anyhow::{Context, Result, ensure};
use crossterm::{
    event::{
        self, Event, KeyCode as K, KeyEvent, KeyEventKind, KeyModifiers as Mods, MouseButton,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    prelude::*,
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    io::{self, Read, Write},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const FIELDS: [&str; 8] = [
    "Resolution",
    "Refresh rate",
    "Scale",
    "Orientation",
    "Enabled",
    "Mirror",
    "X",
    "Y",
];
const HELP: &str = concat!(
    "SETTINGS   Select a display to edit its draft settings.\n",
    "           Scale and coordinates support inline editing.\n\n",
    "MOVE       Reposition displays, align them to a reference,\n",
    "           or snap their edges together.\n\n",
    "APPLY      Review changes before a 15-second preview.\n",
    "           Unconfirmed changes are automatically reverted.\n\n",
    "PROFILES   Save named layouts, open them for editing,\n",
    "           rename them or activate a saved layout."
);
const ROTATIONS: [&str; 8] = [
    "Normal",
    "Left 90°",
    "Inverted 180°",
    "Right 270°",
    "Flipped",
    "Flipped 90°",
    "Flipped 180°",
    "Flipped 270°",
];
enum Dialog {
    Help,
    Choice {
        selected: usize,
        options: Vec<String>,
    },
    Review(Vec<String>),
    Quit,
    Input {
        kind: char,
        value: String,
    },
}
#[derive(Clone, Copy)]
enum Hit {
    Monitor(usize),
    Field(usize),
    Step(usize, bool),
    Choice(usize),
    Key(K),
}
struct Drag {
    index: usize,
    column: u16,
    row: u16,
    x: i32,
    y: i32,
}
struct InlineEdit {
    field: usize,
    value: String,
    replace: bool,
}
struct App {
    base: Layout,
    draft: Layout,
    selected: usize,
    reference: usize,
    field: usize,
    properties: bool,
    moving: bool,
    status: String,
    dialog: Option<Dialog>,
    transaction: Option<Transaction>,
    ready: Option<Instant>,
    overlay: Option<Child>,
    stale: bool,
    demo: bool,
    boxes: Vec<(usize, Rect)>,
    hits: Vec<(Hit, Rect)>,
    replace_input: bool,
    edit: Option<InlineEdit>,
    profile_name: Option<String>,
    rename_source: Option<String>,
    profile_entries: Vec<profiles::Summary>,
    units: f64,
    origin: (i32, i32),
    drag: Option<Drag>,
    quit: bool,
}
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            event::DisableMouseCapture,
            crossterm::cursor::Show,
            LeaveAlternateScreen
        );
        // Restore the previous terminal title using the xterm title stack.
        let _ = io::stdout().write_all(b"\x1b[23;0t");
        let _ = io::stdout().flush();
    }
}
impl Drop for App {
    fn drop(&mut self) {
        if let Some(t) = &mut self.transaction {
            let _ = t.send("CANCEL");
            let _ = t.child.wait();
        }
        if let Some(child) = &mut self.overlay {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn run(base: Layout, demo: bool, profile: Option<&str>) -> Result<()> {
    ensure!(!base.is_empty(), "No connected displays");
    let draft = if let Some(name) = profile {
        profiles::load(name, &base)?
    } else {
        base.clone()
    };
    let mut app = App {
        reference: usize::from(base.len() > 1),
        base,
        draft,
        selected: 0,
        field: 0,
        properties: false,
        moving: false,
        status: "Select a display to edit. Changes take effect only after review.".into(),
        dialog: None,
        transaction: None,
        ready: None,
        overlay: None,
        stale: false,
        demo,
        boxes: Vec::new(),
        hits: Vec::new(),
        replace_input: false,
        edit: None,
        profile_name: profile.map(str::to_owned),
        rename_source: None,
        profile_entries: Vec::new(),
        units: 1.,
        origin: (0, 0),
        drag: None,
        quit: false,
    };
    let (tx, rx) = mpsc::channel();
    if !demo {
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(2));
                if tx
                    .send(backend::query().map_err(|e| format!("{e:#}")))
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    enable_raw_mode()?;
    let _guard = TerminalGuard;
    io::stdout().write_all(b"\x1b[22;0t")?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        event::EnableMouseCapture,
        crossterm::terminal::SetTitle("Hypr Monitor TUI")
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    while !app.quit {
        while let Ok(result) = rx.try_recv() {
            if app.transaction.is_none() {
                match result {
                    Ok(current) => {
                        if !model::same_state(&app.base, &current) {
                            app.stale = true;
                            app.drag = None;
                            app.status = "External layout/hotplug change. Reload required.".into();
                        }
                    }
                    Err(error) => {
                        app.stale = true;
                        app.status = error;
                    }
                }
            }
        }
        app.poll_transaction();
        app.poll_overlay();
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(75))? {
            let result = match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => app.key(key),
                Event::Mouse(mouse) => {
                    app.mouse(mouse);
                    Ok(())
                }
                Event::Resize(..) => {
                    app.drag = None;
                    Ok(())
                }
                _ => Ok(()),
            };
            if let Err(error) = result {
                app.status = format!("{error:#}");
            }
        }
    }
    Ok(())
}

impl App {
    fn poll_transaction(&mut self) {
        let mut done = false;
        let mut confirmed = false;
        if let Some(t) = &mut self.transaction {
            while let Ok(message) = t.messages.try_recv() {
                match message.as_str() {
                    "READY" => {
                        self.ready = Some(Instant::now());
                        self.status =
                            "Preview active. Confirmation required within 15 seconds.".into();
                    }
                    "CONFIRMED" => {
                        confirmed = true;
                        done = true;
                        self.status = "Layout confirmed.".into();
                    }
                    "REVERTED" => {
                        done = true;
                        self.status = "Previous layout restored.".into();
                    }
                    _ => {
                        self.status = message;
                        done = true;
                    }
                }
            }
            if !done && let Ok(Some(_)) = t.child.try_wait() {
                done = true;
                self.status = "Apply worker exited; reloading actual state.".into();
            }
        }
        if done {
            if let Some(mut t) = self.transaction.take() {
                let _ = t.child.wait();
            }
            self.ready = None;
            match backend::query() {
                Ok(current) => {
                    self.base = current.clone();
                    self.draft = current;
                    self.selected = self.selected.min(self.draft.len().saturating_sub(1));
                    self.stale = false;
                }
                Err(e) => {
                    self.stale = true;
                    self.status = format!("{}; {e}", self.status);
                }
            }
            if confirmed {
                self.dialog = None;
            }
        }
    }
    fn poll_overlay(&mut self) {
        if let Some(child) = &mut self.overlay
            && let Ok(Some(status)) = child.try_wait()
        {
            if !status.success() {
                let mut error = String::new();
                if let Some(err) = &mut child.stderr {
                    let _ = err.read_to_string(&mut error);
                }
                self.status = format!("Identification unavailable: {}", error.trim());
            }
            self.overlay = None;
        }
    }
    fn identify(&mut self) -> Result<()> {
        ensure!(!self.demo, "Identification is unavailable in demo mode");
        if let Some(mut old) = self.overlay.take() {
            let _ = old.kill();
            let _ = old.wait();
            return Ok(());
        }
        let labels: std::collections::HashMap<_, _> = self
            .base
            .iter()
            .enumerate()
            .filter(|(_, m)| m.enabled && m.mirror.is_none())
            .map(|(i, m)| (m.name.clone(), i + 1))
            .collect();
        self.overlay = Some(
            Command::new(std::env::current_exe()?)
                .args(["--identify-data", &serde_json::to_string(&labels)?])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?,
        );
        self.status =
            "Identifying for 3 seconds. Mirrored outputs may repeat the source label.".into();
        Ok(())
    }
    fn key(&mut self, key: KeyEvent) -> Result<()> {
        if let Some(transaction) = &mut self.transaction {
            if key.code == K::Enter && self.ready.is_some() {
                transaction.send("CONFIRM")?;
            }
            if matches!(key.code, K::Esc | K::Char('q'))
                || (key.code == K::Char('c') && key.modifiers.contains(Mods::CONTROL))
            {
                transaction.send("CANCEL")?;
            }
            return Ok(());
        }
        if let Some(dialog) = self.dialog.take() {
            return self.dialog_key(dialog, key);
        }
        if self.edit.is_some() {
            return self.edit_key(key);
        }
        if self.draft.is_empty() && !matches!(key.code, K::Char('r' | 'q') | K::Esc) {
            self.status = "No outputs detected.".into();
            return Ok(());
        }
        if key.code == K::Char('c') && key.modifiers.contains(Mods::CONTROL) {
            self.request_quit();
            return Ok(());
        }
        match key.code {
            K::Char('q') if self.moving => {
                self.moving = false;
                self.drag = None;
            }
            K::Char('q') => self.request_quit(),
            K::Char('?') => self.dialog = Some(Dialog::Help),
            K::Char('i') => self.identify()?,
            K::Char('r') => {
                if !self.demo {
                    self.base = backend::query()?;
                }
                self.draft = self.base.clone();
                self.selected = 0;
                self.reference = usize::from(self.draft.len() > 1);
                self.stale = false;
                self.moving = false;
                self.status = "Reloaded current layout; draft discarded.".into();
            }
            K::Char('a') => {
                ensure!(
                    !self.stale,
                    "Refresh before applying; current state changed"
                );
                let mut lines = Vec::new();
                for (i, m) in self.draft.iter().enumerate() {
                    lines.push(format!(
                        "{} {}: {} → {}",
                        i + 1,
                        m.name,
                        self.base
                            .iter()
                            .find(|b| b.name == m.name)
                            .map(summary)
                            .unwrap_or_default(),
                        summary(m)
                    ));
                }
                lines.extend(
                    model::validate(&self.draft)?
                        .into_iter()
                        .map(|w| format!("WARNING: {w}")),
                );
                self.dialog = Some(Dialog::Review(lines));
            }
            K::Char('s') => {
                ensure!(!self.demo, "Profiles are disabled in demo mode");
                self.dialog = Some(Dialog::Input {
                    kind: 's',
                    value: self.profile_name.clone().unwrap_or_default(),
                });
                self.replace_input = true;
            }
            K::Char('p' | 'o') => {
                ensure!(!self.demo, "Profiles are disabled in demo mode");
                self.open_profiles(None)?;
            }
            K::Char('m') => {
                self.moving = !self.moving;
                self.properties = false;
            }
            K::Esc => {
                self.moving = false;
                self.drag = None;
            }
            K::Tab => {
                if self.moving && !self.draft.is_empty() {
                    self.reference = (self.reference + 1) % self.draft.len();
                    if self.reference == self.selected && self.draft.len() > 1 {
                        self.reference = (self.reference + 1) % self.draft.len();
                    }
                } else {
                    self.properties = !self.properties;
                }
            }
            K::Char(c @ ('h' | 'j' | 'k' | 'l' | 'c' | 'v')) if self.moving => {
                model::place(&mut self.draft, self.selected, self.reference, c)?;
            }
            K::Char('n') if self.moving => model::snap(&mut self.draft, self.selected, 30),
            K::Up | K::Down | K::Left | K::Right if !self.draft.is_empty() => {
                let positive = matches!(key.code, K::Down | K::Right);
                if self.moving {
                    let m = &mut self.draft[self.selected];
                    ensure!(m.independent(), "Only extended outputs can move");
                    let step = if key.modifiers.contains(Mods::CONTROL) {
                        1
                    } else if key.modifiers.contains(Mods::SHIFT) {
                        100
                    } else {
                        10
                    };
                    let delta = if positive { step } else { -step };
                    if matches!(key.code, K::Left | K::Right) {
                        m.x = (m.x + delta).clamp(-100_000, 100_000);
                    } else {
                        m.y = (m.y + delta).clamp(-100_000, 100_000);
                    }
                } else if self.properties {
                    if matches!(key.code, K::Up | K::Down) {
                        self.field = cycle(self.field, FIELDS.len(), positive);
                    } else {
                        self.change(positive)?;
                    }
                } else {
                    self.selected = cycle(self.selected, self.draft.len(), positive);
                }
            }
            K::Enter if self.properties && !self.draft.is_empty() => {
                self.open_setting()?;
            }
            K::Char(c)
                if self.properties
                    && matches!(self.field, 2 | 6 | 7)
                    && (c.is_ascii_digit() || c == '-' || c == '.') =>
            {
                self.open_setting()?;
                self.key(key)?;
            }
            _ => {}
        }
        Ok(())
    }
    fn request_quit(&mut self) {
        if self.draft != self.base {
            self.dialog = Some(Dialog::Quit);
        } else {
            self.quit = true;
        }
    }
    fn open_profiles(&mut self, name: Option<&str>) -> Result<()> {
        self.profile_entries = profiles::list(&self.base)?;
        let options: Vec<_> = self
            .profile_entries
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let selected = name
            .and_then(|name| options.iter().position(|p| p == name))
            .unwrap_or(0);
        self.dialog = Some(Dialog::Choice { selected, options });
        Ok(())
    }
    fn dialog_key(&mut self, dialog: Dialog, key: KeyEvent) -> Result<()> {
        match dialog {
            Dialog::Choice {
                mut selected,
                options,
            } => {
                // Preserve the manager on errors instead of dropping the modal.
                self.dialog = Some(Dialog::Choice {
                    selected,
                    options: options.clone(),
                });
                match key.code {
                    K::Esc | K::Char('q') => {
                        self.dialog = None;
                        return Ok(());
                    }
                    K::Enter | K::Char('e' | 'a') => {
                        let name = options
                            .get(selected)
                            .ok_or_else(|| anyhow::anyhow!("No saved profiles."))?;
                        match profiles::load(name, &self.base) {
                            Ok(layout) => {
                                self.draft = layout;
                                self.profile_name = Some(name.clone());
                                self.properties = false;
                                self.moving = false;
                                self.dialog = None;
                                self.status = format!(
                                    "Editing {name}. Changes remain in the draft until saved or applied."
                                );
                            }
                            Err(error) => {
                                self.dialog = Some(Dialog::Choice { selected, options });
                                return Err(error);
                            }
                        }
                        if key.code == K::Char('a') {
                            self.key(KeyEvent::new(K::Char('a'), Mods::NONE))?;
                        }
                        return Ok(());
                    }
                    K::Char('s' | 'n') => {
                        self.replace_input = false;
                        self.dialog = Some(Dialog::Input {
                            kind: 'n',
                            value: String::new(),
                        });
                        return Ok(());
                    }
                    K::Char('r') => {
                        let name = options
                            .get(selected)
                            .context("Select a profile to rename")?;
                        self.rename_source = Some(name.clone());
                        self.replace_input = true;
                        self.dialog = Some(Dialog::Input {
                            kind: 'r',
                            value: name.clone(),
                        });
                        return Ok(());
                    }
                    K::Char('u') => {
                        let name = options
                            .get(selected)
                            .context("Select the profile being edited")?;
                        ensure!(
                            self.profile_name.as_deref() == Some(name),
                            "Open this profile for editing first"
                        );
                        profiles::save(name, &self.draft)?;
                        self.status = format!("Saved changes to {name}.");
                        self.open_profiles(Some(name))?;
                        return Ok(());
                    }
                    K::Up | K::Left => selected = cycle(selected, options.len(), false),
                    K::Down | K::Right => selected = cycle(selected, options.len(), true),
                    _ => {}
                }
                self.dialog = Some(Dialog::Choice { selected, options });
            }
            Dialog::Help => {
                if !matches!(key.code, K::Esc | K::Char('?') | K::Char('q') | K::Enter) {
                    self.dialog = Some(Dialog::Help);
                }
            }
            Dialog::Quit => match key.code {
                K::Char('y') => self.quit = true,
                K::Esc | K::Char('n' | 'q') => {}
                _ => self.dialog = Some(Dialog::Quit),
            },
            Dialog::Review(lines) => match key.code {
                K::Char('y') => {
                    if self.demo {
                        self.status = "Demo validation passed; no runtime changes applied.".into();
                    } else {
                        backend::check_version()?;
                        let current = backend::query()?;
                        ensure!(
                            model::same_state(&self.base, &current),
                            "External change detected; refresh first"
                        );
                        self.transaction = Some(Transaction::start(current, self.draft.clone())?);
                        self.status = "Applying with independent rollback guard…".into();
                    }
                }
                K::Esc | K::Char('n' | 'q') => {}
                _ => self.dialog = Some(Dialog::Review(lines)),
            },
            Dialog::Input { kind, mut value } => match key.code {
                K::Esc | K::Char('q') => {
                    if matches!(kind, 'n' | 'r' | 's') {
                        self.open_profiles(None)?;
                    }
                }
                K::Enter => {
                    let result = (|| -> Result<()> {
                        match kind {
                            's' | 'n' => {
                                if kind == 'n' {
                                    profiles::create(&value, &self.draft)?;
                                } else {
                                    profiles::save(&value, &self.draft)?;
                                }
                                self.profile_name = Some(value.clone());
                                self.status =
                                    format!("Saved {value}. Open to edit or Activate to apply.");
                                self.open_profiles(Some(&value))?;
                            }
                            'r' => {
                                let old = self
                                    .rename_source
                                    .as_ref()
                                    .context("No profile selected for rename")?;
                                profiles::rename(old, &value)?;
                                if self.profile_name.as_ref() == Some(old) {
                                    self.profile_name = Some(value.clone());
                                }
                                self.rename_source = None;
                                self.status = format!("Renamed profile to {value}.");
                                self.open_profiles(Some(&value))?;
                            }
                            'o' => {
                                self.draft = profiles::load(&value, &self.base)?;
                                self.status = format!("Loaded {value} as draft.");
                            }
                            'z' => {
                                let scale: f64 = value.parse()?;
                                ensure!(
                                    scale.is_finite() && (0.25..=4.).contains(&scale),
                                    "Scale must be 0.25–4"
                                );
                                self.draft[self.selected].scale = scale;
                            }
                            'x' | 'y' => {
                                let position: i32 = value.parse()?;
                                ensure!(
                                    position.abs_diff(0) <= 100_000,
                                    "Position must be within ±100000"
                                );
                                if kind == 'x' {
                                    self.draft[self.selected].x = position;
                                } else {
                                    self.draft[self.selected].y = position;
                                }
                            }
                            _ => {}
                        };
                        Ok(())
                    })();
                    if let Err(error) = result {
                        self.replace_input = false;
                        self.dialog = Some(Dialog::Input { kind, value });
                        return Err(error);
                    }
                }
                K::Backspace => {
                    if self.replace_input {
                        value.clear();
                    } else {
                        value.pop();
                    }
                    self.replace_input = false;
                    self.dialog = Some(Dialog::Input { kind, value });
                }
                K::Char('u') if key.modifiers.contains(Mods::CONTROL) => {
                    self.replace_input = false;
                    self.dialog = Some(Dialog::Input {
                        kind,
                        value: String::new(),
                    });
                }
                K::Char(c) if value.len() < 80 => {
                    if self.replace_input {
                        value.clear();
                    }
                    self.replace_input = false;
                    value.push(c);
                    self.dialog = Some(Dialog::Input { kind, value });
                }
                _ => self.dialog = Some(Dialog::Input { kind, value }),
            },
        }
        Ok(())
    }
    fn open_setting(&mut self) -> Result<()> {
        let m = &self.draft[self.selected];
        let numeric = match self.field {
            2 => Some(('z', m.scale.to_string())),
            6 => Some(('x', m.x.to_string())),
            7 => Some(('y', m.y.to_string())),
            _ => None,
        };
        if let Some((kind, value)) = numeric {
            let _ = kind;
            self.edit = Some(InlineEdit {
                field: self.field,
                value,
                replace: true,
            });
            self.status = "Editing value.".into();
        } else {
            self.status = "Setting selected.".into();
        }
        Ok(())
    }
    fn finish_edit(&mut self) -> Result<()> {
        let Some(edit) = &self.edit else {
            return Ok(());
        };
        let m = &mut self.draft[self.selected];
        if edit.field == 2 {
            let value: f64 = edit
                .value
                .parse()
                .map_err(|_| anyhow::anyhow!("Enter a valid scale"))?;
            ensure!(
                value.is_finite() && (0.25..=4.).contains(&value),
                "Scale must be 0.25–4"
            );
            m.scale = value;
        } else {
            let value: i32 = edit
                .value
                .parse()
                .map_err(|_| anyhow::anyhow!("Enter an integer position"))?;
            ensure!(
                value.abs_diff(0) <= 100_000,
                "Position must be within ±100000"
            );
            if edit.field == 6 {
                m.x = value;
            } else {
                m.y = value;
            }
        }
        self.edit = None;
        self.status = "Value updated in draft.".into();
        Ok(())
    }
    fn edit_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            K::Enter => return self.finish_edit(),
            K::Esc | K::Char('q') => {
                self.edit = None;
                self.status = "Edit cancelled.".into();
            }
            K::Char('u') if key.modifiers.contains(Mods::CONTROL) => {
                let e = self.edit.as_mut().unwrap();
                e.value.clear();
                e.replace = false;
            }
            K::Backspace => {
                let e = self.edit.as_mut().unwrap();
                if e.replace {
                    e.value.clear();
                } else {
                    e.value.pop();
                }
                e.replace = false;
            }
            K::Char(c) if c.is_ascii_digit() || matches!(c, '-' | '.') => {
                let e = self.edit.as_mut().unwrap();
                if e.replace {
                    e.value.clear();
                }
                e.replace = false;
                if e.value.len() < 20 {
                    e.value.push(c);
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn choices(&self, field: usize) -> (usize, Vec<String>) {
        let m = &self.draft[self.selected];
        let (value, options) = match field {
            0 => {
                let mut sizes: Vec<_> = m
                    .modes
                    .iter()
                    .map(|mode| (mode.width, mode.height))
                    .collect();
                sizes.sort_by_key(|(w, h)| (i64::from(*w) * i64::from(*h), *w, *h));
                sizes.dedup();
                let values = sizes.iter().map(|(w, h)| format!("{w} × {h}")).collect();
                (format!("{} × {}", m.mode.width, m.mode.height), values)
            }
            1 => {
                let mut modes: Vec<_> = m
                    .modes
                    .iter()
                    .filter(|mode| mode.width == m.mode.width && mode.height == m.mode.height)
                    .collect();
                modes.sort_by(|a, b| a.hz.total_cmp(&b.hz));
                let mut options: Vec<_> = modes
                    .iter()
                    .map(|mode| format!("{:.3} Hz", mode.hz))
                    .collect();
                options.dedup();
                (format!("{:.3} Hz", m.mode.hz), options)
            }
            3 => (
                ROTATIONS[m.transform.min(7) as usize].into(),
                ROTATIONS.iter().map(|s| s.to_string()).collect(),
            ),
            4 => (
                if m.enabled { "On" } else { "Off" }.into(),
                vec!["Off".into(), "On".into()],
            ),
            5 => (
                m.mirror.clone().unwrap_or_else(|| "Extended".into()),
                std::iter::once("Extended".into())
                    .chain(
                        self.draft
                            .iter()
                            .enumerate()
                            .filter(|(i, m)| *i != self.selected && m.independent())
                            .map(|(_, m)| m.name.clone()),
                    )
                    .collect(),
            ),
            _ => return (0, Vec::new()),
        };
        (
            options.iter().position(|s| s == &value).unwrap_or(0),
            options,
        )
    }
    fn set_choice(&mut self, index: usize) -> Result<()> {
        let (_, options) = self.choices(self.field);
        let value = options
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("Choice no longer available"))?;
        let m = &mut self.draft[self.selected];
        match self.field {
            0 => {
                m.mode = m
                    .modes
                    .iter()
                    .filter(|mode| format!("{} × {}", mode.width, mode.height) == *value)
                    .max_by(|a, b| a.hz.total_cmp(&b.hz))
                    .ok_or_else(|| anyhow::anyhow!("Resolution unavailable"))?
                    .clone()
            }
            1 => {
                m.mode = m
                    .modes
                    .iter()
                    .find(|mode| {
                        mode.width == m.mode.width
                            && mode.height == m.mode.height
                            && format!("{:.3} Hz", mode.hz) == *value
                    })
                    .ok_or_else(|| anyhow::anyhow!("Refresh rate unavailable"))?
                    .clone()
            }
            3 => m.transform = index as u32,
            4 => m.enabled = index == 1,
            5 => {
                m.mirror = if index == 0 {
                    None
                } else {
                    Some(value.clone())
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn can_step(&self, field: usize, positive: bool) -> bool {
        if self.edit.as_ref().is_some_and(|e| e.field == field) {
            return false;
        }
        let m = &self.draft[self.selected];
        match field {
            2 => {
                if positive {
                    m.scale < 4.
                } else {
                    m.scale > 0.25
                }
            }
            6 | 7 => {
                let value = if field == 6 { m.x } else { m.y };
                if positive {
                    value < 100_000
                } else {
                    value > -100_000
                }
            }
            _ => {
                let (index, options) = self.choices(field);
                if positive {
                    index + 1 < options.len()
                } else {
                    index > 0
                }
            }
        }
    }
    fn change(&mut self, positive: bool) -> Result<()> {
        if !self.can_step(self.field, positive) {
            return Ok(());
        }
        if matches!(self.field, 0 | 1 | 3 | 4 | 5) {
            let (index, options) = self.choices(self.field);
            return self.set_choice(cycle(index, options.len(), positive));
        }
        let m = &mut self.draft[self.selected];
        match self.field {
            2 => {
                m.scale = (m.scale + if positive { 0.25 } else { -0.25 }).clamp(0.25, 4.);
            }
            6 => m.x = (m.x + if positive { 10 } else { -10 }).clamp(-100_000, 100_000),
            7 => m.y = (m.y + if positive { 10 } else { -10 }).clamp(-100_000, 100_000),
            _ => {}
        }
        Ok(())
    }
    fn mouse(&mut self, mouse: event::MouseEvent) {
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            if let Err(error) = self.finish_edit() {
                self.status = error.to_string();
                return;
            }
            let positive = mouse.kind == MouseEventKind::ScrollDown;
            if let Some(Dialog::Choice { selected, options }) = &mut self.dialog {
                *selected = cycle(*selected, options.len(), positive);
                return;
            }
            if self.dialog.is_none()
                && self.transaction.is_none()
                && !self.stale
                && let Some((hit, _)) = self
                    .hits
                    .iter()
                    .rev()
                    .find(|(_, r)| r.contains(Position::new(mouse.column, mouse.row)))
            {
                match hit {
                    Hit::Field(_) | Hit::Step(..) => {
                        self.properties = true;
                        self.moving = false;
                        self.field = cycle(self.field, FIELDS.len(), positive);
                    }
                    Hit::Monitor(_) => {
                        self.selected = cycle(self.selected, self.draft.len(), positive);
                    }
                    _ => {}
                }
            }
            return;
        }
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            if self.edit.is_some()
                && let Some((Hit::Key(code @ (K::Enter | K::Esc | K::Char('q'))), _)) = self
                    .hits
                    .iter()
                    .rev()
                    .find(|(_, rect)| rect.contains(Position::new(mouse.column, mouse.row)))
            {
                let code = *code;
                if let Err(error) = self.edit_key(KeyEvent::new(code, Mods::NONE)) {
                    self.status = error.to_string();
                }
                return;
            }
            if let Err(error) = self.finish_edit() {
                self.status = error.to_string();
                return;
            }
            let point = Position::new(mouse.column, mouse.row);
            if let Some((hit, _)) = self
                .hits
                .iter()
                .rev()
                .find(|(_, rect)| rect.contains(point))
            {
                let hit = *hit;
                self.drag = None;
                let result = match hit {
                    Hit::Key(code) => self.key(KeyEvent::new(code, Mods::NONE)),
                    Hit::Choice(index) => {
                        if let Some(Dialog::Choice { selected, .. }) = &mut self.dialog {
                            *selected = index;
                        }
                        Ok(())
                    }
                    Hit::Field(index) => {
                        self.field = index;
                        self.properties = true;
                        self.moving = false;
                        self.status = "Setting selected.".into();
                        Ok(())
                    }
                    Hit::Step(index, positive) => {
                        self.field = index;
                        self.properties = true;
                        self.moving = false;
                        self.change(positive)
                    }
                    Hit::Monitor(index) => {
                        self.selected = index;
                        self.properties = false;
                        self.moving = false;
                        Ok(())
                    }
                };
                if let Err(error) = result {
                    self.status = format!("{error:#}");
                }
                return;
            }
        }
        if self.transaction.is_some() || self.dialog.is_some() || self.stale {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((index, _)) = self
                    .boxes
                    .iter()
                    .rev()
                    .find(|(_, rect)| rect.contains(Position::new(mouse.column, mouse.row)))
                {
                    self.selected = *index;
                    let m = &self.draft[*index];
                    self.drag = Some(Drag {
                        index: *index,
                        column: mouse.column,
                        row: mouse.row,
                        x: m.x,
                        y: m.y,
                    });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(d) = &self.drag {
                    let m = &mut self.draft[d.index];
                    m.x = (d.x
                        + ((mouse.column as f64 - d.column as f64) * self.units).round() as i32)
                        .clamp(-100_000, 100_000);
                    m.y = (d.y
                        + ((mouse.row as f64 - d.row as f64) * self.units * 2.).round() as i32)
                        .clamp(-100_000, 100_000);
                }
            }
            MouseEventKind::Up(_) => {
                if let Some(d) = self.drag.take()
                    && (self.draft[d.index].x, self.draft[d.index].y) != (d.x, d.y)
                {
                    model::snap(&mut self.draft, d.index, 30);
                }
            }
            _ => {}
        }
    }
    fn controls(&self) -> &'static [(&'static str, &'static str)] {
        if self.transaction.is_some() {
            &[("Enter", "Keep layout"), ("q", "Revert")]
        } else if let Some(dialog) = &self.dialog {
            match dialog {
                Dialog::Review(_) => &[("y", "Test changes"), ("q", "Cancel")],
                Dialog::Quit => &[("y", "Discard/quit"), ("q", "Return")],
                Dialog::Help => &[("q", "Close guide")],
                Dialog::Input {
                    kind: 's' | 'n', ..
                } => &[("Enter", "Save profile"), ("q", "Back to profiles")],
                Dialog::Input { kind: 'r', .. } => {
                    &[("Enter", "Rename"), ("q", "Back to profiles")]
                }
                Dialog::Input { .. } => &[("Enter", "Accept"), ("q", "Cancel")],
                Dialog::Choice { .. } => &[
                    ("↑ / ↓", "Choose"),
                    ("Enter", "Open/edit"),
                    ("n", "Create profile"),
                    ("r", "Rename"),
                    ("u", "Save changes"),
                    ("a", "Activate"),
                    ("q", "Close"),
                ],
            }
        } else if self.edit.is_some() {
            &[
                ("Enter", "Accept value"),
                ("q", "Cancel edit"),
                ("Ctrl+U", "Clear value"),
            ]
        } else if self.moving {
            &[
                ("↑↓←→", "Move"),
                ("Shift+↑↓←→", "Coarse move"),
                ("Ctrl+↑↓←→", "Fine move"),
                ("Tab", "Reference"),
                ("h/j/k/l", "Place beside"),
                ("c / v", "Center Y / X"),
                ("n", "Snap edges"),
                ("q", "Finish moving"),
                ("?", "Help"),
            ]
        } else if self.properties {
            &[
                ("↑ / ↓", "Select field"),
                ("← / →", "Change value"),
                ("Enter", "Edit value"),
                ("Tab", "Displays"),
                ("a", "Review/apply"),
                ("i", "Identify"),
                ("p", "Profiles"),
                ("s", "Save profile"),
                ("?", "Help"),
                ("q", "Quit"),
            ]
        } else {
            &[
                ("↑ / ↓", "Select"),
                ("Tab", "Settings"),
                ("m", "Move"),
                ("i", "Identify"),
                ("a", "Review/apply"),
                ("s", "Save profile"),
                ("p", "Profiles"),
                ("r", "Reload"),
                ("?", "Help"),
                ("q", "Quit"),
            ]
        }
    }
    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(
            Block::default().style(Style::default().fg(theme::TEXT)),
            area,
        );
        self.boxes.clear();
        self.hits.clear();
        if area.width < 72 || area.height < 22 {
            let width = area.width.saturating_sub(2).min(54);
            let height = area.height.saturating_sub(2).min(11);
            let popup = Rect::new(
                area.x + (area.width - width) / 2,
                area.y + (area.height - height) / 2,
                width,
                height,
            );
            let preview = self.ready.map(|ready| {
                format!(
                    "Reverting in {}s",
                    15u64.saturating_sub(ready.elapsed().as_secs())
                )
            });
            let text = if height >= 9 {
                format!(
                    "\nMake room for your displays\n\nCurrent  {} × {}\nMinimum  72 × 22\n\n{}",
                    area.width,
                    area.height,
                    preview.as_deref().unwrap_or("Resize to continue")
                )
            } else {
                format!(
                    "{} × {} → 72 × 22\n{}",
                    area.width,
                    area.height,
                    preview.as_deref().unwrap_or("Resize to continue")
                )
            };
            frame.render_widget(
                Paragraph::new(text)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false })
                    .block(panel().title(" Resize terminal ")),
                popup,
            );
            return;
        }
        let controls = self.controls();
        let profile_dialog = matches!(
            self.dialog,
            Some(
                Dialog::Choice { .. }
                    | Dialog::Input {
                        kind: 's' | 'n' | 'r',
                        ..
                    }
            )
        );
        let control_columns = (area.width / 28).clamp(2, 4) as usize;
        let controls_height = if profile_dialog {
            0
        } else {
            1 + controls.len().div_ceil(control_columns) as u16
        };
        let rows = ratatui::layout::Layout::vertical([
            Constraint::Min(10),
            Constraint::Length(3 + controls_height),
        ])
        .split(area);
        let columns = ratatui::layout::Layout::horizontal([
            Constraint::Length(31),
            Constraint::Length(1),
            Constraint::Min(30),
        ])
        .split(rows[0]);
        let left = ratatui::layout::Layout::vertical([
            Constraint::Length((self.draft.len() + 2).min(8) as u16),
            Constraint::Min(8),
        ])
        .split(columns[0]);
        let items: Vec<_> = self
            .draft
            .iter()
            .enumerate()
            .map(|(i, m)| {
                ListItem::new(format!(
                    "{} {} {}",
                    i + 1,
                    m.name,
                    if !m.enabled {
                        "[off]"
                    } else if m.mirror.is_some() {
                        "[mirror]"
                    } else {
                        ""
                    }
                ))
            })
            .collect();
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(
            List::new(items)
                .block(panel().title(" Displays "))
                .highlight_symbol("› ")
                .highlight_style(Style::default().fg(theme::ACCENT).bold()),
            left[0],
            &mut state,
        );
        if self.transaction.is_none() && !self.stale {
            let inner = panel().inner(left[0]);
            for index in
                state.offset()..self.draft.len().min(state.offset() + inner.height as usize)
            {
                self.hits.push((
                    Hit::Monitor(index),
                    Rect::new(
                        inner.x,
                        inner.y + (index - state.offset()) as u16,
                        inner.width,
                        1,
                    ),
                ));
            }
        }
        if let Some(m) = self.draft.get(self.selected) {
            let values = [
                format!("{}x{}", m.mode.width, m.mode.height),
                format!("{:.3} Hz", m.mode.hz),
                m.scale.to_string(),
                ROTATIONS[m.transform.min(7) as usize].into(),
                if m.enabled { "On" } else { "Off" }.into(),
                m.mirror.clone().unwrap_or_else(|| "Extended".into()),
                m.x.to_string(),
                m.y.to_string(),
            ];
            let settings_inner = panel().inner(left[1]);
            let value_width = settings_inner.width.saturating_sub(18) as usize;
            let text: Vec<Line> = FIELDS
                .iter()
                .zip(values)
                .enumerate()
                .map(|(i, (name, value))| {
                    let value = if let Some(edit) = self.edit.as_ref().filter(|e| e.field == i) {
                        format!("{}_", edit.value)
                    } else {
                        value
                    };
                    let left = if self.can_step(i, false) { '‹' } else { ' ' };
                    let right = if self.can_step(i, true) { '›' } else { ' ' };
                    let value: String = value.chars().take(value_width).collect();
                    Line::styled(
                        format!("{name:<12} {left} {value:<value_width$} {right}"),
                        if self.properties && i == self.field {
                            Style::default().fg(theme::ACCENT).bold()
                        } else {
                            Style::default()
                        },
                    )
                })
                .collect();
            let mut settings_state =
                ListState::default().with_selected(self.properties.then_some(self.field));
            frame.render_stateful_widget(
                List::new(text.into_iter().map(ListItem::new).collect::<Vec<_>>()).block(
                    panel().title(if self.edit.is_some() {
                        " Settings · editing "
                    } else {
                        " Settings "
                    }),
                ),
                left[1],
                &mut settings_state,
            );
            if self.transaction.is_none() && !self.stale {
                let inner = panel().inner(left[1]);
                for index in settings_state.offset()
                    ..FIELDS
                        .len()
                        .min(settings_state.offset() + inner.height as usize)
                {
                    let y = inner.y + (index - settings_state.offset()) as u16;
                    self.hits
                        .push((Hit::Field(index), Rect::new(inner.x, y, inner.width, 1)));
                    if self.can_step(index, false) {
                        self.hits
                            .push((Hit::Step(index, false), Rect::new(inner.x + 13, y, 1, 1)));
                    }
                    if self.can_step(index, true) {
                        self.hits.push((
                            Hit::Step(index, true),
                            Rect::new(inner.right() - 2, y, 1, 1),
                        ));
                    }
                }
            }
        }
        let block = panel().title(if self.moving {
            format!(
                " Move display · reference: {} ",
                self.draft
                    .get(self.reference)
                    .map(|m| m.name.as_str())
                    .unwrap_or("none")
            )
        } else {
            " Layout ".into()
        });
        let inner = block.inner(columns[2]).inner(Margin::new(2, 1));
        frame.render_widget(block, columns[2]);
        self.diagram(frame, inner);
        let footer = if let Some(ready) = self.ready {
            format!(
                "Keep this layout? Reverting automatically in {} seconds.",
                15u64.saturating_sub(ready.elapsed().as_secs())
            )
        } else {
            self.status.clone()
        };
        let bottom = ratatui::layout::Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(controls_height),
        ])
        .split(rows[1]);
        frame.render_widget(
            Paragraph::new(footer).wrap(Wrap { trim: false }).block(
                Block::default()
                    .border_type(BorderType::Thick)
                    .border_style(Style::default().fg(theme::BORDER))
                    .borders(Borders::TOP)
                    .title(format!(
                        " {}{} ",
                        self.profile_name
                            .as_ref()
                            .map(|name| format!("Profile: {name}"))
                            .unwrap_or_default(),
                        if self.demo { " · Demo" } else { "" }
                    )),
            ),
            bottom[0],
        );
        if !profile_dialog {
            draw_controls(frame, bottom[1], controls, &mut self.hits);
        }
        if let Some(dialog) = &self.dialog {
            // Only the modal and its bottom controls are clickable while open.
            self.hits.retain(|(_, rect)| rect.y >= bottom[1].y);
            let text = match dialog {
                Dialog::Choice { .. } => String::new(),
                Dialog::Help => HELP.into(),
                Dialog::Review(lines) => format!(
                    "{}\n\nApply temporarily?\nConfirm within 15 seconds after apply; otherwise rollback.",
                    lines.join("\n")
                ),
                Dialog::Quit => "Discard draft and quit?".into(),
                Dialog::Input { kind, value } => format!(
                    "{}\n\n{value}_",
                    match kind {
                        's' | 'n' => "Name",
                        'r' => "New name",
                        'o' => "Load profile name",
                        'x' => "X position (logical pixels)",
                        'y' => "Y position (logical pixels)",
                        _ => "Scale",
                    }
                ),
            };
            let title = match dialog {
                Dialog::Help => " Help ",
                Dialog::Quit => " Unsaved changes ",
                Dialog::Input { kind: 's', .. } => " Save profile ",
                Dialog::Input { kind: 'n', .. } => " Create profile ",
                Dialog::Input { kind: 'r', .. } => " Rename profile ",
                Dialog::Input { kind: 'o', .. } => " Open profile ",
                Dialog::Input { .. } => " Edit value ",
                Dialog::Choice { .. } => " Profiles ",
                Dialog::Review(_) => " Review changes ",
            };
            let height = if profile_dialog {
                rows[0].height.saturating_sub(2)
            } else if matches!(dialog, Dialog::Input { .. } | Dialog::Quit) {
                10
            } else {
                area.height - 10
            };
            let popup = Rect::new(area.x + 3, area.y + 1, area.width - 6, height);
            let guide_height = if profile_dialog {
                1 + controls
                    .len()
                    .div_ceil(((popup.width - 2) / 28).clamp(2, 4) as usize)
                    as u16
            } else {
                0
            };
            self.hits
                .retain(|(_, rect)| rect.intersection(popup).is_empty());
            frame.render_widget(Clear, popup);
            if let Dialog::Choice { selected, options } = dialog {
                let mut state = ListState::default().with_selected(Some(*selected));
                let block = panel().title(" Profiles ");
                let mut inner = block.inner(popup);
                frame.render_widget(
                    Paragraph::new("Saved profiles"),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                inner.y += 2;
                inner.height = inner.height.saturating_sub(5 + guide_height);
                frame.render_widget(block, popup);
                frame.render_stateful_widget(
                    List::new(
                        options
                            .iter()
                            .enumerate()
                            .map(|(i, s)| {
                                let p = &self.profile_entries[i];
                                ListItem::new(format!(
                                    "{s} · {} displays · {}",
                                    p.displays,
                                    if p.compatible {
                                        "compatible"
                                    } else {
                                        "different displays"
                                    }
                                ))
                            })
                            .collect::<Vec<_>>(),
                    )
                    .highlight_symbol("› ")
                    .highlight_style(Style::default().fg(theme::ACCENT).bold()),
                    inner,
                    &mut state,
                );
                if options.is_empty() {
                    frame.render_widget(
                        Paragraph::new("No saved profiles.").wrap(Wrap { trim: false }),
                        inner,
                    );
                }
                if let Some(p) = self.profile_entries.get(*selected) {
                    let date = |s: &str| {
                        if s.is_empty() {
                            "unknown".to_string()
                        } else {
                            s.strip_suffix('Z').unwrap_or(s).replace('T', " ")
                        }
                    };
                    frame.render_widget(
                        Paragraph::new(format!(
                            "Created: {}\nSaved:   {}",
                            date(&p.created_at),
                            date(&p.updated_at)
                        )),
                        Rect::new(inner.x, inner.bottom() + 1, inner.width, 2),
                    );
                }
                for index in
                    state.offset()..options.len().min(state.offset() + inner.height as usize)
                {
                    self.hits.push((
                        Hit::Choice(index),
                        Rect::new(
                            inner.x,
                            inner.y + (index - state.offset()) as u16,
                            inner.width,
                            1,
                        ),
                    ));
                }
            } else {
                frame.render_widget(
                    Paragraph::new(text)
                        .wrap(Wrap { trim: false })
                        .block(panel().title(title)),
                    popup,
                );
            }
            let buttons: &[(&str, K)] = match dialog {
                Dialog::Review(_) => &[("[ Test changes ]", K::Char('y')), ("[ Cancel ]", K::Esc)],
                Dialog::Quit => &[("[ Discard / Quit ]", K::Char('y')), ("[ Cancel ]", K::Esc)],
                Dialog::Help => &[("[ Close ]", K::Esc)],
                Dialog::Choice { .. } => &[],
                Dialog::Input {
                    kind: 's' | 'n' | 'r',
                    ..
                } => &[],
                _ => &[("[ Accept ]", K::Enter), ("[ Cancel ]", K::Esc)],
            };
            let button_y = popup.bottom().saturating_sub(2);
            let mut button_x = popup.x + 2;
            for (label, code) in buttons.iter() {
                let rect = Rect::new(button_x, button_y, label.len() as u16, 1);
                frame.render_widget(
                    Paragraph::new(*label).style(Style::default().fg(theme::ACCENT).bold()),
                    rect,
                );
                self.hits.push((Hit::Key(*code), rect));
                button_x += label.len() as u16 + 3;
            }
            if profile_dialog {
                draw_controls(
                    frame,
                    Rect::new(
                        popup.x + 1,
                        popup.bottom() - 1 - guide_height,
                        popup.width - 2,
                        guide_height,
                    ),
                    controls,
                    &mut self.hits,
                );
            }
        }
    }
    fn diagram(&mut self, frame: &mut Frame, area: Rect) {
        let active: Vec<_> = self
            .draft
            .iter()
            .enumerate()
            .filter(|(_, m)| m.independent())
            .collect();
        if active.is_empty() {
            return;
        }
        let minx = active.iter().map(|(_, m)| m.x).min().unwrap();
        let miny = active.iter().map(|(_, m)| m.y).min().unwrap();
        let maxx = active.iter().map(|(_, m)| m.x + m.size().0).max().unwrap();
        let maxy = active.iter().map(|(_, m)| m.y + m.size().1).max().unwrap();
        // Freeze the conversion during drag to prevent geometry feedback jumps.
        if self.drag.is_none() {
            self.units = ((maxx - minx) as f64 / area.width.saturating_sub(2).max(1) as f64)
                .max((maxy - miny) as f64 / (area.height.saturating_sub(2).max(1) as f64 * 2.))
                .max(1.);
            let spare_x = (area.width as f64 * self.units - (maxx - minx) as f64) / 2.;
            let spare_y = (area.height as f64 * self.units * 2. - (maxy - miny) as f64) / 2.;
            self.origin = (minx - spare_x.round() as i32, miny - spare_y.round() as i32);
        }
        let (minx, miny) = self.origin;
        for (i, m) in active {
            let (w, h) = m.size();
            let x = area.x + ((m.x - minx) as f64 / self.units) as u16;
            let y = area.y + ((m.y - miny) as f64 / (self.units * 2.)) as u16;
            let rect = Rect::new(
                x,
                y,
                ((w as f64 / self.units) as u16).max(5),
                ((h as f64 / (self.units * 2.)) as u16).max(3),
            )
            .intersection(area);
            // Visual gutters do not alter desktop coordinates or snapping.
            let rect = if rect.width >= 9 {
                rect.inner(Margin::new(1, 0))
            } else {
                rect
            };
            let rect = Rect::new(
                rect.x,
                rect.y,
                rect.width,
                if rect.height >= 5 {
                    rect.height - 1
                } else {
                    rect.height
                },
            );
            if rect.width == 0 || rect.height == 0 {
                continue;
            }
            let text = format!(
                "{}\n{} × {}\n{}, {}",
                m.name, m.mode.width, m.mode.height, m.x, m.y
            );
            frame.render_widget(
                Paragraph::new(text).alignment(Alignment::Center).block(
                    Block::default()
                        .border_type(BorderType::Thick)
                        .borders(Borders::ALL)
                        .title(format!(" {} ", i + 1))
                        .border_style(Style::default().fg(if i == self.selected {
                            theme::ACCENT
                        } else {
                            theme::BORDER
                        })),
                ),
                rect,
            );
            self.boxes.push((i, rect));
        }
    }
}
fn draw_controls(
    frame: &mut Frame,
    area: Rect,
    controls: &[(&str, &str)],
    hits: &mut Vec<(Hit, Rect)>,
) {
    let block = Block::default()
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(theme::BORDER))
        .borders(Borders::TOP)
        .title(" Controls ");
    let grid = block.inner(area);
    frame.render_widget(block, area);
    let count = (grid.width / 28).clamp(2, 4) as usize;
    let columns =
        ratatui::layout::Layout::horizontal(vec![Constraint::Ratio(1, count as u32); count])
            .split(grid);
    for (i, (key, label)) in controls.iter().enumerate() {
        let column = columns[i % count];
        let row = (i / count) as u16;
        if row >= column.height {
            break;
        }
        let rect = Rect::new(column.x, column.y + row, column.width, 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{key:<11}"),
                    Style::default().fg(theme::ACCENT).bold(),
                ),
                Span::raw(*label),
            ])),
            rect,
        );
        if let Some(code) = control_key(key) {
            hits.push((Hit::Key(code), rect));
        }
    }
}

fn panel() -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(theme::BORDER))
}

fn control_key(label: &str) -> Option<K> {
    match label {
        "Enter" | "Esc/Enter" => Some(K::Enter),
        "Esc" | "n / Esc" => Some(K::Esc),
        "Tab" => Some(K::Tab),
        s if s.chars().count() == 1 => Some(K::Char(s.chars().next()?.to_ascii_lowercase())),
        _ => None,
    }
}

fn cycle(index: usize, len: usize, positive: bool) -> usize {
    if len == 0 {
        0
    } else if positive {
        index.saturating_add(1).min(len - 1)
    } else {
        index.saturating_sub(1)
    }
}
fn summary(m: &Monitor) -> String {
    if !m.enabled {
        "OFF".into()
    } else {
        format!(
            "{} ({},{}) s={} rot={} {}",
            m.mode.label(),
            m.x,
            m.y,
            m.scale,
            m.transform,
            m.mirror
                .as_ref()
                .map(|s| format!("mirror {s}"))
                .unwrap_or_else(|| "extend".into())
        )
    }
}

pub fn demo() -> Layout {
    ["eDP-1", "DP-1", "HDMI-A-1"]
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let mode = Mode::parse("1920x1080@60Hz").unwrap();
            Monitor {
                name: (*name).into(),
                model: "Demo display".into(),
                enabled: true,
                mode: mode.clone(),
                modes: vec![
                    mode,
                    Mode::parse("1920x1080@120Hz").unwrap(),
                    Mode::parse("1280x720@60Hz").unwrap(),
                ],
                x: i as i32 * 1920,
                y: 0,
                scale: 1.,
                transform: 0,
                mirror: None,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app() -> App {
        let base = demo();
        App {
            draft: base.clone(),
            base,
            selected: 1,
            reference: 0,
            field: 0,
            properties: false,
            moving: false,
            status: String::new(),
            dialog: None,
            transaction: None,
            ready: None,
            overlay: None,
            stale: false,
            demo: true,
            boxes: Vec::new(),
            hits: Vec::new(),
            replace_input: false,
            edit: None,
            profile_name: None,
            rename_source: None,
            profile_entries: Vec::new(),
            units: 1.,
            origin: (0, 0),
            drag: None,
            quit: false,
        }
    }
    fn key(code: K) -> KeyEvent {
        KeyEvent::new(code, Mods::NONE)
    }
    #[test]
    fn keyboard_edits_stay_in_draft() {
        let mut a = app();
        a.key(key(K::Char('m'))).unwrap();
        a.key(key(K::Right)).unwrap();
        assert_eq!(a.draft[1].x, 1930);
        a.key(KeyEvent::new(K::Down, Mods::SHIFT)).unwrap();
        a.key(KeyEvent::new(K::Left, Mods::CONTROL)).unwrap();
        assert_eq!((a.draft[1].x, a.draft[1].y), (1929, 100));
        a.key(key(K::Char('k'))).unwrap();
        assert_eq!((a.draft[1].x, a.draft[1].y), (0, -1080));
        assert_eq!(a.base[1].x, 1920);
        a.key(key(K::Esc)).unwrap();
        a.key(key(K::Tab)).unwrap();
        a.key(key(K::Left)).unwrap();
        assert_eq!(a.draft[1].mode.width, 1280);
        a.key(key(K::Down)).unwrap();
        a.key(key(K::Right)).unwrap();
        assert_eq!(a.draft[1].mode.hz, 60.);
    }
    #[test]
    fn diagram_mouse_drag_and_small_terminal() {
        let mut a = app();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(110, 34)).unwrap();
        terminal.draw(|f| a.draw(f)).unwrap();
        let (_, rect) = a.boxes.iter().find(|(i, _)| *i == 1).unwrap();
        let (x, y) = (rect.x + 1, rect.y + 1);
        let event = |kind, col| event::MouseEvent {
            kind,
            column: col,
            row: y,
            modifiers: Mods::NONE,
        };
        a.mouse(event(MouseEventKind::Down(MouseButton::Left), x));
        a.mouse(event(MouseEventKind::Drag(MouseButton::Left), x + 3));
        a.mouse(event(MouseEventKind::Up(MouseButton::Left), x + 3));
        assert!(a.draft[1].x > 1920);
        assert!(a.drag.is_none());
        terminal.draw(|f| a.draw(f)).unwrap();
        let mut small = Terminal::new(ratatui::backend::TestBackend::new(30, 8)).unwrap();
        small.draw(|f| a.draw(f)).unwrap();
        assert!(a.boxes.is_empty());
    }
    #[test]
    fn invalid_layout_never_opens_apply_dialog() {
        let mut a = app();
        for monitor in &mut a.draft {
            monitor.enabled = false;
        }
        assert!(a.key(key(K::Char('a'))).is_err());
        assert!(a.dialog.is_none());
    }
    fn click(a: &mut App, rect: Rect) {
        a.mouse(event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: Mods::NONE,
        });
    }
    #[test]
    fn clickable_numeric_fields_replace_and_validate() {
        let mut a = app();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(110, 34)).unwrap();
        terminal.draw(|f| a.draw(f)).unwrap();
        let rect = a
            .hits
            .iter()
            .find(|(hit, _)| matches!(hit, Hit::Field(6)))
            .unwrap()
            .1;
        click(&mut a, rect);
        assert!(a.dialog.is_none());
        assert!(a.edit.is_none());
        assert_eq!(a.field, 6);
        for c in "-250".chars() {
            a.key(key(K::Char(c))).unwrap();
        }
        terminal.draw(|f| a.draw(f)).unwrap();
        let accept = a
            .hits
            .iter()
            .find(|(hit, _)| matches!(hit, Hit::Key(K::Enter)))
            .unwrap()
            .1;
        click(&mut a, accept);
        assert_eq!(a.draft[1].x, -250);
        assert!(a.edit.is_none());
        assert_eq!(a.base[1].x, 1920);
        a.field = 7;
        a.properties = true;
        a.key(key(K::Char('9'))).unwrap();
        for _ in 0..6 {
            a.key(key(K::Char('9'))).unwrap();
        }
        assert!(a.key(key(K::Enter)).is_err());
        assert!(a.dialog.is_none());
        assert!(a.edit.is_some());
        assert_eq!(a.draft[1].y, 0);
    }
    #[test]
    fn scrolling_commits_to_original_monitor_and_invalid_input_blocks_selection() {
        let mut a = app();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(110, 34)).unwrap();
        a.properties = true;
        a.field = 6;
        for c in "-250".chars() {
            a.key(key(K::Char(c))).unwrap();
        }
        terminal.draw(|f| a.draw(f)).unwrap();
        let rect = a
            .hits
            .iter()
            .find(|(h, _)| matches!(h, Hit::Monitor(1)))
            .unwrap()
            .1;
        let scroll = event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: rect.x,
            row: rect.y,
            modifiers: Mods::NONE,
        };
        a.mouse(scroll);
        assert_eq!(a.selected, 2);
        assert_eq!(a.draft[1].x, -250);
        assert_eq!(a.draft[2].x, a.base[2].x);
        a.key(key(K::Char('-'))).unwrap();
        a.mouse(event::MouseEvent {
            kind: MouseEventKind::ScrollUp,
            ..scroll
        });
        assert_eq!(a.selected, 2);
        assert!(a.edit.is_some());
        a.key(key(K::Esc)).unwrap();
        assert_eq!(a.draft[2].x, a.base[2].x);
    }
    #[test]
    fn dialogs_close_with_q_or_escape_and_only_advertise_q() {
        for code in [K::Char('q'), K::Esc] {
            for dialog in [
                Dialog::Help,
                Dialog::Quit,
                Dialog::Review(Vec::new()),
                Dialog::Choice {
                    selected: 0,
                    options: Vec::new(),
                },
                Dialog::Input {
                    kind: 'x',
                    value: "0".into(),
                },
            ] {
                let mut a = app();
                a.dialog = Some(dialog);
                assert!(a.controls().iter().any(|(key, _)| *key == "q"));
                assert!(!a.controls().iter().any(|(key, _)| key.contains("Esc")));
                a.key(key(code)).unwrap();
                assert!(a.dialog.is_none());
                assert!(!a.quit);
            }
            let mut a = app();
            a.properties = true;
            a.field = 6;
            a.key(key(K::Char('5'))).unwrap();
            a.key(key(code)).unwrap();
            assert!(a.edit.is_none());
            assert_eq!(a.draft, a.base);
        }
    }
    #[test]
    fn profile_buttons_fit_and_empty_actions_keep_manager_open() {
        for (width, height) in [(72, 22), (110, 34)] {
            let mut a = app();
            a.dialog = Some(Dialog::Choice {
                selected: 0,
                options: Vec::new(),
            });
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| a.draw(f)).unwrap();
            for code in [
                K::Char('n'),
                K::Enter,
                K::Char('r'),
                K::Char('u'),
                K::Char('a'),
                K::Char('q'),
            ] {
                let rect = a
                    .hits
                    .iter()
                    .rev()
                    .find(|(h, _)| matches!(h,Hit::Key(k) if *k==code))
                    .unwrap()
                    .1;
                assert!(rect.right() <= width && rect.bottom() <= height);
                assert!(rect.x >= 4 && rect.right() <= width - 4);
                assert!(
                    rect.y >= height - 10 && rect.bottom() <= height - 4,
                    "Profile controls belong inside the window"
                );
            }
            let open = a
                .hits
                .iter()
                .rev()
                .find(|(h, _)| matches!(h, Hit::Key(K::Enter)))
                .unwrap()
                .1;
            click(&mut a, open);
            assert!(matches!(a.dialog, Some(Dialog::Choice { .. })));
            let create = a
                .hits
                .iter()
                .rev()
                .find(|(h, _)| matches!(h, Hit::Key(K::Char('n'))))
                .unwrap()
                .1;
            click(&mut a, create);
            assert!(matches!(a.dialog, Some(Dialog::Input { kind: 'n', .. })));
        }
    }
    #[test]
    fn mouse_choices_and_nonwrapping_limits() {
        let mut a = app();
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(110, 34)).unwrap();
        terminal.draw(|f| a.draw(f)).unwrap();
        assert!(!a.hits.iter().any(|(h, _)| matches!(h, Hit::Step(0, true))));
        let field = a
            .hits
            .iter()
            .find(|(h, _)| matches!(h, Hit::Field(0)))
            .unwrap()
            .1;
        click(&mut a, field);
        assert!(a.dialog.is_none());
        assert_eq!(a.draft[1].mode.width, 1920);
        terminal.draw(|f| a.draw(f)).unwrap();
        let low = a
            .hits
            .iter()
            .find(|(h, _)| matches!(h, Hit::Step(0, false)))
            .unwrap()
            .1;
        click(&mut a, low);
        assert_eq!(a.draft[1].mode.width, 1280);
        terminal.draw(|f| a.draw(f)).unwrap();
        assert!(!a.hits.iter().any(|(h, _)| matches!(h, Hit::Step(1, _))));
        a.key(key(K::Left)).unwrap();
        assert_eq!(a.draft[1].mode.width, 1280);
        a.key(key(K::Right)).unwrap();
        assert_eq!(a.draft[1].mode.width, 1920);
        a.key(key(K::Right)).unwrap();
        assert_eq!(a.draft[1].mode.width, 1920);
        a.field = 1;
        a.change(true).unwrap();
        assert_eq!(a.draft[1].mode.hz, 120.);
        a.change(true).unwrap();
        assert_eq!(a.draft[1].mode.hz, 120.);
        a.change(false).unwrap();
        a.change(false).unwrap();
        assert_eq!(a.draft[1].mode.hz, 60.);
    }
}
