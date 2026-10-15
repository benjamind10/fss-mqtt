//! Protocol Buffers without a schema: the raw fields view, like
//! `protoc --decode_raw`. Field numbers and wire-level values, with nested
//! messages indented.
//!
//! Without the `.proto` the wire format is ambiguous: a varint may be an int,
//! a bool or an enum, and a length-delimited field may be a string, bytes, a
//! nested message or a packed repeated field. Values are shown as what they
//! most likely are, never as a guessed type name.
//!
//! Raw protobuf can't be recognised from its bytes, so only payloads with a
//! protobuf content-type are decoded. Runs on the full payload as it arrives;
//! lengths are never trusted, depth and output are capped, and nothing panics.
//! A payload that breaks off or goes wrong after some fields shows those
//! fields with a note; one that fails at its first field is an error.

use crate::fmt::{commas, plural};
use crate::view::{Body, DocView, Fact};

/// Rendered lines stop at about this many bytes.
const TEXT_BUDGET: usize = 4096;
/// Fields kept while parsing; more could never be shown within the budget.
const MAX_FIELDS: usize = 1000;
const MAX_DEPTH: usize = 16;
/// Characters of a string, bytes in a hex dump and values of a packed list shown.
const MAX_STR: usize = 200;
const MAX_HEX: usize = 32;
const MAX_LIST: usize = 32;

/// Decoder entry point (see `pretty::Decoder`): recognised by content-type only.
pub fn sniff(ct: &str, b: &[u8]) -> Option<Result<DocView, String>> {
    is_protobuf(ct).then(|| decode(ct, b))
}

/// `application/x-protobuf`, `application/vnd.google.protobuf`, `application/proto`,
/// `application/grpc` and `application/grpc+proto` (`ct` is lowercase). Protobuf's
/// JSON and text formats (`+json`, `text/…`) aren't binary and are left alone.
fn is_protobuf(ct: &str) -> bool {
    let mime = ct.split(';').next().unwrap_or("").trim();
    if mime.starts_with("text/") || mime.contains("json") {
        return false;
    }
    mime.contains("protobuf")
        || mime == "application/grpc"
        || mime.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| t == "proto")
}

/// The message inside a gRPC length-prefixed frame: compressed flag, then a
/// 4-byte big-endian length that must cover exactly the rest.
fn grpc_frame(b: &[u8]) -> Option<(bool, &[u8])> {
    let (head, rest) = b.split_at_checked(5)?;
    let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
    (head[0] <= 1 && len == rest.len()).then_some((head[0] == 1, rest))
}

fn decode(ct: &str, payload: &[u8]) -> Result<DocView, String> {
    let mut b = payload;
    let mut framed = false;
    if ct.contains("grpc")
        && let Some((compressed, msg)) = grpc_frame(payload)
    {
        if compressed {
            return Err("the gRPC message is compressed, and its grpc-encoding header doesn't travel over MQTT".into());
        }
        b = msg;
        framed = true;
    }
    let mut p = Parser::default();
    let mut fields = Vec::new();
    let mut pos = 0;
    let err = p.fields(b, &mut pos, 0, None, &mut fields).err();
    if let Some(e) = &err
        && fields.is_empty()
    {
        return Err(e.clone());
    }

    let mut out = Out::default();
    out.fields(&fields, 0);
    let mut v = DocView::new("Protobuf");
    v.summary = vec![
        Fact::new(commas(b.len() as u64), "bytes"),
        Fact::new(
            format!("{}{}", fields.len(), if p.cut { "+" } else { "" }),
            if fields.len() == 1 {
                "top-level field"
            } else {
                "top-level fields"
            },
        ),
        Fact::new("", "raw fields, no schema"),
    ];
    if framed {
        v.summary.push(Fact::new("", "gRPC frame"));
    }
    if !out.lines.is_empty() {
        v.section("", "", Body::Lines(out.lines));
    }
    if let Some(e) = err {
        v.notes.push(format!(
            "malformed after {}: {e}",
            plural(fields.len(), "top-level field", "top-level fields")
        ));
    }
    if p.cut || out.cut {
        v.notes
            .push(format!("cut to about {} KiB of output", TEXT_BUDGET / 1024));
    }
    Ok(v)
}

// ── Wire format ────────────────────────────────────────────────────────────

struct Field<'a> {
    num: u32,
    val: Val<'a>,
}

enum Val<'a> {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    /// Length-delimited, parsed as a nested message.
    Msg(Vec<Field<'a>>),
    /// A (deprecated) group: the fields between start and end tags.
    Group(Vec<Field<'a>>),
    Str(&'a str),
    /// Length-delimited, all varints.
    Packed(&'a [u8]),
    Bytes(&'a [u8]),
}

#[derive(Default)]
struct Parser {
    /// Fields kept so far, across all levels.
    kept: usize,
    /// Stopped at `MAX_FIELDS`; what was parsed so far stands.
    cut: bool,
}

impl Parser {
    /// Parses fields from `b[*pos..]` into `out` until the end, or until the end
    /// tag of `group`. On error, `out` keeps the fields before it.
    fn fields<'a>(
        &mut self,
        b: &'a [u8],
        pos: &mut usize,
        depth: usize,
        group: Option<u32>,
        out: &mut Vec<Field<'a>>,
    ) -> Result<(), String> {
        loop {
            if self.cut {
                return Ok(());
            }
            if *pos >= b.len() {
                return match group {
                    Some(g) => Err(format!("group {g} is not closed")),
                    None => Ok(()),
                };
            }
            let at = *pos;
            let tag = varint(b, pos).map_err(|e| format!("{e} at byte {at}"))?;
            let num = tag >> 3;
            if num == 0 || num > (1 << 29) - 1 {
                return Err(format!("field number {num} at byte {at}"));
            }
            let num = num as u32;
            let val = match tag & 7 {
                0 => Val::Varint(varint(b, pos).map_err(|e| format!("{e} at byte {at}"))?),
                1 => Val::Fixed64(u64::from_le_bytes(take(b, pos, at)?)),
                5 => Val::Fixed32(u32::from_le_bytes(take(b, pos, at)?)),
                2 => {
                    let len = varint(b, pos).map_err(|e| format!("{e} at byte {at}"))?;
                    let end = usize::try_from(len)
                        .ok()
                        .and_then(|l| pos.checked_add(l))
                        .filter(|&e| e <= b.len())
                        .ok_or_else(|| format!("field {num} at byte {at}: length {len} runs past the end"))?;
                    let s = &b[*pos..end];
                    *pos = end;
                    self.len_delimited(s, depth)
                }
                3 => {
                    if depth >= MAX_DEPTH {
                        return Err(format!("groups nested deeper than {MAX_DEPTH} at byte {at}"));
                    }
                    let mut kids = Vec::new();
                    let r = self.fields(b, pos, depth + 1, Some(num), &mut kids);
                    out.push(Field {
                        num,
                        val: Val::Group(kids),
                    });
                    r?;
                    continue;
                }
                4 if group == Some(num) => return Ok(()),
                4 => return Err(format!("end of group {num} without its start at byte {at}")),
                w => return Err(format!("field {num} has wire type {w} at byte {at}")),
            };
            out.push(Field { num, val });
            self.kept += 1;
            if self.kept >= MAX_FIELDS {
                self.cut = true;
            }
        }
    }

    /// What a length-delimited value most likely is: readable text, a nested
    /// message, packed varints, or else bytes.
    fn len_delimited<'a>(&mut self, s: &'a [u8], depth: usize) -> Val<'a> {
        // Text first: short strings often happen to parse as a message too
        // ("hi" is field 13 = 105), while real messages are rarely all printable.
        if let Some(t) = text(s) {
            return Val::Str(t);
        }
        if depth < MAX_DEPTH {
            let kept = self.kept;
            let mut kids = Vec::new();
            let mut pos = 0;
            let ok = self.fields(s, &mut pos, depth + 1, None, &mut kids).is_ok();
            if ok && !kids.is_empty() && (pos == s.len() || self.cut) {
                return Val::Msg(kids);
            }
            self.kept = kept;
        }
        if packed(s).is_some() {
            Val::Packed(s)
        } else {
            Val::Bytes(s)
        }
    }
}

pub(crate) fn varint(b: &[u8], pos: &mut usize) -> Result<u64, &'static str> {
    let mut v = 0u64;
    for i in 0..10 {
        let byte = *b.get(*pos).ok_or("truncated varint")?;
        *pos += 1;
        if i == 9 && byte > 1 {
            return Err("varint overflows 64 bits");
        }
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte < 0x80 {
            return Ok(v);
        }
    }
    Err("varint longer than 10 bytes")
}

pub(crate) fn take<const N: usize>(b: &[u8], pos: &mut usize, at: usize) -> Result<[u8; N], String> {
    let s = pos
        .checked_add(N)
        .and_then(|end| b.get(*pos..end))
        .ok_or_else(|| format!("truncated fixed{} at byte {at}", N * 8))?;
    *pos += N;
    s.try_into()
        .map_err(|_| format!("truncated fixed{} at byte {at}", N * 8))
}

/// Printable UTF-8 (tabs and line breaks allowed), and not only whitespace
/// unless empty.
fn text(s: &[u8]) -> Option<&str> {
    let t = std::str::from_utf8(s).ok()?;
    let printable = t.chars().all(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'));
    let blank = !t.is_empty() && t.chars().all(|c| matches!(c, '\t' | '\n' | '\r'));
    (printable && !blank).then_some(t)
}

/// The values of a packed repeated varint field, if `s` plausibly is one:
/// minimally encoded varints that are all small (< 16384) or negative 64-bit
/// (as negative int32/int64 are written). Random bytes rarely pass that, so
/// hashes and other binary blobs stay bytes; packed large values do too.
fn packed(s: &[u8]) -> Option<Vec<u64>> {
    if s.is_empty() {
        return None;
    }
    let mut vals = Vec::new();
    let mut pos = 0;
    while pos < s.len() {
        let start = pos;
        let v = varint(s, &mut pos).ok()?;
        let len = pos - start;
        let minimal = len == 1 || s[pos - 1] != 0;
        if !minimal || !(len <= 2 || (len == 10 && v > i64::MAX as u64)) {
            return None;
        }
        vals.push(v);
    }
    Some(vals)
}

// ── Rendering ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct Out {
    lines: Vec<String>,
    bytes: usize,
    cut: bool,
}

impl Out {
    fn push(&mut self, line: String) {
        if self.cut || self.bytes + line.len() > TEXT_BUDGET {
            self.cut = true;
            return;
        }
        self.bytes += line.len() + 1;
        self.lines.push(line);
    }

    fn fields(&mut self, fields: &[Field], indent: usize) {
        let pad = "  ".repeat(indent);
        for f in fields {
            if self.cut {
                return;
            }
            match &f.val {
                Val::Msg(kids) | Val::Group(kids) => {
                    self.push(format!("{pad}{} {{", f.num));
                    self.fields(kids, indent + 1);
                    self.push(format!("{pad}}}"));
                }
                v => self.push(format!("{pad}{}: {}", f.num, value(v))),
            }
        }
    }
}

fn value(v: &Val) -> String {
    match *v {
        Val::Varint(n) if n > i64::MAX as u64 => format!("{n} ({})", n as i64),
        Val::Varint(n) => n.to_string(),
        Val::Fixed64(n) => {
            let f = f64::from_bits(n);
            if plausible(f) {
                format!("{f} (double)")
            } else if n > i64::MAX as u64 {
                format!("{n} (fixed64, {})", n as i64)
            } else {
                format!("{n} (fixed64)")
            }
        }
        Val::Fixed32(n) => {
            let f = f32::from_bits(n);
            if plausible(f64::from(f)) {
                format!("{f} (float)")
            } else if n > i32::MAX as u32 {
                format!("{n} (fixed32, {})", n as i32)
            } else {
                format!("{n} (fixed32)")
            }
        }
        Val::Str(s) => quote(s),
        Val::Packed(s) => {
            let vals = packed(s).unwrap_or_default();
            let mut out = String::from("[");
            for (i, v) in vals.iter().take(MAX_LIST).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                // Only small values and negative 64-bit ones are taken as packed.
                out.push_str(&(*v as i64).to_string());
            }
            if vals.len() > MAX_LIST {
                out.push_str(&format!(", …] ({} values)", vals.len()));
            } else {
                out.push(']');
            }
            out
        }
        Val::Bytes(b) => {
            let mut h: String = b.iter().take(MAX_HEX).map(|x| format!("{x:02x}")).collect();
            if b.len() > MAX_HEX {
                h.push('…');
            }
            format!("<{} bytes: {h}>", commas(b.len() as u64))
        }
        Val::Msg(_) | Val::Group(_) => String::new(),
    }
}

/// A float that reads like a measurement rather than integer bits: zero, or
/// finite and between 1e-9 and 1e15 in magnitude.
fn plausible(f: f64) -> bool {
    f == 0.0 || (f.is_finite() && (1e-9..=1e15).contains(&f.abs()))
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for (i, c) in s.chars().enumerate() {
        if i == MAX_STR {
            out.push('…');
            break;
        }
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CT: &str = "application/x-protobuf";

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn lines(v: &DocView) -> Vec<String> {
        match v.sections.as_slice() {
            [s] => match &s.body {
                Body::Lines(l) => l.clone(),
                _ => panic!("not lines"),
            },
            [] => Vec::new(),
            _ => panic!("more than one section"),
        }
    }

    /// The rendered fields of `b`, which must decode without notes.
    fn render(b: &[u8]) -> Vec<String> {
        let v = sniff(CT, b).unwrap().unwrap();
        assert!(v.notes.is_empty(), "{:?}", v.notes);
        lines(&v)
    }

    fn summary(v: &DocView) -> String {
        v.summary
            .iter()
            .map(|f| format!("{} {}", f.value, f.label).trim().to_string())
            .collect::<Vec<_>>()
            .join(" · ")
    }

    const TURBINE: [&str; 22] = [
        "1: \"T01\"",
        "2: 2265.8 (double)",
        "3: 11.5 (float)",
        "4: 18446744073709551574 (-42)",
        "5: 5",
        "6: 1",
        "7: 3",
        "8 {",
        "  1: 59.91 (double)",
        "  2: 10.75 (double)",
        "}",
        "9: [9, 10, 11]",
        "10: \"north\"",
        "10: \"offshore\"",
        "11: <4 bytes: 0001feff>",
        "12 {",
        "  1: \"gearbox\"",
        "  2: 61.5 (double)",
        "}",
        "12 {",
        "  1: \"nacelle\"",
        "  2: 24.25 (double)",
    ];

    #[test]
    fn renders_the_fixture() {
        let v = sniff(CT, &fixture("turbine.pb")).unwrap().unwrap();
        assert_eq!(v.format, "Protobuf");
        assert_eq!(summary(&v), "124 bytes · 14 top-level fields · raw fields, no schema");
        let mut want = TURBINE.to_vec();
        want.push("}");
        assert_eq!(lines(&v), want);
        assert!(v.notes.is_empty());
    }

    /// `protoc --decode_raw` style, to compare field numbers and values with
    /// its output for the fixture.
    fn protoc_style(fields: &[Field], indent: usize, out: &mut Vec<String>) {
        let pad = "  ".repeat(indent);
        for f in fields {
            let raw = match &f.val {
                Val::Msg(kids) | Val::Group(kids) => {
                    out.push(format!("{pad}{} {{", f.num));
                    protoc_style(kids, indent + 1, out);
                    out.push(format!("{pad}}}"));
                    continue;
                }
                Val::Varint(n) => n.to_string(),
                Val::Fixed64(n) => format!("0x{n:016x}"),
                Val::Fixed32(n) => format!("0x{n:08x}"),
                Val::Str(s) => cescape(s.as_bytes()),
                Val::Packed(b) | Val::Bytes(b) => cescape(b),
            };
            out.push(format!("{pad}{}: {raw}", f.num));
        }
    }

    fn cescape(b: &[u8]) -> String {
        let mut s = String::from("\"");
        for &c in b {
            match c {
                b'\n' => s.push_str("\\n"),
                b'\r' => s.push_str("\\r"),
                b'\t' => s.push_str("\\t"),
                b'"' | b'\'' | b'\\' => {
                    s.push('\\');
                    s.push(c as char);
                }
                0x20..0x7f => s.push(c as char),
                _ => s.push_str(&format!("\\{c:03o}")),
            }
        }
        s.push('"');
        s
    }

    #[test]
    fn agrees_with_protoc_decode_raw() {
        let b = fixture("turbine.pb");
        let mut p = Parser::default();
        let mut fields = Vec::new();
        p.fields(&b, &mut 0, 0, None, &mut fields).unwrap();
        let mut ours = Vec::new();
        protoc_style(&fields, 0, &mut ours);
        let want = String::from_utf8(fixture("turbine.txt")).unwrap();
        assert_eq!(ours, want.lines().collect::<Vec<_>>());
    }

    #[test]
    fn strips_grpc_framing() {
        let v = sniff("application/grpc+proto", &fixture("turbine.grpc"))
            .unwrap()
            .unwrap();
        assert_eq!(
            summary(&v),
            "124 bytes · 14 top-level fields · raw fields, no schema · gRPC frame"
        );
        assert_eq!(lines(&v)[..22], TURBINE);
        // Only with a gRPC content-type, and only when the length matches.
        assert!(sniff(CT, &fixture("turbine.grpc")).unwrap().is_err());
        assert!(sniff("application/grpc", &[0, 0, 0, 0, 3, 8, 1]).unwrap().is_err());
        let v = sniff("application/grpc", &[0, 0, 0, 0, 2, 8, 1]).unwrap().unwrap();
        assert_eq!(lines(&v), ["1: 1"]);
        let v = sniff("application/grpc", &[1, 0, 0, 0, 2, 8, 1]).unwrap();
        assert!(v.err().unwrap().contains("compressed"));
    }

    #[test]
    fn recognised_by_content_type_only() {
        let b = fixture("turbine.pb");
        for ct in [
            "application/x-protobuf",
            "application/protobuf",
            "application/vnd.google.protobuf",
            "application/x-protobuf; messagetype=\"demo.turbine\"",
            "application/grpc+proto",
            "application/grpc",
            "application/proto",
        ] {
            assert!(sniff(ct, &b).is_some(), "{ct}");
        }
        for ct in [
            "",
            "application/octet-stream",
            "application/grpc+json",
            "text/prototype",
            "application/protobuf+json",
            "application/x-protobuf+json",
            "text/x-proto-text",
            "text/x-protobuf",
            "text/plain; proto=1",
        ] {
            assert!(sniff(ct, &b).is_none(), "{ct}");
        }
        let d = crate::pretty::decode_on_arrival(Some("application/x-protobuf"), &b).unwrap();
        assert_eq!(d.format, "Protobuf");
        assert_eq!(lines(&d)[..22], TURBINE);
        assert!(crate::pretty::decode_on_arrival(None, &b).is_none());
    }

    #[test]
    fn gzipped_protobuf() {
        // Unpacked by the gzip decoder, then decoded as protobuf by its content type.
        let b = fixture("turbine.pb");
        let mut gz = b"\x1f\x8b\x08\0\0\0\0\0\0\xff".to_vec();
        gz.extend(miniz_oxide::deflate::compress_to_vec(&b, 6));
        gz.extend(crate::crc32::crc32(&b).to_le_bytes());
        gz.extend((b.len() as u32).to_le_bytes());
        let d = crate::pretty::decode_on_arrival(Some("application/x-protobuf"), &gz).unwrap();
        assert_eq!(d.format, "Protobuf");
        assert_eq!(d.summary[0].value, "gzip");
        assert_eq!(lines(&d)[..22], TURBINE);
    }

    #[test]
    fn varints_and_fixed_widths() {
        // 1: 150, 2: -1 as int64, 3: 0
        let mut b = vec![0x08, 0x96, 0x01, 0x10];
        b.extend([0xff; 9]);
        b.extend([0x01, 0x18, 0x00]);
        // 4: double 1.5, 5: fixed64 5, 6: sfixed64 -2
        b.push(0x21);
        b.extend(1.5f64.to_le_bytes());
        b.push(0x29);
        b.extend(5u64.to_le_bytes());
        b.push(0x31);
        b.extend((-2i64).to_le_bytes());
        // 7: float -0.25, 8: fixed32 7, 9: sfixed32 -1
        b.push(0x3d);
        b.extend((-0.25f32).to_le_bytes());
        b.push(0x45);
        b.extend(7u32.to_le_bytes());
        b.push(0x4d);
        b.extend((-1i32).to_le_bytes());
        // 536870911: 1 (the largest field number)
        b.extend([0xf8, 0xff, 0xff, 0xff, 0x0f, 0x01]);
        assert_eq!(
            render(&b),
            [
                "1: 150",
                "2: 18446744073709551615 (-1)",
                "3: 0",
                "4: 1.5 (double)",
                "5: 5 (fixed64)",
                "6: 18446744073709551614 (fixed64, -2)",
                "7: -0.25 (float)",
                "8: 7 (fixed32)",
                "9: 4294967295 (fixed32, -1)",
                "536870911: 1",
            ]
        );
    }

    #[test]
    fn strings_bytes_packed_and_messages() {
        let ld = |num: u8, s: &[u8]| {
            let mut b = vec![num << 3 | 2, s.len() as u8];
            b.extend(s);
            b
        };
        let cases: [(&[u8], &str); 11] = [
            // Printable text wins even when it also parses as a message.
            (b"hi", "1: \"hi\""),
            (b"", "1: \"\""),
            ("a \"q\"\n✓".as_bytes(), "1: \"a \\\"q\\\"\\n✓\""),
            // Only whitespace: packed varints.
            (b"\t\n", "1: [9, 10]"),
            (&[1, 2, 3, 0xac, 0x02], "1: [1, 2, 3, 300]"),
            // Too large, not minimal, or not varints at all: bytes.
            (&[0x80, 0x80, 0x01], "1: <3 bytes: 808001>"),
            (&[0x81, 0x00], "1: <2 bytes: 8100>"),
            (&[0x00, 0x01, 0xfe, 0xff], "1: <4 bytes: 0001feff>"),
            (&[0x7f], "1: [127]"),
            // A field 1 varint 1: a message, though also two small varints.
            (&[0x08, 0x01], "1 {"),
            (&[0x0a, 0x00], "1 {"),
        ];
        for (s, want) in cases {
            assert_eq!(render(&ld(1, s))[0], want, "{s:?}");
        }
        let mut neg = vec![0x01];
        neg.extend([0xff; 9]);
        neg.push(0x01);
        assert_eq!(render(&ld(4, &neg)), ["4: [1, -1]"]);
        let long: Vec<u8> = (0..40).collect();
        assert_eq!(
            render(&ld(2, &long))[0],
            "2: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, \
             26, 27, 28, 29, 30, 31, …] (40 values)"
        );
        let blob: Vec<u8> = (0..40).map(|i| 0x80 | i).collect();
        assert_eq!(
            render(&ld(3, &blob))[0],
            format!(
                "3: <40 bytes: {}…>",
                (0x80..0xa0).map(|x| format!("{x:02x}")).collect::<String>()
            )
        );
        let long = "x".repeat(300);
        let mut b = vec![0x0a, 0xac, 0x02];
        b.extend(long.as_bytes());
        assert_eq!(render(&b)[0], format!("1: \"{}…\"", "x".repeat(MAX_STR)));
    }

    #[test]
    fn nested_messages_and_groups() {
        // 1 { 2 { 3: 1 } 4: "x" }, then group 5 { 6: 2 }
        let b = [
            0x0a, 0x09, 0x12, 0x02, 0x18, 0x01, 0x22, 0x01, b'x', 0x30, 0x02, 0x2b, 0x30, 0x02, 0x2c,
        ];
        assert_eq!(
            render(&b),
            [
                "1 {",
                "  2 {",
                "    3: 1",
                "  }",
                "  4: \"x\"",
                "  6: 2",
                "}",
                "5 {",
                "  6: 2",
                "}"
            ]
        );
        // Nested deeper than MAX_DEPTH: the innermost levels stay bytes.
        let mut b = vec![0x08, 0x01];
        for _ in 0..20 {
            let mut outer = vec![0x0a, b.len() as u8];
            outer.extend(b);
            b = outer;
        }
        let l = render(&b);
        assert_eq!(l.iter().filter(|l| l.ends_with('{')).count(), MAX_DEPTH);
        assert!(l[MAX_DEPTH].trim_start().starts_with("1: "), "{}", l[MAX_DEPTH]);
        // Groups that don't close, or close the wrong one.
        assert!(
            sniff(CT, &[0x0b, 0x08, 0x01])
                .unwrap()
                .is_ok_and(|v| v.notes[0].contains("not closed"))
        );
        assert_eq!(
            sniff(CT, &[0x0c]).unwrap().err().unwrap(),
            "end of group 1 without its start at byte 0"
        );
        assert_eq!(sniff(CT, &[0x0b, 0x14]).unwrap().unwrap().notes.len(), 1);
    }

    #[test]
    fn malformed_payloads() {
        let err = |b: &[u8]| sniff(CT, b).unwrap().err().unwrap();
        assert_eq!(err(&[0x07]), "field number 0 at byte 0");
        assert_eq!(err(&[0x0e]), "field 1 has wire type 6 at byte 0");
        assert_eq!(err(&[0x08]), "truncated varint at byte 0");
        assert_eq!(
            err(&[0x0a, 0x05, 0x01]),
            "field 1 at byte 0: length 5 runs past the end"
        );
        assert_eq!(err(&[0x09, 0x01]), "truncated fixed64 at byte 0");
        // Good fields, then garbage: those fields and a note.
        let v = sniff(CT, &[0x08, 0x01, 0x10, 0x02, 0xff]).unwrap().unwrap();
        assert_eq!(lines(&v), ["1: 1", "2: 2"]);
        assert_eq!(
            v.notes,
            ["malformed after 2 top-level fields: truncated varint at byte 4"]
        );
        // Empty: a valid message without fields.
        let v = sniff(CT, b"").unwrap().unwrap();
        assert_eq!(summary(&v), "0 bytes · 0 top-level fields · raw fields, no schema");
        assert!(v.sections.is_empty());
    }

    #[test]
    fn output_is_capped() {
        let b: Vec<u8> = (0..5000).flat_map(|i| [0x08, (i % 100) as u8]).collect();
        let v = sniff(CT, &b).unwrap().unwrap();
        let l = lines(&v);
        assert!(l.iter().map(|l| l.len() + 1).sum::<usize>() <= TEXT_BUDGET);
        assert!(l.len() > 500);
        assert_eq!(v.notes, ["cut to about 4 KiB of output"]);
        assert!(summary(&v).starts_with("10,000 bytes · 1000+ top-level fields"));
    }

    #[test]
    fn never_panics_on_damaged_input() {
        let good = fixture("turbine.pb");
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..2000 {
            let mut f = good.clone();
            for _ in 0..(rnd() % 8 + 1) {
                let i = (rnd() as usize) % f.len();
                f[i] = rnd() as u8;
            }
            let _ = sniff(CT, &f);
            let cut = (rnd() as usize) % f.len();
            let _ = sniff(CT, &f[..cut]);
            let _ = sniff("application/grpc", &f[..cut]);
            let noise: Vec<u8> = (0..rnd() % 300).map(|_| rnd() as u8).collect();
            let _ = sniff(CT, &noise);
        }
        // Deeply nested groups and messages.
        assert!(sniff(CT, &[0x0b; 10_000]).unwrap().is_ok_and(|v| !v.notes.is_empty()));
        let _ = sniff(CT, &[0x0a; 10_000]);
    }
}
