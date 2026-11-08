//! Magnet links: obtain the info dictionary from peers via `ut_metadata` (BEP 9).

use std::cell::{Cell, RefCell};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout, Instant};

use crate::config::Config;
use crate::discovery::{self, Discovery, Stats};
use crate::error::{Error, Result};
use crate::magnet::Magnet;
use crate::metadata::{self, MetadataAssembler, MetadataMsg, OUR_UT_METADATA_ID};
use crate::metainfo::Torrent;
use crate::peers::PeerQueue;
use crate::vlog;
use crate::wire::{self, FrameReader, Message};

/// Concurrent peers used while hunting for metadata. Cheap (no piece buffers) and most
/// DHT-sourced peers are dead or lack the data, so we try many in parallel.
const MAX_META_PEERS: usize = 24;
const PEER_STEP_TIMEOUT: Duration = Duration::from_secs(20);

struct MetaShared {
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    assembler: RefCell<Option<MetadataAssembler>>,
    done: RefCell<Option<Vec<u8>>>,
    notify: Notify,
    live: Cell<usize>,
    connect_timeout: Duration,
}

/// Fetch and verify the torrent metadata for `magnet`.
///
/// Returns the parsed [`Torrent`], the raw info bytes (so callers can persist a
/// `.torrent`), and every peer address discovered meanwhile (to seed the download).
/// Must run inside a `LocalSet`.
pub async fn fetch_metadata(
    magnet: &Magnet,
    cfg: &Config,
) -> Result<(Torrent, Vec<u8>, Vec<SocketAddr>)> {
    let queue = Rc::new(PeerQueue::new());
    queue.add(magnet.peers.iter().copied());
    queue.add(cfg.extra_peers.iter().copied());

    let shared = Rc::new(MetaShared {
        info_hash: magnet.info_hash,
        peer_id: cfg.peer_id,
        assembler: RefCell::new(None),
        done: RefCell::new(None),
        notify: Notify::new(),
        live: Cell::new(0),
        connect_timeout: cfg.connect_timeout,
    });

    let mut tasks: JoinSet<()> = JoinSet::new();
    let stats = Rc::new(Stats::default());
    stats.left.set(1);
    tasks.spawn_local(discovery::run(
        Discovery {
            info_hash: magnet.info_hash,
            peer_id: cfg.peer_id,
            port: cfg.port,
            trackers: magnet.trackers.clone(),
            use_dht: cfg.enable_dht,
            dht_bootstrap: cfg.dht_bootstrap.clone(),
            min_gap: crate::discovery::MIN_ROUND_GAP,
        },
        queue.clone(),
        stats,
    ));

    let deadline = Instant::now() + cfg.metadata_timeout;
    loop {
        while tasks.try_join_next().is_some() {}

        if let Some(info) = shared.done.borrow_mut().take() {
            let torrent = Torrent::from_info(&info, magnet.trackers.clone())?;
            return Ok((torrent, info, queue.seen()));
        }

        while shared.live.get() < MAX_META_PEERS {
            let Some(addr) = queue.pop() else { break };
            shared.live.set(shared.live.get() + 1);
            tasks.spawn_local(meta_peer_task(shared.clone(), addr));
        }

        tokio::select! {
            _ = shared.notify.notified() => {}
            _ = queue.notify.notified() => {}
            _ = sleep(Duration::from_secs(2)) => {}
            _ = tokio::time::sleep_until(deadline) => {
                return Err(Error::Timeout("fetching magnet metadata (no peer provided it)"));
            }
        }
    }
}

async fn meta_peer_task(sh: Rc<MetaShared>, addr: SocketAddr) {
    let mut claimed: Option<u32> = None;
    if let Err(e) = meta_peer(&sh, addr, &mut claimed).await {
        vlog!("metadata peer {addr}: {e}");
    }
    if let (Some(piece), Some(a)) = (claimed, sh.assembler.borrow_mut().as_mut()) {
        a.unclaim(piece);
    }
    sh.live.set(sh.live.get() - 1);
    sh.notify.notify_one();
}

async fn step<T>(what: &'static str, limit: Duration, f: impl std::future::Future<Output = std::io::Result<T>>) -> Result<T> {
    Ok(timeout(limit, f).await.map_err(|_| Error::Timeout(what))??)
}

async fn meta_peer(sh: &MetaShared, addr: SocketAddr, claimed: &mut Option<u32>) -> Result<()> {
    let stream = step("connect", sh.connect_timeout, TcpStream::connect(addr)).await?;
    stream.set_nodelay(true)?;
    let (mut rd, mut wr) = stream.into_split();

    wr.write_all(&wire::encode_handshake(&sh.info_hash, &sh.peer_id, true)).await?;
    let mut hs = [0u8; wire::HANDSHAKE_LEN];
    step("handshake", sh.connect_timeout, rd.read_exact(&mut hs)).await?;
    let hs = wire::decode_handshake(&hs)?;
    if hs.info_hash != sh.info_hash {
        return Err(Error::Protocol("info-hash mismatch".into()));
    }
    if !hs.supports_extensions {
        return Err(Error::Protocol("peer lacks extension protocol".into()));
    }

    let mut out = Vec::new();
    Message::Extended { id: 0, payload: &metadata::encode_ext_handshake() }.encode_into(&mut out);
    wr.write_all(&out).await?;

    let mut frames = FrameReader::new(wire::BLOCK_SIZE as usize + 1024);
    let mut remote_id: Option<u8> = None;

    loop {
        while let Some(payload) = frames.try_frame()? {
            let Message::Extended { id, payload } = Message::parse(payload)? else { continue };
            if id == 0 {
                let h = metadata::decode_ext_handshake(payload)?;
                let (Some(rid), Some(size)) = (h.ut_metadata, h.metadata_size) else {
                    return Err(Error::Protocol("peer does not serve metadata".into()));
                };
                let mut slot = sh.assembler.borrow_mut();
                match slot.as_ref() {
                    None => *slot = Some(MetadataAssembler::new(size)?),
                    Some(a) if a.size() != size => {
                        return Err(Error::Protocol("peers disagree on metadata size".into()))
                    }
                    _ => {}
                }
                remote_id = Some(rid);
            } else if id == OUR_UT_METADATA_ID {
                match metadata::decode_message(payload)? {
                    MetadataMsg::Data { piece, data, .. } => {
                        let mut slot = sh.assembler.borrow_mut();
                        let a = slot.as_mut().ok_or_else(|| Error::Protocol("metadata reset".into()))?;
                        if a.add_piece(piece, data)? {
                            let a = slot.take().unwrap();
                            drop(slot);
                            match a.finish(&sh.info_hash) {
                                Ok(info) => {
                                    *sh.done.borrow_mut() = Some(info);
                                    sh.notify.notify_one();
                                    *claimed = None;
                                    return Ok(());
                                }
                                Err(e) => return Err(e), // slot stays empty: restart with another peer
                            }
                        }
                        if *claimed == Some(piece) {
                            *claimed = None;
                        }
                    }
                    MetadataMsg::Reject { .. } => {
                        return Err(Error::Protocol("peer rejected metadata request".into()))
                    }
                    MetadataMsg::Request { piece } => {
                        // We have nothing to give; politely refuse.
                        let (rid, body) = (remote_id.unwrap_or(0), metadata::encode_reject(piece));
                        out.clear();
                        Message::Extended { id: rid, payload: &body }.encode_into(&mut out);
                        wr.write_all(&out).await?;
                    }
                }
            }
        }

        if sh.done.borrow().is_some() {
            return Ok(());
        }

        // Ask for the next missing piece, one at a time per peer.
        if let (Some(rid), None) = (remote_id, *claimed) {
            let next = sh.assembler.borrow_mut().as_mut().and_then(|a| a.claim_next());
            if let Some(piece) = next {
                *claimed = Some(piece);
                out.clear();
                Message::Extended { id: rid, payload: &metadata::encode_request(piece) }.encode_into(&mut out);
                wr.write_all(&out).await?;
            } else if sh.assembler.borrow().is_none() {
                return Err(Error::Protocol("metadata reset".into()));
            }
            // else: every piece is claimed by other peers; keep waiting for completion
        }

        let n = tokio::select! {
            r = timeout(PEER_STEP_TIMEOUT, frames.fill(&mut rd)) => {
                r.map_err(|_| Error::Timeout("metadata exchange"))??
            }
            _ = sh.notify.notified() => {
                if sh.done.borrow().is_some() { return Ok(()); }
                continue;
            }
        };
        if n == 0 {
            return Err(Error::Protocol("peer closed connection".into()));
        }
    }
}
