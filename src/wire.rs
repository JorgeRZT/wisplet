//! BitTorrent peer wire protocol (BEP 3 + BEP 10 framing).
//!
//! Messages borrow their payloads from the receive buffer: a 16 KiB `piece` message is
//! parsed without any allocation and copied exactly once, into the piece buffer.
//!
//! [`FrameReader`] is *cancellation safe*: all partial state lives in the struct, and the
//! only await point is a plain `AsyncRead::read`, so it can sit in a `select!`.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::{Error, Result};

pub const BLOCK_SIZE: u32 = 16 * 1024;
pub const HANDSHAKE_LEN: usize = 68;
const PROTOCOL: &[u8; 19] = b"BitTorrent protocol";
/// Reserved byte 5, bit 0x10 advertises the extension protocol (BEP 10).
const EXT_BIT: u8 = 0x10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub supports_extensions: bool,
}

pub fn encode_handshake(
    info_hash: &[u8; 20],
    peer_id: &[u8; 20],
    extensions: bool,
) -> [u8; HANDSHAKE_LEN] {
    let mut out = [0u8; HANDSHAKE_LEN];
    out[0] = 19;
    out[1..20].copy_from_slice(PROTOCOL);
    if extensions {
        out[25] |= EXT_BIT;
    }
    out[28..48].copy_from_slice(info_hash);
    out[48..68].copy_from_slice(peer_id);
    out
}

pub fn decode_handshake(buf: &[u8; HANDSHAKE_LEN]) -> Result<Handshake> {
    if buf[0] != 19 || &buf[1..20] != PROTOCOL {
        return Err(Error::Protocol("not a BitTorrent handshake".into()));
    }
    Ok(Handshake {
        info_hash: buf[28..48].try_into().unwrap(),
        peer_id: buf[48..68].try_into().unwrap(),
        supports_extensions: buf[25] & EXT_BIT != 0,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message<'a> {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(&'a [u8]),
    Request { index: u32, begin: u32, length: u32 },
    Piece { index: u32, begin: u32, data: &'a [u8] },
    Cancel { index: u32, begin: u32, length: u32 },
    Port(u16),
    Extended { id: u8, payload: &'a [u8] },
    /// A message id we do not understand; safely skipped.
    Unknown(u8),
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

impl<'a> Message<'a> {
    /// Parse a frame payload (the bytes *after* the 4-byte length prefix).
    pub fn parse(payload: &'a [u8]) -> Result<Message<'a>> {
        let Some((&id, body)) = payload.split_first() else {
            return Ok(Message::KeepAlive);
        };
        let fixed = |n: usize| -> Result<()> {
            if body.len() == n {
                Ok(())
            } else {
                Err(Error::Protocol(format!(
                    "message {id} has wrong length {}",
                    body.len() + 1
                )))
            }
        };
        Ok(match id {
            0 => {
                fixed(0)?;
                Message::Choke
            }
            1 => {
                fixed(0)?;
                Message::Unchoke
            }
            2 => {
                fixed(0)?;
                Message::Interested
            }
            3 => {
                fixed(0)?;
                Message::NotInterested
            }
            4 => {
                fixed(4)?;
                Message::Have(be32(body))
            }
            5 => Message::Bitfield(body),
            6 | 8 => {
                fixed(12)?;
                let (index, begin, length) = (be32(body), be32(&body[4..]), be32(&body[8..]));
                if id == 6 {
                    Message::Request { index, begin, length }
                } else {
                    Message::Cancel { index, begin, length }
                }
            }
            7 => {
                if body.len() < 8 {
                    return Err(Error::Protocol("piece message too short".into()));
                }
                Message::Piece {
                    index: be32(body),
                    begin: be32(&body[4..]),
                    data: &body[8..],
                }
            }
            9 => {
                fixed(2)?;
                Message::Port(u16::from_be_bytes([body[0], body[1]]))
            }
            20 => {
                if body.is_empty() {
                    return Err(Error::Protocol("extended message too short".into()));
                }
                Message::Extended {
                    id: body[0],
                    payload: &body[1..],
                }
            }
            other => Message::Unknown(other),
        })
    }

    /// Serialise with the 4-byte length prefix, appending to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        fn frame(out: &mut Vec<u8>, id: u8, parts: &[&[u8]]) {
            let len: usize = 1 + parts.iter().map(|p| p.len()).sum::<usize>();
            out.extend_from_slice(&(len as u32).to_be_bytes());
            out.push(id);
            for p in parts {
                out.extend_from_slice(p);
            }
        }
        match self {
            Message::KeepAlive => out.extend_from_slice(&[0, 0, 0, 0]),
            Message::Choke => frame(out, 0, &[]),
            Message::Unchoke => frame(out, 1, &[]),
            Message::Interested => frame(out, 2, &[]),
            Message::NotInterested => frame(out, 3, &[]),
            Message::Have(i) => frame(out, 4, &[&i.to_be_bytes()]),
            Message::Bitfield(b) => frame(out, 5, &[b]),
            Message::Request { index, begin, length } => frame(
                out,
                6,
                &[&index.to_be_bytes(), &begin.to_be_bytes(), &length.to_be_bytes()],
            ),
            Message::Cancel { index, begin, length } => frame(
                out,
                8,
                &[&index.to_be_bytes(), &begin.to_be_bytes(), &length.to_be_bytes()],
            ),
            Message::Piece { index, begin, data } => {
                frame(out, 7, &[&index.to_be_bytes(), &begin.to_be_bytes(), data])
            }
            Message::Port(p) => frame(out, 9, &[&p.to_be_bytes()]),
            Message::Extended { id, payload } => frame(out, 20, &[&[*id], payload]),
            Message::Unknown(id) => frame(out, *id, &[]),
        }
    }
}

/// Incremental, cancellation-safe length-prefixed frame reader.
pub struct FrameReader {
    buf: Vec<u8>,
    start: usize,
    end: usize,
    max_len: usize,
}

impl FrameReader {
    /// `max_len` bounds the size of an accepted frame payload; larger frames are a
    /// protocol error (prevents a peer from making us allocate gigabytes).
    pub fn new(max_len: usize) -> Self {
        FrameReader {
            buf: vec![0; 4 + 16 * 1024 + 16],
            start: 0,
            end: 0,
            max_len,
        }
    }

    /// Largest frame we expect for a torrent with `num_pieces` pieces: either a block
    /// (plus headers / BEP 9 data) or the bitfield, whichever is bigger.
    pub fn max_len_for(num_pieces: usize) -> usize {
        (BLOCK_SIZE as usize + 1024).max(5 + num_pieces.div_ceil(8))
    }

    /// Return the next complete frame payload, if one is fully buffered.
    pub fn try_frame(&mut self) -> Result<Option<&[u8]>> {
        let avail = self.end - self.start;
        if avail < 4 {
            return Ok(None);
        }
        let len = be32(&self.buf[self.start..]) as usize;
        if len > self.max_len {
            return Err(Error::Protocol(format!("frame of {len} bytes exceeds limit")));
        }
        if avail < 4 + len {
            return Ok(None);
        }
        let from = self.start + 4;
        self.start = from + len;
        Ok(Some(&self.buf[from..from + len]))
    }

    /// Read more bytes from `r`. Returns the number read (`0` = EOF). Cancellation safe.
    pub async fn fill<R: AsyncRead + Unpin>(&mut self, r: &mut R) -> io::Result<usize> {
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
        }
        // Make sure the frame currently being assembled fits in the buffer.
        let needed = if self.end - self.start >= 4 {
            4 + be32(&self.buf[self.start..]) as usize
        } else {
            4
        }
        .min(4 + self.max_len);
        if self.start + needed > self.buf.len() {
            if self.start > 0 {
                self.buf.copy_within(self.start..self.end, 0);
                self.end -= self.start;
                self.start = 0;
            }
            if needed > self.buf.len() {
                self.buf.resize(needed, 0);
            }
        }
        debug_assert!(self.end < self.buf.len(), "complete frames must be consumed before fill");
        let n = r.read(&mut self.buf[self.end..]).await?;
        self.end += n;
        Ok(n)
    }

    /// Shrink the buffer back to its default size if a large frame inflated it.
    pub fn shrink(&mut self) {
        if self.start == self.end && self.buf.len() > 4 + 16 * 1024 + 16 {
            self.buf = vec![0; 4 + 16 * 1024 + 16];
            self.start = 0;
            self.end = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Message<'_>) {
        let mut out = Vec::new();
        msg.encode_into(&mut out);
        let len = u32::from_be_bytes(out[..4].try_into().unwrap()) as usize;
        assert_eq!(len, out.len() - 4);
        assert_eq!(Message::parse(&out[4..]).unwrap(), msg);
    }

    #[test]
    fn handshake_roundtrip() {
        let ih = [1u8; 20];
        let pid = [2u8; 20];
        let hs = encode_handshake(&ih, &pid, true);
        assert_eq!(hs.len(), 68);
        let d = decode_handshake(&hs).unwrap();
        assert_eq!(d.info_hash, ih);
        assert_eq!(d.peer_id, pid);
        assert!(d.supports_extensions);
        let hs = encode_handshake(&ih, &pid, false);
        assert!(!decode_handshake(&hs).unwrap().supports_extensions);
    }

    #[test]
    fn handshake_rejects_wrong_protocol() {
        let mut hs = encode_handshake(&[0; 20], &[0; 20], false);
        hs[5] = b'X';
        assert!(decode_handshake(&hs).is_err());
        let mut hs = encode_handshake(&[0; 20], &[0; 20], false);
        hs[0] = 18;
        assert!(decode_handshake(&hs).is_err());
    }

    #[test]
    fn all_messages_roundtrip() {
        roundtrip(Message::KeepAlive);
        roundtrip(Message::Choke);
        roundtrip(Message::Unchoke);
        roundtrip(Message::Interested);
        roundtrip(Message::NotInterested);
        roundtrip(Message::Have(0xdead_beef));
        roundtrip(Message::Bitfield(&[0xff, 0x80]));
        roundtrip(Message::Request { index: 1, begin: 16384, length: 16384 });
        roundtrip(Message::Cancel { index: 9, begin: 0, length: 5 });
        roundtrip(Message::Piece { index: 3, begin: 32768, data: b"hello" });
        roundtrip(Message::Port(6881));
        roundtrip(Message::Extended { id: 0, payload: b"d1:v1:xe" });
    }

    #[test]
    fn known_wire_bytes() {
        let mut out = Vec::new();
        Message::Request { index: 1, begin: 2, length: 3 }.encode_into(&mut out);
        assert_eq!(
            out,
            [0, 0, 0, 13, 6, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3]
        );
        out.clear();
        Message::Interested.encode_into(&mut out);
        assert_eq!(out, [0, 0, 0, 1, 2]);
    }

    #[test]
    fn empty_payload_is_keepalive() {
        assert_eq!(Message::parse(&[]).unwrap(), Message::KeepAlive);
    }

    #[test]
    fn unknown_ids_are_tolerated() {
        assert_eq!(Message::parse(&[13, 0, 0, 0, 1]).unwrap(), Message::Unknown(13));
    }

    #[test]
    fn rejects_malformed_messages() {
        assert!(Message::parse(&[0, 1]).is_err()); // choke with body
        assert!(Message::parse(&[4, 0, 0]).is_err()); // short have
        assert!(Message::parse(&[6, 0, 0, 0, 1]).is_err()); // short request
        assert!(Message::parse(&[7, 0, 0, 0, 1]).is_err()); // short piece
        assert!(Message::parse(&[9, 1]).is_err()); // short port
        assert!(Message::parse(&[20]).is_err()); // extended without id
    }

    #[test]
    fn piece_message_borrows_payload() {
        let mut out = Vec::new();
        Message::Piece { index: 0, begin: 0, data: &[1, 2, 3] }.encode_into(&mut out);
        match Message::parse(&out[4..]).unwrap() {
            Message::Piece { data, .. } => assert_eq!(data, &[1, 2, 3]),
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn frame_reader_handles_split_and_coalesced_input() {
        let mut stream = Vec::new();
        Message::Have(7).encode_into(&mut stream);
        Message::KeepAlive.encode_into(&mut stream);
        Message::Piece { index: 1, begin: 0, data: &[5u8; 16384] }.encode_into(&mut stream);
        Message::Unchoke.encode_into(&mut stream);

        // Feed one byte at a time through a duplex pipe to exercise partial reads.
        let (mut tx, mut rx) = tokio::io::duplex(64);
        let feeder = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            for chunk in stream.chunks(37) {
                tx.write_all(chunk).await.unwrap();
            }
        });

        let mut fr = FrameReader::new(FrameReader::max_len_for(8));
        let mut seen = Vec::new();
        'outer: loop {
            while let Some(p) = fr.try_frame().unwrap() {
                let m = Message::parse(p).unwrap();
                let tag = match m {
                    Message::Have(7) => "have",
                    Message::KeepAlive => "ka",
                    Message::Piece { data, .. } => {
                        assert_eq!(data.len(), 16384);
                        assert!(data.iter().all(|&b| b == 5));
                        "piece"
                    }
                    Message::Unchoke => {
                        seen.push("unchoke");
                        break 'outer;
                    }
                    _ => panic!("unexpected"),
                };
                seen.push(tag);
            }
            assert!(fr.fill(&mut rx).await.unwrap() > 0);
        }
        feeder.await.unwrap();
        assert_eq!(seen, vec!["have", "ka", "piece", "unchoke"]);
    }

    #[tokio::test]
    async fn frame_reader_rejects_oversized_frames() {
        let (mut tx, mut rx) = tokio::io::duplex(64);
        use tokio::io::AsyncWriteExt;
        tx.write_all(&[0xff, 0xff, 0xff, 0xff]).await.unwrap();
        let mut fr = FrameReader::new(1000);
        fr.fill(&mut rx).await.unwrap();
        assert!(fr.try_frame().is_err());
    }

    #[tokio::test]
    async fn frame_reader_reports_eof() {
        let (tx, mut rx) = tokio::io::duplex(64);
        drop(tx);
        let mut fr = FrameReader::new(1000);
        assert_eq!(fr.fill(&mut rx).await.unwrap(), 0);
    }

    #[test]
    fn max_len_covers_bitfield_and_block() {
        assert!(FrameReader::max_len_for(10) >= 16384 + 9);
        assert!(FrameReader::max_len_for(1_000_000) >= 125_000);
    }
}
