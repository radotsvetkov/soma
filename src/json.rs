//! Minimal JSON parser/serializer. Hand-rolled (zero dependencies, R18).
//!
//! Objects preserve insertion order (`Vec<(String, Json)>`), which makes
//! serialization deterministic — important because journal events are hashed
//! line-by-line (R1).

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

// ---------- constructors ----------

pub fn jstr(s: impl Into<String>) -> Json {
    Json::Str(s.into())
}
pub fn jnum(n: f64) -> Json {
    Json::Num(n)
}
pub fn jint(n: i64) -> Json {
    Json::Num(n as f64)
}
pub fn jbool(b: bool) -> Json {
    Json::Bool(b)
}
pub fn jarr(items: Vec<Json>) -> Json {
    Json::Arr(items)
}
pub fn jobj(pairs: Vec<(&str, Json)>) -> Json {
    Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

// ---------- accessors ----------

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn s(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn f(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn i(&self) -> Option<i64> {
        match self {
            Json::Num(n) => Some(*n as i64),
            _ => None,
        }
    }
    pub fn b(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn arr(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
    pub fn obj(&self) -> Option<&Vec<(String, Json)>> {
        match self {
            Json::Obj(o) => Some(o),
            _ => None,
        }
    }
    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    // Convenience field readers used throughout the codebase.
    pub fn str_of(&self, key: &str) -> String {
        self.get(key).and_then(|v| v.s()).unwrap_or("").to_string()
    }
    pub fn i_of(&self, key: &str) -> i64 {
        self.get(key).and_then(|v| v.i()).unwrap_or(0)
    }
    pub fn f_of(&self, key: &str) -> f64 {
        self.get(key).and_then(|v| v.f()).unwrap_or(0.0)
    }
    pub fn b_of(&self, key: &str) -> bool {
        self.get(key).and_then(|v| v.b()).unwrap_or(false)
    }
    pub fn arr_of(&self, key: &str) -> Vec<Json> {
        self.get(key).and_then(|v| v.arr()).cloned().unwrap_or_default()
    }
    pub fn strs_of(&self, key: &str) -> Vec<String> {
        self.arr_of(key)
            .iter()
            .filter_map(|v| v.s().map(|s| s.to_string()))
            .collect()
    }

    /// Insert or replace a key on an object. No-op on non-objects.
    pub fn set(&mut self, key: &str, value: Json) {
        if let Json::Obj(pairs) = self {
            if let Some(slot) = pairs.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value;
            } else {
                pairs.push((key.to_string(), value));
            }
        }
    }

    pub fn to_string(&self) -> String {
        let mut out = String::new();
        write_value(self, &mut out);
        out
    }

    pub fn pretty(&self) -> String {
        let mut out = String::new();
        write_pretty(self, &mut out, 0);
        out.push('\n');
        out
    }
}

// ---------- serialization ----------

fn write_value(v: &Json, out: &mut String) {
    match v {
        Json::Null => out.push_str("null"),
        Json::Bool(true) => out.push_str("true"),
        Json::Bool(false) => out.push_str("false"),
        Json::Num(n) => write_num(*n, out),
        Json::Str(s) => write_string(s, out),
        Json::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Json::Obj(pairs) => {
            out.push('{');
            for (i, (k, val)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(k, out);
                out.push(':');
                write_value(val, out);
            }
            out.push('}');
        }
    }
}

fn write_pretty(v: &Json, out: &mut String, depth: usize) {
    let pad = |out: &mut String, d: usize| {
        for _ in 0..d {
            out.push_str("  ");
        }
    };
    match v {
        Json::Arr(items) if !items.is_empty() => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                pad(out, depth + 1);
                write_pretty(item, out, depth + 1);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            pad(out, depth);
            out.push(']');
        }
        Json::Obj(pairs) if !pairs.is_empty() => {
            out.push_str("{\n");
            for (i, (k, val)) in pairs.iter().enumerate() {
                pad(out, depth + 1);
                write_string(k, out);
                out.push_str(": ");
                write_pretty(val, out, depth + 1);
                if i + 1 < pairs.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            pad(out, depth);
            out.push('}');
        }
        other => write_value(other, out),
    }
}

fn write_num(n: f64, out: &mut String) {
    if !n.is_finite() {
        out.push_str("null"); // JSON has no NaN/Inf; degrade safely
    } else if n.fract() == 0.0 && n.abs() < 9.007_199_254_740_992e15 {
        out.push_str(&format!("{}", n as i64));
    } else {
        out.push_str(&format!("{}", n));
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------- parsing ----------

#[derive(Debug)]
pub struct ParseErr {
    pub pos: usize,
    pub msg: String,
}

impl fmt::Display for ParseErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "json parse error at byte {}: {}", self.pos, self.msg)
    }
}

const MAX_DEPTH: usize = 128;

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

pub fn parse(input: &str) -> Result<Json, ParseErr> {
    let mut p = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.value(0)?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(p.err("trailing characters after value"));
    }
    Ok(v)
}

impl<'a> Parser<'a> {
    fn err(&self, msg: &str) -> ParseErr {
        ParseErr {
            pos: self.pos,
            msg: msg.to_string(),
        }
    }
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }
    fn expect(&mut self, b: u8) -> Result<(), ParseErr> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected '{}'", b as char)))
        }
    }
    fn literal(&mut self, lit: &str, v: Json) -> Result<Json, ParseErr> {
        if self.bytes[self.pos..].starts_with(lit.as_bytes()) {
            self.pos += lit.len();
            Ok(v)
        } else {
            Err(self.err(&format!("expected '{lit}'")))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ParseErr> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        match self.peek() {
            Some(b'n') => self.literal("null", Json::Null),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.object(depth),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(self.err("unexpected character")),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, ParseErr> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err(self.err("expected ',' or ']' in array")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, ParseErr> {
        self.expect(b'{')?;
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let val = self.value(depth + 1)?;
            pairs.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(pairs));
                }
                _ => return Err(self.err("expected ',' or '}' in object")),
            }
        }
    }

    fn number(&mut self) -> Result<Json, ParseErr> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("invalid utf8 in number"))?;
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| self.err("invalid number"))
    }

    fn string(&mut self) -> Result<String, ParseErr> {
        self.expect(b'"')?;
        let mut s = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(s);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    match self.peek() {
                        Some(b'"') => {
                            s.push('"');
                            self.pos += 1;
                        }
                        Some(b'\\') => {
                            s.push('\\');
                            self.pos += 1;
                        }
                        Some(b'/') => {
                            s.push('/');
                            self.pos += 1;
                        }
                        Some(b'n') => {
                            s.push('\n');
                            self.pos += 1;
                        }
                        Some(b't') => {
                            s.push('\t');
                            self.pos += 1;
                        }
                        Some(b'r') => {
                            s.push('\r');
                            self.pos += 1;
                        }
                        Some(b'b') => {
                            s.push('\u{08}');
                            self.pos += 1;
                        }
                        Some(b'f') => {
                            s.push('\u{0c}');
                            self.pos += 1;
                        }
                        Some(b'u') => {
                            self.pos += 1;
                            let cp = self.hex4()?;
                            let c = if (0xD800..0xDC00).contains(&cp) {
                                // high surrogate — a \uXXXX low surrogate must follow
                                if self.peek() == Some(b'\\') {
                                    self.pos += 1;
                                    self.expect(b'u')?;
                                    let low = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&low) {
                                        return Err(self.err("invalid low surrogate"));
                                    }
                                    let combined =
                                        0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00);
                                    char::from_u32(combined)
                                        .ok_or_else(|| self.err("invalid surrogate pair"))?
                                } else {
                                    return Err(self.err("lone high surrogate"));
                                }
                            } else if (0xDC00..0xE000).contains(&cp) {
                                return Err(self.err("lone low surrogate"));
                            } else {
                                char::from_u32(cp).ok_or_else(|| self.err("invalid codepoint"))?
                            };
                            s.push(c);
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                Some(c) if c < 0x20 => return Err(self.err("control character in string")),
                Some(_) => {
                    // consume one UTF-8 encoded char
                    let rest = std::str::from_utf8(&self.bytes[self.pos..])
                        .map_err(|_| self.err("invalid utf8"))?;
                    let c = rest.chars().next().unwrap();
                    s.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseErr> {
        let mut v: u32 = 0;
        for _ in 0..4 {
            let c = self.peek().ok_or_else(|| self.err("truncated \\u escape"))?;
            let d = (c as char)
                .to_digit(16)
                .ok_or_else(|| self.err("invalid hex digit"))?;
            v = v * 16 + d;
            self.pos += 1;
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_basic() {
        let src = r#"{"name":"soma","version":1,"tags":["a","b"],"ok":true,"none":null,"pi":3.5}"#;
        let v = parse(src).unwrap();
        assert_eq!(v.str_of("name"), "soma");
        assert_eq!(v.i_of("version"), 1);
        assert_eq!(v.strs_of("tags"), vec!["a", "b"]);
        assert!(v.b_of("ok"));
        assert!(v.get("none").unwrap().is_null());
        assert_eq!(v.f_of("pi"), 3.5);
        assert_eq!(v.to_string(), src); // order-preserving, deterministic
    }

    #[test]
    fn escapes_and_unicode() {
        let v = parse(r#""line\nquote\"tab\tback\\slash\/ué😀""#).unwrap();
        assert_eq!(v.s().unwrap(), "line\nquote\"tab\tback\\slash/ué😀");
        // serialize → parse roundtrip keeps content
        let ser = v.to_string();
        assert_eq!(parse(&ser).unwrap(), v);
    }

    #[test]
    fn control_chars_serialized_safely() {
        let v = jstr("bell\u{07}end");
        assert_eq!(v.to_string(), "\"bell\\u0007end\"");
        assert_eq!(parse(&v.to_string()).unwrap(), v);
    }

    #[test]
    fn numbers() {
        for (src, want) in [
            ("0", 0.0),
            ("-12", -12.0),
            ("3.25", 3.25),
            ("1e3", 1000.0),
            ("-2.5E-2", -0.025),
        ] {
            assert_eq!(parse(src).unwrap().f().unwrap(), want, "{src}");
        }
        assert_eq!(jint(42).to_string(), "42");
        assert_eq!(jnum(42.5).to_string(), "42.5");
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["{", "[1,", "\"abc", "{\"a\":}", "tru", "01x", "{} extra", "\"\\ud800\""] {
            assert!(parse(bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn depth_limit() {
        let deep = "[".repeat(200) + &"]".repeat(200);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn set_and_get() {
        let mut v = jobj(vec![("a", jint(1))]);
        v.set("b", jstr("x"));
        v.set("a", jint(2));
        assert_eq!(v.i_of("a"), 2);
        assert_eq!(v.str_of("b"), "x");
    }

    #[test]
    fn pretty_parses_back() {
        let v = jobj(vec![
            ("name", jstr("soma")),
            ("arr", jarr(vec![jint(1), jint(2)])),
            ("obj", jobj(vec![("k", jnull())])),
            ("empty", jarr(vec![])),
        ]);
        assert_eq!(parse(&v.pretty()).unwrap(), v);
    }

    fn jnull() -> Json {
        Json::Null
    }
}
