//! Daemon + control-protocol tests, run in-process against fake seeders.
#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use common::seeder::*;
use common::test_config;
use wisplet::config::Config;
use wisplet::daemon;
use wisplet::protocol::{AddSource, Command, MoveSpec, Row, Target};
use wisplet::util::hex_encode;
use tokio::sync::Notify;
use tokio::task::{JoinHandle, LocalSet};
use tokio::time::{sleep, Instant};

struct Daemon {
    socket: PathBuf,
    shutdown: Rc<Notify>,
    handle: JoinHandle<std::io::Result<()>>,
}

async fn start_daemon(state: &Path, dl: &Path, cfg: Config) -> Daemon {
    let shutdown = Rc::new(Notify::new());
    let (state_p, dl_p, sd) = (state.to_path_buf(), dl.to_path_buf(), shutdown.clone());
    let handle = tokio::task::spawn_local(async move { daemon::run(cfg, &state_p, &dl_p, false, sd).await });
    let socket = daemon::socket_path(state);
    let start = Instant::now();
    while !daemon::is_running(&socket).await {
        assert!(start.elapsed() < Duration::from_secs(5), "daemon did not start");
        sleep(Duration::from_millis(20)).await;
    }
    Daemon { socket, shutdown, handle }
}

impl Daemon {
    async fn stop(self) {
        daemon::request(&self.socket, &Command::Shutdown).await.unwrap();
        self.handle.await.unwrap().unwrap();
        assert!(!self.socket.exists(), "socket removed on shutdown");
        let _ = &self.shutdown;
    }

    async fn rows(&self) -> Vec<Row> {
        daemon::request(&self.socket, &Command::List)
            .await
            .unwrap()
            .iter()
            .map(|l| Row::parse(l).unwrap())
            .collect()
    }

    async fn row(&self, id: u32) -> Row {
        self.rows().await.into_iter().find(|r| r.id == id).expect("row exists")
    }

    async fn wait_state(&self, id: u32, state: &str) -> Row {
        let start = Instant::now();
        loop {
            let r = self.row(id).await;
            if r.state == state {
                return r;
            }
            assert!(start.elapsed() < Duration::from_secs(20), "timeout waiting for {state}; now {r:?}");
            sleep(Duration::from_millis(25)).await;
        }
    }

    async fn add_file(&self, path: &Path) -> u32 {
        let lines = daemon::request(
            &self.socket,
            &Command::Add { source: AddSource::File(path.display().to_string()), dir: None, move_to: MoveSpec::Inherit },
        )
        .await
        .unwrap();
        lines[0].parse().unwrap()
    }
}

struct Fixture {
    state: tempfile::TempDir,
    dl: tempfile::TempDir,
    torrent_path: PathBuf,
    spec: TorrentSpec,
    data: Vec<u8>,
    seeder: Seeder,
}

async fn fixture(len: usize, behavior: Behavior) -> Fixture {
    let data = pseudo_bytes(len, 5);
    let src = std::sync::Arc::new(MemSource(data.clone()));
    let spec = build_single("clip.bin", &*src, 32 * 1024);
    let seeder = Seeder::start(&spec, src, behavior).await;
    let state = tempfile::tempdir().unwrap();
    let dl = tempfile::tempdir().unwrap();
    let torrent_dir = tempfile::tempdir().unwrap().keep();
    let torrent_path = torrent_dir.join("clip.torrent");
    fs::write(&torrent_path, &spec.torrent_file).unwrap();
    Fixture { state, dl, torrent_path, spec, data, seeder }
}

#[tokio::test]
async fn add_list_and_complete_a_torrent_file() {
    LocalSet::new()
        .run_until(async {
            let f = fixture(300_000, Behavior::default()).await;
            let d = start_daemon(f.state.path(), f.dl.path(), test_config(vec![f.seeder.addr])).await;

            assert!(d.rows().await.is_empty());
            let id = d.add_file(&f.torrent_path).await;
            assert_eq!(id, 1);
            let row = d.wait_state(id, "complete").await;
            assert_eq!(row.name, "clip.bin");
            assert_eq!(row.done, f.data.len() as u64);
            assert_eq!(row.total, f.data.len() as u64);
            assert_eq!(row.info_hash, hex_encode(&f.spec.info_hash));
            assert_eq!(fs::read(f.dl.path().join("clip.bin")).unwrap(), f.data);

            let info = daemon::request(&d.socket, &Command::Info(id)).await.unwrap();
            assert!(info.iter().any(|l| l == "state\tcomplete"));
            assert!(info.iter().any(|l| l.starts_with("pieces\t10 x 32768")));
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn add_magnet_link_with_custom_directory() {
    LocalSet::new()
        .run_until(async {
            let f = fixture(120_000, Behavior { serve_metadata: true, ..Default::default() }).await;
            let d = start_daemon(f.state.path(), f.dl.path(), test_config(vec![])).await;
            let custom = tempfile::tempdir().unwrap();

            let uri = format!("magnet:?xt=urn:btih:{}&dn=Nice+Clip&x.pe={}", hex_encode(&f.spec.info_hash), f.seeder.addr);
            let lines = daemon::request(
                &d.socket,
                &Command::Add { source: AddSource::Magnet(uri), dir: Some(custom.path().display().to_string()), move_to: MoveSpec::Inherit },
            )
            .await
            .unwrap();
            let id: u32 = lines[0].parse().unwrap();
            // before metadata arrives the display name comes from dn=
            d.wait_state(id, "complete").await;
            assert_eq!(fs::read(custom.path().join("clip.bin")).unwrap(), f.data);
            assert!(!f.dl.path().join("clip.bin").exists());
            // the fetched metadata was stored for restarts
            assert!(f.state.path().join("torrents").join(format!("{}.torrent", hex_encode(&f.spec.info_hash))).exists());
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn pause_and_resume_midway() {
    LocalSet::new()
        .run_until(async {
            let f = fixture(600_000, Behavior { block_delay_ms: 30, ..Default::default() }).await;
            let mut cfg = test_config(vec![f.seeder.addr]);
            cfg.pipeline = 2;
            cfg.pipeline_max = 2;
            let d = start_daemon(f.state.path(), f.dl.path(), cfg).await;
            let id = d.add_file(&f.torrent_path).await;

            // wait for some progress
            let start = Instant::now();
            while d.row(id).await.done == 0 {
                assert!(start.elapsed() < Duration::from_secs(10));
                sleep(Duration::from_millis(20)).await;
            }
            let n = daemon::request(&d.socket, &Command::Pause(Target::Id(id))).await.unwrap();
            assert_eq!(n, vec!["1"]);
            let paused = d.row(id).await;
            assert_eq!(paused.state, "paused");
            assert_eq!((paused.rate, paused.peers), (0, 0));

            // let requests that were already in flight when we paused drain, then expect silence
            sleep(Duration::from_millis(150)).await;
            let served = f.seeder.served();
            sleep(Duration::from_millis(400)).await;
            assert_eq!(f.seeder.served(), served, "no traffic while paused");
            assert_ne!(d.row(id).await.state, "complete");

            // pausing again is a no-op
            let n = daemon::request(&d.socket, &Command::Pause(Target::All)).await.unwrap();
            assert_eq!(n, vec!["0"]);

            let n = daemon::request(&d.socket, &Command::Resume(Target::All)).await.unwrap();
            assert_eq!(n, vec!["1"]);
            d.wait_state(id, "complete").await;
            assert_eq!(fs::read(f.dl.path().join("clip.bin")).unwrap(), f.data);
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn remove_keeps_data_unless_purged() {
    LocalSet::new()
        .run_until(async {
            let f = fixture(100_000, Behavior::default()).await;
            let d = start_daemon(f.state.path(), f.dl.path(), test_config(vec![f.seeder.addr])).await;
            let id = d.add_file(&f.torrent_path).await;
            d.wait_state(id, "complete").await;

            daemon::request(&d.socket, &Command::Remove { id, purge: false }).await.unwrap();
            assert!(d.rows().await.is_empty());
            assert!(f.dl.path().join("clip.bin").exists(), "data kept");

            // re-adding the same torrent works after removal and completes instantly (resume)
            let id2 = d.add_file(&f.torrent_path).await;
            assert_eq!(id2, 2);
            d.wait_state(id2, "complete").await;

            daemon::request(&d.socket, &Command::Remove { id: id2, purge: true }).await.unwrap();
            assert!(!f.dl.path().join("clip.bin").exists(), "data purged");
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn duplicate_and_invalid_requests_are_rejected() {
    LocalSet::new()
        .run_until(async {
            let f = fixture(100_000, Behavior::default()).await;
            let d = start_daemon(f.state.path(), f.dl.path(), test_config(vec![f.seeder.addr])).await;
            let id = d.add_file(&f.torrent_path).await;

            let dup = daemon::request(
                &d.socket,
                &Command::Add { source: AddSource::File(f.torrent_path.display().to_string()), dir: None, move_to: MoveSpec::Inherit },
            )
            .await;
            assert!(dup.unwrap_err().to_string().contains(&format!("already added as id {id}")));

            let uri = format!("magnet:?xt=urn:btih:{}", hex_encode(&f.spec.info_hash));
            let dup = daemon::request(&d.socket, &Command::Add { source: AddSource::Magnet(uri), dir: None, move_to: MoveSpec::Inherit }).await;
            assert!(dup.is_err());

            for bad in [
                Command::Add { source: AddSource::File("/definitely/not/here.torrent".into()), dir: None, move_to: MoveSpec::Inherit },
                Command::Add { source: AddSource::Magnet("magnet:?dn=nohash".into()), dir: None, move_to: MoveSpec::Inherit },
                Command::Info(99),
                Command::Pause(Target::Id(99)),
                Command::Resume(Target::Id(99)),
                Command::Remove { id: 99, purge: true },
            ] {
                assert!(daemon::request(&d.socket, &bad).await.is_err(), "{bad:?}");
            }

            // a garbage file is not a torrent
            let junk = f.state.path().join("junk.torrent");
            fs::write(&junk, b"this is not bencode").unwrap();
            let r = daemon::request(&d.socket, &Command::Add { source: AddSource::File(junk.display().to_string()), dir: None, move_to: MoveSpec::Inherit }).await;
            assert!(r.is_err());
            assert_eq!(d.rows().await.len(), 1);
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn session_survives_daemon_restart() {
    LocalSet::new()
        .run_until(async {
            let f = fixture(600_000, Behavior { block_delay_ms: 30, ..Default::default() }).await;
            let mut cfg = test_config(vec![f.seeder.addr]);
            cfg.pipeline = 2;
            cfg.pipeline_max = 2;

            // second, instantly-completing torrent
            let data2 = pseudo_bytes(50_000, 8);
            let src2 = std::sync::Arc::new(MemSource(data2.clone()));
            let spec2 = build_single("quick.bin", &*src2, 16 * 1024);
            let seeder2 = Seeder::start(&spec2, src2, Behavior::default()).await;
            let path2 = f.state.path().join("quick.torrent");
            fs::write(&path2, &spec2.torrent_file).unwrap();
            cfg.extra_peers.push(seeder2.addr);

            let d = start_daemon(f.state.path(), f.dl.path(), cfg.clone()).await;
            let slow = d.add_file(&f.torrent_path).await;
            let quick = d.add_file(&path2).await;
            d.wait_state(quick, "complete").await;
            // pause the slow one so we also test persisting "paused"
            let start = Instant::now();
            while d.row(slow).await.done == 0 {
                assert!(start.elapsed() < Duration::from_secs(10));
                sleep(Duration::from_millis(20)).await;
            }
            daemon::request(&d.socket, &Command::Pause(Target::Id(slow))).await.unwrap();
            d.stop().await;

            // --- restart
            let d = start_daemon(f.state.path(), f.dl.path(), cfg.clone()).await;
            let rows = d.rows().await;
            assert_eq!(rows.len(), 2);
            assert_eq!(d.row(quick).await.state, "complete");
            assert_eq!(d.row(quick).await.done, data2.len() as u64);
            assert_eq!(d.row(slow).await.state, "paused");
            // ids keep counting after the highest persisted id
            let third_path = f.state.path().join("third.torrent");
            let data3 = pseudo_bytes(20_000, 12);
            let src3 = MemSource(data3);
            fs::write(&third_path, build_single("third.bin", &src3, 16 * 1024).torrent_file).unwrap();
            assert_eq!(d.add_file(&third_path).await, 3);

            daemon::request(&d.socket, &Command::Resume(Target::Id(slow))).await.unwrap();
            d.wait_state(slow, "complete").await;
            assert_eq!(fs::read(f.dl.path().join("clip.bin")).unwrap(), f.data);
            d.stop().await;

            // --- restart again: both complete, nothing is re-downloaded
            let served = f.seeder.served();
            let d = start_daemon(f.state.path(), f.dl.path(), cfg).await;
            assert!(d.rows().await.iter().filter(|r| r.id != 3).all(|r| r.state == "complete"));
            assert_ne!(d.row(3).await.state, "complete", "third torrent has no seeder");
            sleep(Duration::from_millis(300)).await;
            assert_eq!(f.seeder.served(), served);
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn active_download_is_resumed_after_restart() {
    LocalSet::new()
        .run_until(async {
            let f = fixture(600_000, Behavior { block_delay_ms: 30, ..Default::default() }).await;
            let mut cfg = test_config(vec![f.seeder.addr]);
            cfg.pipeline = 2;
            cfg.pipeline_max = 2;
            let d = start_daemon(f.state.path(), f.dl.path(), cfg.clone()).await;
            let id = d.add_file(&f.torrent_path).await;
            let start = Instant::now();
            while d.row(id).await.done == 0 {
                assert!(start.elapsed() < Duration::from_secs(10));
                sleep(Duration::from_millis(20)).await;
            }
            d.stop().await; // killed mid-download, still flagged "active"

            let d = start_daemon(f.state.path(), f.dl.path(), cfg).await;
            d.wait_state(id, "complete").await;
            assert_eq!(fs::read(f.dl.path().join("clip.bin")).unwrap(), f.data);
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn second_daemon_is_refused_and_stale_socket_is_replaced() {
    LocalSet::new()
        .run_until(async {
            let state = tempfile::tempdir().unwrap();
            let dl = tempfile::tempdir().unwrap();
            let d = start_daemon(state.path(), dl.path(), test_config(vec![])).await;

            let err = daemon::run(test_config(vec![]), state.path(), dl.path(), false, Rc::new(Notify::new()))
                .await
                .unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);
            assert!(daemon::is_running(&d.socket).await, "first daemon unaffected");

            let mode = fs::metadata(&d.socket).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "socket must be private");
            let mode = fs::metadata(state.path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "state dir must be private");
            d.stop().await;

            // simulate a crash: stale socket file left behind
            let stale = daemon::socket_path(state.path());
            drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
            assert!(stale.exists());
            let d = start_daemon(state.path(), dl.path(), test_config(vec![])).await;
            assert!(daemon::is_running(&d.socket).await);
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn garbage_on_the_socket_does_not_hurt_the_daemon() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    LocalSet::new()
        .run_until(async {
            let state = tempfile::tempdir().unwrap();
            let dl = tempfile::tempdir().unwrap();
            let d = start_daemon(state.path(), dl.path(), test_config(vec![])).await;

            for payload in [&b"\xff\xfe\x00garbage\n"[..], b"frobnicate\n", b"add file\n", b"\n"] {
                let mut s = tokio::net::UnixStream::connect(&d.socket).await.unwrap();
                s.write_all(payload).await.unwrap();
                let mut out = String::new();
                s.read_to_string(&mut out).await.unwrap();
                assert!(out.starts_with("ERR"), "{out:?}");
            }
            // connection that sends nothing and hangs up
            drop(tokio::net::UnixStream::connect(&d.socket).await.unwrap());
            // oversized request line
            let mut s = tokio::net::UnixStream::connect(&d.socket).await.unwrap();
            let _ = s.write_all(&vec![b'a'; 200_000]).await;
            drop(s);

            assert!(daemon::is_running(&d.socket).await);
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn settings_change_the_default_download_folder_and_survive_restart() {
    LocalSet::new()
        .run_until(async {
            let f = fixture(100_000, Behavior::default()).await;
            let new_dir = tempfile::tempdir().unwrap().path().join("fresh");
            let d = start_daemon(f.state.path(), f.dl.path(), test_config(vec![f.seeder.addr])).await;

            let shown = daemon::request(&d.socket, &Command::Settings).await.unwrap();
            assert_eq!(shown[0], format!("download_dir\t{}", f.dl.path().display()));
            assert_eq!(&shown[1..], ["watch_dir\t", "move_dir\t"], "the other settings are off by default");

            // invalid changes are refused and change nothing
            let err = daemon::request(&d.socket, &Command::SetDownloadDir("relative".into())).await.unwrap_err();
            assert!(err.to_string().contains("absolute"), "{err}");
            assert_eq!(daemon::request(&d.socket, &Command::Settings).await.unwrap(), shown);

            // a valid change applies to the next `add` without an explicit directory
            daemon::request(&d.socket, &Command::SetDownloadDir(new_dir.display().to_string())).await.unwrap();
            assert!(new_dir.is_dir());
            let id = d.add_file(&f.torrent_path).await;
            d.wait_state(id, "complete").await;
            assert_eq!(fs::read(new_dir.join("clip.bin")).unwrap(), f.data);
            assert!(!f.dl.path().join("clip.bin").exists());
            d.stop().await;

            // after a restart the saved folder wins over the startup default
            let d = start_daemon(f.state.path(), f.dl.path(), test_config(vec![f.seeder.addr])).await;
            let shown = daemon::request(&d.socket, &Command::Settings).await.unwrap();
            assert_eq!(shown[0], format!("download_dir\t{}", new_dir.display()));
            d.stop().await;
        })
        .await;
}

// ------------------------------------------------------------------ watch folder & move on completion

async fn wait_for_path(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(start.elapsed() < Duration::from_secs(20), "timeout waiting for {}", path.display());
        sleep(Duration::from_millis(25)).await;
    }
}

async fn settings(d: &Daemon) -> std::collections::HashMap<String, String> {
    daemon::request(&d.socket, &Command::Settings)
        .await
        .unwrap()
        .iter()
        .filter_map(|l| l.split_once('\t').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

#[tokio::test]
async fn completed_torrent_moves_to_the_global_folder_and_stays_there_after_restart() {
    LocalSet::new()
        .run_until(async {
            let f = fixture(300_000, Behavior::default()).await;
            let done = tempfile::tempdir().unwrap().path().join("finished stuff");
            let cfg = test_config(vec![f.seeder.addr]);
            let d = start_daemon(f.state.path(), f.dl.path(), cfg.clone()).await;

            // invalid destinations are refused, nothing changes
            let err = daemon::request(&d.socket, &Command::SetMoveDir(Some("relative".into()))).await.unwrap_err();
            assert!(err.to_string().contains("absolute"), "{err}");
            assert_eq!(settings(&d).await["move_dir"], "");

            daemon::request(&d.socket, &Command::SetMoveDir(Some(done.display().to_string()))).await.unwrap();
            assert_eq!(settings(&d).await["move_dir"], done.display().to_string());
            let id = d.add_file(&f.torrent_path).await;
            wait_for_path(&done.join("clip.bin")).await;
            d.wait_state(id, "complete").await;
            assert_eq!(fs::read(done.join("clip.bin")).unwrap(), f.data, "content intact after the move");
            assert!(!f.dl.path().join("clip.bin").exists(), "moved, not copied");
            let info = daemon::request(&d.socket, &Command::Info(id)).await.unwrap();
            assert!(info.contains(&format!("directory\t{}", done.display())), "{info:?}");
            d.stop().await;

            // after a restart the torrent is still complete, still in the new place, and not moved again
            let d = start_daemon(f.state.path(), f.dl.path(), cfg).await;
            assert_eq!(d.row(id).await.state, "complete");
            let info = daemon::request(&d.socket, &Command::Info(id)).await.unwrap();
            assert!(info.contains(&format!("directory\t{}", done.display())), "{info:?}");
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn multi_file_torrents_move_as_a_whole_folder() {
    LocalSet::new()
        .run_until(async {
            let files = vec![
                ("disc1/one.bin".to_string(), pseudo_bytes(40_000, 21)),
                ("disc2/two.bin".to_string(), pseudo_bytes(30_000, 22)),
            ];
            let (spec, src) = build_multi("album", &files, 16 * 1024);
            let seeder = Seeder::start(&spec, std::sync::Arc::new(src), Behavior::default()).await;
            let (state, dl, done) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let torrent = state.path().join("album.torrent");
            fs::write(&torrent, &spec.torrent_file).unwrap();
            let d = start_daemon(state.path(), dl.path(), test_config(vec![seeder.addr])).await;
            daemon::request(&d.socket, &Command::SetMoveDir(Some(done.path().display().to_string()))).await.unwrap();
            let id = d.add_file(&torrent).await;
            wait_for_path(&done.path().join("album/disc2/two.bin")).await;
            d.wait_state(id, "complete").await;
            assert_eq!(fs::read(done.path().join("album/disc1/one.bin")).unwrap(), files[0].1);
            assert!(!dl.path().join("album").exists());
            // purge after the move finds the files in their new place
            daemon::request(&d.socket, &Command::Remove { id, purge: true }).await.unwrap();
            assert!(!done.path().join("album").exists());
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn per_torrent_destination_overrides_the_global_one() {
    LocalSet::new()
        .run_until(async {
            let a = fixture(100_000, Behavior::default()).await; // clip.bin
            let data2 = pseudo_bytes(60_000, 9);
            let src2 = std::sync::Arc::new(MemSource(data2.clone()));
            let spec2 = build_single("other.bin", &*src2, 16 * 1024);
            let seeder2 = Seeder::start(&spec2, src2, Behavior::default()).await;
            let path2 = a.state.path().join("other.torrent");
            fs::write(&path2, &spec2.torrent_file).unwrap();
            let (global, special) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let mut cfg = test_config(vec![a.seeder.addr]);
            cfg.extra_peers.push(seeder2.addr);
            let d = start_daemon(a.state.path(), a.dl.path(), cfg).await;
            daemon::request(&d.socket, &Command::SetMoveDir(Some(global.path().display().to_string()))).await.unwrap();

            // torrent 1: its own folder; torrent 2: opts out of moving
            let add = |p: &Path, spec: MoveSpec| Command::Add { source: AddSource::File(p.display().to_string()), dir: None, move_to: spec };
            let r1 = daemon::request(&d.socket, &add(&a.torrent_path, MoveSpec::Dir(special.path().display().to_string()))).await.unwrap();
            let r2 = daemon::request(&d.socket, &add(&path2, MoveSpec::Off)).await.unwrap();
            let (id1, id2): (u32, u32) = (r1[0].parse().unwrap(), r2[0].parse().unwrap());
            wait_for_path(&special.path().join("clip.bin")).await;
            d.wait_state(id2, "complete").await;
            d.wait_state(id1, "complete").await;
            assert!(!global.path().join("clip.bin").exists(), "the torrent's own folder wins over the global one");
            assert_eq!(fs::read(a.dl.path().join("other.bin")).unwrap(), data2, "Off keeps it where it was downloaded");
            assert!(!global.path().join("other.bin").exists());
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn changing_a_completed_torrent_moves_it_now_and_conflicts_are_reported() {
    LocalSet::new()
        .run_until(async {
            let f = fixture(100_000, Behavior::default()).await;
            let (dest, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let d = start_daemon(f.state.path(), f.dl.path(), test_config(vec![f.seeder.addr])).await;
            let id = d.add_file(&f.torrent_path).await; // no move setting: it stays in dl
            d.wait_state(id, "complete").await;
            assert!(f.dl.path().join("clip.bin").exists());

            // something already occupies the destination: nothing is overwritten and the user is told
            fs::write(other.path().join("clip.bin"), "precious").unwrap();
            daemon::request(&d.socket, &Command::SetMove { id, spec: MoveSpec::Dir(other.path().display().to_string()) }).await.unwrap();
            let start = Instant::now();
            let note = loop {
                let info = daemon::request(&d.socket, &Command::Info(id)).await.unwrap();
                if let Some(n) = info.iter().find(|l| l.starts_with("note\t")) {
                    break n.clone();
                }
                assert!(start.elapsed() < Duration::from_secs(10), "no note: {info:?}");
                sleep(Duration::from_millis(25)).await;
            };
            assert!(note.contains("move failed") && note.contains("destination exists"), "{note}");
            assert_eq!(fs::read_to_string(other.path().join("clip.bin")).unwrap(), "precious");
            assert_eq!(fs::read(f.dl.path().join("clip.bin")).unwrap(), f.data, "original untouched");
            assert_eq!(d.row(id).await.state, "complete");

            // a free destination works, and the note goes away
            daemon::request(&d.socket, &Command::SetMove { id, spec: MoveSpec::Dir(dest.path().display().to_string()) }).await.unwrap();
            wait_for_path(&dest.path().join("clip.bin")).await;
            let start = Instant::now();
            loop {
                let info = daemon::request(&d.socket, &Command::Info(id)).await.unwrap();
                if info.contains(&format!("directory\t{}", dest.path().display())) && !info.iter().any(|l| l.starts_with("note\t")) {
                    break;
                }
                assert!(start.elapsed() < Duration::from_secs(10), "{info:?}");
                sleep(Duration::from_millis(25)).await;
            }
            assert!(!f.dl.path().join("clip.bin").exists());

            // bad requests
            for bad in [Command::SetMove { id: 77, spec: MoveSpec::Off }, Command::SetMove { id, spec: MoveSpec::Dir("rel".into()) }] {
                assert!(daemon::request(&d.socket, &bad).await.is_err(), "{bad:?}");
            }
            d.stop().await;
        })
        .await;
}

#[tokio::test]
async fn watch_folder_adds_dropped_torrents_and_deletes_them() {
    LocalSet::new()
        .run_until(async {
            let f = fixture(120_000, Behavior::default()).await;
            let watch = tempfile::tempdir().unwrap();
            let d = start_daemon(f.state.path(), f.dl.path(), test_config(vec![f.seeder.addr])).await;
            assert_eq!(settings(&d).await["watch_dir"], "");

            // a torrent dropped while the feature is off is ignored
            let dropped = watch.path().join("clip.torrent");
            fs::write(&dropped, &f.spec.torrent_file).unwrap();
            sleep(Duration::from_millis(300)).await;
            assert!(d.rows().await.is_empty() && dropped.exists());

            let err = daemon::request(&d.socket, &Command::SetWatchDir(Some("relative".into()))).await.unwrap_err();
            assert!(err.to_string().contains("absolute"), "{err}");

            daemon::request(&d.socket, &Command::SetWatchDir(Some(watch.path().display().to_string()))).await.unwrap();
            assert_eq!(settings(&d).await["watch_dir"], watch.path().display().to_string());
            let start = Instant::now();
            while d.rows().await.is_empty() {
                assert!(start.elapsed() < Duration::from_secs(20), "the dropped torrent was never picked up");
                sleep(Duration::from_millis(100)).await;
            }
            d.wait_state(1, "complete").await;
            assert_eq!(fs::read(f.dl.path().join("clip.bin")).unwrap(), f.data);
            let start = Instant::now();
            while dropped.exists() {
                assert!(start.elapsed() < Duration::from_secs(5), ".torrent was not deleted after being added");
                sleep(Duration::from_millis(50)).await;
            }
            d.stop().await;

            // the setting survives a restart, and turning it off stops the watching
            let d = start_daemon(f.state.path(), f.dl.path(), test_config(vec![f.seeder.addr])).await;
            assert_eq!(settings(&d).await["watch_dir"], watch.path().display().to_string());
            daemon::request(&d.socket, &Command::SetWatchDir(None)).await.unwrap();
            assert_eq!(settings(&d).await["watch_dir"], "");
            let (data2, _) = (pseudo_bytes(10_000, 31), ());
            let spec2 = build_single("late.bin", &MemSource(data2), 16 * 1024);
            let late = watch.path().join("late.torrent");
            fs::write(&late, &spec2.torrent_file).unwrap();
            sleep(Duration::from_secs(3)).await;
            assert!(late.exists() && d.rows().await.len() == 1, "nothing is watched any more");
            d.stop().await;
        })
        .await;
}
