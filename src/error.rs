//! Crate-wide error type. Hand-written to avoid pulling in `thiserror`/`anyhow`.

use std::{fmt, io};

use crate::bencode;

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Bencode(bencode::Error),
    /// The `.torrent` / info dictionary is malformed or unsafe.
    InvalidTorrent(String),
    /// The magnet URI is malformed.
    InvalidMagnet(String),
    /// A remote peer violated the wire protocol.
    Protocol(String),
    /// A tracker request failed or returned garbage.
    Tracker(String),
    /// Something took too long.
    Timeout(&'static str),
    /// The download cannot proceed (e.g. no peers for the metadata).
    Failed(String),
    /// The torrent is already in the session (carries its id).
    Duplicate(u32),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Bencode(e) => write!(f, "bencode error: {e}"),
            Error::InvalidTorrent(m) => write!(f, "invalid torrent: {m}"),
            Error::InvalidMagnet(m) => write!(f, "invalid magnet link: {m}"),
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Tracker(m) => write!(f, "tracker error: {m}"),
            Error::Timeout(what) => write!(f, "timed out: {what}"),
            Error::Failed(m) => write!(f, "{m}"),
            Error::Duplicate(id) => write!(f, "already added as id {id}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<bencode::Error> for Error {
    fn from(e: bencode::Error) -> Self {
        Error::Bencode(e)
    }
}
