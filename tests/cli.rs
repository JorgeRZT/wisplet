//! Black-box tests of the real `wisplet` binary (separate processes, real daemon).
#![cfg(unix)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::seeder::*;
use wisplet::util::hex_encode;

const BIN: &str = env!("CARGO_BIN_EXE_wisplet");

struct Env {
    home: tempfile::TempDir,
    dl: tempfile::TempDir,
}

impl Env {
    fn new() -> Env {
        Env { home: tempfile::tempdir().unwrap(), dl: tempfile::tempdir().unwrap() }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("WISPLET_HOME", self.home.path())
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let o = self.run(args);
        assert!(o.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn wait_list_contains(&self, needle: &str) -> String {
        let start = Instant::now();
        loop {
            let out = self.ok(&["list"]);
            if out.contains(needle) {
                return out;
            }
            assert!(start.elapsed() < Duration::from_secs(20), "timeout waiting for {needle:?}; list:\n{out}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Always stops the daemon, even when an assertion fails.
impl Drop for Env {
    fn drop(&mut self) {
        let _ = Command::new(BIN).arg("stop").env("WISPLET_HOME", self.home.path()).output();
    }
}

struct Fx {
    torrent: PathBuf,
    data: Vec<u8>,
    spec: TorrentSpec,
    seeder: Seeder,
    _dir: tempfile::TempDir,
}

async fn fx(len: usize, behavior: Behavior) -> Fx {
    let data = pseudo_bytes(len, 3);
    let src = Arc::new(MemSource(data.clone()));
    let spec = build_single("sample.bin", &*src, 32 * 1024);
    let seeder = Seeder::start(&spec, src, behavior).await;
    let dir = tempfile::tempdir().unwrap();
    let torrent = dir.path().join("sample.torrent");
    fs::write(&torrent, &spec.torrent_file).unwrap();
    Fx { torrent, data, spec, seeder, _dir: dir }
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

// The seeder lives on the test's runtime; child processes connect over TCP. `block_in_place`
// needs a multi-thread runtime so the seeder keeps serving while we wait for the child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_pause_resume_info_remove() {
    let f = fx(400_000, Behavior { block_delay_ms: 20, ..Default::default() }).await;
    let peer = f.seeder.addr.to_string();
    let env = Env::new();
    let (t, dl) = (f.torrent.clone(), env.dl.path().to_path_buf());

    tokio::task::block_in_place(|| {
        env.ok(&["start", "--no-dht", "--peer", &peer, "--pipeline", "2", "--download-dir", s(&dl)]);
        assert!(env.ok(&["add", s(&t)]).contains("Added torrent 1"));

        // duplicate add fails with a clear message and non-zero exit
        let dup = env.run(&["add", s(&t)]);
        assert!(!dup.status.success());
        assert!(String::from_utf8_lossy(&dup.stderr).contains("already added"));

        let out = env.wait_list_contains("downloading");
        assert!(out.contains("sample.bin") && out.contains("STATE"));

        assert!(env.ok(&["pause", "1"]).contains("Paused 1"));
        assert!(env.ok(&["list"]).contains("paused"));
        assert!(env.ok(&["info", "1"]).contains("state      paused"));
        assert!(env.ok(&["resume", "all"]).contains("Resumed 1"));
        env.wait_list_contains("complete");
        assert_eq!(fs::read(dl.join("sample.bin")).unwrap(), f.data);

        let info = env.ok(&["info", "1"]);
        assert!(info.contains(&hex_encode(&f.spec.info_hash)) && info.contains("complete"));

        // usage errors exit with code 2
        for bad in [vec!["pause"], vec!["info", "abc"], vec!["add"], vec!["frobnicate"], vec!["remove", "1", "--bogus"]] {
            let o = env.run(&bad);
            assert_eq!(o.status.code(), Some(2), "{bad:?}");
        }
        // unknown id is a runtime error (exit 1)
        assert_eq!(env.run(&["remove", "42"]).status.code(), Some(1));

        assert!(env.ok(&["remove", "1", "--purge"]).contains("Removed"));
        assert!(!dl.join("sample.bin").exists());
        assert!(env.ok(&["list"]).contains("No torrents"));
        assert!(env.ok(&["stop"]).contains("stopped"));
        assert!(!env.run(&["status"]).status.success());
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_autostarts_daemon_and_honours_output_dir() {
    let f = fx(100_000, Behavior { serve_metadata: true, ..Default::default() }).await;
    let peer = f.seeder.addr.to_string();
    let env = Env::new();
    let out_dir = tempfile::tempdir().unwrap();
    let magnet = format!("magnet:?xt=urn:btih:{}&x.pe={}", hex_encode(&f.spec.info_hash), peer);
    let o = out_dir.path().to_path_buf();

    tokio::task::block_in_place(|| {
        // no explicit `start`: add launches the daemon itself
        let out = env.ok(&["add", &magnet, "-o", s(&o)]);
        assert!(out.contains("Added torrent 1"));
        env.wait_list_contains("complete");
        assert_eq!(fs::read(o.join("sample.bin")).unwrap(), f.data);
        env.ok(&["stop"]);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_downloads_in_the_foreground() {
    let f = fx(250_000, Behavior::default()).await;
    let peer = f.seeder.addr.to_string();
    let env = Env::new();
    let (t, dl) = (f.torrent.clone(), env.dl.path().to_path_buf());

    tokio::task::block_in_place(|| {
        let out = env.ok(&["get", s(&t), "-o", s(&dl), "--no-dht", "--peer", &peer]);
        assert!(out.contains("Done: sample.bin"), "{out}");
        assert_eq!(fs::read(dl.join("sample.bin")).unwrap(), f.data);

        // running again resumes instantly (data already complete)
        let before = f.seeder.served();
        env.ok(&["get", s(&t), "-o", s(&dl), "--no-dht"]);
        assert_eq!(f.seeder.served(), before);

        // bad inputs
        assert_eq!(env.run(&["get"]).status.code(), Some(2));
        assert_eq!(env.run(&["get", "/no/such/file.torrent"]).status.code(), Some(1));
        assert_eq!(env.run(&["get", "magnet:?dn=x"]).status.code(), Some(1));
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn help_and_version_of_usage() {
    let env = Env::new();
    tokio::task::block_in_place(|| {
        for args in [vec![], vec!["--help"], vec!["help"], vec!["add", "--help"]] {
            let out = env.ok(&args);
            assert!(out.contains("USAGE") && out.contains("wisplet get"), "{args:?}");
        }
    });
}

#[test]
fn overlong_state_directory_fails_fast_with_a_clear_message() {
    let base = tempfile::tempdir().unwrap();
    let long = base.path().join("d".repeat(120));
    for cmd in ["start", "daemon"] {
        let started = Instant::now();
        let out = Command::new(BIN).arg(cmd).env("WISPLET_HOME", &long).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{cmd}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("WISPLET_HOME") && err.contains("shorter"), "{cmd}: {err}");
        assert!(started.elapsed() < Duration::from_secs(3), "{cmd} must not wait for a daemon that cannot start");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_sets_the_download_folder_used_by_add() {
    let f = fx(100_000, Behavior::default()).await;
    let peer = f.seeder.addr.to_string();
    let env = Env::new();
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("my downloads");
    let t = f.torrent.clone();

    tokio::task::block_in_place(|| {
        env.ok(&["start", "--no-dht", "--peer", &peer]);
        assert!(env.ok(&["config"]).contains("Download folder"));
        assert!(env.ok(&["config", "show"]).contains("Download folder"));

        let out = env.ok(&["config", "set", "download-dir", s(&target)]);
        assert!(out.contains("Download folder set to"), "{out}");
        assert!(target.is_dir());
        assert!(env.ok(&["config"]).contains(s(&target)));

        env.ok(&["add", s(&t)]);
        env.wait_list_contains("complete");
        assert_eq!(fs::read(target.join("sample.bin")).unwrap(), f.data);

        // the setting is persisted: it survives a daemon restart
        env.ok(&["stop"]);
        env.ok(&["start", "--no-dht"]);
        assert!(env.ok(&["config"]).contains(s(&target)));

        // usage errors exit with code 2
        for bad in [vec!["config", "set"], vec!["config", "set", "colour", "red"], vec!["config", "bogus"]] {
            assert_eq!(env.run(&bad).status.code(), Some(2), "{bad:?}");
        }
        // a path that cannot be created is a runtime error
        let file = root.path().join("plain");
        fs::write(&file, b"x").unwrap();
        let blocked = file.join("sub");
        assert_eq!(env.run(&["config", "set", "download-dir", s(&blocked)]).status.code(), Some(1));
    });
}

#[test]
fn no_arguments_without_a_terminal_prints_the_help() {
    let env = Env::new();
    let o = env.run(&[]); // stdout is a pipe here, so no live view
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success());
    assert!(out.contains("USAGE") && out.contains("wisplet config set download-dir"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_watch_dir_and_move_dir_and_per_torrent_move() {
    let f = fx(100_000, Behavior::default()).await;
    let peer = f.seeder.addr.to_string();
    let env = Env::new();
    let root = tempfile::tempdir().unwrap();
    let (watch, done, special) = (root.path().join("inbox"), root.path().join("done"), root.path().join("special"));
    let t = f.torrent.clone();

    tokio::task::block_in_place(|| {
        env.ok(&["start", "--no-dht", "--peer", &peer, "--download-dir", s(env.dl.path())]);
        let shown = env.ok(&["config"]);
        assert!(shown.contains("Watch folder       off") && shown.contains("Move completed to  off"), "{shown}");

        assert!(env.ok(&["config", "set", "watch-dir", s(&watch)]).contains("Watch folder set to"));
        assert!(env.ok(&["config", "set", "move-dir", s(&done)]).contains("Completed torrents folder set to"));
        assert!(watch.is_dir() && done.is_dir());
        let shown = env.ok(&["config"]);
        assert!(shown.contains(s(&watch)) && shown.contains(s(&done)), "{shown}");

        // a torrent added with --move-to goes to its own folder, not the global one
        let out = env.ok(&["add", s(&t), "--move-to", s(&special)]);
        assert!(out.contains("Added torrent 1"), "{out}");
        env.wait_list_contains("complete");
        let start = Instant::now();
        while !special.join("sample.bin").exists() {
            assert!(start.elapsed() < Duration::from_secs(20), "file never moved");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(fs::read(special.join("sample.bin")).unwrap(), f.data);
        assert!(!done.join("sample.bin").exists());
        assert!(env.ok(&["info", "1"]).contains(s(&special)));

        // `move` on a completed torrent moves it now; `none` is accepted too
        assert!(env.ok(&["move", "1", s(&done)]).contains("will be moved to"));
        let start = Instant::now();
        while !done.join("sample.bin").exists() {
            assert!(start.elapsed() < Duration::from_secs(20), "file never moved by `move`");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(env.ok(&["move", "1", "none"]).contains("will not be moved"));
        assert!(env.ok(&["move", "1", "default"]).contains("follows the global"));

        assert!(env.ok(&["config", "set", "watch-dir", "off"]).contains("turned off"));
        assert!(env.ok(&["config", "set", "move-dir", "OFF"]).contains("turned off"));
        let shown = env.ok(&["config"]);
        assert!(shown.contains("Watch folder       off") && shown.contains("Move completed to  off"), "{shown}");

        // usage errors exit with code 2; runtime errors with 1
        for bad in [
            vec!["move"],
            vec!["move", "x", "/a"],
            vec!["move", "1"],
            vec!["config", "set", "watch-dir"],
            vec!["config", "set", "colour", "red"],
            vec!["add", s(&t), "--move-to"],
        ] {
            assert_eq!(env.run(&bad).status.code(), Some(2), "{bad:?}");
        }
        assert_eq!(env.run(&["move", "42", "/a"]).status.code(), Some(1));
    });
}

/// Address of the (fake) Torznab server the search tests talk to.
const TORZNAB: &str = "http://127.0.0.1:9117/api";

/// A fake `curl`: the Internet Archive fails (to prove one failing source is tolerated) and the
/// Torznab server returns two torrents, one with seeders and one dead. Point `WISPLET_CURL` at it.
fn fake_curl(dir: &Path, hash: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("curl");
    let script = format!(
        r##"#!/bin/sh
for last; do :; done
case "$last" in
  *archive.org*) echo 'curl: (22) The requested URL returned error: 500' >&2; exit 22 ;;
  *127.0.0.1:9117*) printf '%s' '<rss xmlns:torznab="x"><channel><item><title>Ubuntu Test ISO</title><size>1048576</size><torznab:attr name="seeders" value="40"/><torznab:attr name="peers" value="42"/><torznab:attr name="infohash" value="{hash}"/></item><item><title>Ubuntu dead</title><size>10</size><torznab:attr name="seeders" value="0"/><torznab:attr name="infohash" value="{h2}"/></item></channel></rss>' ;;
  *) echo "unexpected URL" >&2; exit 2 ;;
esac
"##,
        h2 = "cd".repeat(20)
    );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

impl Env {
    fn run_with_curl(&self, curl: &Path, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("WISPLET_HOME", self.home.path())
            .env("WISPLET_CURL", curl)
            .env("WISPLET_TORZNAB_URL", TORZNAB)
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap()
    }
}

#[test]
fn search_lists_results_reports_failed_sources_and_hides_dead_torrents() {
    let env = Env::new();
    let curl = fake_curl(env.dl.path(), &"ab".repeat(20));
    let o = env.run_with_curl(&curl, &["search", "ubuntu", "test"]);
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(o.status.success(), "{out}\n{err}");
    assert!(out.contains("Ubuntu Test ISO") && out.contains("40") && out.contains("1.0 MiB"), "{out}");
    assert!(!out.contains("Ubuntu dead"), "0-seeder magnets are hidden by default: {out}");
    assert!(err.contains("archive") && err.contains("error"), "failed sources are reported on stderr: {err}");

    let o = env.run_with_curl(&curl, &["search", "ubuntu", "--include-dead", "--magnet", "--source", "torznab"]);
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(out.contains("Ubuntu dead") && out.contains("magnet:?xt=urn:btih:"), "{out}");
    assert!(String::from_utf8_lossy(&o.stderr).is_empty(), "only the Torznab source was asked, and it works");

    let o = env.run_with_curl(&curl, &["search", "ubuntu", "--limit", "1"]);
    assert_eq!(String::from_utf8_lossy(&o.stdout).matches("Ubuntu").count(), 1);
}

#[test]
fn search_errors_and_usage() {
    let env = Env::new();
    let curl = fake_curl(env.dl.path(), &"ab".repeat(20));
    for bad in [vec!["search"], vec!["search", "x", "--source", "nope"], vec!["search", "x", "--limit", "0"], vec!["search", "x", "--add", "9"], vec!["search", "x", "--add", "1,"]] {
        assert_eq!(env.run_with_curl(&curl, &bad).status.code(), Some(2), "{bad:?}");
    }
    // no curl at all: every source fails, which is a runtime error with a helpful message
    let o = env.run_with_curl(Path::new("/definitely/not/curl"), &["search", "x"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("every search source failed"), "{}", String::from_utf8_lossy(&o.stderr));

    let sources = env.ok(&["sources"]);
    for key in ["archive", "torznab"] {
        assert!(sources.contains(key), "{sources}");
    }

    // Torznab asked for explicitly but not configured: a clear message, not a crash
    let o = Command::new(BIN)
        .args(["search", "x", "--source", "torznab"])
        .env("WISPLET_HOME", env.home.path())
        .env("WISPLET_CURL", &curl)
        .env_remove("WISPLET_TORZNAB_URL")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("WISPLET_TORZNAB_URL"), "{}", String::from_utf8_lossy(&o.stderr));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_add_downloads_the_chosen_result_through_its_magnet() {
    let f = fx(100_000, Behavior { serve_metadata: true, ..Default::default() }).await;
    let peer = f.seeder.addr.to_string();
    let env = Env::new();
    let hash = hex_encode(&f.spec.info_hash);
    let curl = fake_curl(env.dl.path(), &hash);
    let dl = env.dl.path().join("out");

    tokio::task::block_in_place(|| {
        env.ok(&["start", "--no-dht", "--peer", &peer, "--download-dir", s(&dl)]);
        let o = env.run_with_curl(&curl, &["search", "ubuntu", "--source", "torznab", "--add", "1"]);
        let out = String::from_utf8_lossy(&o.stdout).into_owned();
        assert!(o.status.success(), "{out}\n{}", String::from_utf8_lossy(&o.stderr));
        assert!(out.contains("Added torrent 1 (Ubuntu Test ISO)"), "{out}");
        // the magnet carries the info-hash; the fake seeder serves the metadata and the data
        env.wait_list_contains("complete");
        assert_eq!(fs::read(dl.join("sample.bin")).unwrap(), f.data);
        let again = env.run_with_curl(&curl, &["search", "ubuntu", "--source", "torznab", "--add", "1"]);
        assert!(!again.status.success(), "adding the same result twice is a clear error");
        assert!(String::from_utf8_lossy(&again.stderr).contains("already added"));
    });
}

#[test]
fn search_add_can_take_several_results_and_reports_duplicates_per_result() {
    let env = Env::new();
    let curl = fake_curl(env.dl.path(), &"ab".repeat(20));
    // both results (the second has 0 seeders, so --include-dead) are queued in one go
    let o = env.run_with_curl(&curl, &["search", "ubuntu", "--source", "torznab", "--include-dead", "--add", "1,2"]);
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(o.status.success(), "{out}\n{}", String::from_utf8_lossy(&o.stderr));
    assert!(out.contains("Added torrent 1 (Ubuntu Test ISO)") && out.contains("Added torrent 2 (Ubuntu dead)"), "{out}");
    let list = env.ok(&["list"]);
    assert!(list.contains("1 ") && list.contains("2 "), "{list}");

    // asking again: each already-added result is reported on its own and the exit code says so
    let again = env.run_with_curl(&curl, &["search", "ubuntu", "--source", "torznab", "--include-dead", "--add", "1,2"]);
    let err = String::from_utf8_lossy(&again.stderr).into_owned();
    assert_eq!(again.status.code(), Some(1), "{err}");
    assert!(err.matches("already added").count() == 2 && err.contains("result 1") && err.contains("result 2"), "{err}");
    env.ok(&["stop"]);
}
