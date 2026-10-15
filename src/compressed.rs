//! gzip and zstd payloads: unpacked on arrival, then shown like any other
//! payload. A binary format inside (Parquet, MessagePack, …) goes to its
//! decoder; text (JSON, XML, CSV, …) is kept as a prefix and formatted when
//! shown, exactly like an uncompressed message.

use std::io::Read;

use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};

use crate::fmt::human_bytes;
use crate::view::{Body, DocView, Fact};

/// Unpacking stops here; the rest is dropped with a note.
const MAX_OUT: usize = 64 << 20;
/// zstd frames asking for a larger window (allocated up front) are refused.
const MAX_WINDOW: u64 = 32 << 20;
/// Bytes of unpacked text kept, as `--max-payload` does for plain payloads.
const KEEP: usize = 4096;
const CHUNK: usize = 64 << 10;

const GZIP_MAGIC: &[u8] = b"\x1f\x8b\x08";
const ZSTD_MAGIC: &[u8] = b"\x28\xb5\x2f\xfd";

/// Decoder entry point (see `pretty::Decoder`): recognised by its magic bytes
/// or a content-type naming gzip or zstd.
pub fn sniff(ct: &str, b: &[u8]) -> Option<Result<DocView, String>> {
    let kind = if b.starts_with(GZIP_MAGIC) {
        "gzip"
    } else if b.starts_with(ZSTD_MAGIC) || skippable(b).is_some() && ct.contains("zstd") {
        "zstd"
    } else if ct.contains("gzip") {
        return Some(Err("gzip: no gzip header".into()));
    } else if ct.contains("zstd") {
        return Some(Err("zstd: no zstd frame".into()));
    } else {
        return None;
    };
    let mut out = Vec::new();
    let mut notes = Vec::new();
    let capped = if kind == "gzip" {
        gunzip(b, &mut out, &mut notes)
    } else {
        unzstd(b, &mut out, &mut notes)
    };
    Some(capped.map(|capped| present(kind, ct, b.len(), out, capped, notes)))
}

/// The view of an unpacked payload: an inner binary format's own view, or
/// the first bytes of the content, formatted when shown.
fn present(kind: &'static str, ct: &str, packed: usize, out: Vec<u8>, capped: bool, mut notes: Vec<String>) -> DocView {
    let over = if capped { "over " } else { "" };
    let sizes = Fact::new(
        kind,
        format!("{} → {over}{}", human_bytes(packed), human_bytes(out.len())),
    );
    if capped {
        notes.push(format!("only the first {} were unpacked", human_bytes(MAX_OUT)));
    }
    if let Some(mut v) = crate::pretty::decode_unpacked(ct, &out) {
        v.summary.insert(0, sizes);
        v.notes.extend(notes);
        return v;
    }
    let keep = &out[..out.len().min(KEEP)];
    let inner = crate::pretty::detect_payload(keep, out.len(), ct);
    let mut v = DocView::new(kind);
    v.summary = vec![sizes, Fact::new(inner.label(), "inside")];
    v.section(
        "",
        "",
        Body::Payload {
            bytes: keep.to_vec(),
            size: out.len(),
            content_type: ct.to_string(),
        },
    );
    v.notes = notes;
    v
}

/// Appends what fits under `MAX_OUT`; true when something didn't fit.
fn append(out: &mut Vec<u8>, b: &[u8]) -> bool {
    let room = MAX_OUT - out.len();
    out.extend_from_slice(&b[..b.len().min(room)]);
    b.len() > room
}

/// Bytes after the last member or frame: zero padding is common, anything
/// else is worth a note.
fn trailing(rest: &[u8], notes: &mut Vec<String>) {
    if rest.iter().any(|&b| b != 0) {
        notes.push(format!("{} after the end ignored", human_bytes(rest.len())));
    }
}

// ── gzip ───────────────────────────────────────────────────────────────────

/// The length of the RFC 1952 member header at the start of `b`.
fn gzip_header(b: &[u8]) -> Result<usize, String> {
    let bad = || "gzip: bad header".to_string();
    if !b.starts_with(GZIP_MAGIC) || b.len() < 10 {
        return Err(bad());
    }
    let flags = b[3];
    let mut p = 10;
    if flags & 4 != 0 {
        // FEXTRA
        let x = b.get(p..p + 2).ok_or_else(bad)?;
        p += 2 + u16::from_le_bytes([x[0], x[1]]) as usize;
    }
    for bit in [8u8, 16] {
        // FNAME, FCOMMENT: zero-terminated
        if flags & bit != 0 {
            p += b
                .get(p..)
                .ok_or_else(bad)?
                .iter()
                .position(|&c| c == 0)
                .ok_or_else(bad)?
                + 1;
        }
    }
    if flags & 2 != 0 {
        p += 2; // FHCRC
    }
    if p > b.len() {
        return Err(bad());
    }
    Ok(p)
}

/// Inflates every member of a gzip stream into `out`; true when capped.
pub(crate) fn gunzip(b: &[u8], out: &mut Vec<u8>, notes: &mut Vec<String>) -> Result<bool, String> {
    let mut state = InflateState::new_boxed(DataFormat::Raw);
    let mut buf = vec![0u8; CHUNK];
    let mut p = 0;
    loop {
        p += gzip_header(&b[p..])?;
        state.reset(DataFormat::Raw);
        let start = out.len();
        loop {
            let r = inflate(&mut state, &b[p..], &mut buf, MZFlush::None);
            p += r.bytes_consumed;
            if append(out, &buf[..r.bytes_written]) {
                return Ok(true);
            }
            match r.status {
                Ok(MZStatus::StreamEnd) => break,
                Ok(_) if r.bytes_consumed > 0 || r.bytes_written > 0 => {}
                Ok(_) | Err(MZError::Buf) => return Err("gzip: the stream ends early".into()),
                Err(_) => return Err("gzip: corrupt data".into()),
            }
        }
        // Trailer: CRC-32 of the member's data, then its size mod 2^32.
        let t = b.get(p..p + 8).ok_or("gzip: the stream ends early")?;
        if u32::from_le_bytes([t[4], t[5], t[6], t[7]]) != (out.len() - start) as u32 {
            return Err("gzip: size check failed".into());
        }
        if u32::from_le_bytes([t[0], t[1], t[2], t[3]]) != crate::crc32::crc32(&out[start..]) {
            return Err("gzip: checksum mismatch, the data is corrupt".into());
        }
        p += 8;
        if !b[p..].starts_with(GZIP_MAGIC) {
            trailing(&b[p..], notes);
            return Ok(false);
        }
    }
}

// ── zstd ───────────────────────────────────────────────────────────────────

/// The total length of a skippable frame at the start of `b`.
fn skippable(b: &[u8]) -> Option<usize> {
    if b.len() < 8 || b[0] & 0xf0 != 0x50 || b[1..4] != *b"\x2a\x4d\x18" {
        return None;
    }
    Some(8 + u32::from_le_bytes([b[4], b[5], b[6], b[7]]) as usize)
}

/// Decodes every frame of a zstd stream into `out`; true when capped.
fn unzstd(b: &[u8], out: &mut Vec<u8>, notes: &mut Vec<String>) -> Result<bool, String> {
    let mut src = b;
    let mut buf = vec![0u8; CHUNK];
    loop {
        if let Some(n) = skippable(src) {
            src = src.get(n..).ok_or("zstd: a skippable frame ends early")?;
        } else {
            let mut d = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(&mut src, MAX_WINDOW)
                .map_err(|e| format!("zstd: {e}"))?;
            loop {
                let n = d.read(&mut buf).map_err(|e| format!("zstd: {e}"))?;
                if n == 0 {
                    break;
                }
                if append(out, &buf[..n]) {
                    return Ok(true);
                }
            }
        }
        if !src.starts_with(ZSTD_MAGIC) && skippable(src).is_none() {
            trailing(src, notes);
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use ratatui::text::Line;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    /// Decoded as on arrival, rendered as shown.
    fn show(ct: Option<&str>, b: &[u8]) -> (DocView, Vec<String>) {
        let v = crate::pretty::decode_on_arrival(ct, b).expect("recognised");
        let t = text(&v.render(&Theme::new(false)));
        (v, t)
    }

    #[test]
    fn gzip_json() {
        let (v, t) = show(None, &fixture("doc.json.gz"));
        assert_eq!(v.format, "gzip");
        assert!(!v.has_table());
        assert_eq!(t[0], "gzip 328 B → 1.9 KB · JSON inside");
        assert_eq!(t[2..5], ["{", "  \"site\": \"oslo\",", "  \"turbines\": ["]);
        assert!(t.iter().any(|l| l == "      \"id\": \"T01\","));
        assert_eq!(t.last().unwrap(), "}", "whole document, not cut");
    }

    #[test]
    fn zstd_json_and_frames() {
        let (v, t) = show(Some("application/zstd"), &fixture("doc.json.zst"));
        assert_eq!(v.format, "zstd");
        assert!(t[0].starts_with("zstd 288 B → "), "{}", t[0]);
        assert_eq!(t[2], "{");
        // Two frames with a skippable one between them.
        let (_, t) = show(None, &fixture("frames.json.zst"));
        assert_eq!(
            t,
            [
                "zstd 60 B → 22 B · JSON inside",
                "",
                "{",
                "  \"part\": 1,",
                "  \"done\": true",
                "}"
            ]
        );
    }

    #[test]
    fn gzip_csv_is_a_table() {
        let (v, t) = show(Some("text/csv+gzip"), &fixture("turbines.csv.gz"));
        assert!(v.has_table());
        assert!(t[0].ends_with(" · CSV inside"), "{}", t[0]);
        assert_eq!(t[2], "turbine │ power │ status ");
        assert_eq!(t[4], "T01     │  12.5 │ RUNNING");
        assert!(!crate::pretty::wraps(crate::pretty::Format::Decoded(
            v.format,
            v.has_table()
        )));
    }

    #[test]
    fn zstd_parquet() {
        let (v, t) = show(None, &fixture("turbine-snappy.parquet.zst"));
        assert_eq!(v.format, "Parquet");
        assert!(v.has_table());
        assert!(t[0].starts_with("zstd 5.5 KB → 20.3 KB · 500 rows"), "{}", t[0]);
        assert!(
            t.iter()
                .any(|l| l.contains("Timestamp") && l.contains("TIMESTAMP(ms, UTC)"))
        );
    }

    #[test]
    fn gzip_members_and_long_text() {
        let (_, t) = show(None, &fixture("multi.txt.gz"));
        assert_eq!(
            t,
            [
                "gzip 67 B → 27 B · text inside",
                "",
                "first member",
                "second member",
                ""
            ]
        );
        let (_, t) = show(Some("application/gzip"), &fixture("long.txt.gz"));
        assert_eq!(t[0], "gzip 1.0 KB → 21.5 KB · text inside");
        assert_eq!(t[2], "line 0000: the quick brown fox jumps over the lazy dog");
        assert_eq!(t.last().unwrap(), "… first 4.0 KB of 21.5 KB");
    }

    #[test]
    fn binary_inside() {
        let (v, t) = show(None, &fixture("random.bin.zst"));
        assert_eq!(v.format, "zstd");
        assert!(t[0].ends_with(" → 2.9 KB · binary inside"), "{}", t[0]);
        assert!(t[2].starts_with("00000000  "), "{}", t[2]);
    }

    #[test]
    fn errors() {
        // Error views end with the reason, after a hex dump of the raw bytes.
        let read = |ct: Option<&str>, b: &[u8]| {
            let t = show(ct, b).1;
            assert!(t[0].starts_with("Raw bytes"), "{t:?}");
            t.last().unwrap().clone()
        };
        // Says gzip, isn't: an error view showing the bytes, not a silent fallback.
        let t = show(Some("application/gzip"), b"{}").1;
        assert_eq!(
            t,
            [
                "Raw bytes · 2 B",
                "00000000  7b 7d                                             {}",
                "note: could not read this compressed payload: gzip: no gzip header"
            ]
        );
        let gz = fixture("doc.json.gz");
        assert_eq!(
            read(None, &gz[..gz.len() / 2]),
            "note: could not read this compressed payload: gzip: the stream ends early"
        );
        // One flipped bit inside the deflate data that still inflates: caught by the CRC.
        let mut bad = gz.clone();
        let crc_at = bad.len() - 8;
        bad[crc_at] ^= 1;
        assert_eq!(
            read(None, &bad),
            "note: could not read this compressed payload: gzip: checksum mismatch, the data is corrupt"
        );
        let zst = fixture("doc.json.zst");
        assert!(read(None, &zst[..zst.len() - 10]).starts_with("note: could not read this compressed payload: zstd: "));
        // A frame that needs a dictionary (id 1) we don't have.
        let dict = b"\x28\xb5\x2f\xfd\x01\x00\x01\x00\x00\x00";
        assert!(read(None, dict).starts_with("note: could not read this compressed payload: zstd: "));
        assert!(crate::pretty::decode_on_arrival(Some("application/json"), b"{}").is_none());
    }

    #[test]
    fn damaged_input() {
        // xorshift: reproducible without a dependency.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let th = Theme::new(false);
        for name in [
            "doc.json.gz",
            "doc.json.zst",
            "turbines.csv.gz",
            "multi.txt.gz",
            "frames.json.zst",
            "turbine-snappy.parquet.zst",
        ] {
            let orig = fixture(name);
            for _ in 0..300 {
                let mut b = orig.clone();
                for _ in 0..1 + rnd(4) {
                    let i = rnd(b.len());
                    b[i] ^= 1 << rnd(8);
                }
                if rnd(3) == 0 {
                    b.truncate(rnd(b.len()));
                }
                if let Some(v) = crate::pretty::decode_on_arrival(None, &b) {
                    v.render(&th);
                }
            }
        }
    }

    #[test]
    fn bombs_are_capped() {
        // zstd: 100 MB of zeros in 3 KB.
        let (v, t) = show(None, &fixture("zeros.zst"));
        assert!(
            t[0].starts_with("zstd 3.1 KB → over 64.0 MB · binary inside"),
            "{}",
            t[0]
        );
        assert_eq!(v.notes, ["only the first 64.0 MB were unpacked"]);
        // gzip: 100 members of 1 MB of zeros each.
        let member = {
            let zeros = vec![0u8; 1 << 20];
            let mut m = b"\x1f\x8b\x08\0\0\0\0\0\0\xff".to_vec();
            m.extend(miniz_oxide::deflate::compress_to_vec(&zeros, 9));
            m.extend(crate::crc32::crc32(&zeros).to_le_bytes());
            m.extend((zeros.len() as u32).to_le_bytes());
            m
        };
        let bomb = member.repeat(100);
        let mut out = Vec::new();
        assert_eq!(gunzip(&bomb, &mut out, &mut Vec::new()), Ok(true));
        assert_eq!((out.len(), out.capacity()), (MAX_OUT, MAX_OUT));
        let (_, t) = show(None, &bomb);
        assert!(t[0].ends_with("→ over 64.0 MB · binary inside"), "{}", t[0]);
    }
}
