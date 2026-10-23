//! Elspec PQZip, the files Elspec's BLACKBOX power quality analysers record
//! (PQZip's patent, US 7,415,370, has expired). What's read so far is the
//! header: when the recording starts and ends, the device (its names,
//! model, firmware, MAC address) and its channels' nominal values. The
//! measurements after it are in a layout not worked out yet; their size is
//! shown.
//!
//! The header, as two analysers' files have it (firmware 0.4.10 and 0.4.11):
//! `97 24 00 03`, eight bytes, then the start and the end, each a pair of
//! big-endian u64 that add up to 100 ns ticks since 1601 (a byte between
//! the pairs); then "Elspec PQZIP File" and some bytes, then strings, each
//! a big-endian u16 length and its text; then each channel as `01`, its id
//! (u16), `0d`, a kind byte and three big-endian f32, the first its nominal.

use crate::view::{Body, DocView, Fact, Section};

const MAGIC: [u8; 4] = [0x97, 0x24, 0x00, 0x03];
const MARK: &[u8] = b"Elspec PQZIP File";
/// 1601-01-01 to 1970-01-01, in 100 ns ticks.
const UNIX_TICKS: u64 = 116_444_736_000_000_000;
/// What the header's strings are, in order.
const FIELDS: [&str; 8] = [
    "Device", "Site", "Model", "Hardware", "Firmware", "MAC", "Operator", "Company",
];

pub fn sniff(ct: &str, b: &[u8]) -> Option<Result<DocView, String>> {
    let by_type = ct.contains("pqzip");
    if b.starts_with(&MAGIC) && b.get(0x2f..0x2f + MARK.len()) == Some(MARK) {
        Some(decode(b))
    } else if by_type {
        Some(Err("no Elspec PQZIP header".into()))
    } else {
        None
    }
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// A time in the header: the pair at `at`, as the device's clock has it.
fn time_at(b: &[u8], at: usize) -> Result<u64, String> {
    let (t, off) = (u64_at(b, at), u64_at(b, at + 8));
    let (Some(t), Some(off)) = (t, off) else {
        return Err("the header is cut short".into());
    };
    t.checked_add(off)
        .ok_or_else(|| "a time in the header is out of range".into())
}

/// A time as shown; one too far out for a date shows as its ticks.
fn shown(ticks: u64) -> String {
    match i64::try_from((ticks as i128 - UNIX_TICKS as i128) / 10_000) {
        Ok(ms) if ms.unsigned_abs() < 1 << 50 => crate::values::timestamp(ms, 3, false),
        _ => format!("{ticks} ticks"),
    }
}

/// Length-prefixed strings from `from`: where the first one starts (a run of
/// them), and the strings.
fn strings(b: &[u8], from: usize) -> (usize, Vec<String>) {
    let text = |at: usize| -> Option<(String, usize)> {
        let n = u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]) as usize;
        let s = b.get(at + 2..at + 2 + n)?;
        (n <= 128 && s.iter().all(|c| (0x20..0x7f).contains(c)))
            .then(|| (String::from_utf8_lossy(s).into_owned(), at + 2 + n))
    };
    // The run starts with a string that has text; after that an empty one
    // (no operator set, say) keeps its place, up to as many as there are.
    for start in from..(from + 64).min(b.len()) {
        if text(start).is_none_or(|(s, _)| s.is_empty()) {
            continue;
        }
        let mut out = Vec::new();
        let mut at = start;
        while out.len() < FIELDS.len()
            && let Some((s, next)) = text(at)
        {
            out.push(s);
            at = next;
        }
        if out.iter().filter(|s| !s.is_empty()).count() >= 3 {
            return (at, out);
        }
    }
    (from, Vec::new())
}

/// The channels after the strings: (id, nominal), as many as follow one
/// another, and where they end (`from` without any).
fn channels(b: &[u8], from: usize) -> (Vec<(u16, f32)>, usize) {
    let Some(first) = b[from..b.len().min(from + 256)]
        .windows(4)
        .position(|w| w[0] == 1 && w[1] == 0x23 && w[3] == 0x0d)
    else {
        return (Vec::new(), from);
    };
    let mut out = Vec::new();
    let mut at = from + first;
    while let Some(r) = b.get(at..at + 17) {
        if r[0] != 1 || r[3] != 0x0d {
            break;
        }
        let id = u16::from_be_bytes([r[1], r[2]]);
        out.push((id, f32::from_be_bytes([r[5], r[6], r[7], r[8]])));
        at += 17;
    }
    (out, at)
}

/// A channel's name and unit, by the ids two analysers' files share.
fn channel(id: u16) -> (String, &'static str) {
    match id {
        9000..=9002 => (format!("V{}", id - 8999), "V"),
        9003..=9005 => (format!("I{}", id - 9002), "A"),
        9012 => ("VN".into(), "V"),
        9015 => ("IN".into(), "A"),
        9009 => ("Frequency".into(), "Hz"),
        _ => (format!("channel {id}"), ""),
    }
}

pub fn decode(b: &[u8]) -> Result<DocView, String> {
    let start = time_at(b, 0x0c)?;
    let end = time_at(b, 0x1d)?;
    let mut v = DocView::new("Elspec PQZip");
    let (after, fields) = strings(b, 0x2f + MARK.len());
    let field = |name: &str| FIELDS.iter().position(|f| *f == name).and_then(|i| fields.get(i));
    if let Some(model) = field("Model") {
        v.summary.push(Fact::new(model.clone(), ""));
    }
    if let Some(d) = field("Device") {
        v.summary.push(Fact::new(d.clone(), "device"));
    }
    let secs = end.saturating_sub(start) as f64 / 1e7;
    v.summary.push(Fact::new(format!("{secs:.1} s"), "recorded"));
    v.summary.push(Fact::new(crate::fmt::human_bytes(b.len()), ""));

    let mut rows = vec![
        vec![Some("Start".to_string()), Some(shown(start))],
        vec![Some("End".to_string()), Some(shown(end))],
    ];
    rows.extend(
        FIELDS
            .iter()
            .zip(&fields)
            .map(|(k, v)| vec![Some(k.to_string()), Some(v.clone())]),
    );
    v.sections.push(Section {
        title: "Recording".into(),
        detail: "· times by the device's clock".into(),
        body: Body::Table {
            header: vec!["field".into(), "value".into()],
            rows,
        },
    });

    let (ch, after) = channels(b, after);
    if !ch.is_empty() {
        let rows = ch
            .iter()
            .map(|&(id, nominal)| {
                let (name, unit) = channel(id);
                vec![Some(name), Some(format!("{nominal} {unit}").trim_end().to_string())]
            })
            .collect();
        v.sections.push(Section {
            title: "Channels".into(),
            detail: String::new(),
            body: Body::Table {
                header: vec!["channel".into(), "nominal".into()],
                rows,
            },
        });
    }
    v.notes.push(format!(
        "The measurements ({}) are in a layout not decoded yet.",
        crate::fmt::human_bytes(b.len().saturating_sub(after))
    ));
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header as an analyser writes it (made up: a test's own names).
    fn header() -> Vec<u8> {
        let strings = [
            "Test01",
            "TESTSITE",
            "BLACKBOX",
            "3x5x2",
            "0.4.10.23",
            "00-60-35-00-00-01",
            "OPS",
            "ACME",
        ];
        header_with(&strings, 0x01a3_0b30_69a2_42fd)
    }

    fn header_with(strings: &[&str], start: u64) -> Vec<u8> {
        let end: u64 = 0x01a3_0b31_2ca3_499b;
        let off: u64 = 0x0039_9ab0_e3d4_be16;
        let mut b = MAGIC.to_vec();
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0x08, 0x80]);
        b.extend_from_slice(&start.to_be_bytes());
        b.extend_from_slice(&off.to_be_bytes());
        b.push(6);
        b.extend_from_slice(&end.to_be_bytes());
        b.extend_from_slice(&off.to_be_bytes());
        b.extend_from_slice(&(MARK.len() as u16).to_be_bytes());
        b.extend_from_slice(MARK);
        b.extend_from_slice(&[1, 0, 0, 1, 0x3a, 3, 2, 1, 0x2c, 0x11, 0x4e, 0, 0x62]);
        for s in strings {
            b.extend_from_slice(&(s.len() as u16).to_be_bytes());
            b.extend_from_slice(s.as_bytes());
        }
        b.extend_from_slice(&[0x04, 0x3c, 0x0b, 0x00, 0xcb]);
        for (id, kind, nominal) in [(9000u16, 9u8, 230.9f32), (9003, 7, 50.0), (9009, 7, 50.0)] {
            b.push(1);
            b.extend_from_slice(&id.to_be_bytes());
            b.extend_from_slice(&[0x0d, kind]);
            for f in [nominal, 1.0, 0.097_656_25] {
                b.extend_from_slice(&f.to_be_bytes());
            }
        }
        b.extend_from_slice(&[0x04, 0x17, 0x72, 0x08]);
        b.extend_from_slice(&[0u8; 100]);
        b
    }

    fn table(v: &DocView, i: usize) -> Vec<(String, String)> {
        let Body::Table { rows, .. } = &v.sections[i].body else {
            panic!()
        };
        rows.iter()
            .map(|r| (r[0].clone().unwrap(), r[1].clone().unwrap()))
            .collect()
    }

    #[test]
    fn the_header() {
        let b = header();
        let v = sniff("", &b).unwrap().unwrap();
        assert_eq!(v.format, "Elspec PQZip");
        let facts: Vec<String> = v.summary.iter().map(|f| format!("{} {}", f.value, f.label)).collect();
        assert_eq!(facts[..3], ["BLACKBOX ", "Test01 device", "327.2 s recorded"]);
        let rec = table(&v, 0);
        // The times the files' names give: the device's clock, to the ms.
        assert_eq!(rec[0], ("Start".into(), "2026-02-24 23:00:00.014".into()));
        assert_eq!(rec[1], ("End".into(), "2026-02-24 23:05:27.176".into()));
        assert!(rec.contains(&("Site".into(), "TESTSITE".into())));
        assert!(rec.contains(&("Firmware".into(), "0.4.10.23".into())));
        assert_eq!(
            table(&v, 1),
            vec![
                ("V1".into(), "230.9 V".into()),
                ("I1".into(), "50 A".into()),
                ("Frequency".into(), "50 Hz".into()),
            ]
        );
        // What follows the channels, not the channels too.
        assert!(
            v.notes[0].contains("(104 B)") && v.notes[0].contains("not decoded yet"),
            "{}",
            v.notes[0]
        );
    }

    #[test]
    fn an_empty_string_keeps_its_place() {
        let strings = [
            "Test01",
            "TESTSITE",
            "BLACKBOX",
            "3x5x2",
            "0.4.10.23",
            "00-60-35-00-00-01",
            "",
            "ACME",
        ];
        let rec = table(
            &sniff("", &header_with(&strings, 0x01a3_0b30_69a2_42fd))
                .unwrap()
                .unwrap(),
            0,
        );
        assert!(rec.contains(&("Operator".into(), "".into())));
        assert!(rec.contains(&("Company".into(), "ACME".into())), "{rec:?}");
    }

    #[test]
    fn times_out_of_range() {
        let strings = ["Test01", "TESTSITE", "BLACKBOX"];
        let e = sniff("", &header_with(&strings, u64::MAX)).unwrap().err().unwrap();
        assert!(e.contains("out of range"), "{e}");
        // Far out but adding up: shown as ticks rather than a date.
        let v = sniff("", &header_with(&strings, u64::MAX - 0x0039_9ab0_e3d4_be16))
            .unwrap()
            .unwrap();
        assert!(table(&v, 0)[0].1.ends_with(" ticks"), "{:?}", table(&v, 0)[0]);
    }

    #[test]
    fn not_pqzip() {
        assert!(sniff("", b"\x97\x24\x00\x03 something else entirely, long enough to look").is_none());
        assert!(sniff("application/x-pqzip", b"nope").unwrap().is_err());
        // Content types reach the decoders lowercased.
        let v = crate::pretty::decode_on_arrival(Some("application/x-PQZip"), b"nope").unwrap();
        assert_eq!(v.format, "Elspec PQZip");
        assert!(sniff("", &header()[..20]).is_none());
    }
}
