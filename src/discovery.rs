//! Peer discovery: trackers and DHT feeding a [`PeerQueue`].

use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::Duration;

use tokio::net::lookup_host;
use tokio::task::JoinSet;

use crate::peers::{PeerQueue, LOW_WATER};
use crate::tracker::{self, AnnounceParams, Event};
use crate::{dht, vlog};

const TRACKER_TIMEOUT: Duration = Duration::from_secs(20);
const DHT_LOOKUP_TIME: Duration = Duration::from_secs(25);
const MIN_INTERVAL: u64 = 60;
/// Default for [`Discovery::min_gap`].
pub const MIN_ROUND_GAP: Duration = Duration::from_secs(60);
const MAX_INTERVAL: u64 = 900;
const RETRY_EMPTY: u64 = 30;

/// Numbers reported to trackers; updated by the engine.
#[derive(Default)]
pub struct Stats {
    pub downloaded: Cell<u64>,
    pub left: Cell<u64>,
}

#[derive(Clone)]
pub struct Discovery {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub trackers: Vec<String>,
    pub use_dht: bool,
    pub dht_bootstrap: Vec<String>,
    /// Least time between two discovery rounds, even when the candidate list runs low.
    pub min_gap: Duration,
}

impl Discovery {
    fn params(&self, stats: &Stats, event: Event) -> AnnounceParams {
        AnnounceParams {
            info_hash: self.info_hash,
            peer_id: self.peer_id,
            port: self.port,
            downloaded: stats.downloaded.get(),
            left: stats.left.get(),
            event,
        }
    }

    /// One-shot announce used to tell trackers we finished (best effort).
    pub async fn announce_event(&self, stats: &Stats, event: Event) {
        let mut set = JoinSet::new();
        for url in self.trackers.clone() {
            let params = self.params(stats, event);
            set.spawn_local(async move {
                let _ = tracker::announce(&url, &params, Duration::from_secs(3)).await;
            });
        }
        while set.join_next().await.is_some() {}
    }
}

async fn resolve_bootstrap(hosts: &[String]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for h in hosts {
        if let Ok(addrs) = lookup_host(h.as_str()).await {
            out.extend(addrs.filter(|a| a.is_ipv4()));
        }
    }
    out
}

/// Run forever, periodically announcing to trackers and querying the DHT.
pub async fn run(d: Discovery, queue: Rc<PeerQueue>, stats: Rc<Stats>) {
    let mut event = Event::Started;
    loop {
        let round_started = tokio::time::Instant::now();
        let mut set: JoinSet<Option<u32>> = JoinSet::new();
        for url in d.trackers.clone() {
            let params = d.params(&stats, event);
            let queue = queue.clone();
            set.spawn_local(async move {
                match tracker::announce(&url, &params, TRACKER_TIMEOUT).await {
                    Ok(resp) => {
                        let n = queue.add(resp.peers);
                        vlog!("tracker {url}: {n} new peers (interval {}s)", resp.interval);
                        Some(resp.interval)
                    }
                    Err(e) => {
                        vlog!("tracker {url}: {e}");
                        None
                    }
                }
            });
        }
        if d.use_dht {
            let hosts = d.dht_bootstrap.clone();
            let info_hash = d.info_hash;
            let queue = queue.clone();
            set.spawn_local(async move {
                let bootstrap = resolve_bootstrap(&hosts).await;
                let mut found = 0;
                let res = dht::find_peers(&info_hash, &bootstrap, DHT_LOOKUP_TIME, |peers| {
                    found += queue.add(peers);
                })
                .await;
                match res {
                    Ok(()) => vlog!("dht: {found} new peers"),
                    Err(e) => vlog!("dht: {e}"),
                }
                None
            });
        }

        let before = queue.len();
        let mut interval: Option<u64> = None;
        while let Some(res) = set.join_next().await {
            if let Ok(Some(i)) = res {
                interval = Some(interval.map_or(i as u64, |cur| cur.min(i as u64)));
            }
        }
        event = Event::None;

        let delay = if queue.len() == before && queue.is_empty() {
            RETRY_EMPTY
        } else {
            interval.unwrap_or(300).clamp(MIN_INTERVAL, MAX_INTERVAL)
        };
        // Sleep until the next scheduled round - or earlier when the connection loop has used up
        // the candidates (after `min_gap`), because rotating peers needs somebody to rotate to.
        let (deadline, earliest) = (round_started + Duration::from_secs(delay), round_started + d.min_gap);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                _ = queue.low.notified() => {
                    if queue.len() < LOW_WATER {
                        tokio::time::sleep_until(earliest).await;
                        vlog!("discovery: only {} candidate peers left, asking again", queue.len());
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::LocalSet;

    #[tokio::test]
    async fn run_feeds_queue_from_tracker() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf).await.unwrap();
            let mut body = b"d8:intervali60e5:peers6:".to_vec();
            body.extend_from_slice(&[7, 7, 7, 7, 0x1a, 0xe1]);
            body.push(b'e');
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            s.write_all(head.as_bytes()).await.unwrap();
            s.write_all(&body).await.unwrap();
        });

        let d = Discovery {
            info_hash: [1; 20],
            peer_id: [2; 20],
            port: 6881,
            trackers: vec![format!("http://{addr}/announce"), "wss://ignored/a".into()],
            use_dht: false,
            dht_bootstrap: vec![],
            min_gap: Duration::from_millis(100),
        };
        let queue = Rc::new(PeerQueue::new());
        let stats = Rc::new(Stats::default());
        let local = LocalSet::new();
        local
            .run_until(async {
                let q = queue.clone();
                let h = tokio::task::spawn_local(run(d, q, stats));
                tokio::time::timeout(Duration::from_secs(5), queue.notify.notified())
                    .await
                    .unwrap();
                h.abort();
            })
            .await;
        assert_eq!(queue.pop(), Some("7.7.7.7:6881".parse().unwrap()));
    }

    #[tokio::test]
    async fn announce_event_tolerates_dead_trackers() {
        let d = Discovery {
            info_hash: [1; 20],
            peer_id: [2; 20],
            port: 1,
            trackers: vec!["http://127.0.0.1:1/a".into(), "udp://127.0.0.1:1/a".into()],
            use_dht: false,
            dht_bootstrap: vec![],
            min_gap: Duration::from_millis(100),
        };
        let local = LocalSet::new();
        local
            .run_until(async {
                tokio::time::timeout(
                    Duration::from_secs(10),
                    d.announce_event(&Stats::default(), Event::Completed),
                )
                .await
                .unwrap();
            })
            .await;
    }

    /// A tracker that counts announces and answers each with `per_answer` fresh peers.
    async fn counting_tracker(per_answer: usize) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = served.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else { return };
                let n = counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = s.read(&mut buf).await;
                    let mut peers = Vec::new();
                    for k in 0..per_answer {
                        let idx = n * per_answer + k;
                        peers.extend_from_slice(&[10, 0, (idx / 250) as u8, (idx % 250) as u8 + 1, 0x1a, 0xe1]);
                    }
                    let mut body = format!("d8:intervali3600e5:peers{}:", peers.len()).into_bytes();
                    body.extend_from_slice(&peers);
                    body.push(b'e');
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(&body).await;
                });
            }
        });
        (format!("http://{addr}/announce"), served)
    }

    fn discovery(url: String) -> Discovery {
        Discovery {
            info_hash: [1; 20],
            peer_id: [2; 20],
            port: 6881,
            trackers: vec![url],
            use_dht: false,
            dht_bootstrap: vec![],
            min_gap: Duration::from_millis(100),
        }
    }

    #[tokio::test]
    async fn asks_again_early_when_the_candidate_list_runs_low() {
        LocalSet::new()
            .run_until(async {
                let (url, count) = counting_tracker(1).await;
                let queue = Rc::new(PeerQueue::new());
                let task = tokio::task::spawn_local(run(discovery(url), queue.clone(), Rc::new(Stats::default())));
                tokio::time::timeout(Duration::from_secs(5), queue.notify.notified()).await.unwrap();
                let first = queue.pop().unwrap(); // the list is now empty -> "running low"
                // the tracker said "come back in an hour", yet a second round happens within seconds
                tokio::time::timeout(Duration::from_secs(5), queue.notify.notified()).await.expect("no early round");
                let second = queue.pop().unwrap();
                assert_ne!(first, second, "the second round brought a new peer");
                assert!(count.load(std::sync::atomic::Ordering::SeqCst) >= 2);
                task.abort();
            })
            .await;
    }

    #[tokio::test]
    async fn does_not_ask_again_while_there_are_plenty_of_candidates() {
        LocalSet::new()
            .run_until(async {
                let (url, count) = counting_tracker(30).await;
                let queue = Rc::new(PeerQueue::new());
                let task = tokio::task::spawn_local(run(discovery(url), queue.clone(), Rc::new(Stats::default())));
                tokio::time::timeout(Duration::from_secs(5), queue.notify.notified()).await.unwrap();
                for _ in 0..5 {
                    queue.pop().unwrap(); // 25 left: still above the low-water mark
                }
                tokio::time::sleep(Duration::from_millis(700)).await;
                assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1, "one announce only");
                task.abort();
            })
            .await;
    }
}
