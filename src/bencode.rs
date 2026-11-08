//! Strict bencode decoder / canonical encoder.
//!
//! * Depth-limited (no stack overflow on hostile input).
//! * Rejects leading zeros, `-0`, truncated data and oversized lengths.
//! * Dictionary keys may arrive unsorted (many real-world torrents do) — we accept
//!   them, but always *encode* in canonical (sorted) order.
//! * [`dict_value_span`] returns the raw byte range of a top-level value so the
//!   info-hash can be computed over the exact bytes found in the file.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;

const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    UnexpectedEof,
    UnexpectedByte { byte: u8, pos: usize },
    InvalidInteger(usize),
    InvalidLength(usize),
    TrailingData(usize),
    TooDeep,
    NotADict,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnexpectedEof => write!(f, "unexpected end of data"),
            Error::UnexpectedByte { byte, pos } => {
                write!(f, "unexpected byte 0x{byte:02x} at offset {pos}")
            }
            Error::InvalidInteger(pos) => write!(f, "invalid integer at offset {pos}"),
            Error::InvalidLength(pos) => write!(f, "invalid string length at offset {pos}"),
            Error::TrailingData(pos) => write!(f, "trailing data after offset {pos}"),
            Error::TooDeep => write!(f, "nesting too deep"),
            Error::NotADict => write!(f, "top-level value is not a dictionary"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = std::result::Result<T, Error>;

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Look up `key` if this value is a dictionary.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_dict()?.get(key.as_bytes())
    }

    /// Build a dictionary from `(key, value)` pairs.
    pub fn dict<'a, I: IntoIterator<Item = (&'a str, Value)>>(pairs: I) -> Value {
        Value::Dict(
            pairs
                .into_iter()
                .map(|(k, v)| (k.as_bytes().to_vec(), v))
                .collect(),
        )
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Bytes(s.as_bytes().to_vec())
    }
}

impl From<&[u8]> for Value {
    fn from(b: &[u8]) -> Self {
        Value::Bytes(b.to_vec())
    }
}

impl From<Vec<u8>> for Value {
    fn from(b: Vec<u8>) -> Self {
        Value::Bytes(b)
    }
}

impl From<Vec<Value>> for Value {
    fn from(l: Vec<Value>) -> Self {
        Value::List(l)
    }
}

struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Result<u8> {
        self.data.get(self.pos).copied().ok_or(Error::UnexpectedEof)
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        match self.peek()? {
            b'i' => self.int().map(Value::Int),
            b'l' => {
                self.pos += 1;
                let mut list = Vec::new();
                while self.peek()? != b'e' {
                    list.push(self.value(depth + 1)?);
                }
                self.pos += 1;
                Ok(Value::List(list))
            }
            b'd' => {
                self.pos += 1;
                let mut dict = BTreeMap::new();
                while self.peek()? != b'e' {
                    let key = self.bytes()?;
                    let val = self.value(depth + 1)?;
                    dict.insert(key, val);
                }
                self.pos += 1;
                Ok(Value::Dict(dict))
            }
            b'0'..=b'9' => self.bytes().map(Value::Bytes),
            byte => Err(Error::UnexpectedByte { byte, pos: self.pos }),
        }
    }

    fn int(&mut self) -> Result<i64> {
        let start = self.pos;
        self.pos += 1; // 'i'
        let neg = self.peek()? == b'-';
        if neg {
            self.pos += 1;
        }
        let digits_start = self.pos;
        while matches!(self.data.get(self.pos), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        let digits = &self.data[digits_start..self.pos];
        if digits.is_empty()
            || (digits.len() > 1 && digits[0] == b'0')
            || (neg && digits == b"0")
        {
            return Err(Error::InvalidInteger(start));
        }
        if self.peek()? != b'e' {
            return Err(Error::InvalidInteger(start));
        }
        let text = std::str::from_utf8(&self.data[if neg { digits_start - 1 } else { digits_start }..self.pos])
            .map_err(|_| Error::InvalidInteger(start))?;
        let n = text.parse::<i64>().map_err(|_| Error::InvalidInteger(start))?;
        self.pos += 1; // 'e'
        Ok(n)
    }

    fn bytes(&mut self) -> Result<Vec<u8>> {
        let start = self.pos;
        while matches!(self.data.get(self.pos), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        let digits = &self.data[start..self.pos];
        if digits.is_empty() {
            return Err(match self.data.get(self.pos) {
                Some(&byte) => Error::UnexpectedByte { byte, pos: self.pos },
                None => Error::UnexpectedEof,
            });
        }
        if digits.len() > 1 && digits[0] == b'0' {
            return Err(Error::InvalidLength(start));
        }
        match self.data.get(self.pos) {
            Some(b':') => {}
            Some(&byte) => return Err(Error::UnexpectedByte { byte, pos: self.pos }),
            None => return Err(Error::UnexpectedEof),
        }
        let mut len: usize = 0;
        for &d in digits {
            len = len
                .checked_mul(10)
                .and_then(|l| l.checked_add((d - b'0') as usize))
                .ok_or(Error::InvalidLength(start))?;
        }
        self.pos += 1; // ':'
        let end = self.pos.checked_add(len).ok_or(Error::InvalidLength(start))?;
        if end > self.data.len() {
            return Err(Error::UnexpectedEof);
        }
        let out = self.data[self.pos..end].to_vec();
        self.pos = end;
        Ok(out)
    }
}

/// Decode a complete buffer; trailing bytes are an error.
pub fn decode(data: &[u8]) -> Result<Value> {
    let (value, used) = decode_prefix(data)?;
    if used != data.len() {
        return Err(Error::TrailingData(used));
    }
    Ok(value)
}

/// Decode the first value in `data` and return it with the number of bytes consumed.
/// Used for BEP 9 messages, where a bencoded header is followed by raw bytes.
pub fn decode_prefix(data: &[u8]) -> Result<(Value, usize)> {
    let mut p = Parser { data, pos: 0 };
    let v = p.value(0)?;
    Ok((v, p.pos))
}

/// Byte range of the value stored under `key` in the top-level dictionary of `data`.
pub fn dict_value_span(data: &[u8], key: &[u8]) -> Result<Option<Range<usize>>> {
    let mut p = Parser { data, pos: 0 };
    if p.peek()? != b'd' {
        return Err(Error::NotADict);
    }
    p.pos += 1;
    while p.peek()? != b'e' {
        let k = p.bytes()?;
        let start = p.pos;
        p.value(1)?;
        if k == key {
            return Ok(Some(start..p.pos));
        }
    }
    Ok(None)
}

pub fn encode(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    encode_into(value, &mut out);
    out
}

pub fn encode_into(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Int(i) => {
            out.push(b'i');
            out.extend_from_slice(i.to_string().as_bytes());
            out.push(b'e');
        }
        Value::Bytes(b) => {
            out.extend_from_slice(b.len().to_string().as_bytes());
            out.push(b':');
            out.extend_from_slice(b);
        }
        Value::List(l) => {
            out.push(b'l');
            for v in l {
                encode_into(v, out);
            }
            out.push(b'e');
        }
        Value::Dict(d) => {
            out.push(b'd');
            for (k, v) in d {
                out.extend_from_slice(k.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(k);
                encode_into(v, out);
            }
            out.push(b'e');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_integers() {
        assert_eq!(decode(b"i42e").unwrap(), Value::Int(42));
        assert_eq!(decode(b"i-7e").unwrap(), Value::Int(-7));
        assert_eq!(decode(b"i0e").unwrap(), Value::Int(0));
        assert_eq!(
            decode(b"i9223372036854775807e").unwrap(),
            Value::Int(i64::MAX)
        );
    }

    #[test]
    fn rejects_bad_integers() {
        for bad in [&b"i-0e"[..], b"i03e", b"ie", b"i-e", b"i1x2e", b"i99999999999999999999e"] {
            assert!(
                matches!(decode(bad), Err(Error::InvalidInteger(_))),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(decode(b"i12"), Err(Error::UnexpectedEof));
    }

    #[test]
    fn decodes_strings() {
        assert_eq!(decode(b"4:spam").unwrap(), Value::Bytes(b"spam".to_vec()));
        assert_eq!(decode(b"0:").unwrap(), Value::Bytes(vec![]));
        assert_eq!(
            decode(b"3:\x00\xff\x01").unwrap(),
            Value::Bytes(vec![0, 255, 1])
        );
    }

    #[test]
    fn rejects_bad_strings() {
        assert_eq!(decode(b"4:spa"), Err(Error::UnexpectedEof));
        assert!(matches!(decode(b"04:spam"), Err(Error::InvalidLength(_))));
        assert!(matches!(decode(b"4spam"), Err(Error::UnexpectedByte { .. })));
        assert!(matches!(
            decode(b"99999999999999999999999:x"),
            Err(Error::InvalidLength(_))
        ));
        assert!(matches!(
            decode(b"18446744073709551615:x"),
            Err(Error::InvalidLength(_) | Error::UnexpectedEof)
        ));
    }

    #[test]
    fn decodes_lists_and_dicts() {
        let v = decode(b"l4:spami42ee").unwrap();
        assert_eq!(
            v,
            Value::List(vec![Value::Bytes(b"spam".to_vec()), Value::Int(42)])
        );
        let d = decode(b"d3:cow3:moo4:spam4:eggse").unwrap();
        assert_eq!(d.get("cow").unwrap().as_str(), Some("moo"));
        assert_eq!(d.get("spam").unwrap().as_str(), Some("eggs"));
        assert!(d.get("missing").is_none());
    }

    #[test]
    fn accepts_unsorted_dict_keys() {
        let d = decode(b"d1:bi1e1:ai2ee").unwrap();
        assert_eq!(d.get("a").unwrap().as_int(), Some(2));
        // encoding is canonical
        assert_eq!(encode(&d), b"d1:ai2e1:bi1ee");
    }

    #[test]
    fn rejects_truncated_and_trailing() {
        assert_eq!(decode(b"l"), Err(Error::UnexpectedEof));
        assert_eq!(decode(b"d3:key"), Err(Error::UnexpectedEof));
        assert_eq!(decode(b""), Err(Error::UnexpectedEof));
        assert_eq!(decode(b"i1eX"), Err(Error::TrailingData(3)));
        assert!(matches!(decode(b"x"), Err(Error::UnexpectedByte { .. })));
        // dict key must be a string
        assert!(matches!(decode(b"di1ei2ee"), Err(Error::UnexpectedByte { .. })));
    }

    #[test]
    fn depth_is_limited() {
        let mut deep = vec![b'l'; 200];
        deep.extend(std::iter::repeat_n(b'e', 200));
        assert_eq!(decode(&deep), Err(Error::TooDeep));

        let mut ok = vec![b'l'; 30];
        ok.extend(std::iter::repeat_n(b'e', 30));
        assert!(decode(&ok).is_ok());
    }

    #[test]
    fn roundtrip() {
        let v = Value::dict([
            ("name", "file".into()),
            ("length", 12345.into()),
            ("list", vec![Value::Int(1), "x".into()].into()),
            ("bin", Value::Bytes(vec![0, 1, 2, 255])),
        ]);
        let bytes = encode(&v);
        assert_eq!(decode(&bytes).unwrap(), v);
    }

    #[test]
    fn prefix_decoding_reports_consumed_bytes() {
        let (v, n) = decode_prefix(b"d1:ai1eeRAWDATA").unwrap();
        assert_eq!(n, 8);
        assert_eq!(v.get("a").unwrap().as_int(), Some(1));
    }

    #[test]
    fn span_of_top_level_value() {
        let data = b"d8:announce3:foo4:infod4:name1:xe5:other3:bare";
        let span = dict_value_span(data, b"info").unwrap().unwrap();
        assert_eq!(&data[span], b"d4:name1:xe");
        assert!(dict_value_span(data, b"nope").unwrap().is_none());
        assert_eq!(dict_value_span(b"i1e", b"x"), Err(Error::NotADict));
    }

    #[test]
    fn span_ignores_nested_keys_with_same_name() {
        let data = b"d5:otherd4:infoi1ee4:infoi2ee";
        let span = dict_value_span(data, b"info").unwrap().unwrap();
        assert_eq!(&data[span], b"i2e");
    }

    #[test]
    fn display_is_informative() {
        assert!(Error::TooDeep.to_string().contains("deep"));
        assert!(Error::UnexpectedByte { byte: 0x78, pos: 3 }
            .to_string()
            .contains("0x78"));
    }
}
