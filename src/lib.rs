//! wisplet: a minimal-footprint, download-only BitTorrent client.
pub mod api;
pub mod autostart;
pub mod bencode;
pub mod bitfield;
pub mod cli;
pub mod config;
pub mod curl;
#[cfg(unix)]
pub mod daemon;
pub mod dht;
pub mod discovery;
pub mod engine;
pub mod fetch;
pub mod format;
pub mod json;
pub mod error;
pub mod magnet;
pub mod manager;
pub mod mcp;
pub mod metadata;
pub mod metainfo;
pub mod peers;
pub mod pieces;
pub mod protocol;
pub mod search;
pub mod storage;
#[cfg(unix)]
pub mod term;
pub mod tracker;
pub mod tui;
pub mod util;
pub mod wire;

pub use error::{Error, Result};
