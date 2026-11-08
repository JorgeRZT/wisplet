//! Bounded, de-duplicated queue of peer addresses waiting to be contacted.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;

use tokio::sync::Notify;

/// Upper bound on remembered addresses (keeps memory flat on huge swarms).
const MAX_SEEN: usize = 4096;

#[derive(Default)]
struct Inner {
    queue: VecDeque<SocketAddr>,
    seen: HashSet<SocketAddr>,
}

#[derive(Default)]
pub struct PeerQueue {
    inner: RefCell<Inner>,
    /// Signalled whenever new addresses are queued.
    pub notify: Notify,
    /// Signalled when taking an address leaves fewer than [`LOW_WATER`] waiting, so discovery
    /// can look for more instead of letting the connection slots starve.
    pub low: Notify,
}

/// Candidates below this number count as "running low".
pub const LOW_WATER: usize = 20;

fn usable(a: &SocketAddr) -> bool {
    a.port() != 0 && !a.ip().is_unspecified() && !a.ip().is_multicast()
}

impl PeerQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue addresses never seen before; returns how many were new.
    pub fn add(&self, peers: impl IntoIterator<Item = SocketAddr>) -> usize {
        let mut inner = self.inner.borrow_mut();
        let mut added = 0;
        for p in peers {
            if !usable(&p) || inner.seen.len() >= MAX_SEEN {
                continue;
            }
            if inner.seen.insert(p) {
                inner.queue.push_back(p);
                added += 1;
            }
        }
        drop(inner);
        if added > 0 {
            self.notify.notify_one();
        }
        added
    }

    pub fn pop(&self) -> Option<SocketAddr> {
        let (peer, left) = {
            let mut inner = self.inner.borrow_mut();
            (inner.queue.pop_front(), inner.queue.len())
        };
        if peer.is_some() && left < LOW_WATER {
            self.low.notify_one();
        }
        peer
    }

    /// Put a previously good peer back for a later reconnect.
    pub fn requeue(&self, peer: SocketAddr) {
        self.inner.borrow_mut().queue.push_back(peer);
        self.notify.notify_one();
    }

    pub fn len(&self) -> usize {
        self.inner.borrow().queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every address ever accepted (used to seed the download phase after a magnet
    /// metadata fetch).
    pub fn seen(&self) -> Vec<SocketAddr> {
        self.inner.borrow().seen.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn deduplicates_and_keeps_fifo_order() {
        let q = PeerQueue::new();
        assert_eq!(q.add([a("1.1.1.1:1"), a("2.2.2.2:2"), a("1.1.1.1:1")]), 2);
        assert_eq!(q.add([a("2.2.2.2:2")]), 0);
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop(), Some(a("1.1.1.1:1")));
        assert_eq!(q.pop(), Some(a("2.2.2.2:2")));
        assert_eq!(q.pop(), None);
        // popped addresses are still "seen" and are not re-added by discovery
        assert_eq!(q.add([a("1.1.1.1:1")]), 0);
    }

    #[test]
    fn filters_unusable_addresses() {
        let q = PeerQueue::new();
        assert_eq!(q.add([a("1.1.1.1:0"), a("0.0.0.0:80"), a("224.0.0.1:80")]), 0);
        assert!(q.is_empty());
    }

    #[test]
    fn requeue_allows_reconnect() {
        let q = PeerQueue::new();
        q.add([a("1.1.1.1:1")]);
        let p = q.pop().unwrap();
        q.requeue(p);
        assert_eq!(q.pop(), Some(p));
    }

    #[test]
    fn seen_is_bounded() {
        let q = PeerQueue::new();
        let many = (0..MAX_SEEN as u32 + 500).map(|i| {
            SocketAddr::from(([10, (i >> 16) as u8, (i >> 8) as u8, i as u8], 6881))
        });
        q.add(many);
        assert_eq!(q.len(), MAX_SEEN);
        assert_eq!(q.seen().len(), MAX_SEEN);
    }

    #[tokio::test]
    async fn add_wakes_waiters() {
        let q = std::rc::Rc::new(PeerQueue::new());
        q.add([a("1.1.1.1:1")]);
        // permit stored by notify_one: this must not hang
        tokio::time::timeout(std::time::Duration::from_millis(200), q.notify.notified())
            .await
            .unwrap();
    }
}
