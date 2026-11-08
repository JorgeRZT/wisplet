//! In-process fake BitTorrent seeder used by integration tests and the benchmark example.
//! It can inject faults (corrupt piece, dropped connections, choke cycles, bad metadata).
#![allow(dead_code)]

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sha1::{Digest, Sha1};
use wisplet::bencode::{self, Value};
use wisplet::bitfield::Bitfield;
use wisplet::metadata::{self, MetadataMsg};
use wisplet::metainfo::build_torrent_file;
use wisplet::wire::{self, FrameReader, Message};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::{sleep, sleep_until, Instant};

pub trait Content: Send + Sync + 'static {
    fn len(&self) -> u64;
    fn read_at(&self, offset: u64, buf: &mut [u8]);
}

pub struct MemSource(pub Vec<u8>);

impl Content for MemSource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) {
        buf.copy_from_slice(&self.0[offset as usize..offset as usize + buf.len()]);
    }
}

/// Deterministic pseudo-random content generated on the fly (no RAM for big payloads).
pub struct PatternSource(pub u64);

pub fn pattern_byte(i: u64) -> u8 {
    (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8
}

impl Content for PatternSource {
    fn len(&self) -> u64 {
        self.0
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) {
        for (k, b) in buf.iter_mut().enumerate() {
            *b = pattern_byte(offset + k as u64);
        }
    }
}

pub fn pseudo_bytes(len: usize, seed: u64) -> Vec<u8> {
    (0..len as u64).map(|i| pattern_byte(i + seed * 7919)).collect()
}

pub struct TorrentSpec {
    pub info_hash: [u8; 20],
    pub info_bytes: Vec<u8>,
    pub torrent_file: Vec<u8>,
    pub piece_length: u32,
    pub num_pieces: usize,
    pub total_length: u64,
}

fn piece_hashes(src: &dyn Content, piece_length: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; piece_length as usize];
    let mut off = 0u64;
    while off < src.len() {
        let n = (src.len() - off).min(piece_length as u64) as usize;
        src.read_at(off, &mut buf[..n]);
        out.extend_from_slice(&Sha1::digest(&buf[..n]));
        off += n as u64;
    }
    out
}

fn finish_spec(info: Value, src: &dyn Content, piece_length: u32) -> TorrentSpec {
    let info_bytes = bencode::encode(&info);
    let info_hash: [u8; 20] = Sha1::digest(&info_bytes).into();
    TorrentSpec {
        info_hash,
        torrent_file: build_torrent_file(&info_bytes, &[]),
        info_bytes,
        piece_length,
        num_pieces: src.len().div_ceil(piece_length as u64) as usize,
        total_length: src.len(),
    }
}

/// Single-file torrent named `name`.
pub fn build_single(name: &str, src: &dyn Content, piece_length: u32) -> TorrentSpec {
    let info = Value::dict([
        ("name", name.into()),
        ("length", (src.len() as i64).into()),
        ("piece length", (piece_length as i64).into()),
        ("pieces", Value::Bytes(piece_hashes(src, piece_length))),
    ]);
    finish_spec(info, src, piece_length)
}

/// Multi-file torrent; `files` are `(relative path with '/', content)`. Returns the spec
/// plus the concatenated payload as a source.
pub fn build_multi(name: &str, files: &[(String, Vec<u8>)], piece_length: u32) -> (TorrentSpec, MemSource) {
    let payload: Vec<u8> = files.iter().flat_map(|(_, c)| c.iter().copied()).collect();
    let src = MemSource(payload);
    let list: Vec<Value> = files
        .iter()
        .map(|(p, c)| {
            Value::dict([
                ("length", (c.len() as i64).into()),
                ("path", Value::List(p.split('/').map(Value::from).collect())),
            ])
        })
        .collect();
    let info = Value::dict([
        ("name", name.into()),
        ("files", list.into()),
        ("piece length", (piece_length as i64).into()),
        ("pieces", Value::Bytes(piece_hashes(&src, piece_length))),
    ]);
    (finish_spec(info, &src, piece_length), src)
}

#[derive(Clone, Default)]
pub struct Behavior {
    /// Advertise BEP 10 and serve `ut_metadata`.
    pub serve_metadata: bool,
    /// Serve garbage metadata (hash will not match).
    pub corrupt_metadata: bool,
    /// Pieces this seeder owns; `None` = all.
    pub have: Option<Vec<usize>>,
    /// Flip a byte in the first block of piece 0, once.
    pub corrupt_piece0_once: Arc<AtomicBool>,
    /// Close each connection after serving this many blocks.
    pub drop_after_blocks: Option<usize>,
    /// Choke for 150 ms (discarding requests) after this many blocks served.
    pub choke_after_blocks: Option<usize>,
    /// Sleep this long before answering each request (throttles the transfer).
    pub block_delay_ms: u64,
    /// Never unchoke the client (requests are ignored for ever).
    pub never_unchoke: bool,
    /// Keep the client choked this long after connecting, then unchoke.
    pub unchoke_delay_ms: u64,
    /// After serving this many blocks, stay connected and unchoked but answer nothing more.
    pub stall_after_blocks: Option<usize>,
}

pub struct Seeder {
    pub addr: SocketAddr,
    pub blocks_served: Arc<AtomicUsize>,
    /// Largest number of requests that arrived together in one read: a proxy for the client's
    /// request queue length.
    pub max_batch: Arc<AtomicUsize>,
    /// `Cancel` messages received from clients.
    pub cancels: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl Drop for Seeder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Seeder {
    pub async fn start(spec: &TorrentSpec, src: Arc<dyn Content>, behavior: Behavior) -> Seeder {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let blocks_served = Arc::new(AtomicUsize::new(0));
        let max_batch = Arc::new(AtomicUsize::new(0));
        let cancels = Arc::new(AtomicUsize::new(0));
        let ctx = Arc::new(Ctx {
            info_hash: spec.info_hash,
            info_bytes: spec.info_bytes.clone(),
            piece_length: spec.piece_length as u64,
            num_pieces: spec.num_pieces,
            behavior,
            counter: blocks_served.clone(),
            max_batch: max_batch.clone(),
            cancels: cancels.clone(),
        });
        let task = tokio::spawn(async move {
            loop {
                let Ok((s, _)) = listener.accept().await else { return };
                let (ctx, src) = (ctx.clone(), src.clone());
                tokio::spawn(async move {
                    let _ = serve(s, ctx, src).await;
                });
            }
        });
        Seeder { addr, blocks_served, max_batch, cancels, task }
    }

    pub fn served(&self) -> usize {
        self.blocks_served.load(Ordering::SeqCst)
    }
}

struct Ctx {
    info_hash: [u8; 20],
    info_bytes: Vec<u8>,
    piece_length: u64,
    num_pieces: usize,
    behavior: Behavior,
    counter: Arc<AtomicUsize>,
    max_batch: Arc<AtomicUsize>,
    cancels: Arc<AtomicUsize>,
}

fn invalid(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

async fn serve(mut s: TcpStream, ctx: Arc<Ctx>, src: Arc<dyn Content>) -> io::Result<()> {
    const OUR_UT: u8 = 7;
    let b = &ctx.behavior;
    let mut hs = [0u8; wire::HANDSHAKE_LEN];
    s.read_exact(&mut hs).await?;
    let remote = wire::decode_handshake(&hs).map_err(invalid)?;
    if remote.info_hash != ctx.info_hash {
        return Ok(());
    }
    s.write_all(&wire::encode_handshake(&ctx.info_hash, b"SEEDERSEEDERSEEDER!!", b.serve_metadata))
        .await?;

    let mut out = Vec::new();
    if b.serve_metadata && remote.supports_extensions {
        let hs = bencode::encode(&Value::dict([
            ("m", Value::dict([("ut_metadata", (OUR_UT as i64).into())])),
            ("metadata_size", (ctx.info_bytes.len() as i64).into()),
        ]));
        Message::Extended { id: 0, payload: &hs }.encode_into(&mut out);
    }
    let mut bf = Bitfield::new(ctx.num_pieces);
    match &b.have {
        None => (0..ctx.num_pieces).for_each(|i| bf.set(i)),
        Some(list) => list.iter().for_each(|&i| bf.set(i)),
    }
    Message::Bitfield(bf.as_bytes()).encode_into(&mut out);
    if !b.never_unchoke && b.unchoke_delay_ms == 0 {
        Message::Unchoke.encode_into(&mut out);
    }
    s.write_all(&out).await?;

    let mut fr = FrameReader::new(1 << 16);
    let mut remote_ut: Option<u8> = None;
    let mut served = 0usize;
    let mut choked_until: Option<Instant> = if b.never_unchoke {
        Some(Instant::now() + Duration::from_secs(24 * 3600))
    } else if b.unchoke_delay_ms > 0 {
        Some(Instant::now() + Duration::from_millis(b.unchoke_delay_ms))
    } else {
        None
    };
    let total = src.len();

    loop {
        let mut batch = 0usize;
        while let Some(payload) = fr.try_frame().map_err(invalid)? {
            match Message::parse(payload).map_err(invalid)? {
                Message::Request { index, begin, length } => {
                    batch += 1;
                    ctx.max_batch.fetch_max(batch, Ordering::SeqCst);
                    if choked_until.is_some() || !bf.get(index as usize) {
                        continue;
                    }
                    if b.stall_after_blocks.is_some_and(|n| served >= n) {
                        continue; // silent: the request is swallowed
                    }
                    let off = index as u64 * ctx.piece_length + begin as u64;
                    if off + length as u64 > total {
                        return Err(invalid("request out of range"));
                    }
                    if b.block_delay_ms > 0 {
                        sleep(Duration::from_millis(b.block_delay_ms)).await;
                    }
                    let mut data = vec![0u8; length as usize];
                    src.read_at(off, &mut data);
                    if index == 0 && begin == 0 && b.corrupt_piece0_once.swap(false, Ordering::SeqCst) {
                        data[0] ^= 0xff;
                    }
                    out.clear();
                    Message::Piece { index, begin, data: &data }.encode_into(&mut out);
                    s.write_all(&out).await?;
                    served += 1;
                    ctx.counter.fetch_add(1, Ordering::SeqCst);
                    if b.drop_after_blocks == Some(served) {
                        return Ok(());
                    }
                    if b.choke_after_blocks == Some(served) {
                        out.clear();
                        Message::Choke.encode_into(&mut out);
                        s.write_all(&out).await?;
                        choked_until = Some(Instant::now() + Duration::from_millis(150));
                    }
                }
                Message::Cancel { .. } => {
                    ctx.cancels.fetch_add(1, Ordering::SeqCst);
                }
                Message::Extended { id: 0, payload } => {
                    remote_ut = metadata::decode_ext_handshake(payload).map_err(invalid)?.ut_metadata;
                }
                Message::Extended { id, payload } if id == OUR_UT => {
                    let (Some(rid), MetadataMsg::Request { piece }) =
                        (remote_ut, metadata::decode_message(payload).map_err(invalid)?)
                    else {
                        continue;
                    };
                    let start = piece as usize * metadata::METADATA_PIECE_SIZE;
                    let reply = if start >= ctx.info_bytes.len() {
                        metadata::encode_reject(piece)
                    } else {
                        let end = (start + metadata::METADATA_PIECE_SIZE).min(ctx.info_bytes.len());
                        let mut chunk = ctx.info_bytes[start..end].to_vec();
                        if b.corrupt_metadata {
                            chunk[0] ^= 0xff;
                        }
                        metadata::encode_data(piece, ctx.info_bytes.len(), &chunk)
                    };
                    out.clear();
                    Message::Extended { id: rid, payload: &reply }.encode_into(&mut out);
                    s.write_all(&out).await?;
                }
                _ => {}
            }
        }

        let timer = async {
            match choked_until {
                Some(t) => sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            n = fr.fill(&mut s) => { if n? == 0 { return Ok(()); } }
            _ = timer => {
                choked_until = None;
                out.clear();
                Message::Unchoke.encode_into(&mut out);
                s.write_all(&out).await?;
            }
        }
    }
}

/// Convenience used by examples: sleep helper so the module stays self-contained.
pub async fn pause(ms: u64) {
    sleep(Duration::from_millis(ms)).await;
}
