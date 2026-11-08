//! Small helpers: hex/base32/percent coding, peer-id generation, verbose logging.

use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

/// Log to stderr only when verbose mode is on. Arguments are not evaluated otherwise.
#[macro_export]
macro_rules! vlog {
    ($($arg:tt)*) => {
        if $crate::util::verbose() {
            eprintln!($($arg)*);
        }
    };
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    b.chunks(2)
        .map(|p| Some(nibble(p[0])? << 4 | nibble(p[1])?))
        .collect()
}

/// RFC 4648 base32 (case-insensitive, optional `=` padding).
pub fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        acc = (acc << 5) | v as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Decode `%XX` escapes (and `+` as space, as used in magnet query strings).
pub fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let digit = |k: usize| b.get(k).and_then(|c| (*c as char).to_digit(16));
                if let (Some(h), Some(l)) = (digit(i + 1), digit(i + 2)) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// Percent-encode arbitrary bytes, leaving only RFC 3986 unreserved characters as-is.
/// Required for the binary `info_hash` / `peer_id` tracker parameters.
pub fn percent_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(bytes.len() * 3);
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            s.push(b as char);
        } else {
            s.push('%');
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    s
}

/// Azureus-style peer id: `-WP0200-` followed by 12 random alphanumerics.
pub fn random_peer_id() -> [u8; 20] {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut id = [0u8; 20];
    id[..8].copy_from_slice(b"-WP0200-");
    for b in &mut id[8..] {
        *b = ALPHABET[fastrand::usize(..ALPHABET.len())];
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let data = [0x00, 0xff, 0x10, 0xab];
        assert_eq!(hex_encode(&data), "00ff10ab");
        assert_eq!(hex_decode("00FF10aB").unwrap(), data);
    }

    #[test]
    fn hex_rejects_bad_input() {
        assert!(hex_decode("abc").is_none());
        assert!(hex_decode("zz").is_none());
    }

    #[test]
    fn base32_known_vectors() {
        // RFC 4648 test vectors
        assert_eq!(base32_decode("MZXW6YTBOI======").unwrap(), b"foobar");
        assert_eq!(base32_decode("mzxw6ytboi").unwrap(), b"foobar");
        assert_eq!(base32_decode("MY======").unwrap(), b"f");
        assert!(base32_decode("MZXW1").is_none());
    }

    #[test]
    fn percent_decode_handles_escapes() {
        assert_eq!(percent_decode("a%20b+c"), b"a b c");
        assert_eq!(percent_decode("%7E%2f"), b"~/");
    }

    #[test]
    fn percent_decode_keeps_malformed_escapes() {
        assert_eq!(percent_decode("100%"), b"100%");
        assert_eq!(percent_decode("%zz"), b"%zz");
        assert_eq!(percent_decode("%4"), b"%4");
    }

    #[test]
    fn percent_encode_binary() {
        assert_eq!(percent_encode(&[0x00, b'a', b' ', 0xff]), "%00a%20%FF");
        assert_eq!(percent_encode(b"-._~"), "-._~");
    }

    #[test]
    fn peer_id_shape() {
        let id = random_peer_id();
        assert_eq!(&id[..8], b"-WP0200-");
        assert!(id[8..].iter().all(|b| b.is_ascii_alphanumeric()));
        assert_ne!(random_peer_id(), random_peer_id());
    }
}
