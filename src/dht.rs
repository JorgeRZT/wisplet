//! Minimal Mainline DHT client (BEP 5): iterative `get_peers` lookups only.
//!
//! We never answer queries and keep no routing table: a lookup starts from the
//! bootstrap nodes, repeatedly asks the closest known nodes, and reports every peer
//! address it sees. Memory use is bounded (64 candidates) and everything is dropped
//! when the lookup ends.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::time::timeout;

use crate::bencode::{self, Value};
use crate::error::{Error, Result};
use crate::tracker::parse_compact_peers_v4;

pub const BOOTSTRAP_NODES: [&str; 3] = [
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
];

const ALPHA: usize = 8;
const MAX_CANDIDATES: usize = 64;
const CLOSEST_K: usize = 16;
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    pub id: [u8; 20],
    pub addr: SocketAddr,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Response {
    pub tid: Vec<u8>,
    pub peers: Vec<SocketAddr>,
    pub nodes: Vec<Node>,
}

pub fn xor_distance(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    let mut d = [0u8; 20];
    for i in 0..20 {
        d[i] = a[i] ^ b[i];
    }
    d
}

pub fn encode_get_peers(tid: &[u8], self_id: &[u8; 20], info_hash: &[u8; 20]) -> Vec<u8> {
    bencode::encode(&Value::dict([
        ("t", tid.into()),
        ("y", "q".into()),
        ("q", "get_peers".into()),
        (
            "a",
            Value::dict([
                ("id", self_id.as_slice().into()),
                ("info_hash", info_hash.as_slice().into()),
            ]),
        ),
    ]))
}

/// Compact node info: 20-byte id + 4-byte IPv4 + 2-byte port (26 bytes each).
pub fn parse_compact_nodes(b: &[u8]) -> Vec<Node> {
    b.chunks_exact(26)
        .filter_map(|c| {
            let port = u16::from_be_bytes([c[24], c[25]]);
            if port == 0 {
                return None;
            }
            Some(Node {
                id: c[..20].try_into().unwrap(),
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(c[20], c[21], c[22], c[23])), port),
            })
        })
        .collect()
}

pub fn decode_response(buf: &[u8]) -> Result<Response> {
    let v = bencode::decode(buf)?;
    let tid = v
        .get("t")
        .and_then(Value::as_bytes)
        .ok_or_else(|| Error::Protocol("DHT message without transaction id".into()))?
        .to_vec();
    match v.get("y").and_then(Value::as_bytes) {
        Some(b"r") => {}
        Some(b"e") => return Err(Error::Protocol("DHT error reply".into())),
        _ => return Err(Error::Protocol("not a DHT response".into())),
    }
    let r = v
        .get("r")
        .ok_or_else(|| Error::Protocol("DHT response without body".into()))?;
    let mut resp = Response {
        tid,
        ..Default::default()
    };
    if let Some(values) = r.get("values").and_then(Value::as_list) {
        for item in values {
            if let Some(b) = item.as_bytes() {
                resp.peers.extend(parse_compact_peers_v4(b));
            }
        }
    }
    if let Some(n) = r.get("nodes").and_then(Value::as_bytes) {
        resp.nodes = parse_compact_nodes(n);
    }
    Ok(resp)
}

struct Candidate {
    node: Node,
    queried: bool,
}

/// Run one iterative lookup, invoking `on_peers` for every batch of peers found.
/// Ends when the closest nodes have all been queried or `limit` elapses.
pub async fn find_peers(
    info_hash: &[u8; 20],
    bootstrap: &[SocketAddr],
    limit: Duration,
    mut on_peers: impl FnMut(Vec<SocketAddr>),
) -> Result<()> {
    if bootstrap.is_empty() {
        return Err(Error::Failed("no DHT bootstrap nodes".into()));
    }
    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    let mut self_id = [0u8; 20];
    self_id.iter_mut().for_each(|b| *b = fastrand::u8(..));
    let deadline = Instant::now() + limit;

    let mut cands: Vec<Candidate> = bootstrap
        .iter()
        .map(|&addr| Candidate {
            // Unknown id: pretend distance 0 so bootstrap nodes are asked first.
            node: Node { id: *info_hash, addr },
            queried: false,
        })
        .collect();
    let mut known: HashSet<SocketAddr> = bootstrap.iter().copied().collect();
    let mut inflight: HashMap<SocketAddr, (Instant, [u8; 2])> = HashMap::new();
    let mut next_tid: u16 = fastrand::u16(..);
    let mut buf = [0u8; 4096];

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        inflight.retain(|_, (sent, _)| now.duration_since(*sent) < QUERY_TIMEOUT);

        // Fire queries at the closest unqueried candidates.
        for c in cands.iter_mut().take(CLOSEST_K) {
            if inflight.len() >= ALPHA {
                break;
            }
            if c.queried {
                continue;
            }
            c.queried = true;
            let tid = next_tid.to_be_bytes();
            next_tid = next_tid.wrapping_add(1);
            let msg = encode_get_peers(&tid, &self_id, info_hash);
            if sock.send_to(&msg, c.node.addr).await.is_ok() {
                inflight.insert(c.node.addr, (now, tid));
            }
        }

        let top_all_queried = cands.iter().take(CLOSEST_K).all(|c| c.queried);
        if inflight.is_empty() && top_all_queried {
            break;
        }

        let wait = (deadline - now).min(Duration::from_millis(500));
        let Ok(Ok((n, from))) = timeout(wait, sock.recv_from(&mut buf)).await else {
            continue;
        };
        let Some((_, tid)) = inflight.get(&from).copied() else {
            continue;
        };
        let Ok(resp) = decode_response(&buf[..n]) else {
            inflight.remove(&from);
            continue;
        };
        if resp.tid != tid {
            continue;
        }
        inflight.remove(&from);
        if !resp.peers.is_empty() {
            on_peers(resp.peers);
        }
        for node in resp.nodes {
            if known.insert(node.addr) {
                cands.push(Candidate { node, queried: false });
            }
        }
        cands.sort_by_key(|c| xor_distance(&c.node.id, info_hash));
        cands.truncate(MAX_CANDIDATES);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_bytes(id: u8, ip: [u8; 4], port: u16) -> Vec<u8> {
        let mut v = vec![id; 20];
        v.extend_from_slice(&ip);
        v.extend_from_slice(&port.to_be_bytes());
        v
    }

    #[test]
    fn query_encoding() {
        let q = encode_get_peers(b"aa", &[1; 20], &[2; 20]);
        let v = bencode::decode(&q).unwrap();
        assert_eq!(v.get("y").unwrap().as_str(), Some("q"));
        assert_eq!(v.get("q").unwrap().as_str(), Some("get_peers"));
        assert_eq!(v.get("t").unwrap().as_bytes(), Some(&b"aa"[..]));
        let a = v.get("a").unwrap();
        assert_eq!(a.get("id").unwrap().as_bytes(), Some(&[1u8; 20][..]));
        assert_eq!(a.get("info_hash").unwrap().as_bytes(), Some(&[2u8; 20][..]));
    }

    #[test]
    fn compact_nodes() {
        let mut b = node_bytes(1, [1, 2, 3, 4], 6881);
        b.extend(node_bytes(2, [5, 6, 7, 8], 0)); // port 0 dropped
        b.extend(node_bytes(3, [9, 9, 9, 9], 1));
        b.extend_from_slice(&[0; 5]); // trailing junk
        let nodes = parse_compact_nodes(&b);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].id, [1; 20]);
        assert_eq!(nodes[0].addr, "1.2.3.4:6881".parse().unwrap());
        assert_eq!(nodes[1].addr, "9.9.9.9:1".parse().unwrap());
    }

    #[test]
    fn response_with_values_and_nodes() {
        let mut body = b"d1:t2:xx1:y1:r1:rd2:id20:".to_vec();
        body.extend_from_slice(&[7; 20]);
        body.extend_from_slice(b"5:nodes26:");
        body.extend(node_bytes(4, [4, 4, 4, 4], 4444));
        body.extend_from_slice(b"6:valuesl6:");
        body.extend_from_slice(&[10, 0, 0, 1, 0x1a, 0xe1]);
        body.extend_from_slice(b"ee");
        body.extend_from_slice(b"e");
        let r = decode_response(&body).unwrap();
        assert_eq!(r.tid, b"xx");
        assert_eq!(r.peers, vec!["10.0.0.1:6881".parse().unwrap()]);
        assert_eq!(r.nodes.len(), 1);
    }

    #[test]
    fn error_and_garbage_responses() {
        assert!(decode_response(b"d1:t2:xx1:y1:eli201e3:badee").is_err());
        assert!(decode_response(b"d1:y1:r1:rdee").is_err()); // no tid
        assert!(decode_response(b"d1:t2:xx1:y1:qe").is_err());
        assert!(decode_response(b"junk").is_err());
        assert!(decode_response(b"d1:t2:xx1:y1:re").is_err()); // no body
    }

    #[test]
    fn xor_metric_orders_correctly() {
        let target = [0u8; 20];
        let mut near = [0u8; 20];
        near[19] = 1;
        let mut far = [0u8; 20];
        far[0] = 0x80;
        assert!(xor_distance(&near, &target) < xor_distance(&far, &target));
        assert_eq!(xor_distance(&near, &near), [0u8; 20]);
    }

    #[tokio::test]
    async fn lookup_follows_nodes_and_reports_peers() {
        // Two fake DHT nodes: A knows only B; B knows the peers.
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (a_addr, b_addr) = (a.local_addr().unwrap(), b.local_addr().unwrap());

        let b_ip = match b_addr.ip() {
            IpAddr::V4(ip) => ip.octets(),
            _ => unreachable!(),
        };
        let nodes_of_a = node_bytes(9, b_ip, b_addr.port());

        async fn serve(s: UdpSocket, nodes: Option<Vec<u8>>, values: Option<[u8; 6]>) {
            let mut buf = [0u8; 1024];
            loop {
                let Ok((n, from)) = s.recv_from(&mut buf).await else { return };
                let q = bencode::decode(&buf[..n]).unwrap();
                let tid = q.get("t").unwrap().as_bytes().unwrap().to_vec();
                let mut r = vec![("id", Value::Bytes(vec![5; 20]))];
                if let Some(n) = &nodes {
                    r.push(("nodes", Value::Bytes(n.clone())));
                }
                if let Some(v) = values {
                    r.push(("values", Value::List(vec![Value::Bytes(v.to_vec())])));
                }
                let resp = bencode::encode(&Value::dict([
                    ("t", Value::Bytes(tid)),
                    ("y", "r".into()),
                    ("r", Value::dict(r)),
                ]));
                s.send_to(&resp, from).await.unwrap();
            }
        }
        tokio::spawn(serve(a, Some(nodes_of_a), None));
        tokio::spawn(serve(b, None, Some([8, 8, 8, 8, 0x1f, 0x90])));

        let mut found = Vec::new();
        find_peers(&[0; 20], &[a_addr], Duration::from_secs(5), |p| found.extend(p))
            .await
            .unwrap();
        assert_eq!(found, vec!["8.8.8.8:8080".parse::<SocketAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn lookup_ends_when_nodes_are_silent() {
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = silent.local_addr().unwrap();
        let started = Instant::now();
        find_peers(&[0; 20], &[addr], Duration::from_secs(10), |_| panic!("no peers"))
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(8));
    }

    #[tokio::test]
    async fn lookup_without_bootstrap_is_an_error() {
        assert!(find_peers(&[0; 20], &[], Duration::from_secs(1), |_| {}).await.is_err());
    }
}
