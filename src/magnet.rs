//! Magnet URI parsing (BEP 9 / BEP 53 subset): `xt=urn:btih:`, `dn`, `tr`, `x.pe`.

use std::net::SocketAddr;

use crate::error::{Error, Result};
use crate::util::{base32_decode, hex_decode, percent_decode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Magnet {
    pub info_hash: [u8; 20],
    pub name: Option<String>,
    pub trackers: Vec<String>,
    /// Explicit peer addresses (`x.pe`).
    pub peers: Vec<SocketAddr>,
}

impl Magnet {
    pub fn parse(uri: &str) -> Result<Magnet> {
        let uri = uri.trim();
        let query = match uri.get(..8) {
            Some(prefix) if prefix.eq_ignore_ascii_case("magnet:?") => &uri[8..],
            _ => return Err(Error::InvalidMagnet("must start with \"magnet:?\"".into())),
        };

        let mut info_hash = None;
        let mut name = None;
        let mut trackers: Vec<String> = Vec::new();
        let mut peers = Vec::new();

        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = String::from_utf8_lossy(&percent_decode(value)).into_owned();
            match key {
                "xt" => {
                    if info_hash.is_none() {
                        if let Some(h) = value.strip_prefix("urn:btih:") {
                            info_hash = Some(parse_info_hash(h)?);
                        }
                    }
                }
                "dn" if name.is_none() && !value.is_empty() => name = Some(value),
                "tr" if !value.is_empty() => {
                    if !trackers.contains(&value) {
                        trackers.push(value);
                    }
                }
                "x.pe" => {
                    if let Ok(addr) = value.parse::<SocketAddr>() {
                        if !peers.contains(&addr) {
                            peers.push(addr);
                        }
                    }
                }
                _ => {}
            }
        }

        let info_hash = info_hash
            .ok_or_else(|| Error::InvalidMagnet("missing xt=urn:btih:<hash> parameter".into()))?;
        Ok(Magnet {
            info_hash,
            name,
            trackers,
            peers,
        })
    }
}

fn parse_info_hash(s: &str) -> Result<[u8; 20]> {
    let bytes = match s.len() {
        40 => hex_decode(s),
        32 => base32_decode(s),
        n => {
            return Err(Error::InvalidMagnet(format!(
                "info-hash must be 40 hex or 32 base32 characters, got {n}"
            )))
        }
    }
    .ok_or_else(|| Error::InvalidMagnet("info-hash contains invalid characters".into()))?;
    bytes
        .try_into()
        .map_err(|_| Error::InvalidMagnet("info-hash has wrong length".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::hex_encode;

    const HASH: &str = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";

    #[test]
    fn parses_minimal_hex_magnet() {
        let m = Magnet::parse(&format!("magnet:?xt=urn:btih:{HASH}")).unwrap();
        assert_eq!(hex_encode(&m.info_hash), HASH);
        assert!(m.name.is_none() && m.trackers.is_empty() && m.peers.is_empty());
    }

    #[test]
    fn parses_full_magnet() {
        let uri = format!(
            "magnet:?xt=urn:btih:{HASH}&dn=My+Cool%20File&tr=udp%3A%2F%2Ftracker.example%3A80%2Fannounce\
             &tr=http%3A%2F%2Ft2.example%2Fa&x.pe=1.2.3.4:6881&x.pe=%5B%3A%3A1%5D:7000"
        );
        let m = Magnet::parse(&uri).unwrap();
        assert_eq!(m.name.as_deref(), Some("My Cool File"));
        assert_eq!(
            m.trackers,
            vec!["udp://tracker.example:80/announce", "http://t2.example/a"]
        );
        assert_eq!(
            m.peers,
            vec![
                "1.2.3.4:6881".parse::<SocketAddr>().unwrap(),
                "[::1]:7000".parse().unwrap()
            ]
        );
    }

    #[test]
    fn parses_base32_hash() {
        // base32 of the 20 bytes behind HASH
        let raw = hex_decode(HASH).unwrap();
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let mut b32 = String::new();
        let (mut acc, mut bits) = (0u32, 0);
        for byte in raw {
            acc = (acc << 8) | byte as u32;
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                b32.push(alphabet[((acc >> bits) & 31) as usize] as char);
            }
            acc &= (1 << bits) - 1;
        }
        assert_eq!(b32.len(), 32);
        let m = Magnet::parse(&format!("magnet:?xt=urn:btih:{b32}")).unwrap();
        assert_eq!(hex_encode(&m.info_hash), HASH);
    }

    #[test]
    fn scheme_is_case_insensitive_and_trimmed() {
        let m = Magnet::parse(&format!("  MAGNET:?xt=urn:btih:{}\n", HASH.to_uppercase())).unwrap();
        assert_eq!(hex_encode(&m.info_hash), HASH);
    }

    #[test]
    fn deduplicates_trackers_and_ignores_bad_peers() {
        let uri = format!(
            "magnet:?xt=urn:btih:{HASH}&tr=http://a/x&tr=http://a/x&x.pe=notanaddr&unknown=1&flag"
        );
        let m = Magnet::parse(&uri).unwrap();
        assert_eq!(m.trackers.len(), 1);
        assert!(m.peers.is_empty());
    }

    #[test]
    fn skips_v2_only_xt_but_uses_btih() {
        let uri = format!("magnet:?xt=urn:btmh:1220abcd&xt=urn:btih:{HASH}");
        assert!(Magnet::parse(&uri).is_ok());
    }

    #[test]
    fn rejects_invalid_input() {
        for bad in [
            "http://example.com",
            "magnet:?dn=foo",
            "magnet:?xt=urn:btih:abc",
            "magnet:?xt=urn:btih:zz2fe1c06bba254a9dc9f519b335aa7c1367a88a",
            "",
            "magnet",
        ] {
            assert!(
                matches!(Magnet::parse(bad), Err(Error::InvalidMagnet(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn does_not_panic_on_multibyte_prefix() {
        assert!(Magnet::parse("ñññññññññ").is_err());
    }
}
