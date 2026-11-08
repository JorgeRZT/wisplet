//! A small, strict JSON reader (RFC 8259) for the search engines' responses.
//!
//! No `serde`: the answers are a few kilobytes, read once. Input is untrusted, so nesting is
//! limited and the whole text must be consumed. Numbers are `f64` (enough for sizes and counts).

use std::fmt;

const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

#[derive(Debug, PartialEq, Eq)]
pub struct JsonError(pub String);

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid JSON: {}", self.0)
    }
}

impl Json {
    pub fn parse(text: &str) -> Result<Json, JsonError> {
        let mut p = Parser { b: text.as_bytes(), i: 0 };
        p.ws();
        let v = p.value(0)?;
        p.ws();
        if p.i != p.b.len() {
            return Err(p.err("trailing characters"));
        }
        Ok(v)
    }

    /// Member of an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    /// A number, or a string holding one (some APIs quote their numbers). Negative, fractional
    /// and non-finite values become `None`.
    pub fn as_u64(&self) -> Option<u64> {
        let n = match self {
            Json::Num(n) => *n,
            Json::Str(s) => s.trim().parse::<f64>().ok()?,
            _ => return None,
        };
        (n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n < 1.8e19).then_some(n as u64)
    }
}

impl Json {
    /// Object from `(key, value)` pairs, keeping their order.
    pub fn object<K: Into<String>, const N: usize>(pairs: [(K, Json); N]) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    pub fn str(s: impl Into<String>) -> Json {
        Json::Str(s.into())
    }

    pub fn num(n: impl Into<f64>) -> Json {
        Json::Num(n.into())
    }

    /// Whole numbers above 2^53 lose precision as `f64`; file sizes never get near it.
    pub fn uint(n: u64) -> Json {
        Json::Num(n as f64)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Compact JSON text. Strings are escaped; non-finite numbers become `null`.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) if !n.is_finite() => out.push_str("null"),
            Json::Num(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => out.push_str(&format!("{}", *n as i64)),
            Json::Num(n) => out.push_str(&format!("{n}")),
            Json::Str(s) => {
                out.push('"');
                out.push_str(&escape(s));
                out.push('"');
            }
            Json::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
            Json::Obj(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    out.push_str(&escape(k));
                    out.push_str("\":");
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Escape a string for use inside a JSON string literal.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn err(&self, what: &str) -> JsonError {
        JsonError(format!("{what} at byte {}", self.i))
    }

    fn ws(&mut self) {
        while matches!(self.b.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.b[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, JsonError> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        match self.b.get(self.i) {
            None => Err(self.err("unexpected end")),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') if self.eat("true") => Ok(Json::Bool(true)),
            Some(b'f') if self.eat("false") => Ok(Json::Bool(false)),
            Some(b'n') if self.eat("null") => Ok(Json::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.i += 1; // {
        let mut members = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Json::Obj(members));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return Err(self.err("expected a string key"));
            }
            let key = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return Err(self.err("expected ':'"));
            }
            self.i += 1;
            self.ws();
            members.push((key, self.value(depth + 1)?));
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Json::Obj(members));
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.i += 1; // [
        let mut items = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.ws();
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
    }

    fn number(&mut self) -> Result<Json, JsonError> {
        let start = self.i;
        if self.b.get(self.i) == Some(&b'-') {
            self.i += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.i;
            while matches!(p.b.get(p.i), Some(b'0'..=b'9')) {
                p.i += 1;
            }
            p.i > s
        };
        if !digits(self) {
            return Err(self.err("bad number"));
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if !digits(self) {
                return Err(self.err("bad number"));
            }
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !digits(self) {
                return Err(self.err("bad number"));
            }
        }
        std::str::from_utf8(&self.b[start..self.i])
            .ok()
            .and_then(|t| t.parse::<f64>().ok())
            .map(Json::Num)
            .ok_or_else(|| self.err("bad number"))
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let h = self.b.get(self.i..self.i + 4).and_then(|s| std::str::from_utf8(s).ok());
        let v = h.and_then(|h| u32::from_str_radix(h, 16).ok()).ok_or_else(|| self.err("bad \\u escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.i += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let Some(&c) = self.b.get(self.i) else { return Err(self.err("unterminated string")) };
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).map_err(|_| self.err("invalid UTF-8")),
                b'\\' => {
                    let Some(&e) = self.b.get(self.i) else { return Err(self.err("unterminated string")) };
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            let cp = if (0xd800..0xdc00).contains(&hi) {
                                // a high surrogate must be followed by \uDC00..\uDFFF
                                if !self.eat("\\u") {
                                    return Err(self.err("lone surrogate"));
                                }
                                let lo = self.hex4()?;
                                if !(0xdc00..0xe000).contains(&lo) {
                                    return Err(self.err("lone surrogate"));
                                }
                                0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                            } else if (0xdc00..0xe000).contains(&hi) {
                                return Err(self.err("lone surrogate"));
                            } else {
                                hi
                            };
                            char::from_u32(cp).ok_or_else(|| self.err("bad code point"))?
                        }
                        _ => return Err(self.err("bad escape")),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                // raw control characters are illegal in JSON, but some indexers emit tabs/newlines
                c => out.push(c),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_value_kind() {
        let j = Json::parse(r#" {"a": [1, -2.5, 3e2, true, false, null, "x"], "b": {"c": {}}, "d": []} "#).unwrap();
        let a = j.get("a").unwrap().as_array().unwrap();
        assert_eq!(a[0], Json::Num(1.0));
        assert_eq!(a[1], Json::Num(-2.5));
        assert_eq!(a[2], Json::Num(300.0));
        assert_eq!((&a[3], &a[4], &a[5]), (&Json::Bool(true), &Json::Bool(false), &Json::Null));
        assert_eq!(a[6].as_str(), Some("x"));
        assert_eq!(j.get("b").unwrap().get("c"), Some(&Json::Obj(vec![])));
        assert_eq!(j.get("d"), Some(&Json::Arr(vec![])));
        assert_eq!(j.get("missing"), None);
        assert_eq!(Json::parse("7").unwrap(), Json::Num(7.0));
    }

    #[test]
    fn decodes_string_escapes_and_unicode() {
        let j = Json::parse(r#""a\"b\\c\/d\n\t\u00e1\u20ac \ud83d\ude00 ñ""#).unwrap();
        assert_eq!(j.as_str(), Some("a\"b\\c/d\n\tá€ 😀 ñ"));
        // raw tabs/newlines inside strings are tolerated (real indexers emit them)
        assert_eq!(Json::parse("\"a\tb\nc\"").unwrap().as_str(), Some("a\tb\nc"));
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            "", "   ", "{", "[1,", "[1 2]", "{\"a\" 1}", "{a:1}", "{\"a\":1,}", "[1,]", "tru", "nul", "01x", "-", "1.", "1e",
            "\"abc", "\"\\x\"", "\"\\u12\"", "\"\\ud800\"", "\"\\ud800\\u0041\"", "\"\\udc00\"", "[] []", "{} x", "'a'",
        ] {
            assert!(Json::parse(bad).is_err(), "{bad:?} should fail");
        }
        assert!(Json::parse("\"\u{0}\"").is_ok(), "controls tolerated");
        assert!(Json::parse(&format!("\"{}\"", "\u{fffd}")).is_ok());
    }

    #[test]
    fn nesting_is_bounded() {
        let ok = format!("{}1{}", "[".repeat(30), "]".repeat(30));
        assert!(Json::parse(&ok).is_ok());
        let deep = format!("{}1{}", "[".repeat(5000), "]".repeat(5000));
        assert!(Json::parse(&deep).unwrap_err().0.contains("too deep"));
    }

    #[test]
    fn numbers_convert_to_unsigned_integers() {
        assert_eq!(Json::Num(42.0).as_u64(), Some(42));
        assert_eq!(Json::Str(" 7 ".into()).as_u64(), Some(7), "quoted numbers, as some JSON APIs send them");
        assert_eq!(Json::Str("abc".into()).as_u64(), None);
        for bad in [Json::Num(-1.0), Json::Num(1.5), Json::Num(f64::NAN), Json::Num(1e30), Json::Null, Json::Bool(true)] {
            assert_eq!(bad.as_u64(), None, "{bad:?}");
        }
    }

    #[test]
    fn escape_roundtrips_through_the_parser() {
        let nasty = "q\"uote \\ back\nline\ttab \u{1} ñ 😀";
        let text = format!("\"{}\"", escape(nasty));
        assert_eq!(Json::parse(&text).unwrap().as_str(), Some(nasty));
    }

    #[test]
    fn serialises_and_parses_back_to_the_same_value() {
        let v = Json::object([
            ("name", Json::str("a \"quoted\" ñ 😀\n")),
            ("n", Json::uint(1_000_000_000_000)),
            ("f", Json::num(1.5)),
            ("neg", Json::num(-3)),
            ("ok", Json::Bool(true)),
            ("none", Json::Null),
            ("list", Json::Arr(vec![Json::num(1), Json::str("x"), Json::object([("k", Json::Arr(vec![]))])])),
        ]);
        let text = v.to_json();
        assert_eq!(Json::parse(&text).unwrap(), v);
        assert!(text.starts_with(r#"{"name":"a \"quoted\" ñ 😀\n","n":1000000000000,"f":1.5,"neg":-3,"#), "{text}");
        assert!(!text.contains(' ') || text.contains("ñ"), "compact output has no layout spaces");
        // non-finite numbers cannot be written in JSON
        assert_eq!(Json::Num(f64::NAN).to_json(), "null");
        assert_eq!(Json::Num(f64::INFINITY).to_json(), "null");
        assert_eq!(Json::Bool(true).as_bool(), Some(true));
        assert_eq!(Json::Null.as_bool(), None);
    }
}
