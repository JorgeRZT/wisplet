//! `.torrent` / info-dictionary parsing and validation.
//!
//! Everything that later touches the filesystem is validated here: names and path
//! components may not contain separators, `.`/`..` or NUL, so a hostile torrent cannot
//! write outside the chosen output directory.

use std::collections::HashSet;
use std::path::PathBuf;

use sha1::{Digest, Sha1};

use crate::bencode::{self, Value};
use crate::error::{Error, Result};

/// Largest piece length we accept (128 MiB). Larger values are nonsensical and would
/// let a hostile torrent request huge buffers.
pub const MAX_PIECE_LENGTH: i64 = 1 << 27;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// Path relative to the output directory (already includes the torrent name for
    /// multi-file torrents).
    pub path: PathBuf,
    pub length: u64,
    /// Byte offset of this file in the concatenated torrent payload.
    pub offset: u64,
}

#[derive(Debug, Clone)]
pub struct Torrent {
    pub info_hash: [u8; 20],
    pub name: String,
    pub piece_length: u32,
    /// Concatenated 20-byte SHA-1 hashes.
    piece_hashes: Vec<u8>,
    pub files: Vec<FileInfo>,
    pub total_length: u64,
    pub trackers: Vec<String>,
    /// Private torrents (BEP 27) must not use DHT.
    pub private: bool,
}

impl Torrent {
    /// Parse a complete `.torrent` file.
    pub fn from_bytes(data: &[u8]) -> Result<Torrent> {
        let root = bencode::decode(data)?;
        if root.as_dict().is_none() {
            return Err(Error::InvalidTorrent("top level is not a dictionary".into()));
        }
        let span = bencode::dict_value_span(data, b"info")?
            .ok_or_else(|| Error::InvalidTorrent("missing \"info\" dictionary".into()))?;

        let mut trackers: Vec<String> = Vec::new();
        let mut push = |t: &Value| {
            if let Some(s) = t.as_str() {
                let s = s.trim();
                if !s.is_empty() && !trackers.iter().any(|x| x == s) {
                    trackers.push(s.to_string());
                }
            }
        };
        if let Some(a) = root.get("announce") {
            push(a);
        }
        if let Some(tiers) = root.get("announce-list").and_then(Value::as_list) {
            for tier in tiers {
                match tier {
                    Value::List(l) => l.iter().for_each(&mut push),
                    other => push(other),
                }
            }
        }
        Torrent::from_info(&data[span], trackers)
    }

    /// Parse the raw bytes of an info dictionary (as found in a `.torrent` or fetched
    /// through BEP 9). The info-hash is the SHA-1 of exactly these bytes.
    pub fn from_info(info_bytes: &[u8], trackers: Vec<String>) -> Result<Torrent> {
        let info_hash: [u8; 20] = Sha1::digest(info_bytes).into();
        let info = bencode::decode(info_bytes)?;
        if info.as_dict().is_none() {
            return Err(Error::InvalidTorrent("info is not a dictionary".into()));
        }

        let name = sanitize_component(
            info.get("name")
                .and_then(Value::as_bytes)
                .ok_or_else(|| Error::InvalidTorrent("missing name".into()))?,
        )
        .map_err(|e| Error::InvalidTorrent(format!("bad name: {e}")))?;

        let piece_length = info
            .get("piece length")
            .and_then(Value::as_int)
            .ok_or_else(|| Error::InvalidTorrent("missing piece length".into()))?;
        if piece_length <= 0 || piece_length > MAX_PIECE_LENGTH {
            return Err(Error::InvalidTorrent(format!(
                "piece length {piece_length} out of range"
            )));
        }

        let piece_hashes = info
            .get("pieces")
            .and_then(Value::as_bytes)
            .ok_or_else(|| Error::InvalidTorrent("missing pieces".into()))?
            .to_vec();
        if piece_hashes.is_empty() || piece_hashes.len() % 20 != 0 {
            return Err(Error::InvalidTorrent(
                "pieces length is not a positive multiple of 20".into(),
            ));
        }

        let mut files = Vec::new();
        let mut total: u64 = 0;
        if let Some(list) = info.get("files") {
            let list = list
                .as_list()
                .ok_or_else(|| Error::InvalidTorrent("files is not a list".into()))?;
            let mut seen = HashSet::new();
            for entry in list {
                let length = entry
                    .get("length")
                    .and_then(Value::as_int)
                    .filter(|&l| l >= 0)
                    .ok_or_else(|| Error::InvalidTorrent("file without valid length".into()))?
                    as u64;
                let parts = entry
                    .get("path")
                    .and_then(Value::as_list)
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| Error::InvalidTorrent("file without path".into()))?;
                let mut path = PathBuf::from(&name);
                for part in parts {
                    let bytes = part
                        .as_bytes()
                        .ok_or_else(|| Error::InvalidTorrent("path component not a string".into()))?;
                    path.push(
                        sanitize_component(bytes)
                            .map_err(|e| Error::InvalidTorrent(format!("bad path: {e}")))?,
                    );
                }
                if !seen.insert(path.clone()) {
                    return Err(Error::InvalidTorrent(format!(
                        "duplicate file path {}",
                        path.display()
                    )));
                }
                files.push(FileInfo {
                    path,
                    length,
                    offset: total,
                });
                total = total
                    .checked_add(length)
                    .ok_or_else(|| Error::InvalidTorrent("total length overflows".into()))?;
            }
            if files.is_empty() {
                return Err(Error::InvalidTorrent("empty file list".into()));
            }
        } else {
            let length = info
                .get("length")
                .and_then(Value::as_int)
                .filter(|&l| l >= 0)
                .ok_or_else(|| Error::InvalidTorrent("missing length".into()))?
                as u64;
            files.push(FileInfo {
                path: PathBuf::from(&name),
                length,
                offset: 0,
            });
            total = length;
        }

        if total == 0 {
            return Err(Error::InvalidTorrent("torrent has no data".into()));
        }
        let expected_pieces = total.div_ceil(piece_length as u64);
        if expected_pieces != (piece_hashes.len() / 20) as u64 {
            return Err(Error::InvalidTorrent(format!(
                "piece count mismatch: {} hashes for {expected_pieces} pieces",
                piece_hashes.len() / 20
            )));
        }

        Ok(Torrent {
            info_hash,
            name,
            piece_length: piece_length as u32,
            piece_hashes,
            files,
            total_length: total,
            trackers,
            private: info.get("private").and_then(Value::as_int) == Some(1),
        })
    }

    pub fn num_pieces(&self) -> u32 {
        (self.piece_hashes.len() / 20) as u32
    }

    /// Length of piece `index` (the last one may be shorter).
    pub fn piece_len(&self, index: u32) -> u32 {
        let start = index as u64 * self.piece_length as u64;
        (self.total_length - start).min(self.piece_length as u64) as u32
    }

    pub fn piece_hash(&self, index: u32) -> &[u8] {
        let i = index as usize * 20;
        &self.piece_hashes[i..i + 20]
    }

    /// `true` if `data` hashes to the expected value for piece `index`.
    pub fn verify_piece(&self, index: u32, data: &[u8]) -> bool {
        Sha1::digest(data).as_slice() == self.piece_hash(index)
    }
}

/// Wrap raw info-dictionary bytes into a minimal `.torrent` file. The bytes are spliced in
/// verbatim so the info-hash is preserved exactly.
pub fn build_torrent_file(info_bytes: &[u8], trackers: &[String]) -> Vec<u8> {
    let mut out = b"d".to_vec();
    if !trackers.is_empty() {
        out.extend_from_slice(b"13:announce-list");
        let tiers = Value::List(
            trackers
                .iter()
                .map(|t| Value::List(vec![Value::from(t.as_str())]))
                .collect(),
        );
        bencode::encode_into(&tiers, &mut out);
    }
    out.extend_from_slice(b"4:info");
    out.extend_from_slice(info_bytes);
    out.push(b'e');
    out
}

fn sanitize_component(bytes: &[u8]) -> std::result::Result<String, &'static str> {
    let s = String::from_utf8_lossy(bytes);
    if s.is_empty() {
        return Err("empty component");
    }
    if s == "." || s == ".." {
        return Err("relative component");
    }
    if s.contains(['/', '\\', '\0']) {
        return Err("component contains a path separator or NUL");
    }
    Ok(s.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(n: usize) -> Value {
        Value::Bytes(vec![7u8; 20 * n])
    }

    fn single(len: i64, piece_len: i64, n_pieces: usize) -> Vec<u8> {
        bencode::encode(&Value::dict([
            ("name", "file.bin".into()),
            ("length", len.into()),
            ("piece length", piece_len.into()),
            ("pieces", hashes(n_pieces)),
        ]))
    }

    fn multi(files: Vec<(Vec<&str>, i64)>, piece_len: i64, n_pieces: usize) -> Vec<u8> {
        let files = files
            .into_iter()
            .map(|(path, len)| {
                Value::dict([
                    ("length", len.into()),
                    ("path", Value::List(path.into_iter().map(Value::from).collect())),
                ])
            })
            .collect::<Vec<_>>();
        bencode::encode(&Value::dict([
            ("name", "dir".into()),
            ("files", files.into()),
            ("piece length", piece_len.into()),
            ("pieces", hashes(n_pieces)),
        ]))
    }

    #[test]
    fn parses_single_file_info() {
        let info = single(100, 40, 3);
        let t = Torrent::from_info(&info, vec![]).unwrap();
        assert_eq!(t.name, "file.bin");
        assert_eq!(t.total_length, 100);
        assert_eq!(t.num_pieces(), 3);
        assert_eq!(t.piece_len(0), 40);
        assert_eq!(t.piece_len(2), 20);
        assert_eq!(t.files.len(), 1);
        assert_eq!(t.files[0].path, PathBuf::from("file.bin"));
        assert!(!t.private);
        let expected: [u8; 20] = Sha1::digest(&info).into();
        assert_eq!(t.info_hash, expected);
    }

    #[test]
    fn exact_multiple_has_full_last_piece() {
        let t = Torrent::from_info(&single(80, 40, 2), vec![]).unwrap();
        assert_eq!(t.piece_len(1), 40);
    }

    #[test]
    fn parses_multi_file_with_offsets() {
        let info = multi(vec![(vec!["a.txt"], 30), (vec!["sub", "b.txt"], 0), (vec!["c"], 50)], 40, 2);
        let t = Torrent::from_info(&info, vec![]).unwrap();
        assert_eq!(t.total_length, 80);
        let paths: Vec<_> = t.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                PathBuf::from("dir/a.txt"),
                PathBuf::from("dir/sub/b.txt"),
                PathBuf::from("dir/c")
            ]
        );
        assert_eq!(t.files.iter().map(|f| f.offset).collect::<Vec<_>>(), vec![0, 30, 30]);
    }

    #[test]
    fn full_torrent_uses_raw_info_bytes_for_hash() {
        // unsorted keys inside info: re-encoding would change the hash
        let info = b"d6:lengthi10e4:name1:x12:piece lengthi16384e6:pieces20:AAAAAAAAAAAAAAAAAAAAe";
        let mut file = Vec::new();
        file.extend_from_slice(b"d8:announce14:http://t/ann/x4:info");
        file.extend_from_slice(info);
        file.extend_from_slice(b"e");
        let t = Torrent::from_bytes(&file).unwrap();
        let expected: [u8; 20] = Sha1::digest(info).into();
        assert_eq!(t.info_hash, expected);
        assert_eq!(t.trackers, vec!["http://t/ann/x"]);
    }

    #[test]
    fn collects_announce_list_without_duplicates() {
        let info = bencode::decode(&single(10, 16384, 1)).unwrap();
        let root = Value::dict([
            ("announce", "http://a".into()),
            (
                "announce-list",
                Value::List(vec![
                    Value::List(vec!["http://a".into(), "udp://b:1".into()]),
                    Value::List(vec!["http://c".into()]),
                ]),
            ),
            ("info", info),
        ]);
        let t = Torrent::from_bytes(&bencode::encode(&root)).unwrap();
        assert_eq!(t.trackers, vec!["http://a", "udp://b:1", "http://c"]);
    }

    #[test]
    fn detects_private_flag() {
        let info = bencode::encode(&Value::dict([
            ("name", "n".into()),
            ("length", 10.into()),
            ("piece length", 16384.into()),
            ("pieces", hashes(1)),
            ("private", 1.into()),
        ]));
        assert!(Torrent::from_info(&info, vec![]).unwrap().private);
    }

    #[test]
    fn rejects_structural_errors() {
        // piece count mismatch
        assert!(Torrent::from_info(&single(100, 40, 2), vec![]).is_err());
        // zero / negative / huge piece length
        assert!(Torrent::from_info(&single(100, 0, 1), vec![]).is_err());
        assert!(Torrent::from_info(&single(100, -5, 1), vec![]).is_err());
        assert!(Torrent::from_info(&single(100, MAX_PIECE_LENGTH + 1, 1), vec![]).is_err());
        // empty payload
        assert!(Torrent::from_info(&single(0, 40, 1), vec![]).is_err());
        // bad pieces blob
        let bad = bencode::encode(&Value::dict([
            ("name", "n".into()),
            ("length", 10.into()),
            ("piece length", 16384.into()),
            ("pieces", Value::Bytes(vec![0; 19])),
        ]));
        assert!(Torrent::from_info(&bad, vec![]).is_err());
        // not an info dict at all
        assert!(Torrent::from_info(b"i1e", vec![]).is_err());
        assert!(Torrent::from_info(b"garbage", vec![]).is_err());
        // missing info in .torrent
        assert!(Torrent::from_bytes(b"d3:fooi1ee").is_err());
        assert!(Torrent::from_bytes(b"i1e").is_err());
    }

    #[test]
    fn rejects_path_traversal() {
        for bad in [vec![".."], vec!["a", ".."], vec!["a/b"], vec!["a\\b"], vec![""], vec!["."], vec!["x\0y"]] {
            let info = multi(vec![(bad.clone(), 10)], 16384, 1);
            assert!(Torrent::from_info(&info, vec![]).is_err(), "{bad:?}");
        }
        // empty path list
        let info = multi(vec![(vec![], 10)], 16384, 1);
        assert!(Torrent::from_info(&info, vec![]).is_err());
    }

    #[test]
    fn rejects_dangerous_name() {
        for bad in ["..", "a/b", "/etc/passwd", ""] {
            let info = bencode::encode(&Value::dict([
                ("name", bad.into()),
                ("length", 10.into()),
                ("piece length", 16384.into()),
                ("pieces", hashes(1)),
            ]));
            assert!(Torrent::from_info(&info, vec![]).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn rejects_duplicate_paths_and_negative_lengths() {
        let info = multi(vec![(vec!["a"], 5), (vec!["a"], 5)], 16384, 1);
        assert!(Torrent::from_info(&info, vec![]).is_err());
        let info = multi(vec![(vec!["a"], -5)], 16384, 1);
        assert!(Torrent::from_info(&info, vec![]).is_err());
    }

    #[test]
    fn rejects_length_overflow() {
        let info = multi(vec![(vec!["a"], i64::MAX), (vec!["b"], i64::MAX), (vec!["c"], 10)], 16384, 1);
        assert!(Torrent::from_info(&info, vec![]).is_err());
    }

    #[test]
    fn built_torrent_file_preserves_info_hash_and_trackers() {
        // deliberately non-canonical key order inside info
        let info = b"d6:lengthi10e4:name1:x12:piece lengthi16384e6:pieces20:AAAAAAAAAAAAAAAAAAAAe";
        let trackers = vec!["http://a/x".to_string(), "udp://b:1".to_string()];
        let file = build_torrent_file(info, &trackers);
        let t = Torrent::from_bytes(&file).unwrap();
        let expected: [u8; 20] = Sha1::digest(info).into();
        assert_eq!(t.info_hash, expected);
        assert_eq!(t.trackers, trackers);
        let bare = Torrent::from_bytes(&build_torrent_file(info, &[])).unwrap();
        assert!(bare.trackers.is_empty());
    }

    #[test]
    fn verify_piece_checks_hash() {
        let data = vec![9u8; 40];
        let digest: [u8; 20] = Sha1::digest(&data).into();
        let info = bencode::encode(&Value::dict([
            ("name", "n".into()),
            ("length", 40.into()),
            ("piece length", 40.into()),
            ("pieces", Value::Bytes(digest.to_vec())),
        ]));
        let t = Torrent::from_info(&info, vec![]).unwrap();
        assert!(t.verify_piece(0, &data));
        assert!(!t.verify_piece(0, &data[1..]));
    }
}
