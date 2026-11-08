//! Black-box tests of the MCP server and of `--json`: the real binary in separate processes, a
//! real daemon, fake seeders and a fake `curl` (no network).
#![cfg(unix)]

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::seeder::*;
use wisplet::json::Json;
use wisplet::util::hex_encode;

const BIN: &str = env!("CARGO_BIN_EXE_wisplet");

struct Env {
    home: tempfile::TempDir,
    work: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        Env { home: tempfile::tempdir().unwrap(), work: tempfile::tempdir().unwrap() }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("WISPLET_HOME", self.home.path())
            .env("WISPLET_TORZNAB_URL", "http://127.0.0.1:9117/api")
            .env_remove("XDG_DATA_HOME");
        c
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.cli(args);
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> Json {
        let text = self.ok(args);
        Json::parse(text.trim()).unwrap_or_else(|e| panic!("{args:?} printed non-JSON {text:?}: {e}"))
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new(BIN).arg("stop").env("WISPLET_HOME", self.home.path()).output();
    }
}

/// A running `wisplet mcp`, spoken to over its pipes.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    out: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn start(env: &Env, extra_env: &[(&str, &Path)]) -> Mcp {
        let mut child = env
            .cmd()
            .arg("mcp")
            .envs(extra_env.iter().map(|(k, v)| (*k, *v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let out = BufReader::new(child.stdout.take().unwrap());
        Mcp { child, stdin, out, next_id: 1 }
    }

    fn send_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    fn read_line(&mut self) -> String {
        let mut line = String::new();
        let n = self.out.read_line(&mut line).unwrap();
        assert!(n > 0, "the server closed stdout unexpectedly");
        line
    }

    fn request(&mut self, method: &str, params: Json) -> Json {
        let id = self.next_id;
        self.next_id += 1;
        let msg = Json::object([("jsonrpc", Json::str("2.0")), ("id", Json::uint(id)), ("method", Json::str(method)), ("params", params)]);
        self.send_line(&msg.to_json());
        let line = self.read_line();
        let reply = Json::parse(line.trim()).unwrap_or_else(|e| panic!("stdout carried a non-JSON line {line:?}: {e}"));
        assert_eq!(reply.get("id").and_then(Json::as_u64), Some(id));
        reply
    }

    fn initialize(&mut self) -> Json {
        let r = self.request(
            "initialize",
            Json::object([
                ("protocolVersion", Json::str("2025-03-26")),
                ("capabilities", Json::Obj(vec![])),
                ("clientInfo", Json::object([("name", Json::str("test")), ("version", Json::str("1"))])),
            ]),
        );
        self.send_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        r
    }

    /// Call a tool: `(is_error, parsed JSON or the error text)`.
    fn tool(&mut self, name: &str, args: &str) -> (bool, Json) {
        let params = Json::object([("name", Json::str(name)), ("arguments", Json::parse(args).unwrap())]);
        let reply = self.request("tools/call", params);
        let res = reply.get("result").unwrap_or_else(|| panic!("protocol error: {}", reply.to_json())).clone();
        let text = res.get("content").unwrap().as_array().unwrap()[0].get("text").and_then(Json::as_str).unwrap().to_string();
        let is_error = res.get("isError").and_then(Json::as_bool).unwrap();
        (is_error, if is_error { Json::Str(text) } else { Json::parse(&text).unwrap() })
    }

    fn ok(&mut self, name: &str, args: &str) -> Json {
        let (err, v) = self.tool(name, args);
        assert!(!err, "{name} {args} failed: {}", v.to_json());
        v
    }

    fn fail(&mut self, name: &str, args: &str) -> String {
        let (err, v) = self.tool(name, args);
        assert!(err, "{name} {args} should fail, got {}", v.to_json());
        v.as_str().unwrap().to_string()
    }

    /// Close stdin and check the server leaves cleanly: exit 0, no stray stdout, empty stderr.
    fn finish(mut self) {
        drop(self.stdin.take());
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut self.out, &mut rest).unwrap();
        assert!(rest.is_empty(), "stray output after the last reply: {rest:?}");
        let status = self.child.wait().unwrap();
        let mut err = String::new();
        if let Some(mut e) = self.child.stderr.take() {
            std::io::Read::read_to_string(&mut e, &mut err).unwrap();
        }
        assert!(status.success(), "{status:?} {err}");
        assert!(err.is_empty(), "stderr should be silent without -v: {err}");
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A `curl` that answers the Torznab server with one result (the info-hash given) and the Internet
/// Archive with nothing.
fn fake_curl(dir: &Path, hash: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("curl");
    let script = format!(
        r##"#!/bin/sh
for last; do :; done
case "$last" in
  *127.0.0.1:9117*) printf '%s' '<rss xmlns:torznab="x"><channel><item><title>Sample ISO</title><size>100000</size><torznab:attr name="seeders" value="40"/><torznab:attr name="peers" value="42"/><torznab:attr name="infohash" value="{hash}"/></item></channel></rss>' ;;
  *) printf '%s' '{{"response":{{"docs":[]}}}}' ;;
esac
"##
    );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Fx {
    torrent: PathBuf,
    data: Vec<u8>,
    spec: TorrentSpec,
    seeder: Seeder,
}

async fn fx(env: &Env, len: usize, behavior: Behavior) -> Fx {
    let data = pseudo_bytes(len, 3);
    let src = Arc::new(MemSource(data.clone()));
    let spec = build_single("sample.bin", &*src, 32 * 1024);
    let seeder = Seeder::start(&spec, src, behavior).await;
    let torrent = env.work.path().join("sample.torrent");
    fs::write(&torrent, &spec.torrent_file).unwrap();
    Fx { torrent, data, spec, seeder }
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(20), "timeout waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

// ------------------------------------------------------------------ MCP

#[test]
fn handshake_catalogue_and_clean_shutdown() {
    let env = Env::new();
    let mut mcp = Mcp::start(&env, &[]);
    let init = mcp.initialize();
    let res = init.get("result").unwrap();
    assert_eq!(res.get("protocolVersion").and_then(Json::as_str), Some("2025-03-26"));
    assert_eq!(res.get("serverInfo").unwrap().get("name").and_then(Json::as_str), Some("wisplet"));

    let list = mcp.request("tools/list", Json::Obj(vec![]));
    let tools = list.get("result").unwrap().get("tools").unwrap().as_array().unwrap();
    assert_eq!(tools.len(), 13, "one tool per feature of the live view");
    assert!(tools.iter().all(|t| t.get("inputSchema").is_some() && t.get("description").is_some()));

    assert!(mcp.request("ping", Json::Obj(vec![])).get("result").is_some());
    let unknown = mcp.request("no/such/method", Json::Obj(vec![]));
    assert_eq!(unknown.get("error").unwrap().get("code").and_then(Json::as_u64), None, "negative codes are not unsigned");
    assert!(unknown.get("error").unwrap().get("message").and_then(Json::as_str).unwrap().contains("method not found"));
    mcp.send_line("{ this is not json");
    let bad = Json::parse(mcp.read_line().trim()).unwrap();
    assert!(bad.get("error").is_some());
    mcp.finish();
}

#[test]
fn the_daemon_starts_on_demand_and_can_be_controlled() {
    let env = Env::new();
    let mut mcp = Mcp::start(&env, &[]);
    mcp.initialize();
    assert_eq!(mcp.ok("daemon_control", r#"{"action":"status"}"#).get("running"), Some(&Json::Bool(false)));
    assert!(!env.cli(&["status"]).status.success(), "nothing was started by a status check");

    // any tool that needs the daemon starts it
    assert_eq!(mcp.ok("list_downloads", "{}").as_array().unwrap().len(), 0);
    assert!(env.cli(&["status"]).status.success(), "the daemon is up now");
    let st = mcp.ok("daemon_control", r#"{"action":"status"}"#);
    assert_eq!((st.get("running"), st.get("torrents")), (Some(&Json::Bool(true)), Some(&Json::Num(0.0))));

    mcp.ok("daemon_control", r#"{"action":"stop"}"#);
    assert_eq!(mcp.ok("daemon_control", r#"{"action":"status"}"#).get("running"), Some(&Json::Bool(false)));
    mcp.ok("daemon_control", r#"{"action":"start"}"#);
    assert!(env.cli(&["status"]).status.success());
    mcp.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_llm_can_add_watch_control_and_remove_a_download() {
    let env = Env::new();
    let f = fx(&env, 3_000_000, Behavior { block_delay_ms: 20, ..Default::default() }).await;
    let peer = f.seeder.addr.to_string();
    let dl = env.work.path().join("downloads");
    let done_dir = env.work.path().join("finished");

    tokio::task::block_in_place(|| {
        env.ok(&["start", "--no-dht", "--peer", &peer, "--pipeline", "2", "--download-dir", s(&dl)]);
        let mut mcp = Mcp::start(&env, &[]);
        mcp.initialize();

        // add by file path, with a per-torrent destination
        let args = format!(r#"{{"source":{},"move_to":{}}}"#, Json::str(s(&f.torrent)).to_json(), Json::str(s(&done_dir)).to_json());
        let added = mcp.ok("add_download", &args);
        let id = added.get("id").and_then(Json::as_u64).unwrap();
        assert_eq!(id, 1);
        assert!(mcp.fail("add_download", &args).contains("already added"), "a duplicate is a tool error");

        // progress is visible and well formed
        wait_until("it to start downloading", || {
            let l = mcp.ok("list_downloads", "{}");
            l.as_array().unwrap()[0].get("done_bytes").and_then(Json::as_u64).unwrap_or(0) > 0
        });
        let row = mcp.ok("list_downloads", "{}").as_array().unwrap()[0].clone();
        assert_eq!(row.get("name").and_then(Json::as_str), Some("sample.bin"));
        assert_eq!(row.get("total_bytes").and_then(Json::as_u64), Some(f.data.len() as u64));
        assert_eq!(row.get("info_hash").and_then(Json::as_str), Some(hex_encode(&f.spec.info_hash).as_str()));

        // pause / resume
        assert_eq!(mcp.ok("pause_downloads", r#"{"id":1}"#).get("affected").and_then(Json::as_u64), Some(1));
        assert_eq!(mcp.ok("list_downloads", "{}").as_array().unwrap()[0].get("state").and_then(Json::as_str), Some("paused"));
        mcp.ok("resume_downloads", r#"{"id":"all"}"#);

        // it finishes and is moved to the destination chosen at add time
        wait_until("the download to finish and move", || done_dir.join("sample.bin").exists());
        wait_until("state complete", || mcp.ok("list_downloads", "{}").as_array().unwrap()[0].get("state").and_then(Json::as_str) == Some("complete"));
        assert_eq!(fs::read(done_dir.join("sample.bin")).unwrap(), f.data);
        let info = mcp.ok("get_download", r#"{"id":1}"#);
        assert_eq!(info.get("directory").and_then(Json::as_str), Some(s(&done_dir)), "{}", info.to_json());
        assert!(info.get("pieces").is_some());

        // removing without delete_files keeps the data; with it the data goes
        let removed = mcp.ok("remove_download", r#"{"id":1}"#);
        assert_eq!(removed.get("files_deleted"), Some(&Json::Bool(false)));
        assert!(done_dir.join("sample.bin").exists());
        assert!(mcp.fail("get_download", r#"{"id":1}"#).contains("no torrent"));

        let args = format!(r#"{{"source":{}}}"#, Json::str(s(&f.torrent)).to_json());
        mcp.ok("add_download", &args);
        wait_until("the second copy to complete", || dl.join("sample.bin").exists() && mcp.ok("list_downloads", "{}").as_array().unwrap()[0].get("state").and_then(Json::as_str) == Some("complete"));
        mcp.ok("remove_download", r#"{"id":2,"delete_files":true}"#);
        assert!(!dl.join("sample.bin").exists(), "delete_files really deleted them");
        assert!(done_dir.join("sample.bin").exists(), "and only those");
        mcp.finish();
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_and_per_torrent_destination_through_mcp() {
    let env = Env::new();
    let f = fx(&env, 100_000, Behavior::default()).await;
    let peer = f.seeder.addr.to_string();
    let (dl, watch, moved, special) = (
        env.work.path().join("dl"),
        env.work.path().join("inbox"),
        env.work.path().join("done"),
        env.work.path().join("special"),
    );
    tokio::task::block_in_place(|| {
        env.ok(&["start", "--no-dht", "--peer", &peer, "--download-dir", s(&dl)]);
        let mut mcp = Mcp::start(&env, &[]);
        mcp.initialize();

        let st = mcp.ok("get_settings", "{}");
        assert_eq!((st.get("watch_dir"), st.get("move_dir")), (Some(&Json::Null), Some(&Json::Null)));
        mcp.ok("update_setting", &format!(r#"{{"name":"watch_dir","value":{}}}"#, Json::str(s(&watch)).to_json()));
        let st = mcp.ok("update_setting", &format!(r#"{{"name":"move_dir","value":{}}}"#, Json::str(s(&moved)).to_json()));
        assert_eq!(st.get("move_dir").and_then(Json::as_str), Some(s(&moved)));
        assert_eq!(st.get("watch_dir").and_then(Json::as_str), Some(s(&watch)));
        assert!(watch.is_dir() && moved.is_dir());
        assert!(mcp.fail("update_setting", r#"{"name":"download_dir","value":"off"}"#).contains("cannot be turned off"));
        assert!(mcp.fail("update_setting", r#"{"name":"move_dir","value":"relative/dir"}"#).contains("absolute path"), "relative paths are refused");
        // the CLI sees exactly what the model changed
        assert!(env.ok(&["config"]).contains(s(&moved)));

        // a torrent dropped in the watch folder is added by the daemon (a few seconds)
        fs::copy(&f.torrent, watch.join("sample.torrent")).unwrap();
        wait_until("the watch folder to pick it up", || !mcp.ok("list_downloads", "{}").as_array().unwrap().is_empty());
        // while it is not finished, give it its own destination; it must win over the global one
        let r = mcp.tool("set_move_destination", &format!(r#"{{"id":1,"destination":{}}}"#, Json::str(s(&special)).to_json()));
        if !r.0 {
            wait_until("it to land in its own folder", || special.join("sample.bin").exists());
            assert!(!moved.join("sample.bin").exists());
        } else {
            // it had already been moved to the global folder; asking again for 'default' is accepted
            assert!(r.1.as_str().unwrap().contains("move"), "{}", r.1.to_json());
        }
        mcp.ok("update_setting", r#"{"name":"watch_dir","value":"off"}"#);
        mcp.ok("update_setting", r#"{"name":"move_dir","value":"off"}"#);
        let st = mcp.ok("get_settings", "{}");
        assert_eq!((st.get("watch_dir"), st.get("move_dir")), (Some(&Json::Null), Some(&Json::Null)));
        mcp.finish();
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_then_download_the_chosen_result() {
    let env = Env::new();
    let f = fx(&env, 100_000, Behavior { serve_metadata: true, ..Default::default() }).await;
    let peer = f.seeder.addr.to_string();
    let dl = env.work.path().join("dl");
    let curl = fake_curl(env.work.path(), &hex_encode(&f.spec.info_hash));
    tokio::task::block_in_place(|| {
        env.ok(&["start", "--no-dht", "--peer", &peer, "--download-dir", s(&dl)]);
        let mut mcp = Mcp::start(&env, &[("WISPLET_CURL", curl.as_path())]);
        mcp.initialize();

        assert!(mcp.fail("download_search_results", r#"{"results":[1]}"#).contains("no previous search"));
        let r = mcp.ok("search_torrents", r#"{"query":"sample iso","limit":5,"sources":["torznab"]}"#);
        assert_eq!(r.get("count").and_then(Json::as_u64), Some(1));
        let first = &r.get("results").unwrap().as_array().unwrap()[0];
        assert_eq!(first.get("name").and_then(Json::as_str), Some("Sample ISO"));
        assert_eq!(first.get("seeders").and_then(Json::as_u64), Some(40));
        assert_eq!(first.get("in_downloads"), Some(&Json::Bool(false)));
        assert!(first.get("magnet").and_then(Json::as_str).unwrap().starts_with("magnet:?xt=urn:btih:"));
        assert_eq!(r.get("sources").unwrap().as_array().unwrap().len(), 1, "one status line per source asked");

        let out = mcp.ok("download_search_results", r#"{"results":[1]}"#);
        assert_eq!(out.get("outcomes").unwrap().as_array().unwrap()[0].get("added"), Some(&Json::Bool(true)));
        wait_until("the magnet to download", || dl.join("sample.bin").exists());
        wait_until("complete", || mcp.ok("list_downloads", "{}").as_array().unwrap()[0].get("state").and_then(Json::as_str) == Some("complete"));
        assert_eq!(fs::read(dl.join("sample.bin")).unwrap(), f.data);

        // searching again now shows it as already downloaded, and adding it again is skipped
        let r = mcp.ok("search_torrents", r#"{"query":"sample iso","sources":["torznab"]}"#);
        let first = &r.get("results").unwrap().as_array().unwrap()[0];
        assert_eq!(first.get("in_downloads"), Some(&Json::Bool(true)));
        assert_eq!(first.get("download_state").and_then(Json::as_str), Some("complete"));
        let out = mcp.ok("download_search_results", r#"{"results":[1]}"#);
        let o = &out.get("outcomes").unwrap().as_array().unwrap()[0];
        assert_eq!(o.get("added"), Some(&Json::Bool(false)));
        assert!(o.get("reason").and_then(Json::as_str).unwrap().contains("already in the downloads"));
        assert!(mcp.ok("list_search_sources", "{}").as_array().unwrap().len() >= 2);
        mcp.finish();
    });
}

// ------------------------------------------------------------------ --json and the guide

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_output_of_the_command_line() {
    let env = Env::new();
    let f = fx(&env, 100_000, Behavior::default()).await;
    let peer = f.seeder.addr.to_string();
    let dl = env.work.path().join("dl");
    let curl = fake_curl(env.work.path(), &hex_encode(&f.spec.info_hash));
    tokio::task::block_in_place(|| {
        // not running: status says so in JSON and exits 1
        let o = env.cli(&["status", "--json"]);
        assert_eq!(o.status.code(), Some(1));
        let st = Json::parse(String::from_utf8_lossy(&o.stdout).trim()).unwrap();
        assert_eq!(st.get("running"), Some(&Json::Bool(false)));

        env.ok(&["start", "--no-dht", "--peer", &peer, "--download-dir", s(&dl)]);
        env.ok(&["add", s(&f.torrent)]);
        wait_until("complete", || env.ok(&["list"]).contains("complete"));

        let list = env.json(&["list", "--json"]);
        let row = &list.as_array().unwrap()[0];
        assert_eq!(row.get("id").and_then(Json::as_u64), Some(1));
        assert_eq!(row.get("state").and_then(Json::as_str), Some("complete"));
        assert_eq!(row.get("progress_percent"), Some(&Json::Num(100.0)));
        assert_eq!(env.json(&["ls", "--json"]), list, "alias");

        let info = env.json(&["info", "1", "--json"]);
        assert_eq!(info.get("name").and_then(Json::as_str), Some("sample.bin"));
        assert_eq!(info.get("done_bytes").and_then(Json::as_u64), Some(f.data.len() as u64));

        let cfg = env.json(&["config", "--json"]);
        assert_eq!(cfg.get("download_dir").and_then(Json::as_str), Some(s(&dl)));
        assert_eq!(cfg.get("watch_dir"), Some(&Json::Null));

        let st = env.json(&["status", "--json"]);
        assert_eq!((st.get("running"), st.get("torrents")), (Some(&Json::Bool(true)), Some(&Json::Num(1.0))));
        assert!(env.json(&["sources", "--json"]).as_array().unwrap().len() >= 2);

        // search --json marks results that are already downloaded
        let o = env.cmd().args(["search", "sample", "--json", "--source", "torznab"]).env("WISPLET_CURL", &curl).output().unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let j = Json::parse(String::from_utf8_lossy(&o.stdout).trim()).unwrap();
        assert_eq!(j.get("count").and_then(Json::as_u64), Some(1));
        let r = &j.get("results").unwrap().as_array().unwrap()[0];
        assert_eq!((r.get("in_downloads"), r.get("download_state").and_then(Json::as_str)), (Some(&Json::Bool(true)), Some("complete")));
        assert_eq!(j.get("sources").unwrap().as_array().unwrap().len(), 1);
    });
}

#[test]
fn guide_and_mcp_config_describe_how_to_connect() {
    let env = Env::new();
    let guide = env.ok(&["guide"]);
    for needle in ["wisplet for LLMs", "list_downloads", "search_torrents", "download_search_results", "delete_files", "untrusted", "claude mcp add"] {
        assert!(guide.contains(needle), "the guide should mention {needle}");
    }
    // the guide documents every tool the server exposes
    let mut mcp = Mcp::start(&env, &[]);
    mcp.initialize();
    let tools = mcp.request("tools/list", Json::Obj(vec![]));
    for t in tools.get("result").unwrap().get("tools").unwrap().as_array().unwrap() {
        let name = t.get("name").and_then(Json::as_str).unwrap();
        assert!(guide.contains(name), "docs/LLM.md does not mention the tool {name}");
    }
    mcp.finish();

    let cfg = env.ok(&["mcp-config"]);
    assert!(cfg.contains("claude mcp add wisplet --") && cfg.contains(r#""args":["mcp"]"#) && cfg.contains("mcpServers"), "{cfg}");
    let exe = fs::canonicalize(BIN).unwrap();
    assert!(cfg.contains(exe.to_str().unwrap()), "the snippet carries the real path of the binary");
}
