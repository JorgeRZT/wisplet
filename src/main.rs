use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use wisplet::cli::{self, classify_source, SourceArg, HELP};
use wisplet::engine::{self, Source, State, Status};
use wisplet::magnet::Magnet;
use wisplet::metainfo::Torrent;
use wisplet::protocol::{AddSource, Command, MoveSpec, Row, Target};
use wisplet::{format, util, Error, Result};
use tokio::runtime::Builder;
use tokio::task::LocalSet;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(msg)) => {
            eprintln!("error: {msg}\n\nRun `wisplet --help` for usage.");
            ExitCode::from(2)
        }
        Err(Failure::Runtime(e)) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
        Err(Failure::Interrupted) => ExitCode::from(130),
    }
}

enum Failure {
    Usage(String),
    Runtime(Error),
    Interrupted,
}

impl From<Error> for Failure {
    fn from(e: Error) -> Self {
        Failure::Runtime(e)
    }
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Failure::Runtime(e.into())
    }
}

fn usage<T>(m: impl Into<String>) -> std::result::Result<T, Failure> {
    Err(Failure::Usage(m.into()))
}

fn runtime() -> tokio::runtime::Runtime {
    Builder::new_current_thread().enable_all().build().expect("tokio runtime")
}

fn run(args: Vec<String>) -> std::result::Result<(), Failure> {
    let Some(cmd) = args.first().map(String::as_str) else {
        #[cfg(unix)]
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            return interactive();
        }
        print!("{HELP}");
        return Ok(());
    };
    if matches!(cmd, "help" | "-h" | "--help") {
        print!("{HELP}");
        return Ok(());
    }
    let opts = cli::parse_opts(&args[1..]).or_else(usage)?;
    if opts.help {
        print!("{HELP}");
        return Ok(());
    }
    util::set_verbose(opts.verbose);

    match cmd {
        "get" => get(&opts),
        #[cfg(unix)]
        _ => unix_commands(cmd, &opts, &args[1..]),
        #[cfg(not(unix))]
        _ => usage("daemon commands need a Unix system; use `wisplet get` instead"),
    }
}

// ---------------------------------------------------------------- foreground download

fn get(opts: &cli::Opts) -> std::result::Result<(), Failure> {
    let [src] = opts.positional.as_slice() else {
        return usage("get needs exactly one torrent file or magnet link");
    };
    let source = match classify_source(src) {
        SourceArg::Magnet(uri) => Source::Magnet(Magnet::parse(&uri)?),
        SourceArg::File(p) => {
            let bytes = std::fs::read(&p).map_err(|e| Error::Failed(format!("{}: {e}", p.display())))?;
            Source::Torrent(Torrent::from_bytes(&bytes)?)
        }
    };
    let out = cli::absolute(&PathBuf::from(opts.output.clone().unwrap_or_else(|| ".".into())));
    let cfg = Rc::new(cli::build_config(opts));
    let hash = match &source {
        Source::Torrent(t) => t.info_hash,
        Source::Magnet(m) => m.info_hash,
    };
    let status = Status::new(hash, "");

    let rt = runtime();
    let local = LocalSet::new();
    let tty = std::io::stderr().is_terminal();
    let result = rt.block_on(local.run_until(async {
        let printer = tokio::task::spawn_local({
            let status = status.clone();
            async move {
                let mut tick = 0u64;
                loop {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    tick += 1;
                    if tty || tick.is_multiple_of(10) {
                        print_progress(&status, tty);
                    }
                }
            }
        });
        let download = engine::run(source, cfg, out, status.clone(), &|_, _| {});
        let res = tokio::select! {
            r = download => Some(r),
            _ = tokio::signal::ctrl_c() => None,
        };
        printer.abort();
        res
    }));
    if tty {
        eprint!("\r\x1b[K");
    }
    match result {
        Some(Ok(())) => {
            println!("Done: {} ({})", status.name.borrow(), format::bytes(status.total.get()));
            Ok(())
        }
        Some(Err(e)) => Err(e.into()),
        None => {
            eprintln!("Interrupted. Run the same command again to resume.");
            Err(Failure::Interrupted)
        }
    }
}

fn print_progress(s: &Status, tty: bool) {
    let (done, total) = (s.done.get(), s.total.get());
    let line = match s.state() {
        State::FetchingMetadata => "fetching metadata...".to_string(),
        State::Checking => format!("checking existing data... {}", format::bytes(done)),
        _ => format!(
            "{} {:5.1}%  {} / {}  {}  {} peers  ETA {}",
            format::bar(done, total, 20),
            format::percent(done, total),
            format::bytes(done),
            format::bytes(total),
            format::rate(s.rate.get()),
            s.peers.get(),
            format::eta(total.saturating_sub(done), s.rate.get()),
        ),
    };
    if tty {
        eprint!("\r\x1b[K{line}");
    } else {
        eprintln!("{line}");
    }
}

// ---------------------------------------------------------------- daemon commands (unix)

#[cfg(unix)]
mod unix_impl {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use wisplet::daemon;

    pub fn socket() -> PathBuf {
        daemon::socket_path(&daemon::state_dir())
    }

    pub fn call(cmd: &Command) -> Result<Vec<String>> {
        runtime().block_on(daemon::request(&socket(), cmd)).map_err(|e| match e {
            Error::Io(ref io) if matches!(io.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
                Error::Failed("the daemon is not running (start it with `wisplet start`)".into())
            }
            other => other,
        })
    }

    pub fn running() -> bool {
        runtime().block_on(daemon::is_running(&socket()))
    }

    pub fn start_background(raw_args: &[String]) -> Result<()> {
        if running() {
            return Ok(());
        }
        let state = daemon::state_dir();
        daemon::check_socket_path(&socket())?; // fail fast instead of waiting for a daemon that can't bind
        std::fs::create_dir_all(&state)?;
        let log = std::fs::OpenOptions::new().create(true).append(true).open(state.join("daemon.log"))?;
        let exe = std::env::current_exe()?;
        std::process::Command::new(exe)
            .arg("daemon")
            .args(raw_args)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0) // detach from the terminal's job control
            .spawn()?;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(50));
            if running() {
                return Ok(());
            }
        }
        Err(Error::Failed(format!("daemon did not start; see {}", state.join("daemon.log").display())))
    }

    pub fn run_daemon(opts: &cli::Opts) -> Result<()> {
        let cfg = cli::build_config(opts);
        let state = daemon::state_dir();
        let dl = opts
            .download_dir
            .as_ref()
            .map(|d| cli::absolute(&PathBuf::from(d)))
            .unwrap_or_else(daemon::default_download_dir);
        let rt = runtime();
        let local = LocalSet::new();
        rt.block_on(local.run_until(daemon::run(cfg, &state, &dl, true, Rc::new(tokio::sync::Notify::new()))))?;
        Ok(())
    }
}

#[cfg(unix)]
fn unix_commands(cmd: &str, opts: &cli::Opts, raw: &[String]) -> std::result::Result<(), Failure> {
    use unix_impl::*;

    let id_arg = |what: &str| -> std::result::Result<u32, Failure> {
        match opts.positional.as_slice() {
            [id] => id.parse().or_else(|_| usage(format!("invalid id {id:?}"))),
            _ => usage(format!("{what} needs exactly one torrent id")),
        }
    };
    let target_arg = || -> std::result::Result<Target, Failure> {
        match opts.positional.as_slice() {
            [t] if t == "all" => Ok(Target::All),
            [id] => id.parse().map(Target::Id).or_else(|_| usage(format!("invalid id {id:?}"))),
            _ => usage("expected a torrent id or `all`"),
        }
    };

    match cmd {
        "daemon" => run_daemon(opts)?,
        "start" => {
            if running() {
                println!("Daemon already running.");
            } else {
                start_background(raw)?;
                println!("Daemon started.");
            }
        }
        "stop" => {
            if !running() {
                println!("Daemon is not running.");
            } else {
                call(&Command::Shutdown)?;
                println!("Daemon stopped.");
            }
        }
        "status" if opts.json => {
            use wisplet::json::Json;
            let up = running();
            let torrents = if up { rows().map(|r| r.len()).ok() } else { None };
            println!(
                "{}",
                Json::object([
                    ("running", Json::Bool(up)),
                    ("torrents", torrents.map_or(Json::Null, |n| Json::num(n as f64))),
                    ("state_dir", Json::str(wisplet::daemon::state_dir().display().to_string())),
                ])
                .to_json()
            );
            if !up {
                return Err(Failure::Runtime(Error::Failed("daemon not running".into())));
            }
        }
        "status" => {
            if running() {
                let rows = call(&Command::List)?.len();
                println!("Daemon running ({rows} torrents). State: {}", wisplet::daemon::state_dir().display());
            } else {
                println!("Daemon not running.");
                return Err(Failure::Runtime(Error::Failed("daemon not running".into())));
            }
        }
        "add" => {
            if opts.positional.is_empty() {
                return usage("add needs at least one torrent file or magnet link");
            }
            start_background(&[])?;
            let dir = opts.output.as_ref().map(|o| cli::absolute(&PathBuf::from(o)).display().to_string());
            let move_to = parse_move(opts.move_to.as_deref());
            let mut failed = false;
            for src in &opts.positional {
                let source = match classify_source(src) {
                    SourceArg::Magnet(m) => AddSource::Magnet(m),
                    SourceArg::File(p) => AddSource::File(cli::absolute(&p).display().to_string()),
                };
                match call(&Command::Add { source, dir: dir.clone(), move_to: move_to.clone() }) {
                    Ok(lines) => println!("Added torrent {} ({})", lines.first().map(String::as_str).unwrap_or("?"), src),
                    Err(e) => {
                        eprintln!("error: {src}: {e}");
                        failed = true;
                    }
                }
            }
            if failed {
                return Err(Failure::Runtime(Error::Failed("some torrents could not be added".into())));
            }
        }
        "config" => config_command(opts)?,
        "search" => search_command(opts)?,
        "mcp" => mcp_command()?,
        "mcp-config" => mcp_config()?,
        "guide" => print!("{}", include_str!("../docs/LLM.md")),
        "sources" if opts.json => {
            use wisplet::json::Json;
            let all = wisplet::search::Source::all()
                .iter()
                .map(|s| Json::object([("key", Json::str(s.key())), ("description", Json::str(s.describe()))]))
                .collect();
            println!("{}", Json::Arr(all).to_json());
        }
        "sources" => {
            for s in wisplet::search::Source::all() {
                println!("  {:<8} {}", s.key(), s.describe());
            }
        }
        "move" => match opts.positional.as_slice() {
            [id, dest] => {
                let id: u32 = id.parse().or_else(|_| usage(format!("invalid id {id:?}")))?;
                let spec = parse_move(Some(dest));
                call(&Command::SetMove { id, spec: spec.clone() })?;
                match spec {
                    MoveSpec::Off => println!("Torrent {id} will not be moved."),
                    MoveSpec::Inherit => println!("Torrent {id} follows the global completed folder."),
                    MoveSpec::Dir(d) => println!("Torrent {id} will be moved to {d} when complete."),
                }
            }
            _ => return usage("usage: wisplet move <id> <DIR|none|default>"),
        },
        "autostart" => autostart_command(opts)?,
        "list" | "ls" if opts.json => println!("{}", wisplet::api::rows_json(&rows()?).to_json()),
        "list" | "ls" => print!("{}", cli::render_list(&rows()?)),
        "watch" => loop {
            print!("\x1b[2J\x1b[H{}", cli::render_list(&rows()?));
            println!("\n(refreshing every second - Ctrl-C to quit)");
            std::thread::sleep(Duration::from_secs(1));
        },
        "info" if opts.json => println!("{}", wisplet::api::info_json(&call(&Command::Info(id_arg("info")?))?).to_json()),
        "info" => print!("{}", cli::render_info(&call(&Command::Info(id_arg("info")?))?)),
        "pause" => println!("Paused {} torrent(s).", call(&Command::Pause(target_arg()?))?.first().map_or("0", String::as_str)),
        "resume" => println!("Resumed {} torrent(s).", call(&Command::Resume(target_arg()?))?.first().map_or("0", String::as_str)),
        "remove" | "rm" => {
            call(&Command::Remove { id: id_arg("remove")?, purge: opts.purge })?;
            println!("Removed.");
        }
        other => return usage(format!("unknown command {other:?}")),
    }
    Ok(())
}

#[cfg(unix)]
fn rows() -> Result<Vec<Row>> {
    Ok(unix_impl::call(&Command::List)?.iter().filter_map(|l| Row::parse(l)).collect())
}

#[cfg(unix)]
fn config_command(opts: &cli::Opts) -> std::result::Result<(), Failure> {
    use unix_impl::{call, start_background};
    let words: Vec<&str> = opts.positional.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] | ["show"] => {
            start_background(&[])?;
            let lines = call(&Command::Settings)?;
            if opts.json {
                println!("{}", wisplet::api::settings_json(&lines).to_json());
            } else {
                print!("{}", cli::render_settings(&lines));
            }
        }
        ["set", "download-dir", dir] => {
            start_background(&[])?;
            let dir = cli::absolute(&cli::expand_path(dir));
            call(&Command::SetDownloadDir(dir.display().to_string()))?;
            println!("Download folder set to {}.", dir.display());
        }
        ["set", "watch-dir", dir] | ["set", "move-dir", dir] => {
            start_background(&[])?;
            let (is_watch, off) = (words[1] == "watch-dir", dir.eq_ignore_ascii_case("off"));
            let path = (!off).then(|| cli::absolute(&cli::expand_path(dir)).display().to_string());
            let cmd = if is_watch { Command::SetWatchDir(path.clone()) } else { Command::SetMoveDir(path.clone()) };
            call(&cmd)?;
            let what = if is_watch { "Watch folder" } else { "Completed torrents folder" };
            match path {
                Some(p) => println!("{what} set to {p}."),
                None => println!("{what} turned off."),
            }
        }
        ["set", other, ..] => {
            return usage(format!("unknown setting {other:?} (available: download-dir, watch-dir, move-dir)"))
        }
        _ => return usage("usage: wisplet config [show] | config set <download-dir|watch-dir|move-dir> <DIR|off>"),
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Default)]
struct DaemonBackend {
    search: Option<std::sync::mpsc::Receiver<wisplet::tui::SearchOutcome>>,
}

#[cfg(unix)]
impl wisplet::tui::Backend for DaemonBackend {
    fn call(&mut self, cmd: &Command) -> Result<Vec<String>> {
        unix_impl::call(cmd)
    }

    fn start_search(&mut self, query: &str) -> Result<()> {
        let query = wisplet::search::check_query(query)?;
        let (tx, rx) = std::sync::mpsc::channel();
        self.search = Some(rx);
        // The search runs on its own thread (and runtime) so the screen keeps refreshing.
        std::thread::spawn(move || {
            let outcome = runtime()
                .block_on(wisplet::search::search(&query, &wisplet::search::Options::default()))
                .map(|r| {
                    let status = r.status.iter().map(|(s, st)| format!("{} {st}", s.key())).collect::<Vec<_>>().join(" · ");
                    (r.results, status)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(outcome); // the receiver is gone if the user cancelled
        });
        Ok(())
    }

    fn cancel_search(&mut self) {
        self.search = None;
    }

    fn poll_search(&mut self) -> Option<wisplet::tui::SearchOutcome> {
        let outcome = self.search.as_ref()?.try_recv().ok()?;
        self.search = None;
        Some(outcome)
    }

    fn add_result(&mut self, r: &wisplet::search::SearchResult) -> Result<Vec<String>> {
        add_search_result(r)
    }

    fn reveal(&mut self, path: &std::path::Path) -> Result<()> {
        // macOS: select the item in Finder. Elsewhere: open the containing folder.
        let mut cmd;
        if cfg!(target_os = "macos") {
            cmd = std::process::Command::new("open");
            cmd.arg("-R").arg(path);
        } else {
            cmd = std::process::Command::new("xdg-open");
            cmd.arg(if path.is_dir() { path } else { path.parent().unwrap_or(path) });
        }
        let status = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| wisplet::error::Error::Failed(format!("cannot open the file manager: {e}")))?;
        if status.success() {
            Ok(())
        } else {
            Err(wisplet::error::Error::Failed(format!("the file manager could not show {}", path.display())))
        }
    }
}

#[cfg(unix)]
fn interactive() -> std::result::Result<(), Failure> {
    unix_impl::start_background(&[])?;
    let flow = wisplet::term::run(&mut DaemonBackend::default())?;
    if flow == wisplet::tui::Flow::QuitAndStop {
        unix_impl::call(&Command::Shutdown)?;
        println!("Daemon stopped.");
    }
    Ok(())
}

#[cfg(unix)]
fn autostart_command(opts: &cli::Opts) -> std::result::Result<(), Failure> {
    use wisplet::autostart::{self, Status};
    if !cfg!(target_os = "macos") {
        return Err(Failure::Runtime(Error::Failed("autostart is only supported on macOS".into())));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Failure::Runtime(Error::Failed("HOME is not set".into())))?;
    match opts.positional.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["on"] => {
            let exe = std::env::current_exe()?.canonicalize()?;
            let state_home = std::env::var_os("WISPLET_HOME").map(PathBuf::from);
            let path = autostart::enable(&home, &exe, &wisplet::daemon::state_dir(), state_home.as_deref())?;
            println!("Autostart enabled: the daemon will start when you log in.");
            println!("  agent:  {}", path.display());
            println!("  binary: {}", exe.display());
        }
        ["off"] => {
            if autostart::disable(&home)? {
                println!("Autostart disabled. A daemon that is running now keeps running; stop it with `wisplet stop`.");
            } else {
                println!("Autostart was not enabled.");
            }
        }
        [] | ["status"] => match autostart::status(&home) {
            Status::Disabled => {
                // exit code 1, like `status` for the daemon, so scripts (install.sh) can branch on it
                println!("Autostart is off. Enable it with `wisplet autostart on`.");
                return Err(Failure::Runtime(Error::Failed("autostart is off".into())));
            }
            Status::Enabled(exe) => {
                println!("Autostart is on (binary: {}).", exe.display());
                if !exe.exists() {
                    println!("warning: that binary no longer exists; run `wisplet autostart on` again.");
                }
            }
        },
        _ => return usage("usage: wisplet autostart [on|off|status]"),
    }
    Ok(())
}

/// `--move-to` / `move` argument: `none`/`off` never move, `default` follows the global setting.
#[cfg(unix)]
fn parse_move(arg: Option<&str>) -> MoveSpec {
    match arg {
        None => MoveSpec::Inherit,
        Some(a) if a.eq_ignore_ascii_case("none") || a.eq_ignore_ascii_case("off") => MoveSpec::Off,
        Some(a) if a.eq_ignore_ascii_case("default") => MoveSpec::Inherit,
        Some(a) => MoveSpec::Dir(cli::absolute(&cli::expand_path(a)).display().to_string()),
    }
}

/// `wisplet search <text> ...`
#[cfg(unix)]
fn search_command(opts: &cli::Opts) -> std::result::Result<(), Failure> {
    use wisplet::search::{self, Source};
    if opts.positional.is_empty() {
        return usage("search needs some text, e.g. wisplet search ubuntu 24.04");
    }
    let mut sources = Vec::new();
    for key in opts.source.iter().flat_map(|s| s.split(',')).filter(|k| !k.trim().is_empty()) {
        match Source::from_key(key) {
            Some(s) => sources.push(s),
            None => return usage(format!("unknown source {key:?} (see `wisplet sources`)")),
        }
    }
    let query = opts.positional.join(" ");
    let options = search::Options {
        sources,
        include_dead: opts.include_dead,
        limit: opts.limit.unwrap_or(30),
        ..search::Options::default()
    };
    let report = runtime().block_on(search::search(&query, &options))?;

    if opts.json {
        use wisplet::json::Json;
        // mark what is already in the downloads, when the daemon is up
        let rows = if unix_impl::running() { rows().unwrap_or_default() } else { Vec::new() };
        let items = report
            .results
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let state = rows.iter().find(|x| !r.info_hash.is_empty() && x.info_hash == r.info_hash).map(|x| x.state.as_str());
                wisplet::api::result_json(i + 1, r, state)
            })
            .collect();
        println!(
            "{}",
            Json::object([
                ("query", Json::str(query.clone())),
                ("count", Json::num(report.results.len() as f64)),
                ("results", Json::Arr(items)),
                ("sources", wisplet::api::sources_status_json(&report)),
            ])
            .to_json()
        );
    } else {
        print!("{}", cli::render_search(&report.results, opts.magnet));
    }
    let failed = report.status.iter().filter(|(_, s)| s.starts_with("error")).count();
    if failed > 0 || opts.verbose {
        eprintln!("\nSources:\n{}", cli::render_search_status(&report.status));
    }
    if failed == report.status.len() {
        return Err(Failure::Runtime(Error::Failed("every search source failed (is curl installed and the network up?)".into())));
    }

    if !opts.add.is_empty() {
        if let Some(n) = opts.add.iter().find(|n| **n > report.results.len()) {
            return usage(format!("--add {n}: there are only {} results", report.results.len()));
        }
        if !opts.json {
            println!();
        }
        let mut failed = false;
        for n in &opts.add {
            let result = &report.results[n - 1];
            match add_search_result(result) {
                Ok(reply) if opts.json => eprintln!("Added torrent {} ({})", reply.first().map_or("?", String::as_str), result.name),
                Ok(reply) => println!("Added torrent {} ({})", reply.first().map_or("?", String::as_str), result.name),
                Err(e) => {
                    eprintln!("error: result {n} ({}): {e}", result.name);
                    failed = true;
                }
            }
        }
        if failed {
            return Err(Failure::Runtime(Error::Failed("some results could not be added".into())));
        }
    }
    Ok(())
}

/// Add a search result to the daemon: by magnet when it has one, else by downloading its `.torrent`.
#[cfg(unix)]
fn add_search_result(r: &wisplet::search::SearchResult) -> Result<Vec<String>> {
    unix_impl::start_background(&[])?;
    let source = if !r.magnet.is_empty() {
        AddSource::Magnet(r.magnet.clone())
    } else if !r.url.is_empty() {
        AddSource::File(fetch_torrent_file(&r.url, &r.name)?)
    } else {
        return Err(Error::Failed("this result has neither a magnet link nor a .torrent URL".into()));
    };
    unix_impl::call(&Command::Add { source, dir: None, move_to: MoveSpec::Inherit })
}

/// Download a `.torrent` over HTTPS (through curl) into the state directory; returns its path.
#[cfg(unix)]
fn fetch_torrent_file(url: &str, name: &str) -> Result<String> {
    let bytes = runtime().block_on(wisplet::search::fetch_torrent(url, &wisplet::curl::bin()))?;
    let dir = wisplet::daemon::state_dir().join("search");
    std::fs::create_dir_all(&dir)?;
    let safe: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).take(60).collect();
    let path = dir.join(format!("{}.torrent", if safe.is_empty() { "download" } else { &safe }));
    std::fs::write(&path, bytes)?;
    Ok(path.display().to_string())
}

/// The MCP server's view of the machine.
#[cfg(unix)]
struct McpHost;

#[cfg(unix)]
impl wisplet::mcp::Host for McpHost {
    fn daemon(&mut self, cmd: &Command) -> Result<Vec<String>> {
        unix_impl::start_background(&[])?; // no-op when it is already running
        unix_impl::call(cmd)
    }

    fn daemon_running(&mut self) -> bool {
        unix_impl::running()
    }

    fn start_daemon(&mut self) -> Result<()> {
        unix_impl::start_background(&[])
    }

    fn stop_daemon(&mut self) -> Result<()> {
        unix_impl::call(&Command::Shutdown).map(|_| ())
    }

    fn search(&mut self, query: &str, o: &wisplet::search::Options) -> Result<wisplet::search::Report> {
        runtime().block_on(wisplet::search::search(query, o))
    }

    fn fetch_torrent(&mut self, url: &str) -> Result<String> {
        let name = url.rsplit('/').next().unwrap_or("download").split(['?', '#']).next().unwrap_or("download");
        fetch_torrent_file(url, name.trim_end_matches(".torrent"))
    }
}

/// `wisplet mcp`: serve the Model Context Protocol on stdin/stdout. Stdout carries protocol
/// messages only; diagnostics (with `-v`) go to stderr.
#[cfg(unix)]
fn mcp_command() -> std::result::Result<(), Failure> {
    let mut server = wisplet::mcp::Server::new(McpHost);
    let stdin = std::io::stdin();
    wisplet::mcp::serve(&mut server, stdin.lock(), std::io::stdout())?;
    Ok(())
}

/// `wisplet mcp-config`: the snippets that register this binary as an MCP server.
#[cfg(unix)]
fn mcp_config() -> std::result::Result<(), Failure> {
    use wisplet::json::Json;
    let exe = std::env::current_exe()?.canonicalize()?.display().to_string();
    let q = |s: &str| Json::str(s).to_json();
    println!("Claude Code:\n  claude mcp add wisplet -- {} mcp\n", exe);
    println!("Claude Desktop (claude_desktop_config.json):");
    println!(
        "{}",
        Json::object([("mcpServers", Json::object([("wisplet", Json::object([("command", Json::str(exe.clone())), ("args", Json::Arr(vec![Json::str("mcp")]))]))]))])
            .to_json()
    );
    println!("\nAny other MCP client: command {}, argument {}, transport stdio.", q(&exe), q("mcp"));
    println!("Guide for the model: `wisplet guide`.");
    Ok(())
}
