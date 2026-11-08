//! Multi-torrent manager used by the daemon: add / pause / resume / remove, plus
//! persistence of the session so torrents survive daemon restarts.
//!
//! Session file (`session.txt`, tab-separated, percent-encoded fields):
//! `id  flag(active|paused|done)  output_dir  kind(file|magnet)  value`

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::engine::{self, Source, State, Status};
use crate::error::{Error, Result};
use crate::magnet::Magnet;
use crate::metainfo::{build_torrent_file, Torrent};
use crate::protocol::{MoveSpec, Row, Target};
use crate::util::{hex_encode, percent_decode, percent_encode};
use crate::vlog;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Stored {
    /// A `.torrent` copy inside the state directory.
    File(PathBuf),
    Magnet(String),
}

#[derive(Clone, Debug)]
struct Record {
    paused: bool,
    done: bool,
    output_dir: PathBuf,
    source: Stored,
    /// Where to move the files once complete (see [`MoveSpec`]).
    move_to: MoveSpec,
    /// Last problem worth telling the user about (not persisted), e.g. a failed move.
    note: Option<String>,
}

/// Settings persisted in `settings.txt`.
#[derive(Clone, Debug)]
struct Settings {
    download_dir: PathBuf,
    watch_dir: Option<PathBuf>,
    move_dir: Option<PathBuf>,
}

/// How often the watch folder is scanned, and how old a file must be (so a copy still in
/// progress is not read half written).
pub const WATCH_INTERVAL: Duration = Duration::from_secs(5);
pub const WATCH_SETTLE: Duration = Duration::from_secs(2);
/// Larger `.torrent` files are refused instead of being read into memory.
const MAX_TORRENT_FILE: u64 = 64 << 20;

struct Entry {
    id: u32,
    status: Rc<Status>,
    record: RefCell<Record>,
    task: RefCell<Option<JoinHandle<()>>>,
}

pub struct Manager {
    cfg: Rc<Config>,
    state_dir: PathBuf,
    settings: RefCell<Settings>,
    entries: RefCell<Vec<Rc<Entry>>>,
    next_id: Cell<u32>,
    /// Wakes the watch loop when the watch folder is set or changed.
    watch_changed: Notify,
    /// Files in the watch folder that could neither be removed nor renamed; never retried.
    ignored: RefCell<HashSet<PathBuf>>,
}

fn enc(s: &str) -> String {
    percent_encode(s.as_bytes())
}

fn dec(s: &str) -> String {
    String::from_utf8_lossy(&percent_decode(s)).into_owned()
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

impl Manager {
    /// Open (or create) the state directory and restore the saved session.
    /// Must be called inside a `LocalSet`, since it starts the restored torrents.
    pub fn open(cfg: Rc<Config>, state_dir: &Path, default_dir: &Path) -> io::Result<Rc<Manager>> {
        fs::create_dir_all(state_dir.join("torrents"))?;
        let m = Rc::new(Manager {
            cfg,
            state_dir: state_dir.to_path_buf(),
            settings: RefCell::new(Settings {
                download_dir: default_dir.to_path_buf(),
                watch_dir: None,
                move_dir: None,
            }),
            entries: RefCell::new(Vec::new()),
            next_id: Cell::new(1),
            watch_changed: Notify::new(),
            ignored: RefCell::new(HashSet::new()),
        });
        m.load_settings();
        m.restore();
        Ok(m)
    }

    fn settings_file(&self) -> PathBuf {
        self.state_dir.join("settings.txt")
    }

    /// Saved settings win over the startup default passed to [`Manager::open`].
    fn load_settings(&self) {
        let Ok(text) = fs::read_to_string(self.settings_file()) else { return };
        let mut st = self.settings.borrow_mut();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('\t') else { continue };
            if v.is_empty() {
                continue;
            }
            let path = PathBuf::from(dec(v));
            match k {
                "download_dir" => st.download_dir = path,
                "watch_dir" => st.watch_dir = Some(path),
                "move_dir" => st.move_dir = Some(path),
                _ => {}
            }
        }
    }

    fn save_settings(&self) -> Result<()> {
        let st = self.settings.borrow();
        let mut text = format!("download_dir\t{}\n", enc(&path_str(&st.download_dir)));
        if let Some(d) = &st.watch_dir {
            text.push_str(&format!("watch_dir\t{}\n", enc(&path_str(d))));
        }
        if let Some(d) = &st.move_dir {
            text.push_str(&format!("move_dir\t{}\n", enc(&path_str(d))));
        }
        let tmp = self.state_dir.join("settings.txt.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, self.settings_file())?;
        Ok(())
    }

    /// Directory used by `add` when the caller gives none.
    pub fn download_dir(&self) -> PathBuf {
        self.settings.borrow().download_dir.clone()
    }

    pub fn watch_dir(&self) -> Option<PathBuf> {
        self.settings.borrow().watch_dir.clone()
    }

    /// Global destination of completed torrents, if any.
    pub fn move_dir(&self) -> Option<PathBuf> {
        self.settings.borrow().move_dir.clone()
    }

    /// Validate an absolute folder and create it when missing.
    fn prepare_dir(dir: &Path, what: &str) -> Result<()> {
        if !dir.is_absolute() {
            return Err(Error::Failed(format!("{what} must be an absolute path: {}", dir.display())));
        }
        fs::create_dir_all(dir).map_err(|e| Error::Failed(format!("{}: {e}", dir.display())))?;
        if !dir.is_dir() {
            return Err(Error::Failed(format!("not a directory: {}", dir.display())));
        }
        Ok(())
    }

    /// Run `change` on the settings and persist them; on a save error the change is undone.
    fn update_settings(&self, change: impl FnOnce(&mut Settings)) -> Result<()> {
        let before = self.settings.borrow().clone();
        change(&mut self.settings.borrow_mut());
        if let Err(e) = self.save_settings() {
            *self.settings.borrow_mut() = before;
            return Err(e);
        }
        Ok(())
    }

    /// Change (and persist) the default download directory. It is created when missing.
    /// Torrents already added keep the directory they were added with.
    pub fn set_download_dir(&self, dir: &Path) -> Result<()> {
        Self::prepare_dir(dir, "download directory")?;
        self.update_settings(|s| s.download_dir = dir.to_path_buf())
    }

    /// Start (`Some`) or stop (`None`) watching a folder for `.torrent` files.
    pub fn set_watch_dir(&self, dir: Option<&Path>) -> Result<()> {
        if let Some(d) = dir {
            Self::prepare_dir(d, "watch directory")?;
        }
        self.update_settings(|s| s.watch_dir = dir.map(Path::to_path_buf))?;
        self.watch_changed.notify_one();
        Ok(())
    }

    /// Set (`Some`) or clear (`None`) the folder completed torrents are moved to.
    /// It applies to torrents that complete from now on and do not override it.
    pub fn set_move_dir(&self, dir: Option<&Path>) -> Result<()> {
        if let Some(d) = dir {
            Self::prepare_dir(d, "completed directory")?;
        }
        self.update_settings(|s| s.move_dir = dir.map(Path::to_path_buf))
    }

    fn session_file(&self) -> PathBuf {
        self.state_dir.join("session.txt")
    }

    fn torrent_path(&self, info_hash: &[u8; 20]) -> PathBuf {
        self.state_dir.join("torrents").join(format!("{}.torrent", hex_encode(info_hash)))
    }

    fn restore(self: &Rc<Self>) {
        let Ok(text) = fs::read_to_string(self.session_file()) else { return };
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let (id, flag, dir, kind, value, mv) = match f.as_slice() {
                [id, flag, dir, kind, value] => (id, flag, dir, kind, value, &"default"),
                [id, flag, dir, kind, value, mv] => (id, flag, dir, kind, value, mv),
                _ => continue,
            };
            let Ok(id) = id.parse::<u32>() else { continue };
            let source = match *kind {
                "file" => Stored::File(PathBuf::from(dec(value))),
                "magnet" => Stored::Magnet(dec(value)),
                _ => continue,
            };
            let record = Record {
                paused: *flag == "paused",
                done: *flag == "done",
                output_dir: PathBuf::from(dec(dir)),
                source,
                move_to: MoveSpec::parse(mv).unwrap_or_default(),
                note: None,
            };
            self.next_id.set(self.next_id.get().max(id + 1));
            match self.make_entry(id, record) {
                Ok(e) => {
                    self.entries.borrow_mut().push(e.clone());
                    self.activate(&e);
                }
                Err(err) => vlog!("skipping saved torrent {id}: {err}"),
            }
        }
    }

    /// Build the in-memory entry (parses the stored torrent / magnet for display info).
    fn make_entry(&self, id: u32, record: Record) -> Result<Rc<Entry>> {
        let (hash, name, total) = match &record.source {
            Stored::File(p) => {
                let t = Torrent::from_bytes(&fs::read(p)?)?;
                (t.info_hash, t.name.clone(), t.total_length)
            }
            Stored::Magnet(uri) => {
                let m = Magnet::parse(uri)?;
                let name = m.name.clone().unwrap_or_else(|| hex_encode(&m.info_hash));
                (m.info_hash, name, 0)
            }
        };
        let status = Status::new(hash, &name);
        status.total.set(total);
        Ok(Rc::new(Entry {
            id,
            status,
            record: RefCell::new(record),
            task: RefCell::new(None),
        }))
    }

    /// Put an entry into its proper state: complete, paused, or running.
    fn activate(self: &Rc<Self>, e: &Rc<Entry>) {
        let rec = e.record.borrow().clone();
        if rec.done {
            e.status.done.set(e.status.total.get());
            e.status.set_state(State::Complete);
            // a move interrupted by a restart (or a changed setting) is picked up again
            let (mgr, entry) = (self.clone(), e.clone());
            tokio::task::spawn_local(async move { mgr.finish(&entry).await });
        } else if rec.paused {
            e.status.set_state(State::Paused);
        } else {
            self.start(e);
        }
    }

    fn start(self: &Rc<Self>, e: &Rc<Entry>) {
        let rec = e.record.borrow().clone();
        let source = match &rec.source {
            Stored::File(p) => match fs::read(p).map_err(Error::from).and_then(|b| Torrent::from_bytes(&b)) {
                Ok(t) => Source::Torrent(t),
                Err(err) => {
                    e.status.set_state(State::Error(err.to_string()));
                    return;
                }
            },
            Stored::Magnet(uri) => match Magnet::parse(uri) {
                Ok(m) => Source::Magnet(m),
                Err(err) => {
                    e.status.set_state(State::Error(err.to_string()));
                    return;
                }
            },
        };
        e.status.set_state(State::Queued);
        let (mgr, entry, cfg) = (self.clone(), e.clone(), self.cfg.clone());
        let handle = tokio::task::spawn_local(async move {
            let hook = |t: &Torrent, info: &[u8]| mgr.save_metadata(&entry, t, info);
            let res = engine::run(source, cfg, rec.output_dir.clone(), entry.status.clone(), &hook).await;
            if res.is_ok() {
                entry.record.borrow_mut().done = true;
                mgr.persist();
                mgr.finish(&entry).await;
            } else if let Err(err) = res {
                vlog!("torrent {}: {err}", entry.id);
            }
        });
        *e.task.borrow_mut() = Some(handle);
    }

    /// Keep the metadata of a magnet so restarts skip the fetch phase.
    fn save_metadata(&self, e: &Entry, t: &Torrent, info: &[u8]) {
        let path = self.torrent_path(&t.info_hash);
        if fs::write(&path, build_torrent_file(info, &t.trackers)).is_ok() {
            e.record.borrow_mut().source = Stored::File(path);
            self.persist();
        }
    }

    fn stop_task(e: &Entry) {
        if let Some(h) = e.task.borrow_mut().take() {
            h.abort();
        }
    }

    fn persist(&self) {
        let mut text = String::new();
        for e in self.entries.borrow().iter() {
            let r = e.record.borrow();
            let flag = if r.done {
                "done"
            } else if r.paused {
                "paused"
            } else {
                "active"
            };
            let (kind, value) = match &r.source {
                Stored::File(p) => ("file", enc(&path_str(p))),
                Stored::Magnet(m) => ("magnet", enc(m)),
            };
            text.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                e.id,
                flag,
                enc(&path_str(&r.output_dir)),
                kind,
                value,
                r.move_to.encode()
            ));
        }
        let tmp = self.state_dir.join("session.txt.tmp");
        if fs::write(&tmp, text).and_then(|_| fs::rename(&tmp, self.session_file())).is_err() {
            vlog!("could not save session file");
        }
    }

    fn find(&self, id: u32) -> Result<Rc<Entry>> {
        self.entries
            .borrow()
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .ok_or_else(|| Error::Failed(format!("no torrent with id {id}")))
    }

    fn targets(&self, t: &Target) -> Result<Vec<Rc<Entry>>> {
        match t {
            Target::All => Ok(self.entries.borrow().clone()),
            Target::Id(id) => Ok(vec![self.find(*id)?]),
        }
    }

    // ---------------------------------------------------------------- public API

    /// Add a `.torrent` file (copied into the state directory). Returns the new id.
    pub fn add_file(self: &Rc<Self>, path: &Path, dir: Option<&Path>, move_to: MoveSpec) -> Result<u32> {
        let io_err = |e: io::Error| Error::Failed(format!("{}: {e}", path.display()));
        if fs::metadata(path).map_err(io_err)?.len() > MAX_TORRENT_FILE {
            return Err(Error::Failed(format!("{}: file too large for a .torrent", path.display())));
        }
        let bytes = fs::read(path).map_err(io_err)?;
        let torrent = Torrent::from_bytes(&bytes)?;
        self.ensure_new(&torrent.info_hash)?;
        let stored = self.torrent_path(&torrent.info_hash);
        fs::write(&stored, &bytes)?;
        self.add_record(Stored::File(stored), dir, move_to)
    }

    pub fn add_magnet(self: &Rc<Self>, uri: &str, dir: Option<&Path>, move_to: MoveSpec) -> Result<u32> {
        let magnet = Magnet::parse(uri)?;
        self.ensure_new(&magnet.info_hash)?;
        self.add_record(Stored::Magnet(uri.trim().to_string()), dir, move_to)
    }

    fn ensure_new(&self, hash: &[u8; 20]) -> Result<()> {
        if let Some(e) = self.entries.borrow().iter().find(|e| &e.status.info_hash == hash) {
            return Err(Error::Duplicate(e.id));
        }
        Ok(())
    }

    fn add_record(self: &Rc<Self>, source: Stored, dir: Option<&Path>, move_to: MoveSpec) -> Result<u32> {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        if let MoveSpec::Dir(d) = &move_to {
            if !Path::new(d).is_absolute() {
                return Err(Error::Failed(format!("move destination must be an absolute path: {d}")));
            }
        }
        let record = Record {
            paused: false,
            done: false,
            output_dir: dir.map(Path::to_path_buf).unwrap_or_else(|| self.download_dir()),
            source,
            move_to,
            note: None,
        };
        let entry = self.make_entry(id, record)?;
        self.entries.borrow_mut().push(entry.clone());
        self.start(&entry);
        self.persist();
        Ok(id)
    }

    /// Override (or reset) where one torrent goes when it completes. On a torrent that is
    /// already complete the move happens now.
    pub fn set_torrent_move(self: &Rc<Self>, id: u32, spec: MoveSpec) -> Result<()> {
        let e = self.find(id)?;
        if let MoveSpec::Dir(d) = &spec {
            if !Path::new(d).is_absolute() {
                return Err(Error::Failed(format!("move destination must be an absolute path: {d}")));
            }
        }
        if e.status.state() == State::Moving {
            return Err(Error::Failed(format!("torrent {id} is being moved; try again in a moment")));
        }
        {
            let mut rec = e.record.borrow_mut();
            rec.move_to = spec;
            rec.note = None;
        }
        self.persist();
        if e.record.borrow().done {
            let (mgr, entry) = (self.clone(), e);
            tokio::task::spawn_local(async move { mgr.finish(&entry).await });
        }
        Ok(())
    }

    /// Name of the file or folder a torrent creates inside its download directory.
    fn top_level_name(e: &Entry, rec: &Record) -> String {
        if let Stored::File(p) = &rec.source {
            if let Ok(t) = fs::read(p).map_err(Error::from).and_then(|b| Torrent::from_bytes(&b)) {
                if let Some(first) = t.files.first().and_then(|f| f.path.components().next()) {
                    return first.as_os_str().to_string_lossy().into_owned();
                }
            }
        }
        e.status.name.borrow().clone()
    }

    /// After a torrent completes: move its files to the configured "completed" folder.
    /// Never overwrites anything; a failure leaves the files where they are and is reported
    /// in the torrent's `note`.
    async fn finish(self: &Rc<Self>, e: &Rc<Entry>) {
        let rec = e.record.borrow().clone();
        let dest_root = match &rec.move_to {
            MoveSpec::Off => return,
            MoveSpec::Dir(d) => PathBuf::from(d),
            MoveSpec::Inherit => match self.move_dir() {
                Some(d) => d,
                None => return,
            },
        };
        if dest_root == rec.output_dir {
            return;
        }
        let top = Self::top_level_name(e, &rec);
        let (from, to) = (rec.output_dir.join(&top), dest_root.join(&top));
        e.status.set_state(State::Moving);
        let result = if !from.exists() && to.exists() {
            Ok(()) // already moved (e.g. a restart between the move and saving the session)
        } else {
            let (f, t) = (from.clone(), to.clone());
            match tokio::task::spawn_blocking(move || move_item(&f, &t)).await {
                Ok(r) => r,
                Err(join) => Err(io::Error::other(join.to_string())),
            }
        };
        match result {
            Ok(()) => {
                let mut r = e.record.borrow_mut();
                r.output_dir = dest_root;
                r.note = None;
            }
            Err(err) => {
                vlog!("torrent {}: move failed: {err}", e.id);
                e.record.borrow_mut().note = Some(format!("move failed: {err}"));
            }
        }
        e.status.set_state(State::Complete);
        self.persist();
    }

    // ---------------------------------------------------------------- watch folder

    /// Add every settled `.torrent` in `dir`, then delete it. Duplicates are deleted too (the
    /// torrent is already in the session). Invalid files are renamed to `*.rejected` so they are
    /// not retried forever. Files younger than `settle` are left for the next scan.
    pub fn scan_watch_dir(self: &Rc<Self>, dir: &Path, settle: Duration) -> ScanReport {
        let mut report = ScanReport::default();
        let Ok(read) = fs::read_dir(dir) else { return report };
        let mut paths: Vec<PathBuf> = read.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if self.ignored.borrow().contains(&path) {
                continue;
            }
            let is_torrent = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("torrent"));
            let Ok(md) = fs::symlink_metadata(&path) else { continue };
            if !is_torrent || !md.is_file() {
                continue; // other files, folders and symlinks are never touched
            }
            let age = md.modified().ok().and_then(|m| m.elapsed().ok()).unwrap_or_default();
            if age < settle {
                report.waiting += 1;
                continue;
            }
            match self.add_file(&path, None, MoveSpec::Inherit) {
                Ok(id) => {
                    report.added.push(id);
                    self.discard(&path);
                }
                Err(Error::Duplicate(_)) => {
                    report.duplicates += 1;
                    self.discard(&path);
                }
                Err(err) => {
                    vlog!("watch folder: {}: {err}", path.display());
                    report.rejected += 1;
                    let mut rejected = path.clone().into_os_string();
                    rejected.push(".rejected");
                    if fs::rename(&path, PathBuf::from(rejected)).is_err() {
                        self.ignored.borrow_mut().insert(path);
                    }
                }
            }
        }
        report
    }

    fn discard(&self, path: &Path) {
        if let Err(e) = fs::remove_file(path) {
            vlog!("watch folder: cannot delete {}: {e}", path.display());
            self.ignored.borrow_mut().insert(path.to_path_buf());
        }
    }

    /// Background task of the daemon: scans the watch folder while one is configured and sleeps
    /// until it is set otherwise (no timer runs while the feature is off).
    pub async fn watch_loop(self: Rc<Self>) {
        loop {
            match self.watch_dir() {
                Some(dir) => {
                    let report = self.scan_watch_dir(&dir, WATCH_SETTLE);
                    if !report.added.is_empty() {
                        vlog!("watch folder: added {} torrent(s)", report.added.len());
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(WATCH_INTERVAL) => {}
                        _ = self.watch_changed.notified() => {}
                    }
                }
                None => self.watch_changed.notified().await,
            }
        }
    }

    pub fn pause(&self, target: &Target) -> Result<usize> {
        let mut n = 0;
        for e in self.targets(target)? {
            if e.record.borrow().done || e.record.borrow().paused {
                continue;
            }
            Self::stop_task(&e);
            e.record.borrow_mut().paused = true;
            e.status.rate.set(0);
            e.status.peers.set(0);
            e.status.set_state(State::Paused);
            n += 1;
        }
        self.persist();
        Ok(n)
    }

    /// Resume paused torrents and retry torrents that ended in an error.
    pub fn resume(self: &Rc<Self>, target: &Target) -> Result<usize> {
        let mut n = 0;
        for e in self.targets(target)? {
            let retry = matches!(e.status.state(), State::Error(_));
            if e.record.borrow().done || !(e.record.borrow().paused || retry) {
                continue;
            }
            e.record.borrow_mut().paused = false;
            self.start(&e);
            n += 1;
        }
        self.persist();
        Ok(n)
    }

    /// Remove a torrent; with `purge` also delete its downloaded files.
    pub fn remove(&self, id: u32, purge: bool) -> Result<()> {
        let e = self.find(id)?;
        if e.status.state() == State::Moving {
            return Err(Error::Failed(format!("torrent {id} is being moved; try again in a moment")));
        }
        Self::stop_task(&e);
        let rec = e.record.borrow().clone();
        if purge {
            if let Stored::File(p) = &rec.source {
                if let Ok(t) = fs::read(p).map_err(Error::from).and_then(|b| Torrent::from_bytes(&b)) {
                    purge_files(&rec.output_dir, &t)?;
                }
            }
        }
        self.entries.borrow_mut().retain(|x| x.id != id);
        if let Stored::File(p) = &rec.source {
            let _ = fs::remove_file(p);
        }
        self.persist();
        Ok(())
    }

    pub fn rows(&self) -> Vec<Row> {
        self.entries
            .borrow()
            .iter()
            .map(|e| Row {
                id: e.id,
                state: e.status.state().label().to_string(),
                done: e.status.done.get(),
                total: e.status.total.get(),
                rate: e.status.rate.get(),
                peers: e.status.peers.get(),
                info_hash: hex_encode(&e.status.info_hash),
                name: e.status.name.borrow().clone(),
            })
            .collect()
    }

    pub fn info(&self, id: u32) -> Result<Vec<String>> {
        let e = self.find(id)?;
        let rec = e.record.borrow().clone();
        let st = e.status.state();
        let mut lines = vec![
            format!("id\t{}", e.id),
            format!("name\t{}", e.status.name.borrow()),
            format!("hash\t{}", hex_encode(&e.status.info_hash)),
            format!("state\t{}", st.label()),
            format!("done\t{}", e.status.done.get()),
            format!("total\t{}", e.status.total.get()),
            format!("rate\t{}", e.status.rate.get()),
            format!("peers\t{}", e.status.peers.get()),
            format!("directory\t{}", rec.output_dir.display()),
        ];
        lines.push(format!(
            "move\t{}",
            match &rec.move_to {
                MoveSpec::Inherit => match self.move_dir() {
                    Some(d) => format!("{} (global setting)", d.display()),
                    None => "off".to_string(),
                },
                MoveSpec::Off => "off (this torrent only)".to_string(),
                MoveSpec::Dir(d) => d.clone(),
            }
        ));
        if let Some(note) = &rec.note {
            lines.push(format!("note\t{note}"));
        }
        if let State::Error(msg) = st {
            lines.push(format!("error\t{msg}"));
        }
        if let Stored::File(p) = &rec.source {
            if let Ok(t) = fs::read(p).map_err(Error::from).and_then(|b| Torrent::from_bytes(&b)) {
                lines.push(format!("files\t{}", t.files.len()));
                lines.push(format!("pieces\t{} x {}", t.num_pieces(), t.piece_length));
                lines.push(format!("private\t{}", t.private));
                for tr in &t.trackers {
                    lines.push(format!("tracker\t{tr}"));
                }
            }
        }
        Ok(lines)
    }

    /// Stop everything and flush the session (called on shutdown).
    pub fn shutdown(&self) {
        for e in self.entries.borrow().iter() {
            Self::stop_task(e);
        }
        self.persist();
    }

    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What one scan of the watch folder did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub added: Vec<u32>,
    pub duplicates: usize,
    pub rejected: usize,
    /// Files too recent to be read yet.
    pub waiting: usize,
}

/// Move a file or folder without ever overwriting: `to` must not exist. Uses `rename`; across
/// file systems it copies to `<to>.tdpart`, renames that into place and only then deletes the
/// source, so an interruption never loses the original.
pub fn move_item(from: &Path, to: &Path) -> io::Result<()> {
    if to.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("destination exists: {}", to.display())));
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
            let mut tmp = to.as_os_str().to_os_string();
            tmp.push(".tdpart");
            let tmp = PathBuf::from(tmp);
            remove_any(&tmp)?; // leftovers of an interrupted attempt
            if from.is_dir() {
                copy_dir(from, &tmp)?;
            } else {
                fs::copy(from, &tmp)?;
            }
            fs::rename(&tmp, to)?;
            remove_any(from).map_err(|e| io::Error::other(format!("copied, but could not remove the original: {e}")))
        }
        Err(e) => Err(e),
    }
}

fn remove_any(p: &Path) -> io::Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p),
        Ok(_) => fs::remove_file(p),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Recursive copy of regular files and folders (symlinks are skipped, never followed).
fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir(&src, &dst)?;
        } else if ty.is_file() {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Delete a torrent's files under `root`, then any directories left empty.
pub fn purge_files(root: &Path, torrent: &Torrent) -> io::Result<()> {
    for f in &torrent.files {
        let path = root.join(&f.path);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut dir = path.parent();
        while let Some(d) = dir {
            if d == root || !d.starts_with(root) || fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::{self, Value};
    use sha1::{Digest, Sha1};

    fn torrent_for(files: &[(&str, &[u8])], piece_len: usize) -> (Vec<u8>, Torrent) {
        let payload: Vec<u8> = files.iter().flat_map(|(_, c)| c.iter().copied()).collect();
        let hashes: Vec<u8> = payload.chunks(piece_len).flat_map(|c| Sha1::digest(c).to_vec()).collect();
        let list: Vec<Value> = files
            .iter()
            .map(|(n, c)| {
                Value::dict([
                    ("length", (c.len() as i64).into()),
                    ("path", Value::List(n.split('/').map(Value::from).collect())),
                ])
            })
            .collect();
        let info = bencode::encode(&Value::dict([
            ("name", "pack".into()),
            ("files", list.into()),
            ("piece length", (piece_len as i64).into()),
            ("pieces", Value::Bytes(hashes)),
        ]));
        let file = build_torrent_file(&info, &[]);
        let t = Torrent::from_bytes(&file).unwrap();
        (file, t)
    }

    fn open(state: &Path, dl: &Path) -> Rc<Manager> {
        Manager::open(Rc::new(Config::default()), state, dl).unwrap()
    }

    #[test]
    fn download_dir_defaults_to_the_startup_value() {
        let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        assert_eq!(open(state.path(), dl.path()).download_dir(), dl.path());
    }

    #[test]
    fn set_download_dir_creates_persists_and_wins_over_the_startup_value() {
        let (state, dl, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let target = root.path().join("a b").join("nested");
        let m = open(state.path(), dl.path());
        m.set_download_dir(&target).unwrap();
        assert!(target.is_dir(), "created when missing");
        assert_eq!(m.download_dir(), target);
        drop(m);

        // a new manager (daemon restart) with a different startup default still uses the saved one
        assert_eq!(open(state.path(), dl.path()).download_dir(), target);
        assert!(!state.path().join("settings.txt.tmp").exists(), "temp file renamed away");
    }

    #[test]
    fn set_download_dir_rejects_relative_paths_and_files() {
        let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let m = open(state.path(), dl.path());
        let err = m.set_download_dir(Path::new("relative/dir")).unwrap_err().to_string();
        assert!(err.contains("absolute"), "{err}");
        let file = dl.path().join("plain");
        fs::write(&file, b"x").unwrap();
        assert!(m.set_download_dir(&file).is_err());
        assert_eq!(m.download_dir(), dl.path(), "a failed change leaves the setting alone");
        assert!(!state.path().join("settings.txt").exists());
    }

    #[test]
    fn corrupt_settings_file_falls_back_to_the_startup_value() {
        let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        fs::write(state.path().join("settings.txt"), "garbage\ndownload_dir\t\nother\tx\n").unwrap();
        assert_eq!(open(state.path(), dl.path()).download_dir(), dl.path());
    }

    #[test]
    fn purge_removes_files_and_empty_directories_only() {
        let dir = tempfile::tempdir().unwrap();
        let (_, t) = torrent_for(&[("a.txt", b"hello"), ("sub/deep/b.txt", b"world")], 16);
        let root = dir.path();
        fs::create_dir_all(root.join("pack/sub/deep")).unwrap();
        fs::write(root.join("pack/a.txt"), "hello").unwrap();
        fs::write(root.join("pack/sub/deep/b.txt"), "world").unwrap();
        // a foreign file the user put in the same directory must survive
        fs::write(root.join("pack/sub/mine.txt"), "keep").unwrap();

        purge_files(root, &t).unwrap();
        assert!(!root.join("pack/a.txt").exists());
        assert!(!root.join("pack/sub/deep").exists());
        assert!(root.join("pack/sub/mine.txt").exists());
        assert!(root.exists());
    }

    #[test]
    fn purge_tolerates_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let (_, t) = torrent_for(&[("a.txt", b"hello")], 16);
        purge_files(dir.path(), &t).unwrap();
    }

    #[test]
    fn purge_removes_emptied_torrent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (_, t) = torrent_for(&[("a.txt", b"hello")], 16);
        fs::create_dir_all(dir.path().join("pack")).unwrap();
        fs::write(dir.path().join("pack/a.txt"), "hello").unwrap();
        purge_files(dir.path(), &t).unwrap();
        assert!(!dir.path().join("pack").exists());
    }

    // ---------------------------------------------------------------- settings

    #[test]
    fn watch_and_move_dirs_persist_and_can_be_turned_off() {
        let (state, dl, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (watch, done) = (root.path().join("in box"), root.path().join("done"));
        let m = open(state.path(), dl.path());
        assert_eq!((m.watch_dir(), m.move_dir()), (None, None));
        m.set_watch_dir(Some(&watch)).unwrap();
        m.set_move_dir(Some(&done)).unwrap();
        assert!(watch.is_dir() && done.is_dir(), "created when missing");
        drop(m);

        let m = open(state.path(), dl.path());
        assert_eq!((m.watch_dir(), m.move_dir()), (Some(watch.clone()), Some(done.clone())));
        assert_eq!(m.download_dir(), dl.path(), "other settings are untouched");

        m.set_watch_dir(None).unwrap();
        drop(m);
        let m = open(state.path(), dl.path());
        assert_eq!((m.watch_dir(), m.move_dir()), (None, Some(done)));
        m.set_move_dir(None).unwrap();
        assert_eq!(open(state.path(), dl.path()).move_dir(), None);
    }

    #[test]
    fn watch_and_move_dirs_reject_bad_paths_and_keep_the_old_value() {
        let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let m = open(state.path(), dl.path());
        let file = dl.path().join("plain");
        fs::write(&file, b"x").unwrap();
        for bad in [Path::new("relative"), file.as_path()] {
            assert!(m.set_watch_dir(Some(bad)).is_err());
            assert!(m.set_move_dir(Some(bad)).is_err());
        }
        assert_eq!((m.watch_dir(), m.move_dir()), (None, None));
        assert!(!state.path().join("settings.txt").exists() || !fs::read_to_string(state.path().join("settings.txt")).unwrap().contains("watch_dir"));
    }

    // ---------------------------------------------------------------- moving files

    #[test]
    fn move_item_moves_files_and_folders_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        fs::write(r.join("a.bin"), "A").unwrap();
        move_item(&r.join("a.bin"), &r.join("out/deeper/a.bin")).unwrap();
        assert!(!r.join("a.bin").exists());
        assert_eq!(fs::read_to_string(r.join("out/deeper/a.bin")).unwrap(), "A");

        fs::create_dir_all(r.join("pack/sub")).unwrap();
        fs::write(r.join("pack/sub/b"), "B").unwrap();
        move_item(&r.join("pack"), &r.join("out/pack")).unwrap();
        assert_eq!(fs::read_to_string(r.join("out/pack/sub/b")).unwrap(), "B");
        assert!(!r.join("pack").exists());

        // an existing destination is an error and both sides are untouched
        fs::write(r.join("c"), "new").unwrap();
        fs::write(r.join("out/c"), "old").unwrap();
        let err = move_item(&r.join("c"), &r.join("out/c")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(r.join("c")).unwrap(), "new");
        assert_eq!(fs::read_to_string(r.join("out/c")).unwrap(), "old");

        // a missing source is an error too
        assert!(move_item(&r.join("nope"), &r.join("out/nope")).is_err());
    }

    #[test]
    fn copy_fallback_helpers_copy_files_skip_symlinks_and_remove_trees() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        fs::create_dir_all(r.join("src/sub")).unwrap();
        fs::write(r.join("src/a"), "A").unwrap();
        fs::write(r.join("src/sub/b"), "B").unwrap();
        std::os::unix::fs::symlink("/etc/hosts", r.join("src/link")).unwrap();

        copy_dir(&r.join("src"), &r.join("dst")).unwrap();
        assert_eq!(fs::read_to_string(r.join("dst/sub/b")).unwrap(), "B");
        assert!(!r.join("dst/link").exists(), "symlinks are never followed or copied");

        remove_any(&r.join("dst")).unwrap();
        assert!(!r.join("dst").exists());
        remove_any(&r.join("dst")).unwrap(); // already gone: fine
        remove_any(&r.join("src/a")).unwrap(); // a file
        assert!(!r.join("src/a").exists());
    }

    // ---------------------------------------------------------------- watch folder

    fn offline() -> Rc<Config> {
        Rc::new(Config { enable_dht: false, ..Config::default() })
    }

    async fn in_local<F: std::future::Future>(f: F) -> F::Output {
        tokio::task::LocalSet::new().run_until(f).await
    }

    #[tokio::test]
    async fn watch_scan_adds_deletes_and_leaves_other_files_alone() {
        in_local(async {
            let (state, dl, watch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let m = Manager::open(offline(), state.path(), dl.path()).unwrap();
            let (a, _) = torrent_for(&[("a.txt", b"first torrent")], 16);
            let (b, _) = torrent_for(&[("b.txt", b"second torrent, other hash")], 16);
            fs::write(watch.path().join("one.torrent"), &a).unwrap();
            fs::write(watch.path().join("TWO.TORRENT"), &b).unwrap(); // extension is case-insensitive
            fs::write(watch.path().join("notes.txt"), "mine").unwrap();
            fs::create_dir(watch.path().join("sub.torrent")).unwrap(); // a folder with that name
            std::os::unix::fs::symlink(watch.path().join("notes.txt"), watch.path().join("link.torrent")).unwrap();

            let report = m.scan_watch_dir(watch.path(), Duration::ZERO);
            assert_eq!(report, ScanReport { added: vec![1, 2], ..Default::default() });
            assert_eq!(m.len(), 2);
            assert!(!watch.path().join("one.torrent").exists() && !watch.path().join("TWO.TORRENT").exists());
            assert!(watch.path().join("notes.txt").exists() && watch.path().join("sub.torrent").is_dir());
            assert!(watch.path().join("link.torrent").symlink_metadata().is_ok(), "symlinks are never followed");
            // the daemon keeps its own copy, so deleting the dropped file loses nothing
            assert!(m.info(1).unwrap().iter().any(|l| l.starts_with("pieces\t")));
            m.shutdown();
        })
        .await;
    }

    #[tokio::test]
    async fn watch_scan_handles_duplicates_invalid_files_and_files_still_being_written() {
        in_local(async {
            let (state, dl, watch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let m = Manager::open(offline(), state.path(), dl.path()).unwrap();
            let (a, _) = torrent_for(&[("a.txt", b"first torrent")], 16);
            fs::write(watch.path().join("a.torrent"), &a).unwrap();
            assert_eq!(m.scan_watch_dir(watch.path(), Duration::ZERO).added, vec![1]);

            // the same torrent dropped again: removed, not added twice
            fs::write(watch.path().join("again.torrent"), &a).unwrap();
            // garbage: renamed so it is not retried every scan
            fs::write(watch.path().join("junk.torrent"), "this is not bencode").unwrap();
            let report = m.scan_watch_dir(watch.path(), Duration::ZERO);
            assert_eq!((report.added.len(), report.duplicates, report.rejected), (0, 1, 1));
            assert!(!watch.path().join("again.torrent").exists());
            assert!(!watch.path().join("junk.torrent").exists());
            assert_eq!(fs::read_to_string(watch.path().join("junk.torrent.rejected")).unwrap(), "this is not bencode");
            assert_eq!(m.scan_watch_dir(watch.path(), Duration::ZERO), ScanReport::default(), "nothing is retried");
            assert_eq!(m.len(), 1);

            // a file younger than the settle time waits for the next scan
            let (b, _) = torrent_for(&[("b.txt", b"second torrent, other hash")], 16);
            fs::write(watch.path().join("new.torrent"), &b).unwrap();
            let report = m.scan_watch_dir(watch.path(), Duration::from_secs(3600));
            assert_eq!((report.waiting, report.added.len()), (1, 0));
            assert!(watch.path().join("new.torrent").exists());
            assert_eq!(m.scan_watch_dir(watch.path(), Duration::ZERO).added, vec![2]);
            m.shutdown();
        })
        .await;
    }

    #[tokio::test]
    async fn watch_scan_refuses_huge_files_and_tolerates_a_missing_folder() {
        in_local(async {
            let (state, dl, watch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let m = Manager::open(offline(), state.path(), dl.path()).unwrap();
            let big = fs::File::create(watch.path().join("big.torrent")).unwrap();
            big.set_len(MAX_TORRENT_FILE + 1).unwrap(); // sparse: costs no disk
            drop(big);
            let report = m.scan_watch_dir(watch.path(), Duration::ZERO);
            assert_eq!((report.rejected, m.len()), (1, 0));
            assert!(watch.path().join("big.torrent.rejected").exists());
            assert_eq!(m.scan_watch_dir(&watch.path().join("gone"), Duration::ZERO), ScanReport::default());
            m.shutdown();
        })
        .await;
    }

    #[tokio::test]
    async fn watch_loop_picks_up_files_once_a_folder_is_set() {
        in_local(async {
            let (state, dl, watch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let m = Manager::open(offline(), state.path(), dl.path()).unwrap();
            let task = tokio::task::spawn_local(m.clone().watch_loop());
            let (a, _) = torrent_for(&[("a.txt", b"first torrent")], 16);
            fs::write(watch.path().join("a.torrent"), &a).unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(m.len(), 0, "off by default: the loop sleeps and touches nothing");

            m.set_watch_dir(Some(watch.path())).unwrap();
            // the file is brand new, so it is added on a later scan once it has settled
            let start = std::time::Instant::now();
            while m.is_empty() {
                assert!(start.elapsed() < Duration::from_secs(20), "never picked up");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            assert!(!watch.path().join("a.torrent").exists());
            task.abort();
            m.shutdown();
        })
        .await;
    }

    // ---------------------------------------------------------------- per-torrent move

    #[tokio::test]
    async fn per_torrent_move_validates_and_persists() {
        in_local(async {
            let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let m = Manager::open(offline(), state.path(), dl.path()).unwrap();
            let (a, _) = torrent_for(&[("a.txt", b"first torrent")], 16);
            let file = dl.path().join("a.torrent");
            fs::write(&file, &a).unwrap();
            let id = m.add_file(&file, None, MoveSpec::Dir("/somewhere/else".into())).unwrap();
            assert!(m.info(id).unwrap().contains(&"move\t/somewhere/else".to_string()));

            // relative destinations are refused both when adding and when changing
            let (b, _) = torrent_for(&[("b.txt", b"second torrent, other hash")], 16);
            fs::write(&file, &b).unwrap();
            assert!(m.add_file(&file, None, MoveSpec::Dir("relative".into())).is_err());
            assert!(m.set_torrent_move(id, MoveSpec::Dir("relative".into())).is_err());
            assert!(m.set_torrent_move(99, MoveSpec::Off).is_err());

            m.set_torrent_move(id, MoveSpec::Off).unwrap();
            assert!(m.info(id).unwrap().contains(&"move\toff (this torrent only)".to_string()));
            m.set_move_dir(Some(dl.path())).unwrap();
            m.set_torrent_move(id, MoveSpec::Inherit).unwrap();
            assert!(m.info(id).unwrap().iter().any(|l| l.starts_with("move\t") && l.ends_with("(global setting)")));
            m.set_torrent_move(id, MoveSpec::Dir("/kept/after/restart".into())).unwrap();
            m.shutdown();
            drop(m);

            let m = Manager::open(offline(), state.path(), dl.path()).unwrap();
            assert!(m.info(id).unwrap().contains(&"move\t/kept/after/restart".to_string()));
            m.shutdown();
        })
        .await;
    }

    #[tokio::test]
    async fn old_session_files_without_the_move_column_still_load() {
        in_local(async {
            let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let (a, t) = torrent_for(&[("a.txt", b"first torrent")], 16);
            fs::create_dir_all(state.path().join("torrents")).unwrap();
            let stored = state.path().join("torrents").join(format!("{}.torrent", hex_encode(&t.info_hash)));
            fs::write(&stored, &a).unwrap();
            let line = format!("7\tpaused\t{}\tfile\t{}\n", enc(&path_str(dl.path())), enc(&path_str(&stored)));
            fs::write(state.path().join("session.txt"), line).unwrap();
            let m = Manager::open(offline(), state.path(), dl.path()).unwrap();
            assert_eq!(m.rows().len(), 1);
            assert!(m.info(7).unwrap().contains(&"move\toff".to_string()), "no move setting means inherit (and none is set)");
            m.shutdown();
        })
        .await;
    }
}
