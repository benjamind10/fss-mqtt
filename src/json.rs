//! A small JSON reader and a string writer, for the files fss-mqtt reads and
//! writes (captures, sessions) and for Avro schemas.

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    /// In the order written; the first of a repeated key wins in `get`.
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// Parses one value (surrounded by whitespace at most), nested at most
    /// `max_depth` arrays and objects deep.
    pub fn parse(b: &[u8], max_depth: usize) -> Result<Json, String> {
        let mut p = Parser { b, p: 0, max_depth };
        let v = p.value(0)?;
        if p.peek().is_some() {
            return Err(format!("unexpected data after the value at byte {}", p.p));
        }
        Ok(v)
    }

    pub fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.iter().find(|(n, _)| n == k).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn num(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// A whole number from 0 up to 2^53 (exact in a double).
    pub fn u64(&self) -> Option<u64> {
        self.num()
            .filter(|n| n.fract() == 0.0 && (0.0..=9_007_199_254_740_992.0).contains(n))
            .map(|n| n as u64)
    }

    pub fn bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
}

struct Parser<'a> {
    b: &'a [u8],
    p: usize,
    max_depth: usize,
}

impl Parser<'_> {
    fn peek(&mut self) -> Option<u8> {
        while self
            .b
            .get(self.p)
            .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.p += 1;
        }
        self.b.get(self.p).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        let ok = self.peek() == Some(c);
        if ok {
            self.p += 1;
        }
        ok
    }

    fn bad<T>(&self) -> Result<T, String> {
        Err(match self.b.get(self.p) {
            Some(_) => format!("invalid JSON at byte {}", self.p),
            None => "the JSON ends early".into(),
        })
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        match self.peek() {
            Some(b'{' | b'[') if depth >= self.max_depth => {
                Err(format!("JSON nested more than {} deep", self.max_depth))
            }
            Some(b'{') => {
                self.p += 1;
                let mut m = Vec::new();
                if self.eat(b'}') {
                    return Ok(Json::Obj(m));
                }
                loop {
                    if self.peek() != Some(b'"') {
                        return self.bad();
                    }
                    let k = self.string()?;
                    if !self.eat(b':') {
                        return self.bad();
                    }
                    m.push((k, self.value(depth + 1)?));
                    if self.eat(b'}') {
                        return Ok(Json::Obj(m));
                    }
                    if !self.eat(b',') {
                        return self.bad();
                    }
                }
            }
            Some(b'[') => {
                self.p += 1;
                let mut a = Vec::new();
                if self.eat(b']') {
                    return Ok(Json::Arr(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    if self.eat(b']') {
                        return Ok(Json::Arr(a));
                    }
                    if !self.eat(b',') {
                        return self.bad();
                    }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b'-' | b'0'..=b'9') => {
                let start = self.p;
                while self.b.get(self.p).is_some_and(|c| b"+-.eE0123456789".contains(c)) {
                    self.p += 1;
                }
                // Rust's float syntax is a superset of JSON's: check the shape.
                let s = std::str::from_utf8(&self.b[start..self.p]).unwrap_or("");
                match s.parse::<f64>() {
                    Ok(n) if json_number(s.as_bytes()) && n.is_finite() => Ok(Json::Num(n)),
                    _ => {
                        self.p = start;
                        self.bad()
                    }
                }
            }
            Some(_) => {
                for (word, v) in [
                    ("true", Json::Bool(true)),
                    ("false", Json::Bool(false)),
                    ("null", Json::Null),
                ] {
                    if self.b[self.p..].starts_with(word.as_bytes()) {
                        self.p += word.len();
                        return Ok(v);
                    }
                }
                self.bad()
            }
            None => self.bad(),
        }
    }

    /// A string; the cursor is on its opening quote.
    fn string(&mut self) -> Result<String, String> {
        let open = self.p;
        self.p += 1;
        let mut s: Vec<u8> = Vec::new();
        loop {
            // Copy the run up to the next quote, escape or control character.
            let run = self.b[self.p..]
                .iter()
                .position(|&c| c == b'"' || c == b'\\' || c < 0x20)
                .ok_or_else(|| format!("unterminated string at byte {open}"))?;
            s.extend_from_slice(&self.b[self.p..self.p + run]);
            self.p += run;
            match self.b[self.p] {
                b'"' => {
                    self.p += 1;
                    return String::from_utf8(s).map_err(|_| format!("a string at byte {open} isn't UTF-8"));
                }
                b'\\' => {
                    let e = *self
                        .b
                        .get(self.p + 1)
                        .ok_or_else(|| format!("unterminated string at byte {open}"))?;
                    self.p += 2;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xd800..0xdc00).contains(&hi) && self.b[self.p..].starts_with(b"\\u") {
                                self.p += 2;
                                let lo = self.hex4()?;
                                if (0xdc00..0xe000).contains(&lo) {
                                    0x10000 + ((hi - 0xd800) << 10) + (lo - 0xdc00)
                                } else {
                                    0xfffd
                                }
                            } else {
                                hi
                            };
                            char::from_u32(code).unwrap_or('\u{fffd}')
                        }
                        _ => {
                            self.p -= 1;
                            return self.bad();
                        }
                    };
                    s.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
                _ => return self.bad(), // a raw control character
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.b.get(self.p..self.p + 4).ok_or("the JSON ends early")?;
        let v = std::str::from_utf8(h)
            .ok()
            .and_then(|h| u32::from_str_radix(h, 16).ok());
        match v {
            Some(v) if h.iter().all(u8::is_ascii_hexdigit) => {
                self.p += 4;
                Ok(v)
            }
            _ => self.bad(),
        }
    }
}

/// `-? (0 | [1-9][0-9]*) (\.[0-9]+)? ([eE][+-]?[0-9]+)?`
fn json_number(b: &[u8]) -> bool {
    let digits = |b: &[u8], i: &mut usize| {
        let start = *i;
        while b.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i > start
    };
    let mut i = usize::from(b.first() == Some(&b'-'));
    match b.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            digits(b, &mut i);
        }
        _ => return false,
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if !digits(b, &mut i) {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(b, &mut i) {
            return false;
        }
    }
    i == b.len()
}

/// Appends `s` as a JSON string, quoted and escaped.
pub fn write_str(out: &mut String, s: &str) {
    out.push('"');
    let mut rest = s;
    while let Some(i) = rest.find(|c: char| c == '"' || c == '\\' || (c as u32) < 0x20) {
        out.push_str(&rest[..i]);
        let c = rest[i..].chars().next().unwrap();
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push_str(&format!("\\u{:04x}", c as u32)),
        }
        rest = &rest[i + 1..];
    }
    out.push_str(rest);
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Json, String> {
        Json::parse(s.as_bytes(), 64)
    }

    #[test]
    fn values() {
        let v = p(r#" {"a": [1, -2.5e3, true, false, null], "b": "x\"y\\z\u00e9\ud83d\ude00\/", "c": {}} "#).unwrap();
        let a = v.get("a").unwrap().arr().unwrap();
        assert_eq!(
            a,
            &[
                Json::Num(1.0),
                Json::Num(-2500.0),
                Json::Bool(true),
                Json::Bool(false),
                Json::Null
            ]
        );
        assert_eq!(v.get("b").unwrap().str(), Some("x\"y\\zé😀/"));
        assert_eq!(v.get("c"), Some(&Json::Obj(vec![])));
        assert_eq!(p("42").unwrap().u64(), Some(42));
        assert_eq!(p("4.5").unwrap().u64(), None);
        assert_eq!(p("-1").unwrap().u64(), None);
        // The first of a repeated key wins.
        assert_eq!(p(r#"{"k":1,"k":2}"#).unwrap().get("k").unwrap().num(), Some(1.0));
    }

    #[test]
    fn errors() {
        for bad in [
            "",
            "{",
            "[1,]",
            "{\"a\" 1}",
            "01",
            "1.",
            ".5",
            "+1",
            "nan",
            "inf",
            "\"x",
            "\"\\q\"",
            "\"\\u12\"",
            "\"a\nb\"",
            "[1] x",
            "tru",
            "{\"a\":1,}",
            "\"\\ud83d\"x",
        ] {
            // A lone surrogate isn't an error, it becomes U+FFFD; trailing data is.
            if bad == "\"\\ud83d\"x" {
                assert!(p(bad).unwrap_err().contains("after the value"));
                continue;
            }
            assert!(p(bad).is_err(), "{bad:?} parsed");
        }
        assert_eq!(p("\"\\ud83d\"").unwrap().str(), Some("\u{fffd}"));
        assert!(Json::parse(&[b'['; 100], 64).unwrap_err().contains("nested"));
        assert!(Json::parse(b"[[[]]]", 3).is_ok());
        assert!(Json::parse(b"[[[[]]]]", 3).is_err());
        assert!(Json::parse(b"\"\xff\"", 4).unwrap_err().contains("UTF-8"));
    }

    #[test]
    fn strings_round_trip() {
        let all: String = (0u32..0x250)
            .filter_map(char::from_u32)
            .chain("😀\u{2028}".chars())
            .collect();
        let mut out = String::new();
        write_str(&mut out, &all);
        assert!(out.bytes().all(|b| b >= 0x20), "control characters escaped");
        assert_eq!(p(&out).unwrap().str(), Some(all.as_str()));
        let mut out = String::new();
        write_str(&mut out, "a\"b\\c\nd\u{1}");
        assert_eq!(out, r#""a\"b\\c\nd\u0001""#);
    }
}
