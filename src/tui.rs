//! Live terminal interface (`wisplet` with no arguments).
//!
//! This module is the *pure* part: keys in, screen text out. [`App`] holds the state, reacts to
//! [`Key`]s and talks to the daemon through the [`Backend`] trait; [`App::render`] draws a frame
//! as plain strings. Nothing here touches the terminal, so all of it is unit-tested. The raw
//! terminal handling lives in `term.rs`.
//!
//! Keys: ↑/↓ (or k/j) select · `a` add · `p`/space pause-resume · `x` remove · `i`/Enter details ·
//! `/` search torrents · `s` download folder · `w` watch folder · `m` completed folder · `M` this torrent's completed folder · `q` quit (downloads keep running) · `Q` quit and stop the daemon.

use std::collections::HashSet;

use crate::cli::{absolute, classify_source, expand_path, render_info, SourceArg};
use crate::error::{Error, Result};
use crate::format;
use crate::protocol::{AddSource, Command, MoveSpec, Row};
use crate::search::SearchResult;

/// Finished search: the results plus a one-line summary of how each source answered, or an error.
pub type SearchOutcome = std::result::Result<(Vec<SearchResult>, String), String>;

/// What the interface needs from the outside world: daemon calls and (optionally) torrent search.
pub trait Backend {
    fn call(&mut self, cmd: &Command) -> Result<Vec<String>>;

    /// Start a search in the background; its answer arrives through [`Backend::poll_search`].
    fn start_search(&mut self, _query: &str) -> Result<()> {
        Err(Error::Failed("search is not available".into()))
    }

    /// Forget the search in progress.
    fn cancel_search(&mut self) {}

    /// `Some` once the search started with [`Backend::start_search`] has finished.
    fn poll_search(&mut self) -> Option<SearchOutcome> {
        None
    }

    /// Show a file or folder in the system file manager (Finder on macOS).
    fn reveal(&mut self, _path: &std::path::Path) -> Result<()> {
        Err(Error::Failed("showing files is not available".into()))
    }

    /// Download a search result (by its magnet link, or by fetching its `.torrent`).
    fn add_result(&mut self, r: &SearchResult) -> Result<Vec<String>> {
        if r.magnet.is_empty() {
            return Err(Error::Failed("this result has no magnet link".into()));
        }
        self.call(&Command::Add { source: AddSource::Magnet(r.magnet.clone()), dir: None, move_to: MoveSpec::Inherit })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Delete,
    Enter,
    Esc,
    Backspace,
    CtrlC,
    CtrlU,
    Char(char),
}

/// Decode raw terminal input. Unknown escape sequences are dropped; a lone ESC is [`Key::Esc`].
pub fn parse_keys(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1b => {
                if bytes.get(i + 1).is_none() {
                    keys.push(Key::Esc);
                    i += 1;
                } else if matches!(bytes[i + 1], b'[' | b'O') {
                    // CSI / SS3: parameters, then one final byte in 0x40..=0x7e
                    let mut j = i + 2;
                    while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                        j += 1;
                    }
                    if j < bytes.len() {
                        match (&bytes[i + 2..j], bytes[j]) {
                            ([], b'A') => keys.push(Key::Up),
                            ([], b'B') => keys.push(Key::Down),
                            ([b'3'], b'~') => keys.push(Key::Delete),
                            _ => {}
                        }
                    }
                    i = j + 1;
                } else {
                    keys.push(Key::Esc); // ESC followed by something else (e.g. Alt-key): just Esc
                    i += 1;
                }
            }
            b'\r' | b'\n' => {
                keys.push(Key::Enter);
                i += 1;
            }
            0x7f | 0x08 => {
                keys.push(Key::Backspace);
                i += 1;
            }
            0x03 => {
                keys.push(Key::CtrlC);
                i += 1;
            }
            0x15 => {
                keys.push(Key::CtrlU);
                i += 1;
            }
            b if b < 0x20 => i += 1, // other control bytes
            _ => {
                let len = match b {
                    0..=0x7f => 1,
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    _ => 4,
                };
                let end = (i + len).min(bytes.len());
                if let Ok(s) = std::str::from_utf8(&bytes[i..end]) {
                    keys.extend(s.chars().map(Key::Char));
                }
                i = end;
            }
        }
    }
    keys
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
    /// Quit and also stop the daemon.
    QuitAndStop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputKind {
    Add,
    /// Default download folder.
    Folder,
    /// Folder watched for `.torrent` files (`off` disables it).
    Watch,
    /// Global folder completed torrents move to (`off` disables it).
    Completed,
    /// Completed-folder override of one torrent (`off` = never move, `default` = global setting).
    MoveTorrent(u32),
    /// Text to search for.
    Search,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    Input(InputKind, String),
    /// Asking whether to remove this torrent id.
    Confirm(u32),
    Info(Vec<String>),
    /// Waiting for the search engines.
    Searching(String),
    Results { query: String, items: Vec<SearchResult>, selected: usize, status: String },
}

#[derive(Debug, Clone)]
struct Notice {
    text: String,
    error: bool,
    ticks_left: u32,
}

const NOTICE_TICKS: u32 = 8;
const SETTINGS_EVERY: u32 = 10;

pub struct App {
    rows: Vec<Row>,
    selected: usize,
    mode: Mode,
    notice: Option<Notice>,
    download_dir: String,
    /// Empty when the feature is off.
    watch_dir: String,
    move_dir: String,
    /// Search results added during this session (by info-hash, or URL when there is none), so
    /// they are marked even before the daemon lists them.
    added: HashSet<String>,
    /// Something was added from the current results screen: select it when going back.
    added_in_view: bool,
    color: bool,
    ticks: u32,
}

impl App {
    pub fn new(color: bool) -> App {
        App {
            rows: Vec::new(),
            selected: 0,
            mode: Mode::Normal,
            notice: None,
            download_dir: String::new(),
            watch_dir: String::new(),
            move_dir: String::new(),
            added: HashSet::new(),
            added_in_view: false,
            color,
            ticks: 0,
        }
    }

    fn say(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice { text: text.into(), error: false, ticks_left: NOTICE_TICKS });
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.notice = Some(Notice { text: text.into(), error: true, ticks_left: NOTICE_TICKS });
    }

    fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    /// Reload the torrent list (keeping the same torrent selected) and, now and then, the settings.
    pub fn refresh(&mut self, b: &mut dyn Backend) {
        let keep = self.selected_row().map(|r| r.id);
        match b.call(&Command::List) {
            Ok(lines) => {
                self.rows = lines.iter().filter_map(|l| Row::parse(l)).collect();
                self.selected = keep
                    .and_then(|id| self.rows.iter().position(|r| r.id == id))
                    .unwrap_or(self.selected)
                    .min(self.rows.len().saturating_sub(1));
            }
            Err(e) => self.fail(format!("daemon: {e}")),
        }
        if self.download_dir.is_empty() || self.ticks.is_multiple_of(SETTINGS_EVERY) {
            self.load_settings(b);
        }
    }

    fn load_settings(&mut self, b: &mut dyn Backend) {
        if let Ok(lines) = b.call(&Command::Settings) {
            let get = |key: &str| lines.iter().find_map(|l| l.strip_prefix(key)).map(str::to_string);
            if let Some(v) = get("download_dir\t") {
                self.download_dir = v;
            }
            self.watch_dir = get("watch_dir\t").unwrap_or_default();
            self.move_dir = get("move_dir\t").unwrap_or_default();
        }
    }

    /// Called when no key arrived for a while.
    pub fn tick(&mut self, b: &mut dyn Backend) {
        self.ticks = self.ticks.wrapping_add(1);
        if let Some(n) = &mut self.notice {
            n.ticks_left = n.ticks_left.saturating_sub(1);
            if n.ticks_left == 0 {
                self.notice = None;
            }
        }
        if let Mode::Searching(query) = &self.mode {
            let query = query.clone();
            if let Some(outcome) = b.poll_search() {
                self.mode = Mode::Normal;
                match outcome {
                    Ok((items, status)) if items.is_empty() => self.fail(format!("No results for \"{query}\" ({status})")),
                    Ok((items, status)) => {
                        self.added_in_view = false;
                        self.mode = Mode::Results { query, items, selected: 0, status };
                    }
                    Err(e) => self.fail(e),
                }
            }
        }
        if !matches!(self.mode, Mode::Info(_)) {
            self.refresh(b);
        }
    }

    pub fn key(&mut self, key: Key, b: &mut dyn Backend) -> Flow {
        if key == Key::CtrlC {
            return Flow::Quit;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => return self.key_normal(key, b),
            Mode::Info(lines) => {
                // any key closes the details, except that scrolling keys keep them open
                if !matches!(key, Key::Up | Key::Down) {
                    return Flow::Continue;
                }
                self.mode = Mode::Info(lines);
            }
            Mode::Confirm(id) => {
                let purge = match key {
                    Key::Char('y') | Key::Char('Y') => Some(false),
                    Key::Char('d') | Key::Char('D') => Some(true),
                    _ => None,
                };
                if let Some(purge) = purge {
                    match b.call(&Command::Remove { id, purge }) {
                        Ok(_) => self.say(if purge { format!("Removed torrent {id} and its files") } else { format!("Removed torrent {id}") }),
                        Err(e) => self.fail(e.to_string()),
                    }
                    self.refresh(b);
                }
            }
            Mode::Searching(q) => {
                if key == Key::Esc {
                    b.cancel_search();
                    self.say("Search cancelled");
                } else {
                    self.mode = Mode::Searching(q);
                }
            }
            Mode::Results { query, items, mut selected, status } => {
                let last = items.len().saturating_sub(1);
                match key {
                    Key::Up | Key::Char('k') => selected = selected.saturating_sub(1),
                    Key::Down | Key::Char('j') => selected = (selected + 1).min(last),
                    Key::Char('g') => selected = 0,
                    Key::Char('G') => selected = last,
                    Key::Esc | Key::Char('q') => {
                        // back to the downloads; if something was added, show it
                        if std::mem::take(&mut self.added_in_view) {
                            self.selected = self.rows.len().saturating_sub(1);
                        }
                        return Flow::Continue;
                    }
                    Key::Char('/') => {
                        self.added_in_view = false;
                        self.mode = Mode::Input(InputKind::Search, String::new());
                        return Flow::Continue;
                    }
                    // Enter adds this result and stays here, so several can be picked one by one
                    Key::Enter | Key::Char('a') => self.add_result(&items[selected], b),
                    _ => {}
                }
                self.mode = Mode::Results { query, items, selected, status };
            }
            Mode::Input(kind, mut buf) => match key {
                Key::Esc => {}
                Key::Backspace => {
                    buf.pop();
                    self.mode = Mode::Input(kind, buf);
                }
                Key::CtrlU => self.mode = Mode::Input(kind, String::new()),
                Key::Char(c) if !c.is_control() => {
                    buf.push(c);
                    self.mode = Mode::Input(kind, buf);
                }
                Key::Enter => {
                    let text = buf.trim().to_string();
                    if !text.is_empty() && !self.submit(kind, &text, b) {
                        self.mode = Mode::Input(kind, buf); // keep the text so it can be fixed
                    }
                }
                _ => self.mode = Mode::Input(kind, buf),
            },
        }
        Flow::Continue
    }

    /// Returns `true` when the input was accepted and the prompt can close.
    fn submit(&mut self, kind: InputKind, text: &str, b: &mut dyn Backend) -> bool {
        match kind {
            InputKind::Add => {
                let source = match classify_source(text) {
                    SourceArg::Magnet(m) => AddSource::Magnet(m),
                    SourceArg::File(_) => {
                        let path = absolute(&expand_path(text));
                        if !path.is_file() {
                            self.fail(format!("{} is not a file", path.display()));
                            return false;
                        }
                        AddSource::File(path.display().to_string())
                    }
                };
                match b.call(&Command::Add { source, dir: None, move_to: MoveSpec::Inherit }) {
                    Ok(lines) => {
                        self.say(format!("Added torrent {}", lines.first().map_or("?", String::as_str)));
                        self.refresh(b);
                        if let Some(last) = self.rows.len().checked_sub(1) {
                            self.selected = last; // show the new torrent
                        }
                        true
                    }
                    Err(e) => {
                        self.fail(e.to_string());
                        false
                    }
                }
            }
            InputKind::Folder | InputKind::Watch | InputKind::Completed => {
                let off = text.eq_ignore_ascii_case("off");
                let dir = (!off).then(|| absolute(&expand_path(text)).display().to_string());
                let (cmd, what) = match kind {
                    InputKind::Folder if off => {
                        self.fail("The download folder cannot be turned off");
                        return false;
                    }
                    InputKind::Folder => (Command::SetDownloadDir(dir.clone().unwrap_or_default()), "Download folder"),
                    InputKind::Watch => (Command::SetWatchDir(dir.clone()), "Watch folder"),
                    _ => (Command::SetMoveDir(dir.clone()), "Completed torrents folder"),
                };
                match b.call(&cmd) {
                    Ok(_) => {
                        self.say(match &dir {
                            Some(d) => format!("{what} set to {d}"),
                            None => format!("{what} turned off"),
                        });
                        self.load_settings(b);
                        true
                    }
                    Err(e) => {
                        self.fail(e.to_string());
                        false
                    }
                }
            }
            InputKind::Search => match b.start_search(text) {
                Ok(()) => {
                    self.mode = Mode::Searching(text.to_string());
                    true
                }
                Err(e) => {
                    self.fail(e.to_string());
                    false
                }
            },
            InputKind::MoveTorrent(id) => {
                let spec = match text.to_ascii_lowercase().as_str() {
                    "off" | "none" => MoveSpec::Off,
                    "default" => MoveSpec::Inherit,
                    _ => MoveSpec::Dir(absolute(&expand_path(text)).display().to_string()),
                };
                match b.call(&Command::SetMove { id, spec: spec.clone() }) {
                    Ok(_) => {
                        self.say(match spec {
                            MoveSpec::Off => format!("Torrent {id} will not be moved"),
                            MoveSpec::Inherit => format!("Torrent {id} follows the global completed folder"),
                            MoveSpec::Dir(d) => format!("Torrent {id} will be moved to {d}"),
                        });
                        self.refresh(b);
                        true
                    }
                    Err(e) => {
                        self.fail(e.to_string());
                        false
                    }
                }
            }
        }
    }

    fn result_key(r: &SearchResult) -> String {
        if r.info_hash.is_empty() { r.url.clone() } else { r.info_hash.clone() }
    }

    /// How a search result already relates to the downloads: the state of the torrent in the list,
    /// or `added` right after adding it. `None` = not in the downloads.
    fn listed_state(&self, r: &SearchResult) -> Option<String> {
        if !r.info_hash.is_empty() {
            if let Some(row) = self.rows.iter().find(|x| x.info_hash == r.info_hash) {
                return Some(row.state.clone());
            }
        }
        self.added.contains(&Self::result_key(r)).then(|| "added".to_string())
    }

    fn add_result(&mut self, r: &SearchResult, b: &mut dyn Backend) {
        if let Some(state) = self.listed_state(r) {
            self.say(format!("Already in your downloads ({state}): {}", r.name));
            return;
        }
        match b.add_result(r) {
            Ok(lines) => {
                self.added.insert(Self::result_key(r));
                self.added_in_view = true;
                self.say(format!("Added torrent {}: {}", lines.first().map_or("?", String::as_str), r.name));
                self.refresh(b);
            }
            Err(e) => self.fail(e.to_string()),
        }
    }

    fn key_normal(&mut self, key: Key, b: &mut dyn Backend) -> Flow {
        let last = self.rows.len().saturating_sub(1);
        match key {
            Key::Up | Key::Char('k') => self.selected = self.selected.saturating_sub(1),
            Key::Down | Key::Char('j') => self.selected = (self.selected + 1).min(last),
            Key::Char('g') => self.selected = 0,
            Key::Char('G') => self.selected = last,
            Key::Char('a') => self.mode = Mode::Input(InputKind::Add, String::new()),
            Key::Char('/') => self.mode = Mode::Input(InputKind::Search, String::new()),
            Key::Char('s') => self.mode = Mode::Input(InputKind::Folder, String::new()),
            Key::Char('w') => self.mode = Mode::Input(InputKind::Watch, String::new()),
            Key::Char('m') => self.mode = Mode::Input(InputKind::Completed, String::new()),
            Key::Char('M') => {
                if let Some(id) = self.selected_row().map(|r| r.id) {
                    self.mode = Mode::Input(InputKind::MoveTorrent(id), String::new());
                }
            }
            Key::Char('r') => self.refresh(b),
            Key::Char('p') | Key::Char(' ') => self.toggle_pause(b),
            Key::Char('x') | Key::Delete => {
                if let Some(r) = self.selected_row() {
                    self.mode = Mode::Confirm(r.id);
                }
            }
            Key::Char('i') | Key::Enter => {
                if let Some(id) = self.selected_row().map(|r| r.id) {
                    match b.call(&Command::Info(id)) {
                        Ok(lines) => self.mode = Mode::Info(render_info(&lines).lines().map(String::from).collect()),
                        Err(e) => self.fail(e.to_string()),
                    }
                }
            }
            Key::Char('f') => self.reveal_selected(b),
            Key::Char('q') => return Flow::Quit,
            Key::Char('Q') => return Flow::QuitAndStop,
            _ => {}
        }
        Flow::Continue
    }

    /// Show the selected torrent's data in the file manager (its folder if the data is not there yet).
    fn reveal_selected(&mut self, b: &mut dyn Backend) {
        let Some(id) = self.selected_row().map(|r| r.id) else { return };
        let lines = match b.call(&Command::Info(id)) {
            Ok(l) => l,
            Err(e) => return self.fail(e.to_string()),
        };
        let get = |key: &str| lines.iter().find_map(|l| l.strip_prefix(key)).map(str::to_string);
        let Some(dir) = get("directory\t") else { return self.fail("no folder known for this torrent") };
        let dir = std::path::PathBuf::from(dir);
        let target = get("name\t").map(|n| dir.join(n)).filter(|p| p.exists()).unwrap_or(dir);
        match b.reveal(&target) {
            Ok(()) => self.say(format!("Showing {}", target.display())),
            Err(e) => self.fail(e.to_string()),
        }
    }

    fn toggle_pause(&mut self, b: &mut dyn Backend) {
        let Some((id, state)) = self.selected_row().map(|r| (r.id, r.state.clone())) else { return };
        let (cmd, verb) = match state.as_str() {
            "paused" | "error" => (Command::Resume(crate::protocol::Target::Id(id)), "Resumed"),
            "complete" => {
                self.say("Already complete");
                return;
            }
            _ => (Command::Pause(crate::protocol::Target::Id(id)), "Paused"),
        };
        match b.call(&cmd) {
            Ok(_) => self.say(format!("{verb} torrent {id}")),
            Err(e) => self.fail(e.to_string()),
        }
        self.refresh(b);
    }

    // ------------------------------------------------------------------ drawing

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn state_color(state: &str) -> &'static str {
        match state {
            "downloading" => "32",
            "complete" => "36",
            "paused" => "33",
            "error" => "31",
            _ => "34",
        }
    }

    /// One frame: exactly `height` lines, none wider than `width` columns.
    pub fn render(&self, width: usize, height: usize) -> Vec<String> {
        let width = width.max(20);
        let height = height.max(8);
        let mut out: Vec<String> = Vec::with_capacity(height);

        // header
        let active = self.rows.iter().filter(|r| r.state == "downloading").count();
        let rate: u64 = self.rows.iter().map(|r| r.rate).sum();
        let summary = format!("{} torrents · {} downloading · ↓ {}", self.rows.len(), active, format::rate(rate));
        let title = " wisplet";
        if title.chars().count() + summary.chars().count() < width {
            let gap = width - title.chars().count() - summary.chars().count() - 1;
            out.push(format!("{}{}{}", self.paint("1", title), " ".repeat(gap), self.paint("2", &summary)));
        } else {
            out.push(self.paint("1", &format::truncate(title, width)));
        }
        let folder = format!(" Download folder: {}", if self.download_dir.is_empty() { "…" } else { &self.download_dir });
        out.push(self.paint("2", &format::truncate(&folder, width)));
        let off = |v: &str| if v.is_empty() { "off".to_string() } else { v.to_string() };
        let extras = format!(" Watch folder: {} · Move completed to: {}", off(&self.watch_dir), off(&self.move_dir));
        out.push(self.paint("2", &format::truncate(&extras, width)));

        let body_rows = height.saturating_sub(out.len() + 3); // column header + 2 footer lines
        match &self.mode {
            Mode::Info(lines) => {
                out.push(self.paint("1", " Details"));
                for l in lines.iter().take(body_rows) {
                    out.push(format::truncate(&format!("  {l}"), width));
                }
            }
            Mode::Searching(q) => {
                out.push(self.paint("1", " Searching"));
                out.push(String::new());
                let dots = ".".repeat(1 + (self.ticks % 4) as usize);
                out.push(format::truncate(&format!("  Asking the search engines for \"{q}\"{dots}"), width));
            }
            Mode::Results { query, items, selected, .. } => self.render_results(&mut out, width, body_rows, query, items, *selected),
            _ => self.render_table(&mut out, width, body_rows),
        }

        while out.len() < height - 2 {
            out.push(String::new());
        }
        out.truncate(height - 2);
        out.push(self.status_line(width));
        out.push(self.last_line(width));
        out
    }

    fn render_table(&self, out: &mut Vec<String>, width: usize, body_rows: usize) {
        // fixed columns: sel(2) id(4) state(12) bar(12) pct(6) size(10) down(12) eta(9) peers(5) + spaces
        const FIXED: usize = 2 + 4 + 12 + 12 + 6 + 10 + 12 + 9 + 5 + 8;
        let name_w = width.saturating_sub(FIXED);
        if width < FIXED + 8 {
            return self.render_narrow_table(out, width, body_rows);
        }
        let head = format!(
            "  {:<4}{:<12}{:<12}{:>6} {:>10} {:>12} {:>9} {:>5}  {}",
            "ID", "STATE", "PROGRESS", "%", "SIZE", "DOWN", "ETA", "PEERS", "NAME"
        );
        out.push(self.paint("2;4", &format::truncate(&head, width)));
        if self.rows.is_empty() {
            out.push(String::new());
            out.push(format::truncate("  No torrents yet. Press `a` to add a .torrent file or a magnet link.", width));
            return;
        }
        let body_rows = body_rows.max(1);
        let first = self.selected.saturating_sub(body_rows - 1).min(self.rows.len().saturating_sub(body_rows));
        for (i, r) in self.rows.iter().enumerate().skip(first).take(body_rows) {
            let pct = if r.total == 0 { "-".to_string() } else { format!("{:.1}", format::percent(r.done, r.total)) };
            let size = if r.total == 0 { "-".to_string() } else { format::bytes(r.total) };
            let eta = if r.state == "downloading" { format::eta(r.total.saturating_sub(r.done), r.rate) } else { "-".into() };
            let marker = if i == self.selected { "▶ " } else { "  " };
            let state = self.paint(Self::state_color(&r.state), &format!("{:<12}", format::truncate(&r.state, 11)));
            let rest = format!(
                "{:<12}{:>6} {:>10} {:>12} {:>9} {:>5}  {}",
                format::bar(r.done, r.total, 10),
                pct,
                size,
                format::rate(r.rate),
                eta,
                r.peers,
                format::truncate(&r.name, name_w.max(8)),
            );
            let line = format!("{marker}{:<4}{state}{rest}", r.id);
            out.push(if i == self.selected && self.color { format!("\x1b[7m{line}\x1b[0m") } else { line });
        }
    }

    fn render_results(&self, out: &mut Vec<String>, width: usize, body_rows: usize, query: &str, items: &[SearchResult], selected: usize) {
        out.push(self.paint("1", &format::truncate(&format!(" Results for \"{query}\" ({}) · ✓ = already in your downloads", items.len()), width)));
        out.push(self.paint("2;4", &format::truncate(&format!("  {:>6} {:>6} {:>10}  {:<13} {:<12} {}", "SEEDS", "LEECH", "SIZE", "IN DOWNLOADS", "SOURCE", "NAME"), width)));
        let body_rows = body_rows.saturating_sub(2).max(1);
        let first = selected.saturating_sub(body_rows - 1).min(items.len().saturating_sub(body_rows));
        for (i, r) in items.iter().enumerate().skip(first).take(body_rows) {
            let size = if r.size == 0 { "-".to_string() } else { format::bytes(r.size) };
            let marker = if i == selected { "▶ " } else { "  " };
            let tag = match self.listed_state(r) {
                Some(state) => format::truncate(&format!("✓ {state}"), 13),
                None => String::new(),
            };
            let line = format::truncate(
                &format!("{marker}{:>6} {:>6} {:>10}  {:<13} {:<12} {}", r.seeders, r.leechers, size, tag, format::truncate(&r.source, 12), r.name),
                width,
            );
            out.push(if i == selected && self.color {
                format!("\x1b[7m{line}\x1b[0m")
            } else if !tag.is_empty() && self.color {
                line.replacen(&tag, &self.paint("32", &tag), 1) // painted after truncating: the width is unaffected
            } else {
                line
            });
        }
    }

    /// Compact layout for narrow terminals: id, state, percentage and name.
    fn render_narrow_table(&self, out: &mut Vec<String>, width: usize, body_rows: usize) {
        out.push(self.paint("2;4", &format::truncate(&format!("  {:<4}{:<12}{:>6}  {}", "ID", "STATE", "%", "NAME"), width)));
        if self.rows.is_empty() {
            out.push(format::truncate("  No torrents yet. Press `a` to add one.", width));
            return;
        }
        let body_rows = body_rows.max(1);
        let first = self.selected.saturating_sub(body_rows - 1).min(self.rows.len().saturating_sub(body_rows));
        for (i, r) in self.rows.iter().enumerate().skip(first).take(body_rows) {
            let pct = if r.total == 0 { "-".to_string() } else { format!("{:.1}", format::percent(r.done, r.total)) };
            let marker = if i == self.selected { "▶ " } else { "  " };
            let plain = format::truncate(&format!("{marker}{:<4}{:<12}{pct:>6}  {}", r.id, format::truncate(&r.state, 11), r.name), width);
            out.push(if i == self.selected && self.color { format!("\x1b[7m{plain}\x1b[0m") } else { plain });
        }
    }

    fn status_line(&self, width: usize) -> String {
        match &self.mode {
            Mode::Input(kind, buf) => {
                let long = match kind {
                    InputKind::Add => " Add torrent (path to a .torrent file or a magnet link, Esc to cancel): ",
                    InputKind::Folder => " New download folder (Esc to cancel): ",
                    InputKind::Search => " Search torrents (Esc to cancel): ",
                    InputKind::Watch => " Watch folder for .torrent files, or `off` (Esc to cancel): ",
                    InputKind::Completed => " Move completed torrents to, or `off` (Esc to cancel): ",
                    InputKind::MoveTorrent(_) => " Move this torrent when done to a folder, `off` or `default` (Esc to cancel): ",
                };
                let label = if long.chars().count() + 10 <= width { long } else { " > " };
                let cursor = self.paint("7", " ");
                let room = width.saturating_sub(label.chars().count() + 1);
                let chars: Vec<char> = buf.chars().collect();
                let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
                format!("{}{shown}{cursor}", self.paint("1", label))
            }
            Mode::Confirm(id) => {
                let name = self.rows.iter().find(|r| r.id == *id).map_or("?", |r| r.name.as_str());
                let text = format!(" Remove torrent {id} \"{name}\"?  y = remove · d = remove and delete files · any other key = cancel");
                self.paint("1;33", &format::truncate(&text, width))
            }
            Mode::Results { status, .. } if self.notice.is_none() => self.paint("2", &format::truncate(&format!(" {status}"), width)),
            _ => match &self.notice {
                Some(n) => self.paint(if n.error { "31" } else { "32" }, &format::truncate(&format!(" {}", n.text), width)),
                None => String::new(),
            },
        }
    }

    /// Bottom line: the key help, or - while a prompt is open - the latest notice, so a failed
    /// submit is never hidden behind the prompt.
    fn last_line(&self, width: usize) -> String {
        if matches!(self.mode, Mode::Input(..)) {
            if let Some(n) = &self.notice {
                return self.paint(if n.error { "31" } else { "32" }, &format::truncate(&format!(" {}", n.text), width));
            }
        }
        self.paint("2", &format::truncate(&self.help_line(), width))
    }

    fn help_line(&self) -> String {
        match &self.mode {
            Mode::Normal => " ↑↓ select · a add · / search · p pause · x remove · i info · f show in Finder · s/w/m folders · M move · q quit · Q quit+stop".into(),
            Mode::Info(_) => " any key to go back".into(),
            Mode::Searching(_) => " Esc to cancel".into(),
            Mode::Results { .. } => " ↑↓ select · Enter add to downloads (then pick more) · / search again · Esc back to the list".into(),
            Mode::Input(..) => " Enter to confirm · Ctrl-U clears the line".into(),
            Mode::Confirm(_) => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    fn row(id: u32, state: &str, done: u64, total: u64, rate: u64, name: &str) -> Row {
        Row { id, state: state.into(), done, total, rate, peers: 3, info_hash: "ab".into(), name: name.into() }
    }

    #[derive(Default)]
    struct Fake {
        rows: Vec<Row>,
        calls: Vec<Command>,
        dir: String,
        watch: String,
        moved: String,
        fail: bool,
        /// `None` = the backend has no search (the trait default is used).
        search: Option<SearchSetup>,
        searched: Vec<String>,
        revealed: Vec<String>,
        cancelled: usize,
        added: Vec<String>,
        pending: Option<SearchOutcome>,
    }

    #[derive(Clone)]
    struct SearchSetup {
        outcome: SearchOutcome,
        add_fails: bool,
    }

    fn result(name: &str, seeders: u32) -> SearchResult {
        // a distinct, valid-looking info-hash per name
        let hex: String = name.bytes().map(|b| format!("{b:02x}")).collect();
        let hash = format!("{hex:0<40}")[..40].to_string();
        SearchResult {
            source: "torznab".into(),
            name: name.into(),
            size: 1_000_000,
            seeders,
            leechers: 1,
            magnet: format!("magnet:?xt=urn:btih:{hash}&dn={name}"),
            info_hash: hash,
            url: String::new(),
        }
    }

    impl Backend for Fake {
        fn reveal(&mut self, path: &std::path::Path) -> Result<()> {
            self.revealed.push(path.display().to_string());
            Ok(())
        }

        fn start_search(&mut self, query: &str) -> Result<()> {
            match &self.search {
                None => Err(Error::Failed("search is not available".into())),
                Some(s) => {
                    self.searched.push(query.to_string());
                    self.pending = Some(s.outcome.clone());
                    Ok(())
                }
            }
        }

        fn cancel_search(&mut self) {
            self.cancelled += 1;
            self.pending = None;
        }

        fn poll_search(&mut self) -> Option<SearchOutcome> {
            self.pending.take()
        }

        fn add_result(&mut self, r: &SearchResult) -> Result<Vec<String>> {
            if self.search.as_ref().is_some_and(|s| s.add_fails) {
                return Err(Error::Failed("cannot add".into()));
            }
            self.added.push(r.name.clone());
            let id = self.rows.len() as u32 + 1;
            self.rows.push(row(id, "metadata", 0, 0, 0, &r.name));
            Ok(vec![id.to_string()])
        }

        fn call(&mut self, cmd: &Command) -> Result<Vec<String>> {
            self.calls.push(cmd.clone());
            if self.fail {
                return Err(Error::Failed("boom".into()));
            }
            match cmd {
                Command::List => Ok(self.rows.iter().map(Row::encode).collect()),
                Command::Settings => Ok(vec![
                    format!("download_dir\t{}", self.dir),
                    format!("watch_dir\t{}", self.watch),
                    format!("move_dir\t{}", self.moved),
                ]),
                Command::SetWatchDir(d) => {
                    self.watch = d.clone().unwrap_or_default();
                    Ok(vec![])
                }
                Command::SetMoveDir(d) if d.as_deref().is_some_and(|d| d.contains("bad")) => {
                    Err(Error::Failed("cannot create".into()))
                }
                Command::SetMoveDir(d) => {
                    self.moved = d.clone().unwrap_or_default();
                    Ok(vec![])
                }
                Command::SetMove { .. } => Ok(vec![]),
                Command::SetDownloadDir(d) if d.contains("bad") => Err(Error::Failed("cannot create".into())),
                Command::SetDownloadDir(d) => {
                    self.dir = d.clone();
                    Ok(vec![])
                }
                Command::Add { .. } => {
                    let id = self.rows.len() as u32 + 1;
                    self.rows.push(row(id, "downloading", 0, 100, 0, "new one"));
                    Ok(vec![id.to_string()])
                }
                Command::Remove { id, .. } => {
                    self.rows.retain(|r| r.id != *id);
                    Ok(vec![])
                }
                Command::Pause(_) | Command::Resume(_) => Ok(vec!["1".into()]),
                Command::Info(_) => Ok(vec!["id\t1".into(), "name\tDebian ISO".into(), "directory\t/dl".into()]),
                _ => Ok(vec![]),
            }
        }
    }

    fn fake() -> Fake {
        Fake {
            dir: "/dl".into(),
            rows: vec![
                row(1, "downloading", 50, 100, 2_000_000, "Debian ISO"),
                row(2, "paused", 10, 100, 0, "Dataset"),
                row(3, "complete", 100, 100, 0, "Done thing"),
            ],
            ..Default::default()
        }
    }

    fn app(f: &mut Fake) -> App {
        let mut a = App::new(false);
        a.refresh(f);
        a
    }

    fn press(a: &mut App, f: &mut Fake, keys: &str) -> Flow {
        let mut flow = Flow::Continue;
        for k in parse_keys(keys.as_bytes()) {
            flow = a.key(k, f);
        }
        flow
    }

    fn screen(a: &App) -> String {
        a.render(100, 20).join("\n")
    }

    #[test]
    fn parses_keys() {
        assert_eq!(parse_keys(b"ab"), vec![Key::Char('a'), Key::Char('b')]);
        assert_eq!(parse_keys(b"\x1b[A\x1b[B"), vec![Key::Up, Key::Down]);
        assert_eq!(parse_keys(b"\x1bOA"), vec![Key::Up]);
        assert_eq!(parse_keys(b"\x1b[3~"), vec![Key::Delete]);
        assert_eq!(parse_keys(b"\x1b"), vec![Key::Esc]);
        assert_eq!(parse_keys(b"\r\n\x7f\x03\x15"), vec![Key::Enter, Key::Enter, Key::Backspace, Key::CtrlC, Key::CtrlU]);
        assert_eq!(parse_keys("ñ€😀".as_bytes()), vec![Key::Char('ñ'), Key::Char('€'), Key::Char('😀')]);
        // unknown sequences (arrows right/left, F-keys) are dropped, the keys around them survive
        assert_eq!(parse_keys(b"a\x1b[C\x1b[1;5Db\x1b[15~c"), vec![Key::Char('a'), Key::Char('b'), Key::Char('c')]);
        // truncated sequence / invalid utf-8 / stray control bytes never panic
        assert_eq!(parse_keys(b"\x1b["), vec![]);
        assert_eq!(parse_keys(&[0xff, 0xe2, 0x82]), vec![]);
        assert_eq!(parse_keys(&[0x01, 0x02]), vec![]);
    }

    #[test]
    fn navigation_is_clamped_and_follows_the_same_torrent() {
        let mut f = fake();
        let mut a = app(&mut f);
        press(&mut a, &mut f, "k"); // already at the top
        assert_eq!(a.selected, 0);
        press(&mut a, &mut f, "jjjj");
        assert_eq!(a.selected, 2);
        press(&mut a, &mut f, "g");
        assert_eq!(a.selected, 0);
        press(&mut a, &mut f, "G");
        assert_eq!(a.selected, 2);

        // torrent 3 stays selected when an earlier one disappears
        f.rows.remove(0);
        a.tick(&mut f);
        assert_eq!(a.selected_row().unwrap().id, 3);
        // and the selection stays valid when the list empties
        f.rows.clear();
        a.tick(&mut f);
        assert!(a.selected_row().is_none());
        press(&mut a, &mut f, "jxip"); // nothing selected: nothing happens, no panic
        assert!(matches!(a.mode, Mode::Normal));
    }

    #[test]
    fn list_refreshes_by_itself_on_every_tick() {
        let mut f = fake();
        let mut a = app(&mut f);
        assert!(screen(&a).contains("1.9 MiB/s"), "{}", screen(&a));
        f.rows[0].rate = 5_000_000;
        f.rows[0].done = 80;
        a.tick(&mut f);
        let s = screen(&a);
        assert!(s.contains("4.8 MiB/s") && s.contains("80.0"), "{s}");
    }

    #[test]
    fn p_toggles_pause_and_resume_by_state() {
        let mut f = fake();
        let mut a = app(&mut f);
        press(&mut a, &mut f, "p");
        assert!(f.calls.contains(&Command::Pause(crate::protocol::Target::Id(1))));
        press(&mut a, &mut f, "j p");
        assert!(f.calls.contains(&Command::Resume(crate::protocol::Target::Id(2))));
        let before = f.calls.len();
        press(&mut a, &mut f, "j p");
        assert!(screen(&a).contains("Already complete"));
        assert!(!f.calls[before..].iter().any(|c| matches!(c, Command::Pause(_) | Command::Resume(_))));
    }

    #[test]
    fn remove_asks_for_confirmation() {
        let mut f = fake();
        let mut a = app(&mut f);
        press(&mut a, &mut f, "x");
        assert!(screen(&a).contains("Remove torrent 1"), "{}", screen(&a));
        press(&mut a, &mut f, "n"); // cancel
        assert!(!f.calls.iter().any(|c| matches!(c, Command::Remove { .. })));
        assert_eq!(a.rows.len(), 3);

        press(&mut a, &mut f, "xy");
        assert!(f.calls.contains(&Command::Remove { id: 1, purge: false }));
        assert_eq!(a.rows.len(), 2);

        press(&mut a, &mut f, "x");
        press(&mut a, &mut f, "d");
        assert!(f.calls.contains(&Command::Remove { id: 2, purge: true }));
        assert!(screen(&a).contains("and its files"));

        // Delete key behaves like x, Esc cancels
        press(&mut a, &mut f, "\x1b[3~");
        assert!(matches!(a.mode, Mode::Confirm(_)));
        press(&mut a, &mut f, "\x1b");
        assert!(matches!(a.mode, Mode::Normal));
    }

    #[test]
    fn add_a_magnet_and_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("my file.torrent");
        std::fs::write(&file, b"x").unwrap();

        let mut f = fake();
        let mut a = app(&mut f);
        press(&mut a, &mut f, "a");
        assert!(screen(&a).contains("Add torrent"));
        press(&mut a, &mut f, "magnet:?xt=urn:btih:abc\r");
        assert!(f.calls.contains(&Command::Add { source: AddSource::Magnet("magnet:?xt=urn:btih:abc".into()), dir: None, move_to: MoveSpec::Inherit }));
        assert!(matches!(a.mode, Mode::Normal));
        assert!(screen(&a).contains("Added torrent 4"));
        assert_eq!(a.selected_row().unwrap().id, 4, "the new torrent is selected");

        // a path typed with an escaped space, as pasted by the terminal
        let typed = file.display().to_string().replace(' ', "\\ ");
        press(&mut a, &mut f, &format!("a{typed}\r"));
        assert!(f.calls.contains(&Command::Add { source: AddSource::File(file.display().to_string()), dir: None, move_to: MoveSpec::Inherit }));
    }

    #[test]
    fn add_errors_keep_the_prompt_open_for_fixing() {
        let mut f = fake();
        let mut a = app(&mut f);
        press(&mut a, &mut f, "a/definitely/missing.torrent\r");
        let s = screen(&a);
        assert!(s.contains("is not a file") && s.contains("missing.torrent"), "{s}");
        assert!(matches!(a.mode, Mode::Input(InputKind::Add, _)));
        assert!(!f.calls.iter().any(|c| matches!(c, Command::Add { .. })));
        // Ctrl-U clears, Esc leaves, empty Enter just closes
        press(&mut a, &mut f, "\x15");
        assert_eq!(a.mode, Mode::Input(InputKind::Add, String::new()));
        press(&mut a, &mut f, "\r");
        assert!(matches!(a.mode, Mode::Normal));
        press(&mut a, &mut f, "axy\x7f\x1b");
        assert!(matches!(a.mode, Mode::Normal));
    }

    #[test]
    fn change_the_download_folder() {
        let mut f = fake();
        let mut a = app(&mut f);
        assert!(screen(&a).contains("Download folder: /dl"));
        press(&mut a, &mut f, "s/new/place\r");
        assert_eq!(f.dir, "/new/place");
        let s = screen(&a);
        assert!(s.contains("Download folder: /new/place") && s.contains("set to /new/place"), "{s}");

        press(&mut a, &mut f, "s/bad\r");
        assert!(screen(&a).contains("cannot create"));
        assert_eq!(f.dir, "/new/place");
        assert!(matches!(a.mode, Mode::Input(InputKind::Folder, _)));
    }

    #[test]
    fn f_shows_the_selected_torrent_in_the_file_manager() {
        let mut f = fake();
        let mut a = app(&mut f);
        press(&mut a, &mut f, "f");
        // the fake's data does not exist on disk, so it falls back to the torrent's folder
        assert_eq!(f.revealed, vec!["/dl".to_string()]);
        assert!(matches!(a.mode, Mode::Normal));
        assert!(a.notice.as_ref().is_some_and(|n| !n.error && n.text.contains("/dl")));
        let mut empty = Fake::default();
        let mut a = app(&mut empty);
        press(&mut a, &mut empty, "f");
        assert!(empty.revealed.is_empty(), "nothing selected, nothing to show");
    }

    #[test]
    fn details_overlay_opens_and_closes() {
        let mut f = fake();
        let mut a = app(&mut f);
        press(&mut a, &mut f, "i");
        let s = screen(&a);
        assert!(s.contains("Details") && s.contains("Debian ISO") && !s.contains("PROGRESS"), "{s}");
        a.tick(&mut f); // the overlay is not redrawn over by a refresh
        assert!(matches!(a.mode, Mode::Info(_)));
        press(&mut a, &mut f, "q"); // closes the overlay instead of quitting
        assert!(matches!(a.mode, Mode::Normal));
        assert!(screen(&a).contains("PROGRESS"));
    }

    #[test]
    fn quit_keys() {
        let mut f = fake();
        let mut a = app(&mut f);
        assert_eq!(press(&mut a, &mut f, "q"), Flow::Quit);
        assert_eq!(press(&mut a, &mut f, "Q"), Flow::QuitAndStop);
        assert_eq!(press(&mut a, &mut f, "\x03"), Flow::Quit);
        // Ctrl-C quits from any mode, but q while typing is just a letter
        press(&mut a, &mut f, "a");
        assert_eq!(press(&mut a, &mut f, "q"), Flow::Continue);
        assert_eq!(press(&mut a, &mut f, "\x03"), Flow::Quit);
    }

    #[test]
    fn daemon_errors_are_shown_not_fatal() {
        let mut f = fake();
        let mut a = app(&mut f);
        f.fail = true;
        a.tick(&mut f);
        assert!(screen(&a).contains("daemon: boom"));
        f.fail = false;
        a.tick(&mut f);
        assert_eq!(a.rows.len(), 3);
    }

    #[test]
    fn notices_expire() {
        let mut f = fake();
        let mut a = app(&mut f);
        press(&mut a, &mut f, "p");
        assert!(a.notice.is_some());
        for _ in 0..NOTICE_TICKS {
            a.tick(&mut f);
        }
        assert!(a.notice.is_none());
    }

    #[test]
    fn frames_always_fit_the_terminal() {
        let mut f = fake();
        for i in 0..40 {
            f.rows.push(row(10 + i, "downloading", i as u64, 100, 1000, &format!("A very long torrent name that will have to be cut {i} ñandú ñandú")));
        }
        let mut a = app(&mut f);
        press(&mut a, &mut f, "G");
        for (w, h) in [(80, 24), (120, 40), (20, 8), (5, 3), (200, 10)] {
            let lines = a.render(w, h);
            assert_eq!(lines.len(), h.max(8), "{w}x{h}");
            for l in &lines {
                assert!(l.chars().count() <= w.max(20), "{w}x{h}: {l:?}");
            }
        }
        // the selected (last) torrent is on screen even in a short window
        assert!(a.render(100, 10).join("\n").contains("▶ 49"));
    }

    #[test]
    fn empty_list_explains_how_to_start() {
        let mut f = Fake { dir: "/dl".into(), ..Default::default() };
        let a = app(&mut f);
        assert!(screen(&a).contains("No torrents yet. Press `a`"));
    }

    #[test]
    fn colors_can_be_switched_off() {
        let mut f = fake();
        let mut plain = App::new(false);
        plain.refresh(&mut f);
        assert!(!plain.render(100, 20).join("").contains('\x1b'));
        let mut colored = App::new(true);
        colored.refresh(&mut f);
        let s = colored.render(100, 20).join("");
        assert!(s.contains("\x1b[32m") && s.contains("\x1b[7m"), "state colour and selection highlight");
    }

    #[test]
    fn watch_and_completed_folders_are_shown_and_editable() {
        let mut f = fake();
        let mut a = app(&mut f);
        assert!(screen(&a).contains("Watch folder: off · Move completed to: off"), "{}", screen(&a));

        press(&mut a, &mut f, "w");
        assert!(screen(&a).contains("Watch folder for .torrent files"));
        press(&mut a, &mut f, "/inbox\r");
        assert_eq!(f.watch, "/inbox");
        let s = screen(&a);
        assert!(s.contains("Watch folder: /inbox") && s.contains("Watch folder set to /inbox"), "{s}");

        press(&mut a, &mut f, "m/finished\r");
        assert_eq!(f.moved, "/finished");
        assert!(screen(&a).contains("Move completed to: /finished"));

        // `off` (any case) turns them off
        press(&mut a, &mut f, "wOFF\r");
        assert_eq!(f.watch, "");
        assert!(screen(&a).contains("Watch folder turned off"));
        press(&mut a, &mut f, "moff\r");
        assert_eq!(f.moved, "");
        assert!(screen(&a).contains("Watch folder: off · Move completed to: off"));
    }

    #[test]
    fn folder_errors_keep_the_prompt_open_and_the_download_folder_cannot_be_turned_off() {
        let mut f = fake();
        let mut a = app(&mut f);
        press(&mut a, &mut f, "m/bad/place\r");
        assert!(screen(&a).contains("cannot create"));
        assert!(matches!(a.mode, Mode::Input(InputKind::Completed, _)));
        press(&mut a, &mut f, "\x1b");
        press(&mut a, &mut f, "soff\r");
        assert!(screen(&a).contains("cannot be turned off"));
        assert_eq!(f.dir, "/dl");
        assert!(!f.calls.iter().any(|c| matches!(c, Command::SetDownloadDir(_))));
    }

    #[test]
    fn capital_m_sets_the_destination_of_the_selected_torrent() {
        let mut f = fake();
        let mut a = app(&mut f);
        press(&mut a, &mut f, "j"); // torrent 2
        press(&mut a, &mut f, "M");
        assert!(screen(&a).contains("Move this torrent when done"));
        press(&mut a, &mut f, "/special\r");
        assert!(f.calls.contains(&Command::SetMove { id: 2, spec: MoveSpec::Dir("/special".into()) }));
        assert!(screen(&a).contains("Torrent 2 will be moved to /special"));

        press(&mut a, &mut f, "Moff\r");
        assert!(f.calls.contains(&Command::SetMove { id: 2, spec: MoveSpec::Off }));
        assert!(screen(&a).contains("Torrent 2 will not be moved"));
        press(&mut a, &mut f, "Mdefault\r");
        assert!(f.calls.contains(&Command::SetMove { id: 2, spec: MoveSpec::Inherit }));
        assert!(screen(&a).contains("follows the global completed folder"));

        // nothing selected: the key does nothing
        f.rows.clear();
        a.tick(&mut f);
        press(&mut a, &mut f, "M");
        assert!(matches!(a.mode, Mode::Normal));
    }

    fn with_search(f: &mut Fake, items: Vec<SearchResult>) {
        f.search = Some(SearchSetup { outcome: Ok((items, "archive ok (3) · torznab error: timeout".into())), add_fails: false });
    }

    #[test]
    fn search_shows_progress_then_results_and_several_can_be_added_one_by_one() {
        let mut f = fake();
        with_search(&mut f, vec![result("Ubuntu 24.04", 500), result("Ubuntu Server", 80), result("Kubuntu", 3)]);
        let mut a = app(&mut f);

        press(&mut a, &mut f, "/");
        assert!(screen(&a).contains("Search torrents"));
        press(&mut a, &mut f, "ubuntu 24\r");
        assert_eq!(f.searched, vec!["ubuntu 24"]);
        let s = screen(&a);
        assert!(s.contains("Searching") && s.contains("\"ubuntu 24\"") && s.contains("Esc to cancel"), "{s}");

        a.tick(&mut f); // the engines answered
        let s = screen(&a);
        assert!(s.contains("Results for \"ubuntu 24\" (3)") && s.contains("Ubuntu 24.04") && s.contains("500"), "{s}");
        assert!(s.contains("IN DOWNLOADS") && s.contains("✓ = already in your downloads"), "{s}");
        assert!(s.contains("archive ok (3) · torznab error: timeout"), "per-source status is shown: {s}");
        assert!(!s.contains("✓ added"), "nothing is marked before adding");

        // Enter adds the selected result and STAYS on the results
        press(&mut a, &mut f, "j"); // Ubuntu Server
        press(&mut a, &mut f, "\r");
        assert_eq!(f.added, vec!["Ubuntu Server"]);
        assert!(matches!(a.mode, Mode::Results { .. }), "still on the results");
        let s = screen(&a);
        assert!(s.contains("Added torrent 4: Ubuntu Server"), "{s}");
        let marked: Vec<&str> = s.lines().filter(|l| l.contains("✓")).filter(|l| l.contains("Ubuntu Server")).collect();
        assert_eq!(marked.len(), 1, "only the added result is marked: {s}");
        assert!(marked[0].contains("✓ added") || marked[0].contains("✓ metadata"), "{}", marked[0]);

        // pick another one
        press(&mut a, &mut f, "j\r"); // Kubuntu
        assert_eq!(f.added, vec!["Ubuntu Server", "Kubuntu"]);
        assert!(screen(&a).contains("Added torrent 5: Kubuntu"));

        // Enter again on one that is already there: no second add, and it says why
        press(&mut a, &mut f, "\r");
        assert_eq!(f.added.len(), 2, "no duplicate request");
        assert!(screen(&a).contains("Already in your downloads") && screen(&a).contains("Kubuntu"));

        // Esc goes back to the downloads with the newest torrent selected
        press(&mut a, &mut f, "\x1b");
        assert!(matches!(a.mode, Mode::Normal));
        assert!(screen(&a).contains("PROGRESS"));
        assert_eq!(a.selected_row().unwrap().name, "Kubuntu");
    }

    #[test]
    fn results_already_in_the_downloads_are_marked_with_their_live_state() {
        let mut f = fake();
        let mut r = result("Already here", 90);
        r.info_hash = "11".repeat(20);
        f.rows[0].info_hash = "11".repeat(20); // torrent 1 "Debian ISO" is the same torrent
        with_search(&mut f, vec![r, result("Brand new", 10)]);
        let mut a = app(&mut f);
        press(&mut a, &mut f, "/x\r");
        a.tick(&mut f);
        let s = screen(&a);
        let here = s.lines().find(|l| l.contains("Already here")).unwrap();
        let new = s.lines().find(|l| l.contains("Brand new")).unwrap();
        assert!(here.contains("✓ downloading"), "{here}");
        assert!(!new.contains("✓"), "{new}");

        // pressing Enter on it does not ask the daemon to add it again
        press(&mut a, &mut f, "\r");
        assert!(f.added.is_empty());
        assert!(screen(&a).contains("Already in your downloads (downloading): Already here"));

        // the mark follows the torrent while the results are open
        f.rows[0].state = "complete".into();
        a.tick(&mut f);
        assert!(screen(&a).lines().find(|l| l.contains("Already here")).unwrap().contains("✓ complete"));
        f.rows[0].state = "paused".into();
        a.tick(&mut f);
        assert!(screen(&a).lines().find(|l| l.contains("Already here")).unwrap().contains("✓ paused"));
    }

    #[test]
    fn results_added_earlier_stay_marked_in_later_searches_even_without_a_hash() {
        let mut f = fake();
        let mut web = result("Archive item", 0);
        (web.info_hash, web.magnet, web.url) = (String::new(), String::new(), "https://archive.org/x.torrent".into());
        with_search(&mut f, vec![web.clone()]);
        let mut a = app(&mut f);
        press(&mut a, &mut f, "/x\r");
        a.tick(&mut f);
        // the fake cannot add a result without a magnet through the default path, but ours can
        press(&mut a, &mut f, "\r");
        assert_eq!(f.added, vec!["Archive item"]);
        assert!(screen(&a).contains("✓ added"), "{}", screen(&a));
        // a new search that returns the same item shows it as added
        press(&mut a, &mut f, "\x1b");
        press(&mut a, &mut f, "/x\r");
        a.tick(&mut f);
        assert!(screen(&a).contains("✓ added"));
        press(&mut a, &mut f, "\r");
        assert_eq!(f.added.len(), 1);
    }

    #[test]
    fn esc_cancels_a_running_search_and_a_late_answer_is_ignored() {
        let mut f = fake();
        with_search(&mut f, vec![result("x", 1)]);
        let mut a = app(&mut f);
        press(&mut a, &mut f, "/x\r");
        press(&mut a, &mut f, "q"); // ordinary keys do nothing while waiting
        assert!(matches!(a.mode, Mode::Searching(_)));
        press(&mut a, &mut f, "\x1b");
        assert_eq!(f.cancelled, 1);
        assert!(matches!(a.mode, Mode::Normal));
        assert!(screen(&a).contains("Search cancelled"));
        a.tick(&mut f);
        assert!(matches!(a.mode, Mode::Normal), "nothing arrives after cancelling");
    }

    #[test]
    fn empty_and_failed_searches_are_reported_and_leave_the_list_visible() {
        let mut f = fake();
        f.search = Some(SearchSetup { outcome: Ok((vec![], "archive ok (0)".into())), add_fails: false });
        let mut a = app(&mut f);
        press(&mut a, &mut f, "/nothing\r");
        a.tick(&mut f);
        let s = screen(&a);
        assert!(s.contains("No results for \"nothing\" (archive ok (0))") && s.contains("PROGRESS"), "{s}");

        f.search = Some(SearchSetup { outcome: Err("every search source failed".into()), add_fails: false });
        press(&mut a, &mut f, "/boom\r");
        a.tick(&mut f);
        assert!(screen(&a).contains("every search source failed"));
        assert!(matches!(a.mode, Mode::Normal));
    }

    #[test]
    fn a_failed_download_keeps_the_results_open() {
        let mut f = fake();
        f.search = Some(SearchSetup { outcome: Ok((vec![result("A", 5)], "ok".into())), add_fails: true });
        let mut a = app(&mut f);
        press(&mut a, &mut f, "/a\r");
        a.tick(&mut f);
        press(&mut a, &mut f, "\r");
        let s = screen(&a);
        assert!(s.contains("cannot add") && s.contains("Results for"), "{s}");
        assert!(matches!(a.mode, Mode::Results { .. }));
    }

    #[test]
    fn from_the_results_you_can_search_again_or_go_back_without_quitting() {
        let mut f = fake();
        with_search(&mut f, vec![result("A", 5)]);
        let mut a = app(&mut f);
        press(&mut a, &mut f, "/a\r");
        a.tick(&mut f);
        press(&mut a, &mut f, "/");
        assert!(matches!(a.mode, Mode::Input(InputKind::Search, _)));
        press(&mut a, &mut f, "\x1b");
        assert!(matches!(a.mode, Mode::Normal));

        press(&mut a, &mut f, "/b\r");
        a.tick(&mut f);
        assert_eq!(f.searched, vec!["a", "b"]);
        assert_eq!(press(&mut a, &mut f, "q"), Flow::Continue, "q goes back; it does not quit");
        assert!(matches!(a.mode, Mode::Normal));
        assert_eq!(press(&mut a, &mut f, "q"), Flow::Quit);
    }

    #[test]
    fn a_backend_without_search_says_so_and_keeps_the_prompt() {
        let mut f = fake(); // search: None
        let mut a = app(&mut f);
        press(&mut a, &mut f, "/x\r");
        assert!(screen(&a).contains("search is not available"));
        assert!(matches!(a.mode, Mode::Input(InputKind::Search, _)));
    }

    #[test]
    fn result_frames_fit_every_terminal_size() {
        let mut f = fake();
        let items: Vec<SearchResult> =
            (0..60).map(|i| result(&format!("A very long result name that has to be cut to fit {i} ñandú ñandú ñandú"), 100 - i)).collect();
        with_search(&mut f, items);
        let mut a = app(&mut f);
        press(&mut a, &mut f, "/x\r");
        a.tick(&mut f);
        press(&mut a, &mut f, "G");
        for (w, h) in [(80, 24), (120, 40), (20, 8), (200, 10)] {
            let lines = a.render(w, h);
            assert_eq!(lines.len(), h.max(8), "{w}x{h}");
            assert!(lines.iter().all(|l| l.chars().count() <= w.max(20)), "{w}x{h}");
        }
        assert!(a.render(100, 12).join("\n").contains("▶"), "the selected result is on screen");
    }
}
