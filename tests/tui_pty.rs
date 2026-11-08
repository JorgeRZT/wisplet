//! End-to-end test of the live interface on a real pseudo-terminal: the binary runs with its
//! stdin/stdout on a pty, we type keys and read the screen it draws.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::seeder::*;

const BIN: &str = env!("CARGO_BIN_EXE_wisplet");

struct Pty {
    master: OwnedFd,
    child: Child,
    screen: Vec<u8>,
}

impl Pty {
    fn spawn(home: &std::path::Path, cols: u16, rows: u16) -> Pty {
        Pty::spawn_with_env(home, cols, rows, &[])
    }

    fn spawn_with_env(home: &std::path::Path, cols: u16, rows: u16, env: &[(&str, &std::ffi::OsStr)]) -> Pty {
        let (mut m, mut s) = (0, 0);
        let mut ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        let r = unsafe { libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), &mut ws) };
        assert_eq!(r, 0, "openpty");
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
        let child = Command::new(BIN)
            .env("WISPLET_HOME", home)
            .env("NO_COLOR", "1")
            .envs(env.iter().map(|(k, v)| (*k, *v)))
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave))
            .spawn()
            .unwrap();
        Pty { master, child, screen: Vec::new() }
    }

    fn send(&self, bytes: &[u8]) {
        let n = unsafe { libc::write(self.master.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(n as usize, bytes.len());
    }

    /// Read output until `needle` appears in what was drawn *since the last call*.
    fn wait_for(&mut self, needle: &str) {
        let start = Instant::now();
        let from = self.screen.len();
        loop {
            let seen = String::from_utf8_lossy(&self.screen[from..]).into_owned();
            if seen.contains(needle) {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "timeout waiting for {needle:?}; screen so far:\n{seen}"
            );
            let mut pfd = libc::pollfd { fd: self.master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut pfd, 1, 100) } > 0 {
                let mut buf = [0u8; 4096];
                let n = unsafe { libc::read(self.master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n > 0 {
                    self.screen.extend_from_slice(&buf[..n as usize]);
                }
            }
        }
    }

    /// Without sending any key, wait until the child has drawn `n` different frames
    /// (progress and speed change while it downloads).
    fn wait_until_frames_differ(&mut self, n: usize) {
        let start = Instant::now();
        loop {
            let text = self.text();
            let mut frames: Vec<&str> = text.split("\x1b[H").filter(|f| f.contains("sample.bin")).collect();
            frames.sort_unstable();
            frames.dedup();
            if frames.len() >= n {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(15), "only {} distinct frames", frames.len());
            self.wait_for_more_output();
        }
    }

    fn wait_for_more_output(&mut self) {
        let mut pfd = libc::pollfd { fd: self.master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, 200) } > 0 {
            let mut buf = [0u8; 4096];
            let n = unsafe { libc::read(self.master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                self.screen.extend_from_slice(&buf[..n as usize]);
            }
        }
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "process did not exit");
            // keep draining so the child never blocks on a full pty buffer
            let mut pfd = libc::pollfd { fd: self.master.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut pfd, 1, 50) } > 0 {
                let mut buf = [0u8; 4096];
                let n = unsafe { libc::read(self.master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n > 0 {
                    self.screen.extend_from_slice(&buf[..n as usize]);
                }
            }
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.screen).into_owned()
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn cli(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN).args(args).env("WISPLET_HOME", home).output().unwrap()
}

/// Stops the test daemon when dropped, even if an assertion failed.
struct StopDaemon(std::path::PathBuf);

impl Drop for StopDaemon {
    fn drop(&mut self) {
        let _ = Command::new(BIN).arg("stop").env("WISPLET_HOME", &self.0).output();
    }
}

/// Start a daemon with one slow torrent so the list shows a download in progress.
async fn setup() -> (Seeder, tempfile::TempDir, tempfile::TempDir, StopDaemon) {
    let data = pseudo_bytes(2_000_000, 5);
    let src = Arc::new(MemSource(data));
    let spec = build_single("sample.bin", &*src, 32 * 1024);
    let seeder = Seeder::start(&spec, src, Behavior { block_delay_ms: 80, ..Default::default() }).await;
    let work = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let torrent = work.path().join("sample.torrent");
    fs::write(&torrent, &spec.torrent_file).unwrap();
    let peer = seeder.addr.to_string();
    let dl = work.path().join("dl");
    let o = cli(
        home.path(),
        &["start", "--no-dht", "--peer", &peer, "--pipeline", "2", "--download-dir", dl.to_str().unwrap()],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = cli(home.path(), &["add", torrent.to_str().unwrap()]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let stop = StopDaemon(home.path().to_path_buf());
    (seeder, work, home, stop)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_screen_updates_by_itself_and_restores_the_terminal() {
    let (_seeder, _work, home, _stop) = setup().await;
    let h = home.path().to_path_buf();
    tokio::task::block_in_place(|| {
        let mut p = Pty::spawn(&h, 110, 30);
        p.wait_for("sample.bin"); // list drawn on the alternate screen
        assert!(p.text().contains("\x1b[?1049h"), "enters the alternate screen");
        assert!(p.text().contains("1 torrents"));

        // no key pressed: the progress changes on its own (percent text differs between frames)
        p.wait_until_frames_differ(3);

        // keyboard: pause with p, resume with p, details with i
        p.send(b"p");
        p.wait_for("Paused torrent 1");
        p.send(b"p");
        p.wait_for("Resumed torrent 1");
        p.send(b"i");
        p.wait_for("Details");
        p.send(b" ");
        p.wait_for("PROGRESS");

        // quit leaves the alternate screen and shows the cursor again; the daemon keeps running
        p.send(b"q");
        let st = p.wait_exit();
        assert!(st.success(), "{st:?}");
        let t = p.text();
        assert!(t.ends_with("\x1b[?25h\x1b[?1049l"), "terminal restored: {:?}", &t[t.len().saturating_sub(40)..]);
        assert!(cli(&h, &["status"]).status.success(), "daemon still running after q");
        cli(&h, &["stop"]);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_remove_settings_and_quit_and_stop() {
    let (_seeder, work, home, _stop) = setup().await;
    let h = home.path().to_path_buf();
    let new_dir = work.path().join("other downloads");
    tokio::task::block_in_place(|| {
        let mut p = Pty::spawn(&h, 120, 30);
        p.wait_for("sample.bin");

        // change the download folder from the screen
        p.send(b"s");
        p.wait_for("New download folder");
        p.send(format!("{}\r", new_dir.display()).as_bytes());
        p.wait_for("Download folder set to");
        assert!(new_dir.is_dir());
        assert!(String::from_utf8_lossy(&cli(&h, &["config"]).stdout).contains(new_dir.to_str().unwrap()));

        // remove with confirmation
        p.send(b"x");
        p.wait_for("Remove torrent 1");
        p.send(b"y");
        p.wait_for("Removed torrent 1");
        p.wait_for("No torrents yet");

        // Q quits and stops the daemon
        p.send(b"Q");
        let st = p.wait_exit();
        assert!(st.success(), "{st:?}");
        assert!(p.text().contains("\x1b[?1049l"));
        assert!(!cli(&h, &["status"]).status.success(), "daemon stopped by Q");
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_quits_cleanly_and_sigterm_restores_the_terminal() {
    let (_seeder, _work, home, _stop) = setup().await;
    let h = home.path().to_path_buf();
    tokio::task::block_in_place(|| {
        let mut p = Pty::spawn(&h, 100, 24);
        p.wait_for("sample.bin");
        p.send(b"\x03");
        assert!(p.wait_exit().success());
        assert!(p.text().ends_with("\x1b[?25h\x1b[?1049l"));

        let mut p = Pty::spawn(&h, 100, 24);
        p.wait_for("sample.bin");
        unsafe { libc::kill(p.child.id() as i32, libc::SIGTERM) };
        let st = p.wait_exit();
        assert_eq!(std::os::unix::process::ExitStatusExt::signal(&st), None);
        assert_eq!(st.code(), Some(128 + libc::SIGTERM), "{st:?}");
        assert!(p.text().ends_with("\x1b[?25h\x1b[?1049l"), "terminal restored on SIGTERM");
        cli(&h, &["stop"]);
    });
}

/// A `curl` that answers only the (fake) Torznab server, with two torrents.
fn fake_curl(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("curl");
    let script = format!(
        r##"#!/bin/sh
for last; do :; done
case "$last" in
  *127.0.0.1:9117*) printf '%s' '<rss xmlns:torznab="x"><channel><item><title>Ubuntu Test ISO</title><size>1048576</size><torznab:attr name="seeders" value="40"/><torznab:attr name="peers" value="42"/><torznab:attr name="infohash" value="{}"/></item><item><title>Ubuntu Other</title><size>2048</size><torznab:attr name="seeders" value="7"/><torznab:attr name="peers" value="8"/><torznab:attr name="infohash" value="{}"/></item></channel></rss>' ;;
  *) printf '%s' '{{"response":{{"docs":[]}}}}' ;;
esac
"##,
        "ab".repeat(20),
        "cd".repeat(20)
    );
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_from_the_live_screen_lists_results_and_adds_the_chosen_one() {
    let (_seeder, work, home, _stop) = setup().await;
    let h = home.path().to_path_buf();
    let curl = fake_curl(work.path());
    tokio::task::block_in_place(|| {
        let mut p = Pty::spawn_with_env(
            &h,
            120,
            30,
            &[("WISPLET_CURL", curl.as_os_str()), ("WISPLET_TORZNAB_URL", std::ffi::OsStr::new("http://127.0.0.1:9117/api"))],
        );
        p.wait_for("sample.bin");

        p.send(b"/");
        p.wait_for("Search torrents");
        p.send(b"ubuntu\r");
        p.wait_for("Results for \"ubuntu\" (2)");
        p.wait_for("Ubuntu Test ISO"); // best seeded first
        p.wait_for("Enter add to downloads");

        // moving down and back, then Esc returns to the list without quitting
        p.send(b"j");
        p.wait_for("Ubuntu Other");
        p.send(b"\x1b");
        p.wait_for("PROGRESS");

        // search again: Enter adds a result and STAYS on the list, so several can be picked
        p.send(b"/ubuntu\r");
        p.wait_for("Ubuntu Test ISO");
        p.send(b"\r");
        p.wait_for("Added torrent 2: Ubuntu Test ISO");
        p.wait_for("IN DOWNLOADS"); // still the results screen
        p.send(b"j\r");
        p.wait_for("Added torrent 3: Ubuntu Other");
        p.send(b"\r"); // again on one that is already there
        p.wait_for("Already in your downloads");

        // Esc goes back to the downloads, where both new torrents are listed
        p.send(b"\x1b");
        p.wait_for("PROGRESS");

        p.send(b"q");
        assert!(p.wait_exit().success());
        let list = String::from_utf8_lossy(&cli(&h, &["list"]).stdout).into_owned();
        assert!(list.contains("2 ") && list.contains("3 "), "both magnets were added: {list}");
    });
}
