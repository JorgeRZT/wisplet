//! The download engine: runs one torrent from metadata to completion.
//!
//! Everything runs on a single thread (`LocalSet`), so shared state is plain `Rc`/`RefCell`
//! with no locking. Cancelling the future returned by [`run`] (e.g. on pause) drops the
//! `JoinSet` that owns every peer and discovery task, tearing the whole torrent down.

use std::cell::{Cell, RefCell};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio::time::{sleep, sleep_until, timeout, Instant};

use crate::bitfield::Bitfield;
use crate::config::{Config, CHOKED_PATIENCE};
use crate::discovery::{self, Discovery, Stats};
use crate::error::{Error, Result};
use crate::fetch;
use crate::magnet::Magnet;
use crate::metainfo::Torrent;
use crate::peers::PeerQueue;
use crate::pieces::{BlockOutcome, BlockRequest, PieceTable};
use crate::storage::Storage;
use crate::tracker::Event;
use crate::vlog;
use crate::wire::{self, FrameReader, Message};

const PEER_SILENCE_LIMIT: Duration = Duration::from_secs(150);
const KEEPALIVE_EVERY: Duration = Duration::from_secs(90);
const GOOD_SESSION: Duration = Duration::from_secs(30);
/// Minimum time between status refreshes (progress / rate shown to the user).
const STATUS_INTERVAL: Duration = Duration::from_millis(250);

pub enum Source {
    Torrent(Torrent),
    Magnet(Magnet),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Queued,
    FetchingMetadata,
    Checking,
    Downloading,
    Complete,
    /// Finished downloading; its files are being moved to the "completed" folder.
    Moving,
    Paused,
    Error(String),
}

impl State {
    pub fn label(&self) -> &str {
        match self {
            State::Queued => "queued",
            State::FetchingMetadata => "metadata",
            State::Checking => "checking",
            State::Downloading => "downloading",
            State::Complete => "complete",
            State::Moving => "moving",
            State::Paused => "paused",
            State::Error(_) => "error",
        }
    }
}

/// Live, cheaply readable view of a torrent for UIs / the daemon.
pub struct Status {
    pub info_hash: [u8; 20],
    pub name: RefCell<String>,
    pub state: RefCell<State>,
    pub total: Cell<u64>,
    pub done: Cell<u64>,
    pub peers: Cell<usize>,
    /// Smoothed download rate in bytes/second.
    pub rate: Cell<u64>,
}

impl Status {
    pub fn new(info_hash: [u8; 20], name: &str) -> Rc<Status> {
        Rc::new(Status {
            info_hash,
            name: RefCell::new(name.to_string()),
            state: RefCell::new(State::Queued),
            total: Cell::new(0),
            done: Cell::new(0),
            peers: Cell::new(0),
            rate: Cell::new(0),
        })
    }

    pub fn set_state(&self, s: State) {
        *self.state.borrow_mut() = s;
    }

    pub fn state(&self) -> State {
        self.state.borrow().clone()
    }
}

/// Called once the metadata of a magnet link is known: `(torrent, raw info bytes)`.
pub type MetadataHook<'a> = &'a dyn Fn(&Torrent, &[u8]);

/// Download `source` into `output_dir`. Resolves when the torrent is complete.
/// Must run inside a `LocalSet`.
pub async fn run(
    source: Source,
    cfg: Rc<Config>,
    output_dir: PathBuf,
    status: Rc<Status>,
    on_metadata: MetadataHook<'_>,
) -> Result<()> {
    let result = run_inner(source, cfg, output_dir, &status, on_metadata).await;
    match &result {
        Ok(()) => status.set_state(State::Complete),
        Err(e) => status.set_state(State::Error(e.to_string())),
    }
    result
}

async fn run_inner(
    source: Source,
    cfg: Rc<Config>,
    output_dir: PathBuf,
    status: &Rc<Status>,
    on_metadata: MetadataHook<'_>,
) -> Result<()> {
    let (torrent, initial_peers) = match source {
        Source::Torrent(t) => (t, Vec::new()),
        Source::Magnet(m) => {
            status.set_state(State::FetchingMetadata);
            let (t, info, peers) = fetch::fetch_metadata(&m, &cfg).await?;
            on_metadata(&t, &info);
            (t, peers)
        }
    };
    status.name.replace(torrent.name.clone());
    status.total.set(torrent.total_length);
    download(torrent, cfg, output_dir, status, initial_peers).await
}

struct Shared {
    torrent: Torrent,
    storage: Storage,
    table: RefCell<PieceTable>,
    cfg: Rc<Config>,
    /// Wakes idle peers when new work may exist (`notify_waiters`).
    work: Notify,
    /// Wakes the orchestrator (`notify_one`).
    events: Notify,
    live: Cell<usize>,
    /// Connected peers that currently have us unchoked (diagnostics).
    unchoked: Cell<usize>,
    bytes_received: Cell<u64>,
    fatal: RefCell<Option<String>>,
}

impl Shared {
    fn is_done(&self) -> bool {
        self.table.borrow().is_complete()
    }

    fn on_block(&self, index: u32, begin: u32, data: &[u8]) -> Result<()> {
        self.bytes_received.set(self.bytes_received.get() + data.len() as u64);
        let mut table = self.table.borrow_mut();
        if table.on_block(index, begin, data) != BlockOutcome::PieceComplete {
            return Ok(());
        }
        let piece = table.piece_data(index).expect("piece just completed");
        let ok = self.torrent.verify_piece(index, piece);
        let write = if ok { self.storage.write_piece(index, piece) } else { Ok(()) };
        table.finish_piece(index, ok && write.is_ok());
        drop(table);
        if !ok {
            vlog!("piece {index} failed verification; will retry");
        }
        if let Err(e) = write {
            *self.fatal.borrow_mut() = Some(format!("writing piece {index}: {e}"));
            self.events.notify_one();
            return Err(e.into());
        }
        self.work.notify_waiters();
        self.events.notify_one();
        Ok(())
    }
}

struct LiveGuard(Rc<Shared>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.live.set(self.0.live.get() - 1);
        self.0.events.notify_one();
    }
}

async fn download(
    torrent: Torrent,
    cfg: Rc<Config>,
    output_dir: PathBuf,
    status: &Rc<Status>,
    initial_peers: Vec<SocketAddr>,
) -> Result<()> {
    status.set_state(State::Checking);
    let storage = Storage::create(&output_dir, &torrent)?;
    let max_active = PieceTable::max_active_for(cfg.memory_budget, torrent.piece_length);
    let mut table = PieceTable::new(
        torrent.num_pieces(),
        torrent.piece_length,
        torrent.total_length,
        max_active,
    );

    if storage.has_preexisting() {
        let mut buf = Vec::with_capacity(torrent.piece_length as usize);
        for i in 0..torrent.num_pieces() {
            if storage.check_piece(&torrent, i, &mut buf)? {
                table.mark_have(i);
            }
            if i % 8 == 0 {
                status.done.set(table.downloaded_bytes());
                tokio::task::yield_now().await; // keep the daemon responsive
            }
        }
    }
    status.done.set(table.downloaded_bytes());
    if table.is_complete() {
        return Ok(());
    }

    let discovery = Discovery {
        info_hash: torrent.info_hash,
        peer_id: cfg.peer_id,
        port: cfg.port,
        trackers: torrent.trackers.clone(),
        use_dht: cfg.enable_dht && !torrent.private,
        dht_bootstrap: cfg.dht_bootstrap.clone(),
        min_gap: crate::discovery::MIN_ROUND_GAP,
    };
    let stats = Rc::new(Stats::default());
    let queue = Rc::new(PeerQueue::new());
    queue.add(initial_peers);
    queue.add(cfg.extra_peers.iter().copied());

    let shared = Rc::new(Shared {
        torrent,
        storage,
        table: RefCell::new(table),
        cfg: cfg.clone(),
        work: Notify::new(),
        events: Notify::new(),
        live: Cell::new(0),
        unchoked: Cell::new(0),
        bytes_received: Cell::new(0),
        fatal: RefCell::new(None),
    });

    status.set_state(State::Downloading);
    let mut tasks: JoinSet<()> = JoinSet::new();
    tasks.spawn_local(discovery::run(discovery.clone(), queue.clone(), stats.clone()));

    let mut last_status = Instant::now();
    let mut last_report = Instant::now();
    let mut last_bytes = 0u64;
    loop {
        while tasks.try_join_next().is_some() {}

        if let Some(msg) = shared.fatal.borrow_mut().take() {
            return Err(Error::Failed(msg));
        }

        while shared.live.get() < cfg.max_peers {
            let Some(addr) = queue.pop() else { break };
            shared.live.set(shared.live.get() + 1);
            tasks.spawn_local(peer_task(shared.clone(), addr, queue.clone()));
        }

        let now = Instant::now();
        let done_now = shared.is_done();
        let dt = now.duration_since(last_status);
        if done_now || dt >= STATUS_INTERVAL {
            let table = shared.table.borrow();
            let downloaded = table.downloaded_bytes();
            status.done.set(downloaded);
            stats.downloaded.set(downloaded);
            stats.left.set(shared.torrent.total_length.saturating_sub(downloaded));
            drop(table);
            status.peers.set(shared.live.get());
            let received = shared.bytes_received.get();
            let inst = (received - last_bytes) as f64 / dt.as_secs_f64().max(0.001);
            let prev = status.rate.get() as f64;
            status.rate.set((prev * 0.6 + inst * 0.4) as u64);
            last_bytes = received;
            last_status = now;
            if now.duration_since(last_report) >= Duration::from_secs(10) {
                last_report = now;
                vlog!(
                    "peers: {} connected, {} unchoking us, {} candidates waiting; {}/s",
                    shared.live.get(),
                    shared.unchoked.get(),
                    queue.len(),
                    crate::format::bytes(status.rate.get())
                );
            }
        }

        if done_now {
            break;
        }

        tokio::select! {
            _ = shared.events.notified() => {}
            _ = queue.notify.notified() => {}
            _ = sleep(Duration::from_secs(1)) => {}
        }
    }

    status.rate.set(0);
    status.peers.set(0);
    // Be a good citizen: tell the trackers we finished (bounded to a few seconds).
    let _ = timeout(Duration::from_secs(4), discovery.announce_event(&stats, Event::Completed)).await;
    Ok(())
}

/// Seconds of data a peer's request queue should cover (so a fast or distant peer is never idle
/// waiting for its next request).
const QUEUE_SECONDS: f64 = 3.0;
const BLOCK_BYTES: f64 = 16384.0;
/// A peer holding more than the minimum number of requests that sends nothing for this long is
/// hoarding work other peers could do: the extra requests are cancelled and given back.
const STALL_SHED: Duration = Duration::from_secs(5);

#[derive(Default)]
struct PeerState {
    bf: Option<Bitfield>,
    outstanding: Vec<BlockRequest>,
    meter: RateMeter,
    /// Counted in `Shared::unchoked` while true.
    unchoking: bool,
}

/// Smoothed download rate of one peer, sampled about once a second.
struct RateMeter {
    since: Instant,
    bytes: u64,
    rate: f64,
}

impl Default for RateMeter {
    fn default() -> Self {
        RateMeter { since: Instant::now(), bytes: 0, rate: 0.0 }
    }
}

impl RateMeter {
    fn add(&mut self, n: usize, now: Instant) {
        self.bytes += n as u64;
        let dt = now.duration_since(self.since).as_secs_f64();
        if dt >= 1.0 {
            self.rate = if self.rate == 0.0 { self.bytes as f64 / dt } else { 0.5 * self.rate + 0.5 * (self.bytes as f64 / dt) };
            self.bytes = 0;
            self.since = now;
        }
    }

    /// Forget the measured rate (the peer went quiet), so the queue falls back to its minimum.
    fn reset(&mut self, now: Instant) {
        self.rate = 0.0;
        self.bytes = 0;
        self.since = now;
    }

    /// Requests to keep outstanding: at least `min`, growing with the measured rate up to `max`.
    fn target(&self, min: usize, max: usize) -> usize {
        let want = (self.rate * QUEUE_SECONDS / BLOCK_BYTES).ceil() as usize;
        want.clamp(min, max.max(min))
    }
}

async fn peer_task(sh: Rc<Shared>, addr: SocketAddr, queue: Rc<PeerQueue>) {
    let _guard = LiveGuard(sh.clone());
    let started = Instant::now();
    let mut st = PeerState::default();
    let result = peer_session(&sh, addr, &mut st, &queue).await;
    if st.unchoking {
        sh.unchoked.set(sh.unchoked.get().saturating_sub(1));
    }

    {
        let mut table = sh.table.borrow_mut();
        for r in st.outstanding.drain(..) {
            table.release(&r);
        }
        if let Some(bf) = &st.bf {
            table.peer_lost(bf);
        }
    }
    sh.work.notify_waiters();

    if let Err(e) = &result {
        vlog!("peer {addr}: {e}");
    }
    if started.elapsed() > GOOD_SESSION && !sh.is_done() {
        queue.requeue(addr);
    }
}

fn release_all(sh: &Shared, st: &mut PeerState) {
    if st.outstanding.is_empty() {
        return;
    }
    let mut table = sh.table.borrow_mut();
    for r in st.outstanding.drain(..) {
        table.release(&r);
    }
    drop(table);
    sh.work.notify_waiters();
}

async fn io_step<T>(
    what: &'static str,
    limit: Duration,
    f: impl std::future::Future<Output = std::io::Result<T>>,
) -> Result<T> {
    Ok(timeout(limit, f).await.map_err(|_| Error::Timeout(what))??)
}

async fn peer_session(sh: &Rc<Shared>, addr: SocketAddr, st: &mut PeerState, queue: &PeerQueue) -> Result<()> {
    let cfg = &sh.cfg;
    let n = sh.torrent.num_pieces() as usize;

    let stream = io_step("connect", cfg.connect_timeout, TcpStream::connect(addr)).await?;
    stream.set_nodelay(true)?;
    let (mut rd, mut wr) = stream.into_split();

    wr.write_all(&wire::encode_handshake(&sh.torrent.info_hash, &cfg.peer_id, false)).await?;
    let mut hs = [0u8; wire::HANDSHAKE_LEN];
    io_step("handshake", cfg.connect_timeout, rd.read_exact(&mut hs)).await?;
    let hs = wire::decode_handshake(&hs)?;
    if hs.info_hash != sh.torrent.info_hash {
        return Err(Error::Protocol("info-hash mismatch".into()));
    }
    if hs.peer_id == cfg.peer_id {
        return Err(Error::Protocol("connected to ourselves".into()));
    }

    let mut frames = FrameReader::new(FrameReader::max_len_for(n));
    let mut out = Vec::with_capacity(512);
    Message::Interested.encode_into(&mut out);
    wr.write_all(&out).await?;

    let mut choked = true;
    let mut choked_since = Instant::now();
    let mut choke_check = choked_since + cfg.choked_limit;
    let mut last_recv = Instant::now();
    let mut last_block = Instant::now();
    let mut last_sent = Instant::now();
    let empty = Bitfield::new(n);

    loop {
        while let Some(payload) = frames.try_frame()? {
            last_recv = Instant::now();
            match Message::parse(payload)? {
                Message::Choke => {
                    if st.unchoking {
                        st.unchoking = false;
                        sh.unchoked.set(sh.unchoked.get().saturating_sub(1));
                    }
                    choked = true;
                    choked_since = last_recv;
                    choke_check = choked_since + cfg.choked_limit;
                    release_all(sh, st);
                }
                Message::Unchoke => {
                    choked = false;
                    if !st.unchoking {
                        st.unchoking = true;
                        sh.unchoked.set(sh.unchoked.get() + 1);
                    }
                }
                Message::Have(i) => {
                    let bf = st.bf.get_or_insert_with(|| Bitfield::new(n));
                    if (i as usize) < n && !bf.get(i as usize) {
                        bf.set(i as usize);
                        sh.table.borrow_mut().peer_have(i);
                    }
                }
                Message::Bitfield(bytes) => {
                    let new = Bitfield::from_bytes(bytes, n)
                        .ok_or_else(|| Error::Protocol("invalid bitfield".into()))?;
                    let mut table = sh.table.borrow_mut();
                    if let Some(old) = &st.bf {
                        table.peer_lost(old);
                    }
                    table.peer_has_bitfield(&new);
                    drop(table);
                    st.bf = Some(new);
                }
                Message::Piece { index, begin, data } => {
                    if let Some(pos) = st
                        .outstanding
                        .iter()
                        .position(|r| r.index == index && r.begin == begin)
                    {
                        st.outstanding.swap_remove(pos);
                    }
                    last_block = last_recv;
                    st.meter.add(data.len(), last_recv);
                    sh.on_block(index, begin, data)?;
                }
                // We never upload; ignore requests and everything else.
                _ => {}
            }
        }
        frames.shrink();

        if sh.is_done() {
            return Ok(());
        }
        if let Some(msg) = sh.fatal.borrow().clone() {
            return Err(Error::Failed(msg));
        }

        // Hoarding guard: an adaptive queue can grow on a burst, and then the peer stalls while
        // holding dozens of blocks. Keep the oldest `pipeline` requests, cancel the rest.
        if !choked && st.outstanding.len() > cfg.pipeline && last_block.elapsed() >= STALL_SHED {
            let extra = st.outstanding.split_off(cfg.pipeline);
            out.clear();
            {
                let mut table = sh.table.borrow_mut();
                for r in &extra {
                    Message::Cancel { index: r.index, begin: r.begin, length: r.length }.encode_into(&mut out);
                    table.release(r);
                }
            } // the table is not borrowed across the write below
            wr.write_all(&out).await?;
            last_sent = Instant::now();
            st.meter.reset(Instant::now());
            vlog!("peer {addr}: stalled, gave back {} requests", extra.len());
            sh.work.notify_waiters();
        }

        if !choked {
            out.clear();
            let had_none = st.outstanding.is_empty();
            let bf = st.bf.as_ref().unwrap_or(&empty);
            let target = st.meter.target(cfg.pipeline, cfg.pipeline_max);
            while st.outstanding.len() < target {
                let next = sh.table.borrow_mut().next_request(bf);
                match next {
                    Some(r) if !st.outstanding.contains(&r) => {
                        Message::Request { index: r.index, begin: r.begin, length: r.length }
                            .encode_into(&mut out);
                        st.outstanding.push(r);
                    }
                    _ => break,
                }
            }
            if !out.is_empty() {
                if had_none {
                    last_block = Instant::now();
                }
                wr.write_all(&out).await?;
                last_sent = Instant::now();
            }
        }

        let mut deadline = last_recv + PEER_SILENCE_LIMIT;
        deadline = deadline.min(last_sent + KEEPALIVE_EVERY);
        if !st.outstanding.is_empty() {
            deadline = deadline.min(last_block + cfg.request_timeout);
            if !choked && st.outstanding.len() > cfg.pipeline {
                deadline = deadline.min(last_block + STALL_SHED);
            }
        }
        if choked {
            deadline = deadline.min(choke_check);
        }

        tokio::select! {
            read = frames.fill(&mut rd) => {
                if read? == 0 {
                    return Err(Error::Protocol("peer closed connection".into()));
                }
            }
            _ = sh.work.notified() => {}
            _ = sleep_until(deadline) => {
                let now = Instant::now();
                if now.duration_since(last_recv) >= PEER_SILENCE_LIMIT {
                    return Err(Error::Timeout("peer went silent"));
                }
                if !st.outstanding.is_empty()
                    && now.duration_since(last_block) >= cfg.request_timeout
                {
                    return Err(Error::Timeout("block request"));
                }
                if choked && now >= choke_check {
                    // Without reciprocity (we never upload) most peers choke us. Free the slot for
                    // someone else when others are waiting; otherwise keep hoping, within reason.
                    if !queue.is_empty() || now.duration_since(choked_since) >= CHOKED_PATIENCE {
                        return Err(Error::Timeout("peer keeps us choked"));
                    }
                    choke_check = now + Duration::from_secs(5);
                }
                if now.duration_since(last_sent) >= KEEPALIVE_EVERY {
                    out.clear();
                    Message::KeepAlive.encode_into(&mut out);
                    wr.write_all(&out).await?;
                    last_sent = now;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_queue_grows_with_the_measured_rate_within_its_bounds() {
        let t0 = Instant::now();
        let mut m = RateMeter { since: t0, bytes: 0, rate: 0.0 };
        assert_eq!(m.target(10, 64), 10, "unknown rate: the minimum");
        // 1 MiB/s for one second -> 3 s of data = 192 blocks, clamped to the ceiling
        m.add(1 << 20, t0 + Duration::from_secs(1));
        assert_eq!(m.target(10, 64), 64);
        assert_eq!(m.target(10, 100), 100);
        // a slow peer stays near the minimum: 20 KiB/s * 3 s = 4 blocks
        let mut slow = RateMeter { since: t0, bytes: 0, rate: 0.0 };
        slow.add(20 * 1024, t0 + Duration::from_secs(1));
        assert_eq!(slow.target(10, 64), 10);
        // a ceiling below the minimum never shrinks the queue
        assert_eq!(m.target(10, 4), 10);
        // the rate is smoothed, not replaced: one idle second halves it
        let mut avg = RateMeter { since: t0, bytes: 0, rate: 1_000_000.0 };
        avg.add(0, t0 + Duration::from_secs(1));
        assert!((avg.rate - 500_000.0).abs() < 1.0);
        // a stalled peer falls back to the minimum queue
        let mut quiet = RateMeter { since: t0, bytes: 0, rate: 2_000_000.0 };
        assert_eq!(quiet.target(10, 64), 64);
        quiet.reset(t0 + Duration::from_secs(9));
        assert_eq!(quiet.target(10, 64), 10);
        // bytes that arrive within the same second are accumulated, not lost
        let mut acc = RateMeter { since: t0, bytes: 0, rate: 0.0 };
        acc.add(1000, t0 + Duration::from_millis(300));
        acc.add(1000, t0 + Duration::from_millis(600));
        assert_eq!(acc.bytes, 2000);
        acc.add(1000, t0 + Duration::from_secs(2));
        assert_eq!((acc.bytes, acc.rate as u64), (0, 1500));
    }
}
