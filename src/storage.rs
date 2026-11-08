//! On-disk layout: maps pieces onto (possibly several) files using positional I/O.
//!
//! * Files are pre-sized (sparse where the filesystem supports it).
//! * Only the most recently used file handle is kept open, so torrents with thousands
//!   of files never exhaust file descriptors.
//! * Existing files of the right size are re-hashed on start (resume); files we just
//!   created are skipped, so a fresh download pays no verification cost.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use crate::bitfield::Bitfield;
use crate::error::{Error, Result};
use crate::metainfo::Torrent;

struct Entry {
    path: PathBuf,
    offset: u64,
    length: u64,
    /// Existed before we started with exactly the expected size.
    preexisting: bool,
}

pub struct Storage {
    files: Vec<Entry>,
    piece_length: u64,
    total_length: u64,
    open: RefCell<Option<(usize, File)>>,
}

#[cfg(unix)]
fn write_all_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(f, buf, off)
}

#[cfg(unix)]
fn read_exact_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(f, buf, off)
}

#[cfg(windows)]
fn write_all_at(f: &File, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, off)?;
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(f: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_read(buf, off)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let tmp = buf;
        buf = &mut tmp[n..];
        off += n as u64;
    }
    Ok(())
}

impl Storage {
    /// Create directories and pre-size all files under `root`.
    pub fn create(root: &Path, torrent: &Torrent) -> Result<Storage> {
        let mut files = Vec::with_capacity(torrent.files.len());
        for f in &torrent.files {
            let path = root.join(&f.path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut preexisting = false;
            match fs::metadata(&path) {
                Ok(m) if m.is_file() => {
                    if m.len() == f.length {
                        preexisting = true;
                    } else if m.len() > f.length {
                        return Err(Error::Failed(format!(
                            "{} exists and is larger than expected ({} > {} bytes); \
                             refusing to truncate it",
                            path.display(),
                            m.len(),
                            f.length
                        )));
                    } else {
                        OpenOptions::new().write(true).open(&path)?.set_len(f.length)?;
                    }
                }
                Ok(_) => {
                    return Err(Error::Failed(format!(
                        "{} exists and is not a regular file",
                        path.display()
                    )))
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)?
                        .set_len(f.length)?;
                }
                Err(e) => return Err(e.into()),
            }
            files.push(Entry {
                path,
                offset: f.offset,
                length: f.length,
                preexisting,
            });
        }
        Ok(Storage {
            files,
            piece_length: torrent.piece_length as u64,
            total_length: torrent.total_length,
            open: RefCell::new(None),
        })
    }

    fn with_file<R>(&self, idx: usize, f: impl FnOnce(&File) -> io::Result<R>) -> io::Result<R> {
        let mut slot = self.open.borrow_mut();
        if !matches!(&*slot, Some((i, _)) if *i == idx) {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.files[idx].path)?;
            *slot = Some((idx, file));
        }
        f(&slot.as_ref().unwrap().1)
    }

    /// Visit `(file index, offset inside file, range inside the piece buffer)`.
    fn segments(
        &self,
        piece: u32,
        len: usize,
        mut visit: impl FnMut(usize, u64, std::ops::Range<usize>) -> io::Result<()>,
    ) -> io::Result<()> {
        let start = piece as u64 * self.piece_length;
        debug_assert!(start + len as u64 <= self.total_length);
        let mut pos = start;
        let end = start + len as u64;
        let mut idx = self.files.partition_point(|f| f.offset + f.length <= pos);
        while pos < end && idx < self.files.len() {
            let f = &self.files[idx];
            let file_off = pos - f.offset;
            let n = (f.length - file_off).min(end - pos);
            let buf_start = (pos - start) as usize;
            visit(idx, file_off, buf_start..buf_start + n as usize)?;
            pos += n;
            idx += 1;
        }
        Ok(())
    }

    pub fn write_piece(&self, piece: u32, data: &[u8]) -> io::Result<()> {
        self.segments(piece, data.len(), |idx, off, range| {
            self.with_file(idx, |f| write_all_at(f, &data[range], off))
        })
    }

    pub fn read_piece(&self, piece: u32, buf: &mut Vec<u8>, len: usize) -> io::Result<()> {
        buf.resize(len, 0);
        self.segments(piece, len, |idx, off, range| {
            self.with_file(idx, |f| read_exact_at(f, &mut buf[range], off))
        })
    }

    /// Whether every byte of the piece lives in a file that existed before we started.
    fn piece_may_exist(&self, piece: u32, len: usize) -> bool {
        let mut all = true;
        let _ = self.segments(piece, len, |idx, _, _| {
            all &= self.files[idx].preexisting;
            Ok(())
        });
        all
    }

    /// `true` if at least one file existed before we started (so resume has work to do).
    pub fn has_preexisting(&self) -> bool {
        self.files.iter().any(|f| f.preexisting)
    }

    /// Check a single piece on disk. Returns `false` without any I/O when the piece
    /// cannot possibly be present (it touches a file we just created).
    pub fn check_piece(&self, torrent: &Torrent, index: u32, buf: &mut Vec<u8>) -> io::Result<bool> {
        let len = torrent.piece_len(index) as usize;
        if !self.piece_may_exist(index, len) {
            return Ok(false);
        }
        self.read_piece(index, buf, len)?;
        Ok(torrent.verify_piece(index, buf))
    }

    /// Hash everything already on disk and report which pieces are intact.
    pub fn resume(&self, torrent: &Torrent) -> io::Result<Bitfield> {
        let mut have = Bitfield::new(torrent.num_pieces() as usize);
        if !self.has_preexisting() {
            return Ok(have);
        }
        let mut buf = Vec::with_capacity(torrent.piece_length as usize);
        for i in 0..torrent.num_pieces() {
            if self.check_piece(torrent, i, &mut buf)? {
                have.set(i as usize);
            }
        }
        Ok(have)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bencode::{self, Value};
    use sha1::{Digest, Sha1};

    /// Build a torrent for `files` (name, content) with the given piece length.
    pub(crate) fn make_torrent(files: &[(&str, Vec<u8>)], piece_len: usize) -> (Torrent, Vec<u8>) {
        let payload: Vec<u8> = files.iter().flat_map(|(_, c)| c.clone()).collect();
        let hashes: Vec<u8> = payload
            .chunks(piece_len)
            .flat_map(|c| Sha1::digest(c).to_vec())
            .collect();
        let mut info = vec![
            ("name", Value::from("t")),
            ("piece length", (piece_len as i64).into()),
            ("pieces", Value::Bytes(hashes)),
        ];
        if files.len() == 1 && files[0].0 == "t" {
            info.push(("length", (payload.len() as i64).into()));
        } else {
            let list: Vec<Value> = files
                .iter()
                .map(|(n, c)| {
                    Value::dict([
                        ("length", (c.len() as i64).into()),
                        ("path", Value::List(n.split('/').map(Value::from).collect())),
                    ])
                })
                .collect();
            info.push(("files", list.into()));
        }
        let bytes = bencode::encode(&Value::dict(info));
        (Torrent::from_info(&bytes, vec![]).unwrap(), payload)
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn writes_pieces_across_file_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let files = [
            ("a.bin", pattern(25, 1)),
            ("sub/b.bin", pattern(40, 2)),
            ("empty.bin", vec![]),
            ("c.bin", pattern(10, 3)),
        ];
        let (t, payload) = make_torrent(&files, 16);
        let s = Storage::create(dir.path(), &t).unwrap();
        for i in 0..t.num_pieces() {
            let start = i as usize * 16;
            let end = (start + 16).min(payload.len());
            s.write_piece(i, &payload[start..end]).unwrap();
        }
        for (name, content) in &files {
            let got = fs::read(dir.path().join("t").join(name)).unwrap();
            assert_eq!(&got, content, "{name}");
        }
    }

    #[test]
    fn single_file_roundtrip_and_presizing() {
        let dir = tempfile::tempdir().unwrap();
        let content = pattern(100, 7);
        let (t, _) = make_torrent(&[("t", content.clone())], 32);
        let s = Storage::create(dir.path(), &t).unwrap();
        assert_eq!(fs::metadata(dir.path().join("t")).unwrap().len(), 100);
        s.write_piece(3, &content[96..]).unwrap();
        let mut buf = Vec::new();
        s.read_piece(3, &mut buf, 4).unwrap();
        assert_eq!(buf, &content[96..]);
    }

    #[test]
    fn fresh_download_skips_resume_hashing() {
        let dir = tempfile::tempdir().unwrap();
        let (t, _) = make_torrent(&[("t", pattern(64, 1))], 32);
        let s = Storage::create(dir.path(), &t).unwrap();
        assert_eq!(s.resume(&t).unwrap().count_ones(), 0);
    }

    #[test]
    fn resume_detects_intact_and_corrupt_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let content = pattern(128, 5);
        let (t, _) = make_torrent(&[("t", content.clone())], 32);
        // pre-create the file with piece 1 corrupted
        let mut on_disk = content.clone();
        on_disk[40] ^= 0xff;
        fs::write(dir.path().join("t"), &on_disk).unwrap();
        let s = Storage::create(dir.path(), &t).unwrap();
        let have = s.resume(&t).unwrap();
        assert_eq!(have.iter_ones().collect::<Vec<_>>(), vec![0, 2, 3]);
    }

    #[test]
    fn resume_with_multiple_files_requires_all_segments() {
        let dir = tempfile::tempdir().unwrap();
        let files = [("a", pattern(20, 1)), ("b", pattern(20, 2))];
        let (t, payload) = make_torrent(&files, 16);
        // only file "a" exists beforehand; "b" is created fresh (zeros)
        fs::create_dir_all(dir.path().join("t")).unwrap();
        fs::write(dir.path().join("t/a"), &payload[..20]).unwrap();
        let s = Storage::create(dir.path(), &t).unwrap();
        let have = s.resume(&t).unwrap();
        // piece 0 (0..16) lies in "a" only -> intact; piece 1 (16..32) spans a+b -> unknown
        assert_eq!(have.iter_ones().collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn smaller_existing_file_is_extended_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let content = pattern(64, 9);
        let (t, _) = make_torrent(&[("t", content.clone())], 32);
        fs::write(dir.path().join("t"), &content[..10]).unwrap();
        let s = Storage::create(dir.path(), &t).unwrap();
        assert_eq!(fs::metadata(dir.path().join("t")).unwrap().len(), 64);
        assert_eq!(s.resume(&t).unwrap().count_ones(), 0);
    }

    #[test]
    fn larger_existing_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (t, _) = make_torrent(&[("t", pattern(64, 9))], 32);
        fs::write(dir.path().join("t"), vec![0u8; 1000]).unwrap();
        assert!(Storage::create(dir.path(), &t).is_err());
        // untouched
        assert_eq!(fs::metadata(dir.path().join("t")).unwrap().len(), 1000);
    }

    #[test]
    fn directory_in_place_of_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (t, _) = make_torrent(&[("t", pattern(64, 9))], 32);
        fs::create_dir(dir.path().join("t")).unwrap();
        assert!(Storage::create(dir.path(), &t).is_err());
    }

    #[test]
    fn many_files_do_not_exhaust_handles() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<(String, Vec<u8>)> = (0..600).map(|i| (format!("f{i}"), pattern(20, i as u8))).collect();
        let refs: Vec<(&str, Vec<u8>)> = files.iter().map(|(n, c)| (n.as_str(), c.clone())).collect();
        let (t, payload) = make_torrent(&refs, 64);
        let s = Storage::create(dir.path(), &t).unwrap();
        for i in 0..t.num_pieces() {
            let start = i as usize * 64;
            s.write_piece(i, &payload[start..(start + 64).min(payload.len())]).unwrap();
        }
        assert_eq!(fs::read(dir.path().join("t/f599")).unwrap(), files[599].1);
        assert_eq!(fs::read(dir.path().join("t/f0")).unwrap(), files[0].1);
    }
}
