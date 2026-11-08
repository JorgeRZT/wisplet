//! Extension protocol (BEP 10) handshake and `ut_metadata` (BEP 9) messages, plus the
//! assembler that turns metadata pieces into a verified info dictionary.

use sha1::{Digest, Sha1};

use crate::bencode::{self, Value};
use crate::error::{Error, Result};

/// Metadata is transferred in 16 KiB pieces.
pub const METADATA_PIECE_SIZE: usize = 16 * 1024;
/// Hard cap on advertised metadata size (hostile peers could claim gigabytes).
pub const MAX_METADATA_SIZE: usize = 8 * 1024 * 1024;
/// The extension id *we* ask peers to use when sending us `ut_metadata` messages.
pub const OUR_UT_METADATA_ID: u8 = 1;

/// What a remote advertises in its extension handshake.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtHandshake {
    pub ut_metadata: Option<u8>,
    pub metadata_size: Option<usize>,
}

pub fn encode_ext_handshake() -> Vec<u8> {
    bencode::encode(&Value::dict([
        ("m", Value::dict([("ut_metadata", (OUR_UT_METADATA_ID as i64).into())])),
        ("v", "wisplet".into()),
    ]))
}

pub fn decode_ext_handshake(payload: &[u8]) -> Result<ExtHandshake> {
    let v = bencode::decode(payload)?;
    let ut_metadata = v
        .get("m")
        .and_then(|m| m.get("ut_metadata"))
        .and_then(Value::as_int)
        .filter(|&id| (1..=255).contains(&id))
        .map(|id| id as u8);
    let metadata_size = v
        .get("metadata_size")
        .and_then(Value::as_int)
        .filter(|&s| s > 0)
        .map(|s| s as usize);
    Ok(ExtHandshake {
        ut_metadata,
        metadata_size,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataMsg<'a> {
    Request { piece: u32 },
    Data { piece: u32, total_size: usize, data: &'a [u8] },
    Reject { piece: u32 },
}

pub fn encode_request(piece: u32) -> Vec<u8> {
    bencode::encode(&Value::dict([
        ("msg_type", 0.into()),
        ("piece", (piece as i64).into()),
    ]))
}

pub fn encode_reject(piece: u32) -> Vec<u8> {
    bencode::encode(&Value::dict([
        ("msg_type", 2.into()),
        ("piece", (piece as i64).into()),
    ]))
}

pub fn encode_data(piece: u32, total_size: usize, data: &[u8]) -> Vec<u8> {
    let mut out = bencode::encode(&Value::dict([
        ("msg_type", 1.into()),
        ("piece", (piece as i64).into()),
        ("total_size", (total_size as i64).into()),
    ]));
    out.extend_from_slice(data);
    out
}

pub fn decode_message(payload: &[u8]) -> Result<MetadataMsg<'_>> {
    let (header, used) = bencode::decode_prefix(payload)?;
    let msg_type = header
        .get("msg_type")
        .and_then(Value::as_int)
        .ok_or_else(|| Error::Protocol("ut_metadata without msg_type".into()))?;
    let piece = header
        .get("piece")
        .and_then(Value::as_int)
        .filter(|&p| (0..=u32::MAX as i64).contains(&p))
        .ok_or_else(|| Error::Protocol("ut_metadata without valid piece".into()))? as u32;
    match msg_type {
        0 => Ok(MetadataMsg::Request { piece }),
        1 => {
            let total_size = header
                .get("total_size")
                .and_then(Value::as_int)
                .filter(|&s| s > 0)
                .ok_or_else(|| Error::Protocol("ut_metadata data without total_size".into()))?
                as usize;
            Ok(MetadataMsg::Data {
                piece,
                total_size,
                data: &payload[used..],
            })
        }
        2 => Ok(MetadataMsg::Reject { piece }),
        other => Err(Error::Protocol(format!("unknown ut_metadata msg_type {other}"))),
    }
}

/// Collects metadata pieces from (possibly many) peers.
///
/// Pieces can be *claimed* so concurrent peer tasks do not fetch the same piece twice;
/// unclaimed on failure so another peer can retry.
#[derive(Debug)]
pub struct MetadataAssembler {
    size: usize,
    buf: Vec<u8>,
    have: Vec<bool>,
    claimed: Vec<bool>,
    received: usize,
}

impl MetadataAssembler {
    pub fn new(size: usize) -> Result<Self> {
        if size == 0 || size > MAX_METADATA_SIZE {
            return Err(Error::Protocol(format!("unreasonable metadata size {size}")));
        }
        let n = size.div_ceil(METADATA_PIECE_SIZE);
        Ok(MetadataAssembler {
            size,
            buf: vec![0; size],
            have: vec![false; n],
            claimed: vec![false; n],
            received: 0,
        })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn num_pieces(&self) -> u32 {
        self.have.len() as u32
    }

    fn expected_len(&self, piece: u32) -> usize {
        let start = piece as usize * METADATA_PIECE_SIZE;
        (self.size - start).min(METADATA_PIECE_SIZE)
    }

    /// Claim the lowest piece that is neither received nor claimed.
    pub fn claim_next(&mut self) -> Option<u32> {
        let i = (0..self.have.len()).find(|&i| !self.have[i] && !self.claimed[i])?;
        self.claimed[i] = true;
        Some(i as u32)
    }

    pub fn unclaim(&mut self, piece: u32) {
        if let Some(c) = self.claimed.get_mut(piece as usize) {
            *c = false;
        }
    }

    /// Store a piece. Returns `Ok(true)` once the metadata is complete.
    pub fn add_piece(&mut self, piece: u32, data: &[u8]) -> Result<bool> {
        let i = piece as usize;
        if i >= self.have.len() {
            return Err(Error::Protocol("metadata piece out of range".into()));
        }
        if data.len() != self.expected_len(piece) {
            return Err(Error::Protocol("metadata piece has wrong length".into()));
        }
        if !self.have[i] {
            let start = i * METADATA_PIECE_SIZE;
            self.buf[start..start + data.len()].copy_from_slice(data);
            self.have[i] = true;
            self.received += 1;
        }
        Ok(self.is_complete())
    }

    pub fn is_complete(&self) -> bool {
        self.received == self.have.len()
    }

    /// Verify against the info-hash and hand out the info dictionary bytes.
    pub fn finish(self, info_hash: &[u8; 20]) -> Result<Vec<u8>> {
        if !self.is_complete() {
            return Err(Error::Failed("metadata incomplete".into()));
        }
        let digest: [u8; 20] = Sha1::digest(&self.buf).into();
        if &digest != info_hash {
            return Err(Error::Protocol("metadata does not match info-hash".into()));
        }
        Ok(self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_handshake_roundtrip() {
        let ours = decode_ext_handshake(&encode_ext_handshake()).unwrap();
        assert_eq!(ours.ut_metadata, Some(OUR_UT_METADATA_ID));
        assert_eq!(ours.metadata_size, None);

        let theirs = bencode::encode(&Value::dict([
            ("m", Value::dict([("ut_metadata", 3.into()), ("ut_pex", 1.into())])),
            ("metadata_size", 31235.into()),
        ]));
        let h = decode_ext_handshake(&theirs).unwrap();
        assert_eq!(h.ut_metadata, Some(3));
        assert_eq!(h.metadata_size, Some(31235));
    }

    #[test]
    fn ext_handshake_without_metadata_support() {
        let h = decode_ext_handshake(b"d1:md6:ut_pexi1eee").unwrap();
        assert_eq!(h, ExtHandshake::default());
        // zero / out-of-range ids mean "unsupported"
        assert_eq!(
            decode_ext_handshake(b"d1:md11:ut_metadatai0eee").unwrap().ut_metadata,
            None
        );
        assert_eq!(
            decode_ext_handshake(b"d1:md11:ut_metadatai999eee").unwrap().ut_metadata,
            None
        );
        assert!(decode_ext_handshake(b"not bencode").is_err());
    }

    #[test]
    fn message_roundtrips() {
        assert_eq!(
            decode_message(&encode_request(4)).unwrap(),
            MetadataMsg::Request { piece: 4 }
        );
        assert_eq!(
            decode_message(&encode_reject(2)).unwrap(),
            MetadataMsg::Reject { piece: 2 }
        );
        let data = encode_data(1, 20000, b"payload!");
        assert_eq!(
            decode_message(&data).unwrap(),
            MetadataMsg::Data { piece: 1, total_size: 20000, data: b"payload!" }
        );
    }

    #[test]
    fn rejects_malformed_messages() {
        assert!(decode_message(b"d8:msg_typei1ee").is_err()); // no piece
        assert!(decode_message(b"d8:msg_typei1e5:piecei0ee").is_err()); // data w/o total_size
        assert!(decode_message(b"d8:msg_typei9e5:piecei0ee").is_err()); // unknown type
        assert!(decode_message(b"d8:msg_typei0e5:piecei-1ee").is_err()); // negative
        assert!(decode_message(b"junk").is_err());
    }

    #[test]
    fn assembler_collects_and_verifies() {
        let info: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        let hash: [u8; 20] = Sha1::digest(&info).into();
        let mut a = MetadataAssembler::new(info.len()).unwrap();
        assert_eq!(a.num_pieces(), 3);

        assert_eq!(a.claim_next(), Some(0));
        assert_eq!(a.claim_next(), Some(1));
        assert_eq!(a.claim_next(), Some(2));
        assert_eq!(a.claim_next(), None);

        assert!(!a.add_piece(2, &info[2 * 16384..]).unwrap());
        assert!(!a.add_piece(0, &info[..16384]).unwrap());
        assert!(a.add_piece(1, &info[16384..2 * 16384]).unwrap());
        assert_eq!(a.finish(&hash).unwrap(), info);
    }

    #[test]
    fn assembler_rejects_bad_hash_and_bad_pieces() {
        let info = vec![1u8; 100];
        let mut a = MetadataAssembler::new(100).unwrap();
        assert!(a.add_piece(5, &info).is_err());
        assert!(a.add_piece(0, &info[..50]).is_err());
        assert!(a.add_piece(0, &info).unwrap());
        assert!(a.finish(&[0u8; 20]).is_err());
    }

    #[test]
    fn assembler_unclaim_allows_retry() {
        let mut a = MetadataAssembler::new(100).unwrap();
        assert_eq!(a.claim_next(), Some(0));
        assert_eq!(a.claim_next(), None);
        a.unclaim(0);
        assert_eq!(a.claim_next(), Some(0));
    }

    #[test]
    fn assembler_limits_size() {
        assert!(MetadataAssembler::new(0).is_err());
        assert!(MetadataAssembler::new(MAX_METADATA_SIZE + 1).is_err());
        assert!(MetadataAssembler::new(MAX_METADATA_SIZE).is_ok());
    }

    #[test]
    fn incomplete_assembler_cannot_finish() {
        let a = MetadataAssembler::new(100).unwrap();
        assert!(a.finish(&[0; 20]).is_err());
    }
}
