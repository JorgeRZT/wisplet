//! Tracker clients: HTTP (BEP 3, compact BEP 23) and UDP (BEP 15).
//!
//! The HTTP client is a ~100-line `HTTP/1.1 + Connection: close` implementation over a
//! raw `TcpStream` — no HTTP crate, no TLS stack. `https://` trackers are announced through
//! the system `curl` (see [`crate::curl`]); if `curl` is missing they just fail like any
//! unreachable tracker.
//!
//! Wire encoding/decoding is kept in pure functions so it can be unit-tested; only
//! [`announce`] touches the network.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{lookup_host, TcpStream, UdpSocket};
use tokio::time::timeout;

use crate::bencode::{self, Value};
use crate::error::{Error, Result};
use crate::util::percent_encode;

const MAX_HTTP_RESPONSE: usize = 1 << 20;
const NUMWANT: i32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    None,
    Completed,
    Started,
    Stopped,
}

impl Event {
    fn udp_code(self) -> u32 {
        match self {
            Event::None => 0,
            Event::Completed => 1,
            Event::Started => 2,
            Event::Stopped => 3,
        }
    }

    fn http_name(self) -> Option<&'static str> {
        match self {
            Event::None => None,
            Event::Completed => Some("completed"),
            Event::Started => Some("started"),
            Event::Stopped => Some("stopped"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnnounceParams {
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub port: u16,
    pub downloaded: u64,
    pub left: u64,
    pub event: Event,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnnounceResponse {
    /// Seconds the tracker asks us to wait before re-announcing.
    pub interval: u32,
    pub peers: Vec<SocketAddr>,
}

/// Announce to any supported tracker URL.
pub async fn announce(url: &str, params: &AnnounceParams, limit: Duration) -> Result<AnnounceResponse> {
    let fut = async {
        if url.starts_with("http://") {
            announce_http(url, params).await
        } else if url.starts_with("https://") {
            announce_https(&crate::curl::bin(), url, params).await
        } else if url.starts_with("udp://") {
            announce_udp(url, params).await
        } else {
            Err(Error::Tracker(format!("unsupported tracker scheme: {url}")))
        }
    };
    timeout(limit, fut)
        .await
        .map_err(|_| Error::Timeout("tracker announce"))?
}

// ---------------------------------------------------------------------------------------
// Compact peer lists
// ---------------------------------------------------------------------------------------

/// Parse BEP 23 compact IPv4 peers (6 bytes each). Trailing garbage is ignored.
pub fn parse_compact_peers_v4(b: &[u8]) -> Vec<SocketAddr> {
    b.as_chunks::<6>().0.iter()
        .filter_map(|c| {
            let port = u16::from_be_bytes([c[4], c[5]]);
            (port != 0).then(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(c[0], c[1], c[2], c[3])), port))
        })
        .collect()
}

/// Parse compact IPv6 peers (18 bytes each).
pub fn parse_compact_peers_v6(b: &[u8]) -> Vec<SocketAddr> {
    b.as_chunks::<18>().0.iter()
        .filter_map(|c| {
            let port = u16::from_be_bytes([c[16], c[17]]);
            let ip: [u8; 16] = c[..16].try_into().unwrap();
            (port != 0).then(|| SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port))
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub struct HttpUrl {
    pub host: String,
    pub port: u16,
    pub path_and_query: String,
}

pub fn parse_http_url(url: &str) -> Result<HttpUrl> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| Error::Tracker(format!("not an http:// url: {url}")))?;
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
        Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
        None => (rest, "/".to_string()),
    };
    let (host, port) = split_host_port(authority, 80)?;
    Ok(HttpUrl {
        host,
        port,
        path_and_query: path,
    })
}

fn split_host_port(authority: &str, default_port: u16) -> Result<(String, u16)> {
    let bad = || Error::Tracker(format!("bad host in tracker url: {authority}"));
    if authority.is_empty() {
        return Err(bad());
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(bad)?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| bad())?,
            None if tail.is_empty() => default_port,
            None => return Err(bad()),
        };
        return Ok((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() => Ok((h.to_string(), p.parse().map_err(|_| bad())?)),
        Some(_) => Err(bad()),
        None => Ok((authority.to_string(), default_port)),
    }
}

/// Path + query string for an announce request.
pub fn build_announce_path(base: &str, p: &AnnounceParams) -> String {
    let sep = if base.contains('?') { '&' } else { '?' };
    let mut s = format!(
        "{base}{sep}info_hash={}&peer_id={}&port={}&uploaded=0&downloaded={}&left={}&compact=1&numwant={}",
        percent_encode(&p.info_hash),
        percent_encode(&p.peer_id),
        p.port,
        p.downloaded,
        p.left,
        NUMWANT
    );
    if let Some(ev) = p.event.http_name() {
        s.push_str("&event=");
        s.push_str(ev);
    }
    s
}

/// Extract the body of an HTTP response, decoding chunked transfer-encoding.
pub fn parse_http_response(raw: &[u8]) -> Result<Vec<u8>> {
    let end = find(raw, b"\r\n\r\n")
        .ok_or_else(|| Error::Tracker("truncated HTTP response".into()))?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        return Err(Error::Tracker(format!("HTTP status: {status}")));
    }
    let mut chunked = false;
    let mut content_length = None;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_ascii_lowercase());
            if k == "transfer-encoding" && v.contains("chunked") {
                chunked = true;
            } else if k == "content-length" {
                content_length = v.parse::<usize>().ok();
            }
        }
    }
    let body = &raw[end + 4..];
    if chunked {
        return decode_chunked(body);
    }
    Ok(match content_length {
        Some(n) if n <= body.len() => body[..n].to_vec(),
        _ => body.to_vec(),
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn decode_chunked(mut b: &[u8]) -> Result<Vec<u8>> {
    let bad = || Error::Tracker("malformed chunked body".into());
    let mut out = Vec::new();
    loop {
        let eol = find(b, b"\r\n").ok_or_else(bad)?;
        let size_str = std::str::from_utf8(&b[..eol]).map_err(|_| bad())?;
        let size = usize::from_str_radix(size_str.split(';').next().unwrap().trim(), 16).map_err(|_| bad())?;
        b = &b[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size + 2 {
            return Err(bad());
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

/// Decode a bencoded tracker response (HTTP body).
pub fn parse_announce_response(body: &[u8]) -> Result<AnnounceResponse> {
    let v = bencode::decode(body)?;
    if let Some(reason) = v.get("failure reason") {
        return Err(Error::Tracker(format!(
            "tracker refused: {}",
            String::from_utf8_lossy(reason.as_bytes().unwrap_or(b"?"))
        )));
    }
    let interval = v
        .get("interval")
        .and_then(Value::as_int)
        .map(|i| i.clamp(0, u32::MAX as i64) as u32)
        .unwrap_or(1800);
    let mut peers = Vec::new();
    match v.get("peers") {
        Some(Value::Bytes(b)) => peers.extend(parse_compact_peers_v4(b)),
        Some(Value::List(list)) => {
            for p in list {
                let ip = p.get("ip").and_then(Value::as_str).and_then(|s| s.parse::<IpAddr>().ok());
                let port = p
                    .get("port")
                    .and_then(Value::as_int)
                    .and_then(|p| u16::try_from(p).ok())
                    .filter(|&p| p != 0);
                if let (Some(ip), Some(port)) = (ip, port) {
                    peers.push(SocketAddr::new(ip, port));
                }
            }
        }
        _ => {}
    }
    if let Some(Value::Bytes(b)) = v.get("peers6") {
        peers.extend(parse_compact_peers_v6(b));
    }
    Ok(AnnounceResponse { interval, peers })
}

async fn announce_http(url: &str, params: &AnnounceParams) -> Result<AnnounceResponse> {
    let u = parse_http_url(url)?;
    let path = build_announce_path(&u.path_and_query, params);
    let host_header = if u.port == 80 { u.host.clone() } else { format!("{}:{}", u.host, u.port) };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: wisplet/0.1\r\n\
         Accept-Encoding: identity\r\nConnection: close\r\n\r\n"
    );
    let mut stream = TcpStream::connect((u.host.as_str(), u.port)).await?;
    stream.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    (&mut stream)
        .take(MAX_HTTP_RESPONSE as u64)
        .read_to_end(&mut raw)
        .await?;
    parse_announce_response(&parse_http_response(&raw)?)
}

/// `https://` tracker through `curl`. Same query string and response format as plain HTTP.
pub async fn announce_https(curl_bin: &str, url: &str, params: &AnnounceParams) -> Result<AnnounceResponse> {
    let full = build_announce_path(url, params);
    let req = crate::curl::Request::get(&full, MAX_HTTP_RESPONSE, Duration::from_secs(15));
    let body = crate::curl::fetch(curl_bin, req).await?;
    parse_announce_response(&body)
}

// ---------------------------------------------------------------------------------------
// UDP (BEP 15)
// ---------------------------------------------------------------------------------------

const UDP_PROTOCOL_ID: u64 = 0x0417_2710_1980;

pub fn encode_udp_connect(tid: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&UDP_PROTOCOL_ID.to_be_bytes());
    // action 0 = connect
    b[12..16].copy_from_slice(&tid.to_be_bytes());
    b
}

fn check_udp_header(buf: &[u8], action: u32, tid: u32) -> Result<()> {
    if buf.len() < 8 {
        return Err(Error::Tracker("short UDP response".into()));
    }
    let got_action = u32::from_be_bytes(buf[..4].try_into().unwrap());
    let got_tid = u32::from_be_bytes(buf[4..8].try_into().unwrap());
    if got_action == 3 {
        return Err(Error::Tracker(format!(
            "tracker error: {}",
            String::from_utf8_lossy(&buf[8..])
        )));
    }
    if got_tid != tid {
        return Err(Error::Tracker("transaction id mismatch".into()));
    }
    if got_action != action {
        return Err(Error::Tracker(format!("unexpected UDP action {got_action}")));
    }
    Ok(())
}

pub fn decode_udp_connect(buf: &[u8], tid: u32) -> Result<u64> {
    check_udp_header(buf, 0, tid)?;
    if buf.len() < 16 {
        return Err(Error::Tracker("short connect response".into()));
    }
    Ok(u64::from_be_bytes(buf[8..16].try_into().unwrap()))
}

pub fn encode_udp_announce(conn_id: u64, tid: u32, key: u32, p: &AnnounceParams) -> [u8; 98] {
    let mut b = [0u8; 98];
    b[..8].copy_from_slice(&conn_id.to_be_bytes());
    b[8..12].copy_from_slice(&1u32.to_be_bytes());
    b[12..16].copy_from_slice(&tid.to_be_bytes());
    b[16..36].copy_from_slice(&p.info_hash);
    b[36..56].copy_from_slice(&p.peer_id);
    b[56..64].copy_from_slice(&p.downloaded.to_be_bytes());
    b[64..72].copy_from_slice(&p.left.to_be_bytes());
    // uploaded (72..80) = 0
    b[80..84].copy_from_slice(&p.event.udp_code().to_be_bytes());
    // ip (84..88) = 0 -> use sender address
    b[88..92].copy_from_slice(&key.to_be_bytes());
    b[92..96].copy_from_slice(&NUMWANT.to_be_bytes());
    b[96..98].copy_from_slice(&p.port.to_be_bytes());
    b
}

/// Decode an announce response. `v6` selects 18-byte peer entries (when the tracker was
/// reached over IPv6).
pub fn decode_udp_announce(buf: &[u8], tid: u32, v6: bool) -> Result<AnnounceResponse> {
    check_udp_header(buf, 1, tid)?;
    if buf.len() < 20 {
        return Err(Error::Tracker("short announce response".into()));
    }
    let interval = u32::from_be_bytes(buf[8..12].try_into().unwrap());
    let peers = if v6 {
        parse_compact_peers_v6(&buf[20..])
    } else {
        parse_compact_peers_v4(&buf[20..])
    };
    Ok(AnnounceResponse { interval, peers })
}

async fn udp_exchange(sock: &UdpSocket, req: &[u8], buf: &mut [u8]) -> Result<usize> {
    // BEP 15 suggests 15*2^n seconds; we use a shorter schedule to fail over quickly.
    for attempt in 0..3u32 {
        sock.send(req).await?;
        let wait = Duration::from_secs(4 << attempt);
        if let Ok(r) = timeout(wait, sock.recv(buf)).await {
            return Ok(r?);
        }
    }
    Err(Error::Timeout("UDP tracker"))
}

async fn announce_udp(url: &str, params: &AnnounceParams) -> Result<AnnounceResponse> {
    let rest = url.strip_prefix("udp://").unwrap();
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    let (host, port) = split_host_port(authority, 80)?;
    let target = lookup_host((host.as_str(), port))
        .await?
        .next()
        .ok_or_else(|| Error::Tracker(format!("cannot resolve {host}")))?;
    let bind: SocketAddr = if target.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().unwrap();
    let sock = UdpSocket::bind(bind).await?;
    sock.connect(target).await?;

    let mut buf = [0u8; 2048];
    let tid = fastrand::u32(..);
    let n = udp_exchange(&sock, &encode_udp_connect(tid), &mut buf).await?;
    let conn_id = decode_udp_connect(&buf[..n], tid)?;

    let tid = fastrand::u32(..);
    let req = encode_udp_announce(conn_id, tid, fastrand::u32(..), params);
    let n = udp_exchange(&sock, &req, &mut buf).await?;
    decode_udp_announce(&buf[..n], tid, target.is_ipv6())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(event: Event) -> AnnounceParams {
        AnnounceParams {
            info_hash: [0xab; 20],
            peer_id: *b"-TD0100-abcdefghijkl",
            port: 6881,
            downloaded: 10,
            left: 20,
            event,
        }
    }

    #[test]
    fn compact_v4_peers() {
        let b = [1, 2, 3, 4, 0x1a, 0xe1, 5, 6, 7, 8, 0, 0, 9, 9];
        let peers = parse_compact_peers_v4(&b);
        // zero port filtered, trailing partial entry ignored
        assert_eq!(peers, vec!["1.2.3.4:6881".parse().unwrap()]);
    }

    #[test]
    fn compact_v6_peers() {
        let mut b = vec![0u8; 18];
        b[15] = 1;
        b[16] = 0x1a;
        b[17] = 0xe1;
        assert_eq!(parse_compact_peers_v6(&b), vec!["[::1]:6881".parse().unwrap()]);
    }

    #[test]
    fn url_parsing() {
        assert_eq!(
            parse_http_url("http://tracker.example:8080/announce?pk=1").unwrap(),
            HttpUrl { host: "tracker.example".into(), port: 8080, path_and_query: "/announce?pk=1".into() }
        );
        assert_eq!(
            parse_http_url("http://t.example").unwrap(),
            HttpUrl { host: "t.example".into(), port: 80, path_and_query: "/".into() }
        );
        assert_eq!(
            parse_http_url("http://t.example?x=1").unwrap().path_and_query,
            "/?x=1"
        );
        assert_eq!(
            parse_http_url("http://[::1]:81/a").unwrap(),
            HttpUrl { host: "::1".into(), port: 81, path_and_query: "/a".into() }
        );
        assert!(parse_http_url("https://x/").is_err());
        assert!(parse_http_url("http://:80/").is_err());
        assert!(parse_http_url("http://h:notaport/").is_err());
    }

    #[test]
    fn announce_path_contains_all_parameters() {
        let p = build_announce_path("/announce", &params(Event::Started));
        assert!(p.starts_with("/announce?info_hash=%AB%AB"));
        assert!(p.contains("&peer_id=-TD0100-abcdefghijkl&"));
        assert!(p.contains("&port=6881&"));
        assert!(p.contains("&downloaded=10&left=20&"));
        assert!(p.contains("&compact=1"));
        assert!(p.ends_with("&event=started"));
        assert!(!build_announce_path("/a", &params(Event::None)).contains("event"));
        // existing query string is extended, not replaced
        assert!(build_announce_path("/a?key=1", &params(Event::None)).starts_with("/a?key=1&info_hash="));
    }

    #[test]
    fn http_response_plain_and_chunked() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA";
        assert_eq!(parse_http_response(plain).unwrap(), b"hello");
        let no_len = b"HTTP/1.0 200 OK\r\n\r\nbody";
        assert_eq!(parse_http_response(no_len).unwrap(), b"body");
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_http_response(chunked).unwrap(), b"abcde");
    }

    #[test]
    fn http_response_errors() {
        assert!(parse_http_response(b"HTTP/1.1 404 Not Found\r\n\r\n").is_err());
        assert!(parse_http_response(b"HTTP/1.1 200 OK\r\n").is_err());
        assert!(parse_http_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").is_err());
        assert!(parse_http_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab").is_err());
    }

    #[test]
    fn announce_response_compact() {
        let mut body = b"d8:intervali900e5:peers6:".to_vec();
        body.extend_from_slice(&[10, 0, 0, 1, 0x1f, 0x90]);
        body.push(b'e');
        let r = parse_announce_response(&body).unwrap();
        assert_eq!(r.interval, 900);
        assert_eq!(r.peers, vec!["10.0.0.1:8080".parse().unwrap()]);
    }

    #[test]
    fn announce_response_dictionary_model_and_peers6() {
        let mut body = b"d8:intervali60e5:peersld2:ip7:1.2.3.44:porti80eed2:ip3:bad4:porti1eee6:peers618:".to_vec();
        let mut v6 = vec![0u8; 18];
        v6[15] = 1;
        v6[17] = 7;
        body.extend_from_slice(&v6);
        body.push(b'e');
        let r = parse_announce_response(&body).unwrap();
        assert_eq!(
            r.peers,
            vec!["1.2.3.4:80".parse().unwrap(), "[::1]:7".parse().unwrap()]
        );
    }

    #[test]
    fn announce_response_failure_and_defaults() {
        let e = parse_announce_response(b"d14:failure reason7:blockede").unwrap_err();
        assert!(e.to_string().contains("blocked"));
        let r = parse_announce_response(b"de").unwrap();
        assert_eq!(r.interval, 1800);
        assert!(r.peers.is_empty());
        assert!(parse_announce_response(b"nonsense").is_err());
    }

    #[test]
    fn udp_connect_roundtrip() {
        let req = encode_udp_connect(0xdead_beef);
        assert_eq!(&req[..8], &[0, 0, 4, 0x17, 0x27, 0x10, 0x19, 0x80]);
        assert_eq!(&req[8..12], &[0, 0, 0, 0]);
        assert_eq!(&req[12..], &0xdead_beefu32.to_be_bytes());

        let mut resp = vec![];
        resp.extend_from_slice(&0u32.to_be_bytes());
        resp.extend_from_slice(&0xdead_beefu32.to_be_bytes());
        resp.extend_from_slice(&42u64.to_be_bytes());
        assert_eq!(decode_udp_connect(&resp, 0xdead_beef).unwrap(), 42);
        assert!(decode_udp_connect(&resp, 1).is_err()); // wrong tid
        assert!(decode_udp_connect(&resp[..10], 0xdead_beef).is_err());
    }

    #[test]
    fn udp_announce_layout() {
        let p = params(Event::Started);
        let b = encode_udp_announce(7, 9, 11, &p);
        assert_eq!(&b[..8], &7u64.to_be_bytes());
        assert_eq!(&b[8..12], &1u32.to_be_bytes());
        assert_eq!(&b[12..16], &9u32.to_be_bytes());
        assert_eq!(&b[16..36], &[0xab; 20]);
        assert_eq!(&b[36..56], &p.peer_id);
        assert_eq!(&b[56..64], &10u64.to_be_bytes());
        assert_eq!(&b[64..72], &20u64.to_be_bytes());
        assert_eq!(&b[80..84], &2u32.to_be_bytes());
        assert_eq!(&b[88..92], &11u32.to_be_bytes());
        assert_eq!(&b[92..96], &NUMWANT.to_be_bytes());
        assert_eq!(&b[96..98], &6881u16.to_be_bytes());
    }

    #[test]
    fn udp_announce_response_decoding() {
        let mut resp = vec![];
        resp.extend_from_slice(&1u32.to_be_bytes());
        resp.extend_from_slice(&5u32.to_be_bytes());
        resp.extend_from_slice(&1800u32.to_be_bytes());
        resp.extend_from_slice(&3u32.to_be_bytes());
        resp.extend_from_slice(&4u32.to_be_bytes());
        resp.extend_from_slice(&[1, 1, 1, 1, 0, 80]);
        let r = decode_udp_announce(&resp, 5, false).unwrap();
        assert_eq!(r.interval, 1800);
        assert_eq!(r.peers, vec!["1.1.1.1:80".parse().unwrap()]);
        assert!(decode_udp_announce(&resp[..15], 5, false).is_err());
    }

    #[test]
    fn udp_error_action_surfaces_message() {
        let mut resp = vec![];
        resp.extend_from_slice(&3u32.to_be_bytes());
        resp.extend_from_slice(&5u32.to_be_bytes());
        resp.extend_from_slice(b"torrent not registered");
        let e = decode_udp_announce(&resp, 5, false).unwrap_err();
        assert!(e.to_string().contains("not registered"));
    }

    #[tokio::test]
    async fn http_announce_against_local_server() {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut req = vec![0u8; 4096];
            let n = s.read(&mut req).await.unwrap();
            let req = String::from_utf8_lossy(&req[..n]).to_string();
            let mut body = b"d8:intervali120e5:peers6:".to_vec();
            body.extend_from_slice(&[127, 0, 0, 1, 0x1f, 0x90]);
            body.push(b'e');
            let resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            s.write_all(resp.as_bytes()).await.unwrap();
            s.write_all(&body).await.unwrap();
            req
        });
        let url = format!("http://{addr}/announce");
        let r = announce(&url, &params(Event::Started), Duration::from_secs(5)).await.unwrap();
        assert_eq!(r.interval, 120);
        assert_eq!(r.peers, vec!["127.0.0.1:8080".parse().unwrap()]);
        let req = server.await.unwrap();
        assert!(req.starts_with("GET /announce?info_hash="));
        assert!(req.contains("event=started"));
    }

    #[tokio::test]
    async fn udp_announce_against_local_server() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = server.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 16);
            let mut resp = vec![0, 0, 0, 0];
            resp.extend_from_slice(&buf[12..16]);
            resp.extend_from_slice(&0x1234u64.to_be_bytes());
            server.send_to(&resp, from).await.unwrap();

            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 98);
            assert_eq!(&buf[..8], &0x1234u64.to_be_bytes());
            let mut resp = 1u32.to_be_bytes().to_vec();
            resp.extend_from_slice(&buf[12..16]);
            resp.extend_from_slice(&900u32.to_be_bytes());
            resp.extend_from_slice(&[0; 8]);
            resp.extend_from_slice(&[9, 9, 9, 9, 0x1a, 0xe1]);
            server.send_to(&resp, from).await.unwrap();
        });
        let url = format!("udp://{addr}/announce");
        let r = announce(&url, &params(Event::None), Duration::from_secs(5)).await.unwrap();
        assert_eq!(r.interval, 900);
        assert_eq!(r.peers, vec!["9.9.9.9:6881".parse().unwrap()]);
    }

    #[tokio::test]
    async fn unsupported_scheme_is_rejected() {
        for url in ["wss://t/a", "ftp://t/a", "file:///etc/passwd", "t/a"] {
            let e = announce(url, &params(Event::None), Duration::from_secs(1)).await;
            assert!(matches!(e, Err(Error::Tracker(_))), "{url}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn https_tracker_is_announced_through_curl() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // fake curl: records its arguments and answers with a bencoded compact response
        let script = dir.path().join("curl");
        // 1.2.3.4:6881 and 5.6.7.8:6882 as compact peers, written as shell `printf` octal escapes
        let body = r"d8:intervali900e5:peers12:\001\002\003\004\032\341\005\006\007\010\032\342e";
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}/args'\nprintf '{}'\n", dir.path().display(), body),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let r = announce_https(script.to_str().unwrap(), "https://tracker.example/announce", &params(Event::Started))
            .await
            .unwrap();
        assert_eq!(r.interval, 900);
        assert_eq!(r.peers, vec!["1.2.3.4:6881".parse().unwrap(), "5.6.7.8:6882".parse().unwrap()]);
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        let url = args.lines().skip_while(|l| *l != "--url").nth(1).unwrap();
        assert!(url.starts_with("https://tracker.example/announce?info_hash="), "{url}");
        assert!(url.contains("&event=started") && url.contains("&compact=1"), "{url}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn https_tracker_errors_surface_as_tracker_failures() {
        // curl missing: a clear error, not a panic or a hang
        let e = announce_https("/definitely/not/curl", "https://t.example/a", &params(Event::None)).await.unwrap_err();
        assert!(e.to_string().contains("was not found"), "{e}");
        // garbage instead of bencode
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("curl");
        std::fs::write(&script, "#!/bin/sh\nprintf 'not bencode'\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert!(announce_https(script.to_str().unwrap(), "https://t.example/a", &params(Event::None)).await.is_err());
    }
}
