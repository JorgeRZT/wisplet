//! Torrent search over two kinds of source.
//!
//! BitTorrent itself cannot search by text (the DHT only maps an info-hash to peers), so this asks
//! an index. The project ships exactly two sources and no third-party torrent site:
//!
//! * **`archive`** - the [Internet Archive](https://archive.org), which hosts free and public-domain
//!   material (software, books, audio, films) and offers a `.torrent` for each item;
//! * **`torznab`** - *your own* indexer manager (Jackett, Prowlarr or anything that speaks the
//!   [Torznab](https://torznab.github.io/spec-1.3-draft/) API), set with the environment variable
//!   `WISPLET_TORZNAB_URL`. Which indexers sit behind it is your decision and your
//!   responsibility, exactly as in Sonarr or Radarr.
//!
//! Each [`Source`] builds its request and parses the answer into [`SearchResult`]s; the sources
//! are queried at once and one failing never affects the others; results are merged by info-hash,
//! topped up with public trackers, filtered of dead torrents and sorted by seeders.
//!
//! Sources are entries of a small registry ([`SourceDef`]), so more can be added without touching
//! the rest: with the `extra-sources` Cargo feature the crate also compiles `src/search_extra.rs`,
//! a file that is *not part of this repository* where you can register your own sources
//! (see `docs/EXTENDING.md`). HTTPS goes
//! through the system `curl` ([`crate::curl`]); plain `http://` is accepted only for a loopback
//! Torznab endpoint. Parsing is pure (bytes in, results out) so it is unit-tested with canned answers.

use std::collections::HashMap;
use std::time::Duration;

use tokio::task::JoinSet;

use crate::curl::{self, Request};
use crate::error::{Error, Result};
use crate::json::{self, Json};
use crate::magnet::Magnet;
use crate::util::{hex_encode, percent_encode};

/// Answers larger than this are refused.
const MAX_RESPONSE: usize = 4 << 20;
/// Longest accepted search text.
const MAX_QUERY: usize = 200;
/// Largest `.torrent` fetched from a search result.
const MAX_TORRENT: usize = 16 << 20;

/// Public trackers added to magnets that lack them (`udp` first, then `http(s)` for networks that
/// block UDP).
pub const TRACKERS: [&str; 17] = [
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://exodus.desync.com:6969/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.openbittorrent.com:6969/announce",
    "udp://tracker.tiny-vps.com:6969/announce",
    "udp://explodie.org:6969/announce",
    "udp://open.tracker.cl:1337/announce",
    "udp://opentracker.io:6969/announce",
    "udp://tracker.dler.org:6969/announce",
    "http://tracker.opentrackr.org:1337/announce",
    "https://tracker.tamersunion.org:443/announce",
    "https://tracker.gbitt.info:443/announce",
    "http://tracker.gbitt.info:80/announce",
    "https://tracker1.520.jp:443/announce",
    "http://tracker.bt4g.com:2095/announce",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    /// Source key(s), `+`-joined when several indexes returned the same torrent.
    pub source: String,
    pub name: String,
    /// Bytes; 0 when unknown.
    pub size: u64,
    pub seeders: u32,
    pub leechers: u32,
    /// 40 lowercase hex characters, or empty when the source gives none.
    pub info_hash: String,
    /// Magnet link, or empty.
    pub magnet: String,
    /// Page or `.torrent` URL, or empty.
    pub url: String,
}

/// Name of the environment variable that points at your Torznab endpoint.
pub const TORZNAB_ENV: &str = "WISPLET_TORZNAB_URL";

/// `http://` is only trusted for a Torznab server on this very machine (Jackett and Prowlarr
/// listen on localhost by default); everything else must be `https://`.
pub fn is_loopback_http(url: &str) -> bool {
    ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| url.strip_prefix(p).is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/', '?'])))
}

/// What to fetch for one source.
pub struct SourceRequest {
    pub url: String,
    pub body: Option<Vec<u8>>,
    pub allow_http: bool,
}

/// Everything the engine needs to know about one search source.
pub struct SourceDef {
    /// Short stable key (`--source` and the MCP `sources` argument).
    pub key: &'static str,
    pub description: &'static str,
    /// The request for a query, or `None` when the source is not available (not configured).
    /// The second argument is the Torznab URL from the configuration.
    pub request: fn(&str, Option<&str>) -> Option<SourceRequest>,
    pub parse: fn(&str) -> Result<Vec<SearchResult>>,
}

/// A search source: a handle on its [`SourceDef`]. Equality is by key.
#[derive(Clone, Copy)]
pub struct Source(&'static SourceDef);

impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.0.key == other.0.key
    }
}
impl Eq for Source {}
impl std::hash::Hash for Source {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.key.hash(state)
    }
}
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Source({})", self.0.key)
    }
}

const ARCHIVE_DEF: SourceDef = SourceDef {
    key: "archive",
    description: "Internet Archive: free and public-domain items that offer a .torrent file (always available)",
    request: archive_request,
    parse: parse_archive,
};

const TORZNAB_DEF: SourceDef = SourceDef {
    key: "torznab",
    description: "Your own Jackett / Prowlarr / Torznab server, set with WISPLET_TORZNAB_URL (off until configured)",
    request: torznab_request,
    parse: parse_torznab,
};

#[cfg(feature = "extra-sources")]
#[path = "search_extra.rs"]
mod extra;

// The documented example of an extension file is compiled and tested with the crate, so the
// instructions in docs/EXTENDING.md cannot drift from the real API.
#[cfg(test)]
#[path = "../docs/search_extra.example.rs"]
mod example_extension;

/// Sources compiled in by the `extra-sources` feature (none in the default build).
fn extra_sources() -> Vec<Source> {
    #[cfg(feature = "extra-sources")]
    return extra::sources();
    #[cfg(not(feature = "extra-sources"))]
    Vec::new()
}

impl Source {
    pub const ARCHIVE: Source = Source(&ARCHIVE_DEF);
    pub const TORZNAB: Source = Source(&TORZNAB_DEF);

    /// Wrap a definition (used by extension modules).
    pub const fn from_def(def: &'static SourceDef) -> Source {
        Source(def)
    }

    /// Every source of this build, in a fixed order.
    pub fn all() -> Vec<Source> {
        let mut v = vec![Source::ARCHIVE, Source::TORZNAB];
        v.extend(extra_sources());
        v
    }

    pub fn key(self) -> &'static str {
        self.0.key
    }

    pub fn describe(self) -> &'static str {
        self.0.description
    }

    pub fn from_key(k: &str) -> Option<Source> {
        Source::all().into_iter().find(|s| s.key().eq_ignore_ascii_case(k.trim()))
    }

    /// The request for this query, or `None` when the source is not configured.
    pub fn request(self, q: &str, torznab: Option<&str>) -> Option<SourceRequest> {
        (self.0.request)(q, torznab)
    }

    pub fn parse(self, body: &[u8]) -> Result<Vec<SearchResult>> {
        let text = std::str::from_utf8(body).map_err(|_| Error::Failed("response is not UTF-8".into()))?;
        (self.0.parse)(text)
    }
}

fn archive_request(q: &str, _torznab: Option<&str>) -> Option<SourceRequest> {
    let lucene = format!("title:({q}) AND mediatype:(movies OR audio OR texts OR software)");
    Some(SourceRequest {
        url: format!(
            "https://archive.org/advancedsearch.php?q={}&fl%5B%5D=identifier&rows=50&output=json&sort%5B%5D=downloads+desc",
            percent_encode(lucene.as_bytes())
        ),
        body: None,
        allow_http: false,
    })
}

fn torznab_request(q: &str, torznab: Option<&str>) -> Option<SourceRequest> {
    let base = torznab?.trim();
    if !(base.starts_with("https://") || is_loopback_http(base)) {
        return None;
    }
    let sep = if base.contains('?') { '&' } else { '?' };
    Some(SourceRequest {
        url: format!("{base}{sep}t=search&q={}", percent_encode(q.as_bytes())),
        body: None,
        allow_http: is_loopback_http(base),
    })
}

// ------------------------------------------------------------------ helpers

fn bad_json(e: json::JsonError) -> Error {
    Error::Failed(e.to_string())
}

fn clamp32(n: u64) -> u32 {
    n.min(u32::MAX as u64) as u32
}

/// 40 hex characters or 32 base32 characters -> lowercase hex.
pub fn normalize_hash(h: &str) -> Option<String> {
    let h = h.trim();
    if !(h.len() == 40 || h.len() == 32) || !h.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Magnet::parse(&format!("magnet:?xt=urn:btih:{h}")).ok().map(|m| hex_encode(&m.info_hash))
}

/// A magnet link with the default public trackers.
pub fn make_magnet(hash_hex: &str, name: &str) -> String {
    let mut m = format!("magnet:?xt=urn:btih:{hash_hex}&dn={}", percent_encode(name.as_bytes()));
    for t in TRACKERS {
        m.push_str("&tr=");
        m.push_str(&percent_encode(t.as_bytes()));
    }
    m
}

/// Append the default trackers a magnet does not already list.
pub fn enrich_magnet(m: &str) -> String {
    if !m.starts_with("magnet:") {
        return m.to_string();
    }
    let have: Vec<String> = Magnet::parse(m).map(|x| x.trackers.iter().map(|t| t.to_lowercase()).collect()).unwrap_or_default();
    let mut out = m.to_string();
    for t in TRACKERS {
        if !have.contains(&t.to_lowercase()) {
            out.push_str("&tr=");
            out.push_str(&percent_encode(t.as_bytes()));
        }
    }
    out
}

/// "1.5 GiB", "700MB", "12 KB" -> bytes (1024-based; 0 when unreadable).
pub fn parse_size(text: &str) -> u64 {
    let t = text.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ',')).unwrap_or(t.len());
    let Ok(n) = t[..split].replace(',', ".").parse::<f64>() else { return 0 };
    let mult = match t[split..].trim().to_ascii_uppercase().as_str() {
        "B" | "" => 1.0,
        "KB" | "KIB" | "K" => 1024.0,
        "MB" | "MIB" | "M" => 1024.0 * 1024.0,
        "GB" | "GIB" | "G" => 1024.0 * 1024.0 * 1024.0,
        "TB" | "TIB" | "T" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return 0,
    };
    (n * mult) as u64
}

#[cfg_attr(not(feature = "extra-sources"), allow(dead_code))] // used by the optional extra-sources module
fn field_u64(o: &Json, key: &str) -> u64 {
    o.get(key).and_then(Json::as_u64).unwrap_or(0)
}

fn field_str<'a>(o: &'a Json, key: &str) -> &'a str {
    o.get(key).and_then(Json::as_str).unwrap_or("")
}

// ------------------------------------------------------------------ sources

fn parse_archive(text: &str) -> Result<Vec<SearchResult>> {
    let j = Json::parse(text).map_err(bad_json)?;
    let docs = j
        .get("response")
        .and_then(|r| r.get("docs"))
        .and_then(Json::as_array)
        .ok_or_else(|| Error::Failed("unexpected answer".into()))?;
    let mut out = Vec::new();
    for d in docs {
        let id = field_str(d, "identifier");
        // identifiers go into a URL path: keep only the characters archive.org uses
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
            continue;
        }
        out.push(SearchResult {
            source: "archive".into(),
            name: id.into(),
            size: 0,
            seeders: 0,
            leechers: 0,
            info_hash: String::new(),
            magnet: String::new(),
            url: format!("https://archive.org/download/{id}/{id}_archive.torrent"),
        });
    }
    Ok(out)
}

// ---- Torznab (RSS + torznab:attr): a tiny tag extractor is enough for this feed

fn xml_unescape(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("<![CDATA[").and_then(|x| x.strip_suffix("]]>")).unwrap_or(s);
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Text of the first `<tag ...>text</tag>` in `xml` (tags with a namespace prefix included).
fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let mut from = 0;
    while let Some(i) = xml[from..].find(&open) {
        let start = from + i + open.len();
        match xml[start..].chars().next() {
            Some('>') | Some(' ') => {
                let gt = xml[start..].find('>')? + start + 1;
                let end = xml[gt..].find(&format!("</{tag}>"))? + gt;
                return Some(xml_unescape(&xml[gt..end]));
            }
            _ => from = start, // e.g. <title> vs <titleX>
        }
    }
    None
}

/// All `<torznab:attr name="..." value="..."/>` of one `<item>`.
fn torznab_attrs(item: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for chunk in item.split("<torznab:attr ").skip(1) {
        let tag = chunk.split("/>").next().unwrap_or("");
        let get = |key: &str| {
            let start = tag.find(&format!("{key}=\""))? + key.len() + 2;
            let end = tag[start..].find('"')? + start;
            Some(xml_unescape(&tag[start..end]))
        };
        if let (Some(n), Some(v)) = (get("name"), get("value")) {
            out.entry(n).or_insert(v);
        }
    }
    out
}

fn parse_torznab(text: &str) -> Result<Vec<SearchResult>> {
    if !text.contains("<rss") && !text.contains("<error") {
        return Err(Error::Failed("unexpected answer (is the URL a Torznab endpoint?)".into()));
    }
    if let Some(err) = text.split("<error ").nth(1) {
        let desc = err.split("description=\"").nth(1).and_then(|d| d.split('"').next()).unwrap_or("error");
        return Err(Error::Failed(format!("torznab server: {}", xml_unescape(desc))));
    }
    let mut out = Vec::new();
    for item in text.split("<item>").skip(1) {
        let item = item.split("</item>").next().unwrap_or("");
        let name = tag_text(item, "title").unwrap_or_default();
        let attrs = torznab_attrs(item);
        let magnet_attr = attrs.get("magneturl").cloned().unwrap_or_default();
        let hash = attrs
            .get("infohash")
            .and_then(|h| normalize_hash(h))
            .or_else(|| Magnet::parse(&magnet_attr).ok().map(|m| hex_encode(&m.info_hash)));
        let link = tag_text(item, "link").unwrap_or_default();
        if name.is_empty() || (hash.is_none() && magnet_attr.is_empty() && link.is_empty()) {
            continue;
        }
        let num = |k: &str| attrs.get(k).and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0);
        let (seeders, peers) = (num("seeders"), num("peers"));
        let magnet = if !magnet_attr.is_empty() {
            magnet_attr
        } else {
            hash.as_deref().map(|h| make_magnet(h, &name)).unwrap_or_default()
        };
        out.push(SearchResult {
            source: "torznab".into(),
            size: tag_text(item, "size").and_then(|v| v.trim().parse().ok()).or_else(|| attrs.get("size").and_then(|v| v.parse().ok())).unwrap_or(0),
            seeders: clamp32(seeders),
            // Torznab reports `peers` (seeders + leechers); some servers give `leechers` directly
            leechers: clamp32(attrs.get("leechers").and_then(|v| v.parse().ok()).unwrap_or_else(|| peers.saturating_sub(seeders))),
            info_hash: hash.unwrap_or_default(),
            magnet,
            url: link,
            name,
        });
    }
    Ok(out)
}

// ------------------------------------------------------------------ merging

/// Merge duplicates (same info-hash, else same URL), complete the magnets with public trackers,
/// drop dead torrents unless asked not to, sort by seeders and cut to `limit`.
pub fn finalize(all: Vec<SearchResult>, include_dead: bool, limit: usize) -> Vec<SearchResult> {
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut merged: Vec<SearchResult> = Vec::new();
    for mut r in all {
        r.magnet = enrich_magnet(&r.magnet);
        let key = if !r.info_hash.is_empty() { r.info_hash.clone() } else if !r.url.is_empty() { r.url.clone() } else { continue };
        match by_key.get(&key) {
            Some(&i) => {
                let m = &mut merged[i];
                if !m.source.split('+').any(|s| s == r.source) {
                    m.source = format!("{}+{}", m.source, r.source);
                }
                if r.seeders > m.seeders {
                    (m.seeders, m.leechers) = (r.seeders, r.leechers);
                }
                if m.size == 0 {
                    m.size = r.size;
                }
                if m.magnet.is_empty() {
                    m.magnet = r.magnet;
                }
                if m.url.is_empty() {
                    m.url = r.url;
                }
            }
            None => {
                by_key.insert(key, merged.len());
                merged.push(r);
            }
        }
    }
    // a magnet nobody seeds cannot even fetch its metadata; results without a magnet are kept
    merged.retain(|r| include_dead || r.magnet.is_empty() || r.seeders > 0);
    merged.sort_by(|a, b| {
        b.seeders.cmp(&a.seeders).then(b.leechers.cmp(&a.leechers)).then_with(|| a.name.cmp(&b.name))
    });
    merged.truncate(limit);
    merged
}

// ------------------------------------------------------------------ running a search

#[derive(Debug, Clone)]
pub struct Options {
    /// Sources to ask; empty means all.
    pub sources: Vec<Source>,
    pub include_dead: bool,
    pub limit: usize,
    /// `curl` to run (see [`curl::bin`]).
    pub curl: String,
    /// Torznab endpoint (see [`TORZNAB_ENV`]); `None` leaves that source out.
    pub torznab: Option<String>,
    /// Per-source time limit.
    pub timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            sources: Vec::new(),
            include_dead: false,
            limit: 50,
            curl: curl::bin(),
            torznab: std::env::var(TORZNAB_ENV).ok().filter(|v| !v.trim().is_empty()),
            timeout: Duration::from_secs(15),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Report {
    pub results: Vec<SearchResult>,
    /// One line per source: `ok (12)` or `error: ...`.
    pub status: Vec<(Source, String)>,
}

/// Search text must be non-empty, reasonably short and free of control characters.
pub fn check_query(q: &str) -> Result<String> {
    let q = q.trim();
    if q.is_empty() {
        return Err(Error::Failed("the search text is empty".into()));
    }
    if q.chars().count() > MAX_QUERY || q.chars().any(char::is_control) {
        return Err(Error::Failed("invalid search text".into()));
    }
    Ok(q.to_string())
}

/// Ask every selected source at once and merge the answers.
pub async fn search(query: &str, o: &Options) -> Result<Report> {
    let q = check_query(query)?;
    // No explicit choice = every source that is available (Torznab only when it is configured).
    let sources: Vec<Source> = if o.sources.is_empty() {
        Source::all().into_iter().filter(|s| s.request("x", o.torznab.as_deref()).is_some()).collect()
    } else {
        o.sources.clone()
    };
    let mut set = JoinSet::new();
    let mut status: HashMap<Source, String> = HashMap::new();
    for &s in &sources {
        let Some(r) = s.request(&q, o.torznab.as_deref()) else {
            status.insert(s, format!("error: not configured (set {TORZNAB_ENV} to your Jackett/Prowlarr Torznab URL; it must be https:// or on localhost)"));
            continue;
        };
        let (bin, timeout) = (o.curl.clone(), o.timeout);
        set.spawn(async move {
            let req = Request {
                url: &r.url,
                body: r.body.as_deref(),
                content_type: "application/json",
                max_bytes: MAX_RESPONSE,
                timeout,
                allow_http: r.allow_http,
            };
            let res = curl::fetch(&bin, req).await.and_then(|b| s.parse(&b));
            (s, res)
        });
    }
    let mut answers: HashMap<Source, Vec<SearchResult>> = HashMap::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((s, Ok(r))) => {
                status.insert(s, format!("ok ({})", r.len()));
                answers.insert(s, r);
            }
            Ok((s, Err(e))) => {
                status.insert(s, format!("error: {e}"));
            }
            Err(e) => vlog_join(e),
        }
    }
    // The sources answer in any order; merging them in a fixed one keeps the output deterministic
    // (which index is listed first in a merged result, which duplicate wins a tie).
    let all: Vec<SearchResult> = sources.iter().flat_map(|s| answers.remove(s).unwrap_or_default()).collect();
    let status = sources.iter().map(|s| (*s, status.remove(s).unwrap_or_else(|| "error: task failed".into()))).collect();
    Ok(Report { results: finalize(all, o.include_dead, o.limit), status })
}

fn vlog_join(e: tokio::task::JoinError) {
    crate::vlog!("search task failed: {e}");
}

/// Download the `.torrent` a result points to (for results that have no magnet).
pub async fn fetch_torrent(url: &str, curl_bin: &str) -> Result<Vec<u8>> {
    curl::fetch(curl_bin, Request::get(url, MAX_TORRENT, Duration::from_secs(30))).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(source: &str, name: &str, hash: &str, seeders: u32, leechers: u32) -> SearchResult {
        SearchResult {
            source: source.into(),
            name: name.into(),
            size: 0,
            seeders,
            leechers,
            info_hash: hash.into(),
            magnet: if hash.is_empty() { String::new() } else { make_magnet(hash, name) },
            url: String::new(),
        }
    }

    const H1: &str = "2c6b6858d61da9543d4231a71db4b1c9264b0685";
    const H2: &str = "232cd67eb3ffbd7c37bf9ec3ee887417e5ae1ee6";

    const TORZNAB_FEED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom" xmlns:torznab="http://torznab.com/schemas/2015/feed">
<channel><title>Indexer</title>
<item>
  <title>Debian &amp; friends 13 &quot;netinst&quot;</title>
  <guid>https://tracker.example/details/1</guid>
  <link>http://127.0.0.1:9117/dl/x/?apikey=SECRET&amp;file=Debian</link>
  <size>790626304</size>
  <enclosure url="http://127.0.0.1:9117/dl/x/?apikey=SECRET" length="790626304" type="application/x-bittorrent" />
  <torznab:attr name="category" value="2000" />
  <torznab:attr name="seeders" value="40" />
  <torznab:attr name="peers" value="52" />
  <torznab:attr name="infohash" value="2C6B6858D61DA9543D4231A71DB4B1C9264B0685" />
  <torznab:attr name="magneturl" value="magnet:?xt=urn:btih:2C6B6858D61DA9543D4231A71DB4B1C9264B0685&amp;dn=Debian" />
</item>
<item>
  <title>Only an info-hash</title>
  <size>5</size>
  <torznab:attr name="seeders" value="3" />
  <torznab:attr name="leechers" value="9" />
  <torznab:attr name="infohash" value="232cd67eb3ffbd7c37bf9ec3ee887417e5ae1ee6" />
</item>
<item>
  <title>Only a download link</title>
  <link>https://indexer.example/dl/42</link>
  <torznab:attr name="seeders" value="x" />
</item>
<item><title>Nothing usable</title></item>
<item><title></title><torznab:attr name="infohash" value="232cd67eb3ffbd7c37bf9ec3ee887417e5ae1ee6" /></item>
</channel></rss>"#;

    #[test]
    fn torznab_answer_is_parsed() {
        let v = Source::TORZNAB.parse(TORZNAB_FEED.as_bytes()).unwrap();
        assert_eq!(v.len(), 3, "the two unusable items are skipped");
        let a = &v[0];
        assert_eq!(a.name, "Debian & friends 13 \"netinst\"");
        assert_eq!((a.size, a.seeders, a.leechers, a.source.as_str()), (790_626_304, 40, 12, "torznab"), "leechers = peers - seeders");
        assert_eq!(a.info_hash, H1, "normalised to lowercase");
        assert_eq!(a.magnet, "magnet:?xt=urn:btih:2C6B6858D61DA9543D4231A71DB4B1C9264B0685&dn=Debian", "the server's own magnet is kept (entities decoded)");
        assert!(a.url.contains("apikey=SECRET"));

        let b = &v[1];
        assert_eq!((b.leechers, b.seeders, b.info_hash.as_str()), (9, 3, H2), "an explicit `leechers` wins");
        assert!(b.magnet.contains(H2) && b.magnet.contains("&tr="), "a magnet is built from the hash");

        let c = &v[2];
        assert_eq!((c.info_hash.as_str(), c.magnet.as_str(), c.url.as_str()), ("", "", "https://indexer.example/dl/42"), "a download link alone is enough");
        assert_eq!(c.seeders, 0, "an unreadable number is 0, not an error");
    }

    #[test]
    fn torznab_errors_are_reported_clearly() {
        let err = Source::TORZNAB.parse(br#"<error code="100" description="Invalid API Key &amp; more"/>"#).unwrap_err().to_string();
        assert!(err.contains("torznab server: Invalid API Key & more"), "{err}");
        let err = Source::TORZNAB.parse(b"<html>Login</html>").unwrap_err().to_string();
        assert!(err.contains("Torznab endpoint"), "{err}");
        assert!(Source::TORZNAB.parse(b"<rss></rss>").unwrap().is_empty());
        assert!(Source::TORZNAB.parse(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn archive_answer_builds_torrent_urls_and_rejects_odd_identifiers() {
        let body = r#"{"response":{"numFound":3,"docs":[{"identifier":"ubuntu-20.04-arm64-utm","downloads":521177},
            {"identifier":"../../etc/passwd"},{"identifier":"has space"},{"identifier":""}]}}"#;
        let v = Source::ARCHIVE.parse(body.as_bytes()).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].url, "https://archive.org/download/ubuntu-20.04-arm64-utm/ubuntu-20.04-arm64-utm_archive.torrent");
        assert!(v[0].magnet.is_empty() && v[0].info_hash.is_empty());
        assert!(Source::ARCHIVE.parse(b"{}").is_err() && Source::ARCHIVE.parse(b"<html>").is_err());
    }

    #[test]
    fn sizes_hashes_and_entities() {
        assert_eq!(parse_size("1.5 GiB"), 1_610_612_736);
        assert_eq!(parse_size("700MB"), 700 << 20);
        assert_eq!(parse_size("12 kb"), 12 << 10);
        assert_eq!(parse_size("1,5 GB"), 1_610_612_736);
        assert_eq!(parse_size("2048"), 2048);
        assert_eq!((parse_size(""), parse_size("big"), parse_size("5 parsecs")), (0, 0, 0));

        assert_eq!(normalize_hash(&H1.to_uppercase()).as_deref(), Some(H1));
        let b32 = "FRVWQ2GPYGWKLK2DEMXRGHVRZSM6KDQ7"; // not a real torrent: only the format matters
        assert!(normalize_hash(b32).is_some() && normalize_hash(b32).unwrap().len() == 40);
        for bad in ["", "abc", &"g".repeat(40), &"a".repeat(41), "../../"] {
            assert_eq!(normalize_hash(bad), None, "{bad:?}");
        }
        assert_eq!(xml_unescape("a &amp; b &lt;c&gt; &#65;&#x42; &bogus; &"), "a & b <c> AB &bogus; &");
        assert_eq!(xml_unescape("<![CDATA[Second <one>]]>"), "Second <one>");
    }

    #[test]
    fn magnets_get_missing_trackers_once() {
        let m = make_magnet(H1, "x y");
        assert_eq!(m.matches("&tr=").count(), TRACKERS.len());
        assert_eq!(enrich_magnet(&m), m, "already complete: unchanged");
        let bare = format!("magnet:?xt=urn:btih:{H1}&dn=x&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce");
        assert_eq!(enrich_magnet(&bare).matches("&tr=").count(), TRACKERS.len(), "the one it had is not duplicated");
        assert_eq!(enrich_magnet("https://x/y.torrent"), "https://x/y.torrent");
        assert_eq!(enrich_magnet(""), "");
    }

    #[test]
    fn finalize_merges_duplicates_hides_dead_and_sorts() {
        let mut a = r("archive", "Debian", H1, 10, 2);
        a.size = 0;
        let mut b = r("torznab", "Debian (copy)", H1, 50, 7);
        b.size = 999;
        let dead = r("torznab", "Dead one", H2, 0, 5);
        let mut web = r("archive", "Only a torrent file", "", 0, 0);
        web.url = "https://archive.org/download/x/x_archive.torrent".into();
        let alive = r("torznab", "Another", "aa".repeat(20).as_str(), 3, 1);

        let out = finalize(vec![a.clone(), dead.clone(), b, web.clone(), alive.clone()], false, 50);
        let names: Vec<&str> = out.iter().map(|x| x.name.as_str()).collect();
        assert_eq!(names, ["Debian", "Another", "Only a torrent file"], "dead magnet hidden; url-only result kept last");
        assert_eq!(out[0].source, "archive+torznab");
        assert_eq!((out[0].seeders, out[0].leechers, out[0].size), (50, 7, 999), "best counts, size filled in");

        let all = finalize(vec![a, dead, alive], true, 2);
        assert_eq!(all.len(), 2, "limit applies");
        assert_eq!(all[0].seeders, 10);
        assert!(finalize(vec![r("x", "nothing", "", 5, 5)], true, 10).is_empty(), "no hash and no url: cannot be identified");
        assert!(finalize(vec![], false, 10).is_empty());
    }

    #[test]
    fn requests_are_built_safely() {
        let q = "ubuntu 24.04 & más \"x\"";
        let a = Source::ARCHIVE.request(q, None).unwrap();
        assert!(a.url.starts_with("https://archive.org/advancedsearch.php?q=title%3A%28ubuntu%2024.04%20%26%20m%C3%A1s%20%22x%22%29"), "{}", a.url);
        assert!(a.body.is_none() && !a.allow_http);

        // Torznab: only when configured, and only over https or on this machine
        assert!(Source::TORZNAB.request(q, None).is_none());
        let t = Source::TORZNAB.request("a b", Some("https://jackett.example/api/v2.0/indexers/all/results/torznab/api?apikey=K")).unwrap();
        assert_eq!(t.url, "https://jackett.example/api/v2.0/indexers/all/results/torznab/api?apikey=K&t=search&q=a%20b");
        assert!(!t.allow_http);
        let t = Source::TORZNAB.request("x", Some("http://127.0.0.1:9117/api")).unwrap();
        assert_eq!(t.url, "http://127.0.0.1:9117/api?t=search&q=x", "`?` when the base has no query yet");
        assert!(t.allow_http);
        for bad in ["http://jackett.example/api", "http://127.0.0.1.evil.example/api", "http://localhost.evil.example/", "ftp://x/y", "file:///etc/passwd", "", "   "] {
            assert!(Source::TORZNAB.request("x", Some(bad)).is_none(), "{bad:?} must not be used");
        }
        for ok in ["http://localhost", "http://localhost:9696/api", "http://[::1]:9117/x", "http://127.0.0.1?a=1"] {
            assert!(is_loopback_http(ok), "{ok}");
        }
    }

    #[test]
    fn the_registry_is_consistent() {
        let all = Source::all();
        assert!(all.len() >= 2 && all[0] == Source::ARCHIVE && all[1] == Source::TORZNAB);
        let keys: Vec<&str> = all.iter().map(|s| s.key()).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "keys are unique: {keys:?}");
        for s in &all {
            assert_eq!(Source::from_key(s.key()), Some(*s));
            assert_eq!(Source::from_key(&s.key().to_uppercase()), Some(*s));
            assert!(!s.describe().is_empty() && s.key().chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        }
        assert_eq!(Source::from_key("nope"), None);
        assert_eq!(format!("{:?}", Source::ARCHIVE), "Source(archive)");
    }

    #[test]
    fn query_checks() {
        assert_eq!(check_query("  ubuntu  ").unwrap(), "ubuntu");
        for bad in ["", "   ", "a\nb", "a\u{0}b", &"x".repeat(201)] {
            assert!(check_query(bad).is_err(), "{bad:?}");
        }
        assert!(check_query(&"ñ".repeat(200)).is_ok(), "the limit counts characters");
    }

    // ---- end to end with a fake curl

    #[cfg(unix)]
    mod with_fake_curl {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::PathBuf;

        const TORZNAB_URL: &str = "http://127.0.0.1:9117/api";

        /// A `curl` that answers by looking at the URL it was given.
        fn fake(dir: &std::path::Path, torznab_fails: bool) -> PathBuf {
            let path = dir.join("curl");
            let torznab = if torznab_fails {
                "echo 'curl: (22) The requested URL returned error: 500' >&2; exit 22".to_string()
            } else {
                format!(
                    "printf '%s' '<rss xmlns:torznab=\"x\"><channel><item><title>Debian</title><torznab:attr name=\"seeders\" value=\"9\"/><torznab:attr name=\"infohash\" value=\"{H1}\"/></item></channel></rss>'"
                )
            };
            let script = format!(
                r##"#!/bin/sh
printf '%s\n' "$@" >> '{d}/calls.txt'
for last; do :; done
case "$last" in
  *archive.org*) printf '%s' '{{"response":{{"docs":[{{"identifier":"debian-live"}}]}}}}' ;;
  *127.0.0.1:9117*) {torznab} ;;
  *) echo "unexpected URL" >&2; exit 2 ;;
esac
"##,
                d = dir.display()
            );
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn opts(bin: &std::path::Path, torznab: Option<&str>, sources: Vec<Source>) -> Options {
            Options { curl: bin.display().to_string(), torznab: torznab.map(String::from), sources, ..Options::default() }
        }

        #[tokio::test]
        async fn both_sources_are_asked_and_their_answers_merged() {
            let dir = tempfile::tempdir().unwrap();
            let rep = search("debian", &opts(&fake(dir.path(), false), Some(TORZNAB_URL), vec![Source::ARCHIVE, Source::TORZNAB])).await.unwrap();
            let st: HashMap<_, _> = rep.status.iter().cloned().collect();
            assert_eq!((st[&Source::ARCHIVE].as_str(), st[&Source::TORZNAB].as_str()), ("ok (1)", "ok (1)"));
            let names: Vec<&str> = rep.results.iter().map(|x| x.name.as_str()).collect();
            assert_eq!(names, ["Debian", "debian-live"], "seeded first, the url-only archive item after");
            assert_eq!(rep.status[0].0, Source::ARCHIVE, "status keeps the sources' order");
            // plain http was allowed for the loopback Torznab server only
            let calls = std::fs::read_to_string(dir.path().join("calls.txt")).unwrap();
            assert!(calls.contains("=https,http") && calls.contains("=https\n"), "{calls}");
        }

        #[tokio::test]
        async fn torznab_is_left_out_until_it_is_configured() {
            let dir = tempfile::tempdir().unwrap();
            let rep = search("debian", &opts(&fake(dir.path(), false), None, vec![])).await.unwrap();
            assert!(rep.status.iter().any(|(s, st)| *s == Source::ARCHIVE && st == "ok (1)"), "{:?}", rep.status);
            assert!(rep.status.iter().all(|(s, _)| *s != Source::TORZNAB), "left out, not reported as a failure: {:?}", rep.status);
            // asking for it explicitly says how to set it up
            let rep = search("debian", &opts(&fake(dir.path(), false), None, vec![Source::TORZNAB])).await.unwrap();
            assert!(rep.status[0].1.contains("not configured") && rep.status[0].1.contains(TORZNAB_ENV), "{:?}", rep.status);
            assert!(rep.results.is_empty());
        }

        #[tokio::test]
        async fn one_failing_source_does_not_spoil_the_search() {
            let dir = tempfile::tempdir().unwrap();
            let rep = search("debian", &opts(&fake(dir.path(), true), Some(TORZNAB_URL), vec![])).await.unwrap();
            let st: HashMap<_, _> = rep.status.iter().cloned().collect();
            assert_eq!(st[&Source::ARCHIVE], "ok (1)");
            assert!(st[&Source::TORZNAB].starts_with("error: curl failed (22)"), "{:?}", st[&Source::TORZNAB]);
            assert_eq!(rep.results.len(), 1);
        }

        #[tokio::test]
        async fn bad_queries_and_a_missing_curl_are_reported_not_fatal() {
            let o = opts(std::path::Path::new("/definitely/not/curl"), None, vec![]);
            assert!(search("   ", &o).await.is_err());
            let rep = search("debian", &o).await.unwrap();
            assert!(rep.results.is_empty());
            assert!(rep.status.iter().all(|(_, s)| s.contains("was not found")), "{:?}", rep.status);
        }
    }
}
