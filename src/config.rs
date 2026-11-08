//! Runtime configuration. Defaults are tuned for a small footprint.

use std::net::SocketAddr;
use std::time::Duration;

use crate::util::random_peer_id;

/// Longest a peer is kept while choking us when nobody else is waiting to take its place.
pub const CHOKED_PATIENCE: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct Config {
    /// Maximum simultaneous peer connections *per torrent*.
    pub max_peers: usize,
    /// Outstanding block requests per peer (each block is 16 KiB): the starting value and the
    /// minimum. A peer that delivers fast gets a longer queue, up to `pipeline_max`.
    pub pipeline: usize,
    /// Ceiling of the adaptive request queue. Equal to `pipeline` means a fixed queue.
    pub pipeline_max: usize,
    /// A peer that keeps us choked this long is dropped (and retried later) when other
    /// candidates are waiting; with none waiting it is kept up to [`CHOKED_PATIENCE`].
    pub choked_limit: Duration,
    /// Bytes of piece buffers kept in RAM *per torrent* (at least one piece).
    pub memory_budget: usize,
    pub enable_dht: bool,
    /// `host:port` strings resolved when a DHT lookup starts.
    pub dht_bootstrap: Vec<String>,
    /// Peers to try in addition to whatever trackers / DHT report.
    pub extra_peers: Vec<SocketAddr>,
    pub peer_id: [u8; 20],
    /// Port advertised to trackers. We do not accept inbound connections.
    pub port: u16,
    pub connect_timeout: Duration,
    /// Drop a peer that owes us blocks but delivers nothing for this long.
    pub request_timeout: Duration,
    /// Give up on a magnet link if no metadata arrives within this time.
    pub metadata_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            max_peers: 30,
            pipeline: 10,
            pipeline_max: 64,
            choked_limit: Duration::from_secs(45),
            memory_budget: 16 << 20,
            enable_dht: true,
            dht_bootstrap: crate::dht::BOOTSTRAP_NODES.iter().map(|s| s.to_string()).collect(),
            extra_peers: Vec::new(),
            peer_id: random_peer_id(),
            port: 6881,
            connect_timeout: Duration::from_secs(8),
            request_timeout: Duration::from_secs(30),
            metadata_timeout: Duration::from_secs(300),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative() {
        let c = Config::default();
        assert!(c.max_peers <= 50);
        assert!(c.memory_budget <= 32 << 20);
        assert!(c.pipeline >= 1 && c.pipeline_max >= c.pipeline);
        assert!(c.choked_limit < CHOKED_PATIENCE);
        assert_eq!(c.dht_bootstrap.len(), 3);
    }
}
