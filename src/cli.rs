//! Command-line front end.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::format;
use crate::protocol::Row;
use crate::search::{SearchResult, Source};

pub const HELP: &str = "\
wisplet - a tiny, download-only BitTorrent client

USAGE
  wisplet                                            live view: torrents, add, pause, settings
  wisplet add <file.torrent|magnet> [...] [-o DIR] [--move-to DIR|none]
                                                         queue downloads in the background daemon
  wisplet list                                       show all torrents (alias: ls)
  wisplet watch                                      live view of the list (Ctrl-C to quit)
  wisplet info <id>                                  details of one torrent
  wisplet pause  <id|all>                            pause torrents
  wisplet resume <id|all>                            resume paused (or failed) torrents
  wisplet remove <id> [--purge]                      forget a torrent (--purge also deletes its files; alias: rm)
  wisplet config                                     show the settings
  wisplet config set download-dir <DIR>              folder used when `add` gets no -o
  wisplet config set watch-dir <DIR|off>             auto-add (and delete) .torrent files dropped in DIR
  wisplet config set move-dir <DIR|off>              move completed torrents to DIR
  wisplet move <id> <DIR|none|default>               completed folder of one torrent (none = never move)
  wisplet autostart [on|off|status]                  start the daemon at login (macOS)
  wisplet search <text...> [--source A,B] [--limit N] [--include-dead] [--magnet] [--add N]
                                                         search torrent indexes (needs curl); --add N[,N] downloads results
  wisplet sources                                    list the search sources
  wisplet mcp                                        MCP server on stdin/stdout, so an LLM (Claude...) can use everything here
  wisplet mcp-config                                 how to register that server in Claude Code / Claude Desktop
  wisplet guide                                      the guide for LLMs and scripts (also: docs/LLM.md)
  wisplet start [options]                            start the daemon in the background
  wisplet stop                                       stop the daemon
  wisplet status                                     is the daemon running?
  wisplet daemon [options]                           run the daemon in the foreground
  wisplet get <file.torrent|magnet> [-o DIR]         download in the foreground, no daemon needed

`add` starts the daemon automatically when it is not running.

OPTIONS
  -o, --output DIR      where to save a torrent (add/get)
      --move-to DIR     where to move this torrent when complete (`none` = never move it)
      --source A,B      search only these sources (see `wisplet sources`)
      --limit N         at most N search results (default 30)
      --include-dead    also show results nobody is seeding
      --magnet          print the magnet link of every search result
      --json            machine-readable output (list, info, search, sources, config, status)
      --add N[,N...]    download search result number N (several: --add 1,3,4)
      --download-dir D  default download directory of the daemon (initial value; a saved setting wins)
      --max-peers N     peer connections per torrent (default 30)
      --pipeline N      fixed number of outstanding 16 KiB requests per peer (default: adaptive, 10..64)
      --pipeline-max N  ceiling of the adaptive request queue (default 64)
      --memory-mb N     piece buffer budget per torrent in MiB (default 16)
      --no-dht          do not use the DHT
      --peer ADDR       extra peer ip:port to try (repeatable)
  -v, --verbose         log to stderr
  -h, --help            show this help

ENVIRONMENT
  WISPLET_HOME      state directory (default ~/.local/share/wisplet)
";

#[derive(Debug, Default, PartialEq)]
pub struct Opts {
    pub output: Option<String>,
    pub download_dir: Option<String>,
    pub move_to: Option<String>,
    pub source: Option<String>,
    pub limit: Option<usize>,
    /// Search results to download (`--add 1,3`; the flag may be repeated).
    pub add: Vec<usize>,
    pub include_dead: bool,
    pub magnet: bool,
    pub json: bool,
    pub max_peers: Option<usize>,
    pub pipeline: Option<usize>,
    pub pipeline_max: Option<usize>,
    pub memory_mb: Option<usize>,
    pub no_dht: bool,
    pub peers: Vec<SocketAddr>,
    pub verbose: bool,
    pub purge: bool,
    pub help: bool,
    pub positional: Vec<String>,
}

pub fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut it = args.iter();
    fn value<'a>(it: &mut std::slice::Iter<'a, String>, flag: &str) -> Result<&'a String, String> {
        it.next().ok_or_else(|| format!("{flag} needs a value"))
    }
    fn number(s: &str, flag: &str) -> Result<usize, String> {
        match s.parse::<usize>() {
            Ok(n) if n > 0 => Ok(n),
            _ => Err(format!("{flag} needs a positive number, got {s:?}")),
        }
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" | "--output" => o.output = Some(value(&mut it, a)?.clone()),
            "--download-dir" => o.download_dir = Some(value(&mut it, a)?.clone()),
            "--move-to" => o.move_to = Some(value(&mut it, a)?.clone()),
            "--source" => o.source = Some(value(&mut it, a)?.clone()),
            "--limit" => o.limit = Some(number(value(&mut it, a)?, a)?),
            "--add" => {
                for part in value(&mut it, a)?.split(',') {
                    let n = number(part.trim(), a)?;
                    if !o.add.contains(&n) {
                        o.add.push(n);
                    }
                }
            }
            "--include-dead" => o.include_dead = true,
            "--magnet" => o.magnet = true,
            "--json" => o.json = true,
            "--max-peers" => o.max_peers = Some(number(value(&mut it, a)?, a)?),
            "--pipeline" => o.pipeline = Some(number(value(&mut it, a)?, a)?),
            "--pipeline-max" => o.pipeline_max = Some(number(value(&mut it, a)?, a)?),
            "--memory-mb" => o.memory_mb = Some(number(value(&mut it, a)?, a)?),
            "--no-dht" => o.no_dht = true,
            "--peer" => {
                let v = value(&mut it, a)?;
                o.peers.push(v.parse().map_err(|_| format!("--peer needs ip:port, got {v:?}"))?);
            }
            "-v" | "--verbose" => o.verbose = true,
            "--purge" => o.purge = true,
            "-h" | "--help" => o.help = true,
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option {s}")),
            _ => o.positional.push(a.clone()),
        }
    }
    Ok(o)
}

pub fn build_config(o: &Opts) -> Config {
    let mut c = Config::default();
    if let Some(n) = o.max_peers {
        c.max_peers = n;
    }
    if let Some(n) = o.pipeline {
        c.pipeline = n;
        c.pipeline_max = n; // an explicit --pipeline means a fixed queue...
    }
    if let Some(n) = o.pipeline_max {
        c.pipeline_max = n.max(c.pipeline); // ...unless a ceiling is given too
    }
    if let Some(n) = o.memory_mb {
        c.memory_budget = n.saturating_mul(1 << 20);
    }
    c.enable_dht = !o.no_dht;
    c.extra_peers = o.peers.clone();
    c
}

#[derive(Debug, PartialEq, Eq)]
pub enum SourceArg {
    Magnet(String),
    File(PathBuf),
}

pub fn classify_source(arg: &str) -> SourceArg {
    if arg.trim_start().get(..7).is_some_and(|p| p.eq_ignore_ascii_case("magnet:")) {
        SourceArg::Magnet(arg.trim().to_string())
    } else {
        SourceArg::File(PathBuf::from(arg))
    }
}

/// Make a path absolute relative to the current directory (no filesystem access needed).
pub fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

/// Turn text typed at a prompt into a path: trims it, strips surrounding quotes, unescapes
/// `\ ` (what terminals paste for drag-and-dropped files) and expands a leading `~`.
pub fn expand_path(input: &str) -> PathBuf {
    let t = input.trim();
    let t = t
        .strip_prefix('"').and_then(|x| x.strip_suffix('"'))
        .or_else(|| t.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
        .unwrap_or(t);
    let t = t.replace("\\ ", " ");
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (t.strip_prefix("~/"), t.as_str(), home) {
        (Some(rest), _, Some(h)) => h.join(rest),
        (_, "~", Some(h)) => h,
        _ => PathBuf::from(t),
    }
}

pub fn render_list(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "No torrents. Add one with: wisplet add <file.torrent|magnet>\n".to_string();
    }
    let mut out = format!(
        "{:<4} {:<12} {:<12} {:>6} {:>10} {:>12} {:>9} {:>5}  {}\n",
        "ID", "STATE", "PROGRESS", "%", "SIZE", "DOWN", "ETA", "PEERS", "NAME"
    );
    for r in rows {
        let remaining = r.total.saturating_sub(r.done);
        let pct = if r.total == 0 { "-".to_string() } else { format!("{:.1}", format::percent(r.done, r.total)) };
        let eta = if r.state == "downloading" { format::eta(remaining, r.rate) } else { "-".into() };
        out.push_str(&format!(
            "{:<4} {:<12} {:<12} {:>6} {:>10} {:>12} {:>9} {:>5}  {}\n",
            r.id,
            r.state,
            format::bar(r.done, r.total, 10),
            pct,
            if r.total == 0 { "-".to_string() } else { format::bytes(r.total) },
            format::rate(r.rate),
            eta,
            r.peers,
            format::truncate(&r.name, 60),
        ));
    }
    out
}

/// Numbered table of search results; with `magnets` each result is followed by its magnet link.
pub fn render_search(results: &[SearchResult], magnets: bool) -> String {
    if results.is_empty() {
        return "No results.\n".to_string();
    }
    let mut out = format!("{:>3} {:>6} {:>6} {:>10}  {:<14} {}\n", "#", "SEEDS", "LEECH", "SIZE", "SOURCE", "NAME");
    for (i, r) in results.iter().enumerate() {
        let size = if r.size == 0 { "-".to_string() } else { format::bytes(r.size) };
        out.push_str(&format!(
            "{:>3} {:>6} {:>6} {:>10}  {:<14} {}\n",
            i + 1,
            r.seeders,
            r.leechers,
            size,
            format::truncate(&r.source, 14),
            format::truncate(&r.name, 90)
        ));
        if magnets {
            let link = if r.magnet.is_empty() { crate::api::redact_url(&r.url) } else { r.magnet.clone() };
            out.push_str(&format!("      {link}\n"));
        }
    }
    out
}

/// One line per source: `archive: ok (12)`.
pub fn render_search_status(status: &[(Source, String)]) -> String {
    status.iter().map(|(s, st)| format!("  {:<8} {st}\n", s.key())).collect()
}

/// `key<TAB>value` settings lines from the daemon, as a small table.
pub fn render_settings(lines: &[String]) -> String {
    let mut s = String::from("Settings\n");
    for l in lines {
        if let Some((k, v)) = l.split_once('\t') {
            let label = match k {
                "download_dir" => "Download folder",
                "watch_dir" => "Watch folder",
                "move_dir" => "Move completed to",
                other => other,
            };
            let v = if v.is_empty() { "off" } else { v };
            s.push_str(&format!("  {label:<18} {v}\n"));
        }
    }
    s
}

pub fn render_info(lines: &[String]) -> String {
    let mut out = String::new();
    for l in lines {
        let (k, v) = l.split_once('\t').unwrap_or((l, ""));
        let v = match k {
            "done" | "total" => v.parse::<u64>().map(|n| format!("{} ({n} B)", format::bytes(n))).unwrap_or(v.into()),
            "rate" => v.parse::<u64>().map(format::rate).unwrap_or(v.into()),
            _ => v.to_string(),
        };
        out.push_str(&format!("{k:<10} {v}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_flags_and_positionals() {
        let o = parse_opts(&args("a.torrent -o /tmp/x --max-peers 5 --pipeline 3 --memory-mb 8 --no-dht --peer 1.2.3.4:5 -v b")).unwrap();
        assert_eq!(o.positional, vec!["a.torrent", "b"]);
        assert!(parse_opts(&args("list --json")).unwrap().json);
        let m = parse_opts(&args("x --add 3 --add 1,3,2")).unwrap();
        assert_eq!(m.add, vec![3, 1, 2], "repeated numbers are ignored, order is kept");
        assert!(parse_opts(&args("x --add 1,")).is_err() && parse_opts(&args("x --add a")).is_err() && parse_opts(&args("x --add 0")).is_err());
        let m = parse_opts(&args("x --move-to /done")).unwrap();
        assert_eq!(m.move_to.as_deref(), Some("/done"));
        assert!(parse_opts(&args("x --move-to")).is_err());
        assert_eq!(o.output.as_deref(), Some("/tmp/x"));
        assert_eq!((o.max_peers, o.pipeline, o.memory_mb), (Some(5), Some(3), Some(8)));
        assert!(o.no_dht && o.verbose && !o.purge && !o.help);
        assert_eq!(o.peers, vec!["1.2.3.4:5".parse().unwrap()]);
    }

    #[test]
    fn rejects_bad_flags() {
        for bad in ["--bogus", "-o", "--max-peers", "--max-peers 0", "--max-peers x", "--peer nothing", "--memory-mb -3"] {
            assert!(parse_opts(&args(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn single_dash_is_positional() {
        assert_eq!(parse_opts(&args("-")).unwrap().positional, vec!["-"]);
    }

    #[test]
    fn config_reflects_options() {
        let o = parse_opts(&args("--max-peers 7 --memory-mb 4 --no-dht --peer 9.9.9.9:1")).unwrap();
        let c = build_config(&o);
        assert_eq!(c.max_peers, 7);
        assert_eq!(c.memory_budget, 4 << 20);
        assert!(!c.enable_dht);
        assert_eq!(c.extra_peers.len(), 1);
        let d = build_config(&Opts::default());
        assert_eq!(d.max_peers, Config::default().max_peers);
        assert!(d.enable_dht);
    }

    #[test]
    fn classifies_sources() {
        assert_eq!(classify_source("magnet:?xt=urn:btih:abc"), SourceArg::Magnet("magnet:?xt=urn:btih:abc".into()));
        assert_eq!(classify_source("  MAGNET:?xt=x "), SourceArg::Magnet("MAGNET:?xt=x".into()));
        assert_eq!(classify_source("movie.torrent"), SourceArg::File("movie.torrent".into()));
        assert_eq!(classify_source("ñ"), SourceArg::File("ñ".into()));
    }

    #[test]
    fn absolute_paths() {
        assert_eq!(absolute(Path::new("/a/b")), PathBuf::from("/a/b"));
        assert!(absolute(Path::new("rel")).is_absolute());
    }

    fn row(id: u32, state: &str, done: u64, total: u64, rate: u64) -> Row {
        Row { id, state: state.into(), done, total, rate, peers: 4, info_hash: "00".repeat(20), name: format!("name-{id}") }
    }

    #[test]
    fn renders_list() {
        assert!(render_list(&[]).contains("No torrents"));
        let out = render_list(&[
            row(1, "downloading", 512 << 20, 1024 << 20, 1 << 20),
            row(2, "metadata", 0, 0, 0),
            row(3, "complete", 100, 100, 0),
        ]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("ID"));
        assert!(lines[1].contains("downloading") && lines[1].contains("50.0") && lines[1].contains("1.0 GiB"));
        assert!(lines[1].contains("1.0 MiB/s") && lines[1].contains("8m32s"));
        assert!(lines[2].contains("metadata") && lines[2].contains(" - "));
        assert!(lines[3].contains("100.0") && lines[3].contains("[##########]"));
        assert!(lines[3].ends_with("name-3"));
    }

    #[test]
    fn renders_info() {
        let out = render_info(&["name\tx".into(), "done\t2048".into(), "rate\t0".into(), "error\tboom".into()]);
        assert!(out.contains("name       x"));
        assert!(out.contains("2.0 KiB (2048 B)"));
        assert!(out.contains("error      boom"));
    }

    #[test]
    fn search_table_never_prints_an_api_key() {
        let r = SearchResult {
            source: "torznab".into(),
            name: "Thing".into(),
            size: 5,
            seeders: 1,
            leechers: 0,
            info_hash: String::new(),
            magnet: String::new(),
            url: "http://127.0.0.1:9117/dl/x?apikey=SECRET&file=f".into(),
        };
        let out = render_search(&[r], true);
        assert!(out.contains("http://127.0.0.1:9117/dl/x") && !out.contains("SECRET"), "{out}");
    }

    #[test]
    fn expand_path_handles_quotes_escapes_and_home() {
        std::env::set_var("HOME", "/home/u");
        assert_eq!(expand_path("  /a/b  "), PathBuf::from("/a/b"));
        assert_eq!(expand_path("\"/a b\""), PathBuf::from("/a b"));
        assert_eq!(expand_path("'/a b'"), PathBuf::from("/a b"));
        assert_eq!(expand_path("/a\\ b/c"), PathBuf::from("/a b/c"));
        assert_eq!(expand_path("~/Downloads"), PathBuf::from("/home/u/Downloads"));
        assert_eq!(expand_path("~"), PathBuf::from("/home/u"));
        assert_eq!(expand_path("~other/x"), PathBuf::from("~other/x"));
    }

    #[test]
    fn pipeline_flags_choose_between_fixed_and_adaptive() {
        let c = build_config(&parse_opts(&args("")).unwrap());
        assert!(c.pipeline_max > c.pipeline, "adaptive by default");
        let c = build_config(&parse_opts(&args("--pipeline 7")).unwrap());
        assert_eq!((c.pipeline, c.pipeline_max), (7, 7), "an explicit --pipeline is a fixed queue");
        let c = build_config(&parse_opts(&args("--pipeline 7 --pipeline-max 40")).unwrap());
        assert_eq!((c.pipeline, c.pipeline_max), (7, 40));
        let c = build_config(&parse_opts(&args("--pipeline 30 --pipeline-max 5")).unwrap());
        assert_eq!((c.pipeline, c.pipeline_max), (30, 30), "the ceiling is never below the minimum");
        assert!(parse_opts(&args("--pipeline-max 0")).is_err());
    }

    #[test]
    fn settings_table_renders_labels() {
        let t = render_settings(&[
            "download_dir\t/x".into(),
            "watch_dir\t".into(),
            "move_dir\t/done".into(),
            "weird\t1".into(),
            "junk".into(),
        ]);
        assert!(t.contains("Download folder") && t.contains("/x") && t.contains("weird"));
        assert!(t.contains("Watch folder       off") && t.contains("Move completed to  /done"), "{t}");
        assert!(!t.contains("junk"));
    }

    #[test]
    fn help_mentions_every_command() {
        for c in ["add", "list", "watch", "info", "pause", "resume", "remove", "start", "stop", "status", "daemon", "get", "config", "autostart", "move", "search", "sources", "mcp", "mcp-config", "guide"] {
            assert!(HELP.contains(&format!("wisplet {c}")), "{c}");
        }
    }
}
