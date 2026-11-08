//! End-to-end tests: the real engine downloads from in-process fake seeders over TCP.

mod common;

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use common::seeder::*;
use common::*;
use wisplet::engine::{Source, State};
use wisplet::magnet::Magnet;
use wisplet::metainfo::Torrent;
use wisplet::util::hex_encode;
use wisplet::Error;

const LIMIT: Duration = Duration::from_secs(30);

fn torrent_source(spec: &TorrentSpec) -> Source {
    Source::Torrent(Torrent::from_bytes(&spec.torrent_file).unwrap())
}

fn single(len: usize, piece_len: u32) -> (TorrentSpec, Arc<MemSource>, Vec<u8>) {
    let data = pseudo_bytes(len, 1);
    let src = Arc::new(MemSource(data.clone()));
    (build_single("movie.bin", &*src, piece_len), src, data)
}

#[tokio::test]
async fn downloads_a_single_file() {
    let (spec, src, data) = single(400_000 + 123, 64 * 1024);
    let seeder = Seeder::start(&spec, src, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();

    let (res, status) = run_download(torrent_source(&spec), test_config(vec![seeder.addr]), dir.path(), LIMIT).await;
    res.unwrap();

    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
    assert_eq!(status.state(), State::Complete);
    assert_eq!(status.done.get(), data.len() as u64);
    assert_eq!(status.total.get(), data.len() as u64);
}

#[tokio::test]
async fn downloads_when_total_is_smaller_than_one_block() {
    let (spec, src, data) = single(1000, 16 * 1024);
    let seeder = Seeder::start(&spec, src, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let (res, _) = run_download(torrent_source(&spec), test_config(vec![seeder.addr]), dir.path(), LIMIT).await;
    res.unwrap();
    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
}

#[tokio::test]
async fn downloads_multi_file_torrent_with_unaligned_boundaries() {
    let files: Vec<(String, Vec<u8>)> = vec![
        ("a.txt".into(), pseudo_bytes(70_001, 2)),
        ("sub/dir/b.bin".into(), pseudo_bytes(33, 3)),
        ("empty".into(), vec![]),
        ("c.bin".into(), pseudo_bytes(150_000, 4)),
    ];
    let (spec, src) = build_multi("album", &files, 32 * 1024);
    let seeder = Seeder::start(&spec, Arc::new(src), Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();

    let (res, _) = run_download(torrent_source(&spec), test_config(vec![seeder.addr]), dir.path(), LIMIT).await;
    res.unwrap();

    for (path, content) in &files {
        let got = fs::read(dir.path().join("album").join(path)).unwrap();
        assert_eq!(&got, content, "{path}");
    }
}

#[tokio::test]
async fn recovers_from_a_corrupt_piece() {
    let (spec, src, data) = single(200_000, 32 * 1024);
    let flag = Arc::new(AtomicBool::new(true));
    let seeder = Seeder::start(
        &spec,
        src,
        Behavior { corrupt_piece0_once: flag.clone(), ..Default::default() },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();

    let (res, _) = run_download(torrent_source(&spec), test_config(vec![seeder.addr]), dir.path(), LIMIT).await;
    res.unwrap();
    assert!(!flag.load(Ordering::SeqCst), "corruption was injected");
    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
    // piece 0 (2 blocks) had to be fetched twice
    let blocks = 200_000usize.div_ceil(16 * 1024);
    assert!(seeder.served() > blocks, "served {} <= {}", seeder.served(), blocks);
}

#[tokio::test]
async fn combines_peers_that_each_have_half_the_pieces() {
    let (spec, src, data) = single(512 * 1024, 32 * 1024); // 16 pieces
    let even: Vec<usize> = (0..16).step_by(2).collect();
    let odd: Vec<usize> = (1..16).step_by(2).collect();
    let a = Seeder::start(&spec, src.clone(), Behavior { have: Some(even), ..Default::default() }).await;
    let b = Seeder::start(&spec, src, Behavior { have: Some(odd), ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();

    let (res, _) = run_download(torrent_source(&spec), test_config(vec![a.addr, b.addr]), dir.path(), LIMIT).await;
    res.unwrap();
    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
    assert!(a.served() > 0 && b.served() > 0);
}

#[tokio::test]
async fn survives_a_peer_that_disconnects_mid_download() {
    let (spec, src, data) = single(600_000, 64 * 1024);
    let flaky = Seeder::start(&spec, src.clone(), Behavior { drop_after_blocks: Some(3), ..Default::default() }).await;
    let solid = Seeder::start(&spec, src, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();

    let (res, _) = run_download(
        torrent_source(&spec),
        test_config(vec![flaky.addr, solid.addr]),
        dir.path(),
        LIMIT,
    )
    .await;
    res.unwrap();
    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
}

#[tokio::test]
async fn handles_choke_and_unchoke_cycles() {
    let (spec, src, data) = single(300_000, 32 * 1024);
    let seeder = Seeder::start(&spec, src, Behavior { choke_after_blocks: Some(4), ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let (res, _) = run_download(torrent_source(&spec), test_config(vec![seeder.addr]), dir.path(), LIMIT).await;
    res.unwrap();
    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
}

#[tokio::test]
async fn works_with_minimal_memory_budget_and_pipeline() {
    let (spec, src, data) = single(300_000, 64 * 1024);
    let seeder = Seeder::start(&spec, src, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(vec![seeder.addr]);
    cfg.memory_budget = 1; // clamps to a single in-flight piece
    cfg.pipeline = 1;
    cfg.pipeline_max = 1;
    cfg.max_peers = 1;
    let (res, _) = run_download(torrent_source(&spec), cfg, dir.path(), LIMIT).await;
    res.unwrap();
    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
}

#[tokio::test]
async fn resume_skips_pieces_already_on_disk() {
    let (spec, src, data) = single(512 * 1024, 32 * 1024); // 16 pieces, 32 blocks
    let seeder = Seeder::start(&spec, src, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();

    // Pre-populate the first half; the rest is zeros (as after an interrupted run).
    let mut partial = vec![0u8; data.len()];
    partial[..256 * 1024].copy_from_slice(&data[..256 * 1024]);
    fs::write(dir.path().join("movie.bin"), &partial).unwrap();

    let (res, status) = run_download(torrent_source(&spec), test_config(vec![seeder.addr]), dir.path(), LIMIT).await;
    res.unwrap();
    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
    assert_eq!(status.state(), State::Complete);
    assert_eq!(seeder.served(), 16, "only the missing 8 pieces (16 blocks) should be fetched");
}

#[tokio::test]
async fn fully_present_data_completes_without_any_peer() {
    let (spec, _src, data) = single(100_000, 32 * 1024);
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("movie.bin"), &data).unwrap();
    let (res, status) = run_download(torrent_source(&spec), test_config(vec![]), dir.path(), Duration::from_secs(5)).await;
    res.unwrap();
    assert_eq!(status.state(), State::Complete);
}

#[tokio::test]
async fn refuses_to_overwrite_a_larger_existing_file() {
    let (spec, src, _data) = single(100_000, 32 * 1024);
    let seeder = Seeder::start(&spec, src, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("movie.bin"), vec![1u8; 500_000]).unwrap();
    let (res, status) = run_download(torrent_source(&spec), test_config(vec![seeder.addr]), dir.path(), LIMIT).await;
    assert!(matches!(res, Err(Error::Failed(_))));
    assert!(matches!(status.state(), State::Error(_)));
    assert_eq!(fs::metadata(dir.path().join("movie.bin")).unwrap().len(), 500_000);
}

#[tokio::test]
async fn ignores_peers_serving_a_different_torrent() {
    let (spec, _src, _) = single(100_000, 32 * 1024);
    let other_data = Arc::new(MemSource(pseudo_bytes(100_000, 99)));
    let other_spec = build_single("other.bin", &*other_data, 32 * 1024);
    let wrong = Seeder::start(&other_spec, other_data, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let (res, _) = run_download(
        torrent_source(&spec),
        test_config(vec![wrong.addr]),
        dir.path(),
        Duration::from_secs(2),
    )
    .await;
    assert!(matches!(res, Err(Error::Timeout(_))), "{res:?}");
    assert_eq!(wrong.served(), 0);
}

#[tokio::test]
async fn magnet_link_fetches_metadata_then_downloads() {
    let (spec, src, data) = single(250_000, 32 * 1024);
    let seeder = Seeder::start(&spec, src, Behavior { serve_metadata: true, ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();

    let uri = format!("magnet:?xt=urn:btih:{}&x.pe={}", hex_encode(&spec.info_hash), seeder.addr);
    let magnet = Magnet::parse(&uri).unwrap();

    let got_meta = std::cell::RefCell::new(None);
    let (res, status) = run_download_with_hook(
        Source::Magnet(magnet),
        test_config(vec![]),
        dir.path(),
        LIMIT,
        &|t, info| *got_meta.borrow_mut() = Some((t.name.clone(), info.to_vec())),
    )
    .await;
    res.unwrap();

    let (name, info) = got_meta.into_inner().expect("metadata hook called");
    assert_eq!(name, "movie.bin");
    assert_eq!(info, spec.info_bytes);
    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
    assert_eq!(&*status.name.borrow(), "movie.bin");
}

#[tokio::test]
async fn magnet_metadata_spanning_many_pieces() {
    // ~600 files with long names -> info dictionary of several 16 KiB metadata pieces
    let files: Vec<(String, Vec<u8>)> = (0..600)
        .map(|i| (format!("directory-with-a-long-name/file-number-{i:04}-with-padding-in-name.dat"), pseudo_bytes(50, i)))
        .collect();
    let (spec, src) = build_multi("bulk", &files, 16 * 1024);
    assert!(spec.info_bytes.len() > 32 * 1024, "info is {} bytes", spec.info_bytes.len());
    let src = Arc::new(src);
    let a = Seeder::start(&spec, src.clone(), Behavior { serve_metadata: true, ..Default::default() }).await;
    let b = Seeder::start(&spec, src, Behavior { serve_metadata: true, ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();

    let uri = format!("magnet:?xt=urn:btih:{}&x.pe={}&x.pe={}", hex_encode(&spec.info_hash), a.addr, b.addr);
    let (res, _) = run_download(Source::Magnet(Magnet::parse(&uri).unwrap()), test_config(vec![]), dir.path(), LIMIT).await;
    res.unwrap();
    for (path, content) in files.iter().step_by(97) {
        assert_eq!(&fs::read(dir.path().join("bulk").join(path)).unwrap(), content);
    }
}

#[tokio::test]
async fn magnet_rejects_metadata_that_does_not_match_the_hash() {
    let (spec, src, _) = single(100_000, 32 * 1024);
    let evil = Seeder::start(
        &spec,
        src,
        Behavior { serve_metadata: true, corrupt_metadata: true, ..Default::default() },
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let uri = format!("magnet:?xt=urn:btih:{}&x.pe={}", hex_encode(&spec.info_hash), evil.addr);
    let mut cfg = test_config(vec![]);
    cfg.metadata_timeout = Duration::from_secs(2);
    let (res, _) = run_download(Source::Magnet(Magnet::parse(&uri).unwrap()), cfg, dir.path(), LIMIT).await;
    assert!(matches!(res, Err(Error::Timeout(_))), "{res:?}");
    assert!(!dir.path().join("movie.bin").exists(), "nothing may be written without verified metadata");
}

#[tokio::test]
async fn magnet_needs_a_peer_that_supports_extensions() {
    let (spec, src, _) = single(100_000, 32 * 1024);
    let plain = Seeder::start(&spec, src, Behavior::default()).await; // serve_metadata = false
    let dir = tempfile::tempdir().unwrap();
    let uri = format!("magnet:?xt=urn:btih:{}&x.pe={}", hex_encode(&spec.info_hash), plain.addr);
    let mut cfg = test_config(vec![]);
    cfg.metadata_timeout = Duration::from_secs(2);
    let (res, _) = run_download(Source::Magnet(Magnet::parse(&uri).unwrap()), cfg, dir.path(), LIMIT).await;
    assert!(matches!(res, Err(Error::Timeout(_))));
}

// ------------------------------------------------------------------ peer rotation & request queue

#[tokio::test]
async fn replaces_a_peer_that_keeps_us_choked_when_others_are_waiting() {
    let (spec, src, data) = single(200_000, 32 * 1024);
    let stuck = Seeder::start(&spec, src.clone(), Behavior { never_unchoke: true, ..Default::default() }).await;
    let good = Seeder::start(&spec, src, Behavior::default()).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(vec![stuck.addr, good.addr]); // the stuck one is tried first
    cfg.max_peers = 1; // one slot: it is only freed if the choking peer is dropped
    cfg.choked_limit = Duration::from_millis(300);
    let started = std::time::Instant::now();
    let (res, _) = run_download(torrent_source(&spec), cfg, dir.path(), LIMIT).await;
    res.unwrap();
    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
    assert_eq!(stuck.served(), 0);
    assert!(good.served() > 0);
    assert!(started.elapsed() < Duration::from_secs(10), "waited for the choking peer: {:?}", started.elapsed());
}

#[tokio::test]
async fn keeps_a_choking_peer_when_nobody_else_is_waiting() {
    let (spec, src, data) = single(100_000, 32 * 1024);
    // unchokes after 1.2 s; with a 0.3 s limit it would be dropped if anyone else could take its slot
    let slow = Seeder::start(&spec, src, Behavior { unchoke_delay_ms: 1200, ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(vec![slow.addr]);
    cfg.choked_limit = Duration::from_millis(300);
    let started = std::time::Instant::now();
    let (res, _) = run_download(torrent_source(&spec), cfg, dir.path(), LIMIT).await;
    res.unwrap();
    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
    assert!(started.elapsed() >= Duration::from_millis(1100), "it really waited");
}

#[tokio::test]
async fn request_queue_grows_for_a_fast_peer_and_stays_fixed_when_asked() {
    // ~0.8 MB/s (one block per 20 ms): fast enough for the adaptive queue to grow after a second
    let (spec, src, data) = single(1_800_000, 32 * 1024);
    let adaptive = Seeder::start(&spec, src.clone(), Behavior { block_delay_ms: 20, ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(vec![adaptive.addr]);
    cfg.pipeline = 2;
    cfg.pipeline_max = 30;
    let (res, _) = run_download(torrent_source(&spec), cfg, dir.path(), LIMIT).await;
    res.unwrap();
    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
    let batch = adaptive.max_batch.load(std::sync::atomic::Ordering::SeqCst);
    assert!((10..=30).contains(&batch), "the queue should have grown from 2 towards 30, saw {batch}");

    let fixed = Seeder::start(&spec, src, Behavior { block_delay_ms: 20, ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(vec![fixed.addr]);
    cfg.pipeline = 3;
    cfg.pipeline_max = 3;
    let (res, _) = run_download(torrent_source(&spec), cfg, dir.path(), LIMIT).await;
    res.unwrap();
    let batch = fixed.max_batch.load(std::sync::atomic::Ordering::SeqCst);
    assert!(batch <= 3, "a fixed queue must not grow, saw {batch}");
}

#[tokio::test]
async fn a_peer_that_stalls_after_a_fast_start_cannot_hoard_the_download() {
    // 60 MB in 32 KiB pieces. The first seeder is very fast for a couple of seconds (so its request
    // queue grows to the ceiling) and then goes silent while still unchoked, holding dozens of
    // blocks. The memory budget is only 32 pieces (64 blocks), so those blocks are most of what
    // can be in flight: without the hoarding guard the second, slower seeder would have to wait
    // for the 60 s request timeout before getting any more work.
    let (spec, src, data) = single(60_000_000, 32 * 1024);
    let stalling = Seeder::start(&spec, src.clone(), Behavior { block_delay_ms: 1, stall_after_blocks: Some(1500), ..Default::default() }).await;
    let steady = Seeder::start(&spec, src, Behavior { block_delay_ms: 5, ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(vec![stalling.addr, steady.addr]);
    cfg.memory_budget = 32 * 32 * 1024;
    cfg.pipeline = 4;
    cfg.pipeline_max = 64;
    cfg.request_timeout = Duration::from_secs(60);
    let started = std::time::Instant::now();
    let (res, _) = run_download(torrent_source(&spec), cfg, dir.path(), Duration::from_secs(55)).await;
    res.unwrap();
    assert_eq!(fs::read(dir.path().join("movie.bin")).unwrap(), data);
    assert!(stalling.served() >= 1500, "the first seeder really did serve its fast start: {}", stalling.served());
    assert!(
        stalling.max_batch.load(std::sync::atomic::Ordering::SeqCst) > 4,
        "its request queue grew beyond the minimum, which is what makes hoarding possible"
    );
    assert!(started.elapsed() < Duration::from_secs(35), "took {:?}: the stalled peer's blocks were not given back", started.elapsed());
    // the guard acted: the surplus requests were cancelled (this is what fails without it)
    let cancelled = stalling.cancels.load(std::sync::atomic::Ordering::SeqCst);
    assert!(cancelled >= 10, "expected the stalled peer's extra requests to be cancelled, saw {cancelled} Cancel messages");
    assert_eq!(steady.cancels.load(std::sync::atomic::Ordering::SeqCst), 0, "a healthy peer is never cancelled");
}
