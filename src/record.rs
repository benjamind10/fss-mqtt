//! One message as a line of JSON: the format of capture files and sessions.
//!
//! ```json
//! {"time":"2026-10-15T08:20:30.123Z","topic":"site/a","qos":1,"retain":false,
//!  "content_type":"application/json","user":[["k","v"]],"size":13,"payload":"{\"t\":21.5}"}
//! ```
//!
//! Properties appear only when the message has them. The payload is a JSON
//! string when it is UTF-8 and `payload_base64` otherwise. A capture holds
//! whole payloads (`size` is their length); a session holds what was kept in
//! memory, `size` is the size on the wire, and each line also says which
//! connection it came from (`conn`) and, for payloads cut on arrival, the
//! view that was decoded from the whole payload (`decoded`).

use web_time::{Duration, SystemTime, UNIX_EPOCH};

use crate::json::{self, Json};
use crate::store::Props;
use crate::view::DocView;

/// Deep enough for any decoded view we write (tables are two levels).
const MAX_DEPTH: usize = 16;

pub struct Record {
    pub conn: Option<String>,
    pub time: SystemTime,
    pub topic: String,
    pub qos: u8,
    pub retain: bool,
    pub props: Props,
    /// The size on the wire; more than `payload.len()` when it was cut.
    pub size: usize,
    pub payload: Vec<u8>,
    pub decoded: Option<DocView>,
}

/// A message as one line of JSON, with the newline.
#[allow(clippy::too_many_arguments)]
pub fn line(
    conn: Option<&str>,
    t: SystemTime,
    topic: &str,
    payload: &[u8],
    size: usize,
    qos: u8,
    retain: bool,
    p: &Props,
    decoded: Option<&DocView>,
) -> String {
    let mut s = String::with_capacity(payload.len() * 4 / 3 + topic.len() + 128);
    s.push('{');
    if let Some(c) = conn {
        s.push_str("\"conn\":");
        json::write_str(&mut s, c);
        s.push(',');
    }
    s.push_str("\"time\":\"");
    s.push_str(&rfc3339(t));
    s.push_str("\",\"topic\":");
    json::write_str(&mut s, topic);
    s.push_str(&format!(",\"qos\":{qos},\"retain\":{retain}"));
    if let Some(v) = &p.content_type {
        s.push_str(",\"content_type\":");
        json::write_str(&mut s, v);
    }
    if let Some(v) = &p.response_topic {
        s.push_str(",\"response_topic\":");
        json::write_str(&mut s, v);
    }
    if let Some(v) = &p.correlation_data {
        s.push_str(&format!(",\"correlation_data\":\"{}\"", crate::base64::encode(v)));
    }
    if let Some(v) = p.message_expiry {
        s.push_str(&format!(",\"message_expiry\":{v}"));
    }
    if let Some(v) = p.payload_format {
        s.push_str(&format!(",\"payload_format\":{v}"));
    }
    if !p.user.is_empty() {
        s.push_str(",\"user\":[");
        for (i, (k, v)) in p.user.iter().enumerate() {
            s.push_str(if i == 0 { "[" } else { ",[" });
            json::write_str(&mut s, k);
            s.push(',');
            json::write_str(&mut s, v);
            s.push(']');
        }
        s.push(']');
    }
    s.push_str(&format!(",\"size\":{size}"));
    match std::str::from_utf8(payload) {
        Ok(text) => {
            s.push_str(",\"payload\":");
            json::write_str(&mut s, text);
        }
        Err(_) => s.push_str(&format!(",\"payload_base64\":\"{}\"", crate::base64::encode(payload))),
    }
    if let Some(d) = decoded {
        s.push_str(",\"decoded\":");
        d.write_json(&mut s);
    }
    s.push_str("}\n");
    s
}

/// Reads one line written by `line` (or by hand, as long as it has a topic).
pub fn parse(b: &[u8]) -> Result<Record, String> {
    let j = Json::parse(b, MAX_DEPTH)?;
    let topic = j.get("topic").and_then(Json::str).ok_or("no topic")?.to_string();
    let time = match j.get("time") {
        Some(t) => t.str().and_then(parse_rfc3339).ok_or("time isn't an RFC 3339 date")?,
        None => SystemTime::now(),
    };
    let payload = match (j.get("payload"), j.get("payload_base64")) {
        (Some(p), _) => p.str().ok_or("payload isn't a string")?.as_bytes().to_vec(),
        (None, Some(p)) => p
            .str()
            .and_then(crate::base64::decode)
            .ok_or("payload_base64 isn't base64")?,
        (None, None) => Vec::new(),
    };
    let size = j
        .get("size")
        .and_then(Json::u64)
        .map_or(payload.len(), |n| n as usize)
        .max(payload.len());
    let num = |k: &str| j.get(k).and_then(Json::u64);
    let text = |k: &str| j.get(k).and_then(Json::str).map(String::from);
    let user = j
        .get("user")
        .and_then(Json::arr)
        .unwrap_or_default()
        .iter()
        .filter_map(|kv| match kv.arr()? {
            [k, v] => Some((k.str()?.to_string(), v.str()?.to_string())),
            _ => None,
        })
        .collect();
    Ok(Record {
        conn: text("conn"),
        time,
        topic,
        qos: num("qos").unwrap_or(0).min(2) as u8,
        retain: j.get("retain").and_then(Json::bool).unwrap_or(false),
        props: Props {
            content_type: text("content_type"),
            response_topic: text("response_topic"),
            correlation_data: text("correlation_data").and_then(|s| crate::base64::decode(&s)),
            payload_format: num("payload_format").map(|n| n.min(255) as u8),
            message_expiry: num("message_expiry").map(|n| n.min(u32::MAX as u64) as u32),
            subscription_id: None,
            user,
        },
        size,
        payload,
        decoded: j.get("decoded").and_then(DocView::from_json),
    })
}

/// RFC 3339 in UTC with milliseconds: `2026-10-15T08:20:30.123Z`.
pub fn rfc3339(t: SystemTime) -> String {
    let ms = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    };
    let (days, ms) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let s = ms / 1000;
    format!(
        "{}T{:02}:{:02}:{:02}.{:03}Z",
        crate::values::date(days),
        s / 3600,
        s / 60 % 60,
        s % 60,
        ms % 1000
    )
}

/// `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`; `t` or a space may stand for
/// the `T`, and `z` for the `Z`.
pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let d = b.get(r)?;
        d.iter()
            .all(u8::is_ascii_digit)
            .then(|| d.iter().fold(0i64, |n, c| n * 10 + (c - b'0') as i64))
    };
    let sep = |i: usize, set: &[u8]| b.get(i).is_some_and(|c| set.contains(c));
    if !(sep(4, b"-") && sep(7, b"-") && sep(10, b"Tt ") && sep(13, b":") && sep(16, b":")) {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut nanos = 0i64;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        i = start;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
        let digits = &s[start..i.min(start + 9)];
        nanos = digits.parse::<i64>().ok()? * 10i64.pow(9 - digits.len() as u32);
    }
    let offset = match b.get(i)? {
        b'Z' | b'z' if i + 1 == b.len() => 0,
        c @ (b'+' | b'-') if i + 6 == b.len() && b[i + 3] == b':' => {
            let (oh, om) = (num(i + 1..i + 3)?, num(i + 4..i + 6)?);
            if oh > 23 || om > 59 {
                return None;
            }
            let o = oh * 3600 + om * 60;
            if *c == b'+' { o } else { -o }
        }
        _ => return None,
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec.min(59) - offset;
    let total = Duration::from_nanos(nanos as u64);
    if secs >= 0 {
        Some(UNIX_EPOCH + Duration::from_secs(secs as u64) + total)
    } else {
        Some(UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs()) + total)
    }
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 (proleptic Gregorian), after H. Hinnant.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::{Body, Fact, Section};

    #[test]
    fn times() {
        for (s, ms) in [
            ("1970-01-01T00:00:00.000Z", 0i64),
            ("2026-10-15T08:20:30.123Z", 1_792_052_430_123),
            ("2000-02-29T23:59:59.999Z", 951_868_799_999),
            ("1969-12-31T23:59:59.000Z", -1000),
        ] {
            let t = parse_rfc3339(s).unwrap_or_else(|| panic!("{s}"));
            let got = match t.duration_since(UNIX_EPOCH) {
                Ok(d) => d.as_millis() as i64,
                Err(e) => -(e.duration().as_millis() as i64),
            };
            assert_eq!(got, ms, "{s}");
            assert_eq!(rfc3339(t), s);
        }
        let z = parse_rfc3339("2026-10-15T08:20:30Z").unwrap();
        assert_eq!(parse_rfc3339("2026-10-15T10:20:30+02:00"), Some(z));
        assert_eq!(parse_rfc3339("2026-10-15 07:20:30-01:00"), Some(z));
        assert_eq!(
            parse_rfc3339("2026-10-15t08:20:30.5z"),
            Some(z + Duration::from_millis(500))
        );
        for bad in [
            "",
            "2026-10-15",
            "2026-13-01T00:00:00Z",
            "2026-02-29T00:00:00Z",
            "2026-10-15T24:00:00Z",
            "2026-10-15T08:20:30",
            "2026-10-15T08:20:30.Z",
            "2026-10-15T08:20:30+0200",
            "2026-10-15T08:20:30Zx",
            "x026-10-15T08:20:30Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn lines_round_trip() {
        let t = parse_rfc3339("2026-10-15T08:20:30.123Z").unwrap();
        let props = Props {
            content_type: Some("text/plain".into()),
            response_topic: Some("re/ply".into()),
            correlation_data: Some(vec![0, 0xff]),
            payload_format: Some(1),
            message_expiry: Some(600),
            subscription_id: Some(7), // not written
            user: vec![("k".into(), "v\"1".into()), ("k".into(), "é".into())],
        };
        let mut view = DocView::new("Parquet");
        view.summary.push(Fact::new("3", "rows"));
        view.meta.push(("creator".into(), "x".into()));
        view.sections.push(Section {
            title: "Rows".into(),
            detail: "· all".into(),
            body: Body::Table {
                header: vec!["a".into()],
                rows: vec![vec![Some("1".into())], vec![None]],
            },
        });
        view.notes.push("a note".into());
        for (conn, payload, size, decoded) in [
            (None, "x\"y\\z\n\t\u{1}é🌬".as_bytes(), 14usize, None),
            (Some("oslo"), &[0u8, 0xff, 0xfe][..], 3_000_000, Some(&view)),
        ] {
            let l = line(conn, t, "a/\"b\"", payload, size, 2, true, &props, decoded);
            assert!(l.ends_with("}\n") && l.matches('\n').count() == 1, "{l}");
            let r = parse(l.trim_end().as_bytes()).unwrap();
            assert_eq!(r.conn.as_deref(), conn);
            assert_eq!((r.time, r.topic.as_str(), r.qos, r.retain), (t, "a/\"b\"", 2, true));
            assert_eq!(r.payload, payload);
            assert_eq!(r.size, size);
            assert_eq!(
                r.props,
                Props {
                    subscription_id: None,
                    ..props.clone()
                }
            );
            assert_eq!(r.decoded.is_some(), decoded.is_some());
            if let Some(d) = r.decoded {
                assert_eq!((d.format, d.summary[0].value.as_str()), ("Parquet", "3"));
                assert_eq!(d.notes, ["a note"]);
            }
        }
        // A capture line has no conn, size is the payload's own.
        let l = line(None, t, "c", b"hi", 2, 0, false, &Props::default(), None);
        assert_eq!(
            l,
            "{\"time\":\"2026-10-15T08:20:30.123Z\",\"topic\":\"c\",\"qos\":0,\"retain\":false,\"size\":2,\"payload\":\"hi\"}\n"
        );
    }

    #[test]
    fn lenient_and_strict_reading() {
        // Only the topic is needed; a size below the payload's is ignored.
        let r = parse(br#"{"topic":"t","payload":"abc","size":1,"extra":{"x":[1]}}"#).unwrap();
        assert_eq!((r.payload.as_slice(), r.size, r.qos), (&b"abc"[..], 3, 0));
        assert!(parse(br#"{"payload":"x"}"#).err().unwrap().contains("no topic"));
        assert!(
            parse(br#"{"topic":"t","time":"yesterday"}"#)
                .err()
                .unwrap()
                .contains("RFC 3339")
        );
        assert!(
            parse(br#"{"topic":"t","payload_base64":"!!"}"#)
                .err()
                .unwrap()
                .contains("base64")
        );
        assert!(parse(b"{\"topic\":").is_err());
        // A damaged decoded view is dropped, not fatal.
        let r = parse(br#"{"topic":"t","payload":"x","decoded":{"format":7}}"#).unwrap();
        assert!(r.decoded.is_none());
    }
}
