//! Numbers to chart for the topic in view. A message's own rows: the columns
//! of a table payload (Parquet, Arrow, Avro, SQLite, CSV, …) and the number
//! arrays and record lists in JSON. And, message by message, the numbers each
//! message carries once: a number payload, a JSON document's number fields,
//! Sparkplug metrics.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::json::Json;
use crate::pretty::{self, Format};
use crate::store::Message;
use crate::view::Body;

/// A column, array or field worth a chart.
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    /// `ActivePower`, `power_kw`, `readings.v`, `turbines · rated_kw`.
    pub name: String,
    /// x counts rows from 0, or (over time) seconds from the first message.
    pub points: Vec<(f64, f64)>,
    /// The ends of the x axis: a table's first column (when it isn't a
    /// number column) or row numbers; the first and last arrival times.
    pub ends: (String, String),
    /// Where the points come from: "60 rows", "first 52 of 500 rows", "10 messages".
    pub source: String,
    /// One value per message, rather than the rows of one.
    pub over_time: bool,
}

/// The series for one topic, and which one is shown.
#[derive(Default)]
pub struct State {
    /// What the series were made from (topic, newest message, picked
    /// message, count); unchanged means nothing to do.
    key: Option<Key>,
    pub series: Vec<Series>,
    pub sel: usize,
    /// The series last picked, by name, so that another topic with the
    /// same field or column shows it too.
    picked: Option<String>,
    /// The numbers each message carries (by seq), read once.
    fields: HashMap<u64, Arc<Vec<(String, f64)>>>,
}

/// (topic node, newest message's seq, picked message's seq, messages).
pub type Key = (usize, u64, Option<u64>, u64);

impl State {
    /// Whether the series are those of `key`.
    pub fn is_for(&self, key: Key) -> bool {
        self.key == Some(key)
    }

    /// Makes the series for a topic's messages (newest first, as the store
    /// keeps them); `focused` is the one whose own rows are charted.
    pub fn update(&mut self, key: Key, msgs: &[Arc<Message>], focused: usize) {
        if self.key.map(|k| k.0) != Some(key.0) {
            self.fields.clear();
        }
        self.key = Some(key);
        let kept: HashSet<u64> = msgs.iter().map(|m| m.seq).collect();
        self.fields.retain(|seq, _| kept.contains(seq));
        let shown = self.series.get(self.sel).map(|s| s.name.clone());
        let mut series = msgs.get(focused).map(|m| rows(m)).unwrap_or_default();
        series.extend(over_time(msgs, &mut self.fields));
        self.series = series;
        // The one picked, else the one shown before, else the first.
        let find = |name: &Option<String>| {
            name.as_ref()
                .and_then(|n| self.series.iter().position(|s| &s.name == n))
        };
        self.sel = find(&self.picked).or_else(|| find(&shown)).unwrap_or(0);
    }

    pub fn clear(&mut self) {
        self.key = None;
        self.series.clear();
        self.sel = 0;
        self.fields.clear();
    }

    /// The topic node the series are of.
    pub fn topic(&self) -> Option<usize> {
        self.key.map(|k| k.0)
    }

    pub fn current(&self) -> Option<&Series> {
        self.series.get(self.sel)
    }

    /// Picks the series `delta` places on (back when negative), short of the ends.
    pub fn step(&mut self, delta: isize) {
        self.pick(self.sel.saturating_add_signed(delta));
    }

    /// Picks the `i`th series, or the last when there are fewer.
    pub fn pick(&mut self, i: usize) {
        let Some(last) = self.series.len().checked_sub(1) else {
            return;
        };
        self.sel = i.min(last);
        self.picked = Some(self.series[self.sel].name.clone());
    }
}

/// A message's own rows: a series per number column of its tables and per
/// number array (or record field) of its JSON.
pub fn rows(m: &Message) -> Vec<Series> {
    let mut out = Vec::new();
    let Some(v) = m.decoded.as_deref() else {
        text_rows(&m.payload, m.size, content_type(m), &mut out);
        return out;
    };
    // Tables of data, not of what the payload holds (its schema, SQLite's
    // list of tables) or of named values (fields, below).
    let data = |s: &crate::view::Section| match &s.body {
        Body::Table { header, rows } => {
            !["Schema", "Tables"].contains(&s.title.as_str()) && named_values(header, rows).is_none()
        }
        _ => false,
    };
    let tables = v.sections.iter().filter(|s| data(s)).count();
    for s in &v.sections {
        match &s.body {
            Body::Table { header, rows } if data(s) => {
                // Several tables (SQLite): their columns go by table name.
                let prefix = if tables > 1 { s.title.as_str() } else { "" };
                out.extend(table(prefix, header, rows, &s.detail));
            }
            Body::Json(t) => json_rows(t.as_bytes(), &mut out),
            Body::Payload {
                bytes,
                size,
                content_type,
            } => text_rows(bytes, *size, content_type, &mut out),
            _ => {}
        }
    }
    out
}

fn content_type(m: &Message) -> &str {
    m.props.content_type.as_deref().unwrap_or("")
}

fn text_rows(p: &[u8], size: usize, ct: &str, out: &mut Vec<Series>) {
    match pretty::detect_payload(p, size, ct) {
        Format::Json => json_rows(p, out),
        Format::Csv(d) => csv_rows(p, d, p.len() < size, out),
        _ => {}
    }
}

/// A number, as a table shows one; not the empty string, NaN or infinity.
fn number(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// A series per column whose cells are all numbers (nulls and blanks
/// aside), at least two of them. `detail` is the section's ("· first 52 of 500").
fn table(prefix: &str, header: &[String], rows: &[Vec<Option<String>>], detail: &str) -> Vec<Series> {
    fn cell(r: &[Option<String>], j: usize) -> Option<&str> {
        r.get(j).and_then(|c| c.as_deref()).filter(|c| !c.trim().is_empty())
    }
    let column = |j: usize| -> Option<Vec<(f64, f64)>> {
        let mut pts = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            if let Some(c) = cell(r, j) {
                pts.push((i as f64, number(c)?));
            }
        }
        (pts.len() >= 2).then_some(pts)
    };
    // The x axis goes by the first column when it names the rows (a time,
    // a turbine), else by row number.
    let ends = match (
        column(0),
        rows.first().and_then(|r| cell(r, 0)),
        rows.last().and_then(|r| cell(r, 0)),
    ) {
        (None, Some(a), Some(b)) => without_common_day(a, b),
        _ => ("1".to_string(), rows.len().to_string()),
    };
    let source = rows_source(rows.len(), detail);
    let mut out = Vec::new();
    for (j, name) in header.iter().enumerate() {
        if let Some(points) = column(j) {
            out.push(Series {
                name: if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix} · {name}")
                },
                points,
                ends: ends.clone(),
                source: source.clone(),
                over_time: false,
            });
        }
    }
    out
}

/// Two times on the same day without it: "10:20:00Z" and "10:20:51Z" for
/// "2026-10-08 10:20:00Z" and "2026-10-08 10:20:51Z" (or with a `T`).
/// Anything else as it is.
fn without_common_day(a: &str, b: &str) -> (String, String) {
    let common = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    // The day ends at a space or a `T` that follows a digit.
    let cut = a[..common]
        .char_indices()
        .filter(|&(i, c)| c == ' ' || (c == 'T' && i > 0 && a.as_bytes()[i - 1].is_ascii_digit()))
        .map(|(i, _)| i + 1)
        .next_back();
    match cut {
        Some(i) if i < a.len() && i < b.len() && a[..i].bytes().any(|c| c.is_ascii_digit()) => {
            (a[i..].to_string(), b[i..].to_string())
        }
        _ => (a.to_string(), b.to_string()),
    }
}

/// "60 rows", or "first 52 of 500 rows" when the table was cut short.
fn rows_source(n: usize, detail: &str) -> String {
    let total = detail
        .split_once(" of ")
        .map(|(_, rest)| {
            rest.chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',')
                .filter(|c| *c != ',')
        })
        .and_then(|digits| digits.collect::<String>().parse::<usize>().ok());
    match total {
        Some(t) if t > n => format!("first {n} of {} rows", crate::fmt::commas(t as u64)),
        _ if n == 1 => "1 row".into(),
        _ => format!("{n} rows"),
    }
}

/// A table that lists named values, as Sparkplug's metrics do: a `name` and
/// a `value` column, each name once. Its numbers, by name.
fn named_values(header: &[String], rows: &[Vec<Option<String>>]) -> Option<Vec<(String, f64)>> {
    let col = |n: &str| header.iter().position(|h| h == n);
    let (name, value) = (col("name")?, col("value")?);
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let n = r.get(name)?.as_deref()?;
        if !seen.insert(n) {
            return None; // a name twice: rows of data, not a list
        }
        if let Some(v) = r.get(value).and_then(|c| c.as_deref()).and_then(number) {
            out.push((n.to_string(), v));
        }
    }
    Some(out)
}

fn csv_rows(p: &[u8], d: u8, cut: bool, out: &mut Vec<Series>) {
    let text = String::from_utf8_lossy(p);
    let mut rows: Vec<Vec<String>> = text
        .replace("\r\n", "\n")
        .split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| pretty::split_row(l, d))
        .collect();
    if cut && rows.len() > 1 {
        rows.pop(); // likely incomplete
    }
    if rows.is_empty() {
        return;
    }
    // A header, unless the first row is numbers too.
    let header = if rows[0].iter().all(|c| number(c).is_some()) {
        (1..=rows[0].len()).map(|i| format!("column {i}")).collect()
    } else {
        rows.remove(0)
    };
    let rows: Vec<Vec<Option<String>>> = rows.into_iter().map(|r| r.into_iter().map(Some).collect()).collect();
    out.extend(table("", &header, &rows, ""));
}

/// Deep enough for any document worth a chart.
const MAX_DEPTH: usize = 64;

fn json_rows(p: &[u8], out: &mut Vec<Series>) {
    if let Ok(j) = Json::parse(p, MAX_DEPTH) {
        arrays(&j, "", out);
    }
}

/// Number arrays (a series each) and arrays of records (a series per number
/// field), anywhere in a document.
fn arrays(j: &Json, path: &str, out: &mut Vec<Series>) {
    match j {
        Json::Obj(fields) => {
            for (k, v) in fields {
                arrays(v, &join(path, k), out);
            }
        }
        Json::Arr(items) if items.len() >= 2 => {
            let n = items.len().to_string();
            if items.iter().all(|i| i.num().is_some() || matches!(i, Json::Null)) {
                let points: Vec<(f64, f64)> = number_points(items.iter().map(Json::num));
                if points.len() >= 2 {
                    out.push(Series {
                        name: if path.is_empty() { "values".into() } else { path.into() },
                        points,
                        ends: ("1".into(), n),
                        source: format!("{} items", items.len()),
                        over_time: false,
                    });
                }
            } else if items.iter().all(|i| matches!(i, Json::Obj(_))) {
                records(items, path, out);
            }
        }
        _ => {}
    }
}

/// Points for a list of maybe-numbers: (index, value), skipping the gaps.
fn number_points(values: impl Iterator<Item = Option<f64>>) -> Vec<(f64, f64)> {
    values
        .enumerate()
        .filter_map(|(i, v)| v.filter(|v| v.is_finite()).map(|v| (i as f64, v)))
        .collect()
}

/// An array of objects as a table: a series per field that's a number (or
/// missing, or null) in every record, at least twice.
fn records(items: &[Json], path: &str, out: &mut Vec<Series>) {
    let mut names: Vec<String> = Vec::new();
    for i in items {
        if let Json::Obj(f) = i {
            for (k, _) in f {
                if !names.contains(k) {
                    names.push(k.clone());
                }
            }
        }
    }
    // Rows go by the first field when it's text (a time, a name).
    let first = names.first().cloned().unwrap_or_default();
    let label = |i: &Json| i.get(&first).and_then(Json::str).map(str::to_string);
    let ends = match (items.first().and_then(label), items.last().and_then(label)) {
        (Some(a), Some(b)) => without_common_day(&a, &b),
        _ => ("1".into(), items.len().to_string()),
    };
    for name in names {
        let values: Option<Vec<Option<f64>>> = items
            .iter()
            .map(|i| match i.get(&name) {
                None | Some(Json::Null) => Some(None),
                Some(v) => v.num().map(Some),
            })
            .collect();
        let Some(values) = values else { continue };
        let points = number_points(values.into_iter());
        if points.len() >= 2 {
            out.push(Series {
                name: join(path, &name),
                points,
                ends: ends.clone(),
                source: format!("{} records", items.len()),
                over_time: false,
            });
        }
    }
}

fn join(path: &str, k: &str) -> String {
    if path.is_empty() {
        k.to_string()
    } else {
        format!("{path}.{k}")
    }
}

/// The numbers a message carries once each, by name: a number payload
/// (`value`), a JSON document's number fields outside arrays, the values a
/// table lists by name (Sparkplug metrics).
pub fn fields(m: &Message) -> Vec<(String, f64)> {
    let mut out = Vec::new();
    let Some(v) = m.decoded.as_deref() else {
        text_fields(&m.payload, m.size, content_type(m), &mut out);
        return out;
    };
    for s in &v.sections {
        match &s.body {
            Body::Table { header, rows } => {
                if let Some(nv) = named_values(header, rows) {
                    out.extend(nv);
                }
            }
            Body::Json(t) => json_fields(t.as_bytes(), &mut out),
            Body::Payload {
                bytes,
                size,
                content_type,
            } => text_fields(bytes, *size, content_type, &mut out),
            _ => {}
        }
    }
    out
}

fn text_fields(p: &[u8], size: usize, ct: &str, out: &mut Vec<(String, f64)>) {
    if p.len() < size {
        return; // cut off: its last number may be too
    }
    match pretty::detect_payload(p, size, ct) {
        Format::Json => json_fields(p, out),
        Format::Text => {
            if let Some(v) = std::str::from_utf8(p).ok().and_then(number) {
                out.push(("value".into(), v));
            }
        }
        _ => {}
    }
}

fn json_fields(p: &[u8], out: &mut Vec<(String, f64)>) {
    fn walk(j: &Json, path: &str, out: &mut Vec<(String, f64)>) {
        match j {
            Json::Num(v) if v.is_finite() => out.push((if path.is_empty() { "value" } else { path }.to_string(), *v)),
            Json::Obj(fields) => {
                for (k, v) in fields {
                    walk(v, &join(path, k), out);
                }
            }
            _ => {}
        }
    }
    if let Ok(j) = Json::parse(p, MAX_DEPTH) {
        walk(&j, "", out);
    }
}

/// A series per number the messages carry, over the messages kept: named as
/// in the newest message first, then those only older ones have.
fn over_time(msgs: &[Arc<Message>], cache: &mut HashMap<u64, Arc<Vec<(String, f64)>>>) -> Vec<Series> {
    let mut names: Vec<String> = Vec::new();
    let mut by: HashMap<String, Vec<(SystemTime, f64)>> = HashMap::new();
    for m in msgs.iter().rev() {
        let f = cache.entry(m.seq).or_insert_with(|| Arc::new(fields(m))).clone();
        for (k, v) in f.iter() {
            by.entry(k.clone()).or_default().push((m.time, *v));
        }
    }
    if let Some(newest) = msgs.first().and_then(|m| cache.get(&m.seq)) {
        names.extend(newest.iter().map(|(k, _)| k.clone()));
    }
    for m in msgs.iter().rev() {
        for (k, _) in cache[&m.seq].iter() {
            if !names.contains(k) {
                names.push(k.clone());
            }
        }
    }
    let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    let mut out = Vec::new();
    for name in names {
        let Some(pts) = by.remove(&name) else { continue };
        if pts.len() < 2 {
            continue;
        }
        let (t0, t1) = (pts[0].0, pts[pts.len() - 1].0);
        let start = secs(t0);
        out.push(Series {
            name,
            ends: (clock(t0, t1), clock(t1, t0)),
            source: format!("{} messages", pts.len()),
            points: pts.iter().map(|&(t, v)| (secs(t) - start, v)).collect(),
            over_time: true,
        });
    }
    out
}

/// `t` to the second, with the day when `other` is on another one.
fn clock(t: SystemTime, other: SystemTime) -> String {
    let day = |t: SystemTime| crate::fmt::clock(t, true)[..10].to_string();
    let s = crate::fmt::clock(t, day(t) != day(other));
    s[..s.len() - 4].to_string() // without the milliseconds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Props, Store};
    use crate::view::{DocView, Section};
    use std::time::Duration;

    fn message(seq: u64, secs: u64, payload: &str, ct: Option<&str>) -> Message {
        Message {
            seq,
            time: UNIX_EPOCH + Duration::from_secs(1_760_000_000 + secs),
            payload: payload.as_bytes().to_vec(),
            size: payload.len(),
            qos: 0,
            retain: false,
            props: Props {
                content_type: ct.map(String::from),
                ..Default::default()
            },
            decoded: None,
        }
    }

    fn msg(seq: u64, secs: u64, payload: &str, ct: Option<&str>) -> Arc<Message> {
        Arc::new(message(seq, secs, payload, ct))
    }

    fn names(s: &[Series]) -> Vec<&str> {
        s.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn fields_of_a_message() {
        let f = |p: &str| fields(&msg(1, 0, p, None));
        assert_eq!(f("19.5"), [("value".to_string(), 19.5)]);
        assert_eq!(f(" -3 \n"), [("value".to_string(), -3.0)]);
        assert_eq!(
            f(r#"{"t":20.5,"ok":true,"tags":["a"],"pos":{"x":1,"y":-2e3},"n":null,"s":"7"}"#),
            [("t".into(), 20.5), ("pos.x".into(), 1.0), ("pos.y".into(), -2000.0)]
        );
        assert!(f("RUNNING").is_empty() && f("NaN").is_empty() && f("inf").is_empty() && f("").is_empty());
        // A number cut off on arrival may be wrong: not used.
        let mut cut = message(1, 0, "123", None);
        cut.size = 4;
        assert!(fields(&cut).is_empty());
    }

    #[test]
    fn json_arrays_and_records_are_rows() {
        let r = |p: &str| rows(&msg(1, 0, p, None));
        let s = r(r#"{"samples":[1,2.5,null,4],"meta":{"gain":[3,4]},"one":[5],"mixed":[1,"a"]}"#);
        assert_eq!(names(&s), ["samples", "meta.gain"]);
        assert_eq!(s[0].points, [(0.0, 1.0), (1.0, 2.5), (3.0, 4.0)]);
        assert_eq!(
            (s[0].ends.clone(), s[0].source.as_str()),
            (("1".into(), "4".into()), "4 items")
        );
        let s = r(r#"[{"ts":"08:00","v":1,"ok":true},{"ts":"08:01","v":2},{"ts":"08:02","v":null,"w":3}]"#);
        // `ok` isn't a number; `w` is there once.
        assert_eq!(names(&s), ["v"]);
        assert_eq!(s[0].points, [(0.0, 1.0), (1.0, 2.0)]);
        assert_eq!(s[0].ends, ("08:00".into(), "08:02".into()));
        assert_eq!(names(&r("[1,2,3]")), ["values"]);
    }

    #[test]
    fn csv_columns_are_rows() {
        let s = rows(&msg(
            1,
            0,
            "turbine;energy_kwh;availability;status\nT01;28450.5;99.2;RUNNING\nT02;27110;;RUNNING\nT03;0;0;IDLE\n",
            None,
        ));
        assert_eq!(names(&s), ["energy_kwh", "availability"]);
        assert_eq!(s[1].points, [(0.0, 99.2), (2.0, 0.0)], "a blank cell is a gap");
        assert_eq!(s[0].ends, ("T01".into(), "T03".into()));
        assert_eq!(s[0].source, "3 rows");
        // No header: numbered columns, rows by number.
        let s = rows(&msg(1, 0, "1,2\n3,4\n5,6\n", Some("text/csv")));
        assert_eq!(names(&s), ["column 1", "column 2"]);
        assert_eq!(s[1].ends, ("1".into(), "3".into()));
    }

    #[test]
    fn decoded_tables_by_column() {
        let pq = std::fs::read(format!(
            "{}/tests/fixtures/turbine-snappy.parquet",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let mut m = message(1, 0, "", None);
        m.size = pq.len();
        m.decoded = crate::pretty::decode_on_arrival(Some("application/parquet"), &pq).map(Box::new);
        let s = rows(&m);
        assert!(!s.is_empty(), "number columns");
        assert!(
            s.iter().all(|x| !x.name.contains(" · ")),
            "the rows only, not the schema: {:?}",
            names(&s)
        );
        for x in &s {
            assert!(x.points.len() >= 2 && !x.over_time, "{x:?}");
            assert!(x.source.ends_with("rows"), "{}", x.source);
        }

        // Several tables go by table name; a list of named values isn't rows.
        let t = |title: &str, header: &[&str], rows: &[&[&str]]| Section {
            title: title.into(),
            detail: String::new(),
            body: Body::Table {
                header: header.iter().map(|h| h.to_string()).collect(),
                rows: rows
                    .iter()
                    .map(|r| r.iter().map(|c| (!c.is_empty()).then(|| c.to_string())).collect())
                    .collect(),
            },
        };
        let mut v = DocView::new("SQLite");
        v.sections.push(t(
            "Tables",
            &["table", "rows"],
            &[&["turbines", "2"], &["setpoints", "3"]],
        ));
        v.sections
            .push(t("turbines", &["id", "rated_kw"], &[&["1", "3600"], &["2", "4200"]]));
        v.sections
            .push(t("setpoints", &["id", "kw"], &[&["1", "10"], &["2", ""], &["3", "30"]]));
        v.sections.push(t(
            "Metrics",
            &["name", "type", "value"],
            &[&["a", "Float", "1.5"], &["b", "String", "x"]],
        ));
        m.decoded = Some(Box::new(v));
        let s = rows(&m);
        assert_eq!(
            names(&s),
            [
                "turbines · id",
                "turbines · rated_kw",
                "setpoints · id",
                "setpoints · kw"
            ]
        );
        assert_eq!(s[3].points, [(0.0, 10.0), (2.0, 30.0)]);
        assert_eq!(fields(&m), [("a".to_string(), 1.5)], "named values are fields");
    }

    #[test]
    fn rows_source_says_when_cut() {
        assert_eq!(rows_source(52, "· first 52 of 500"), "first 52 of 500 rows");
        assert_eq!(rows_source(60, "· first 60 of 60"), "60 rows");
        assert_eq!(rows_source(10, "· first 10 of 1,234 rows"), "first 10 of 1,234 rows");
        assert_eq!(rows_source(1, ""), "1 row");
        assert_eq!(rows_source(10, "· first 10 of 1,234+ rows"), "first 10 of 1,234 rows");
        assert_eq!(rows_source(3, "· 3 rows"), "3 rows");
    }

    #[test]
    fn axis_ends_without_the_common_day() {
        let w = |a: &str, b: &str| without_common_day(a, b);
        let pair = |a: &str, b: &str| (a.to_string(), b.to_string());
        assert_eq!(
            w("2026-10-08 10:20:00Z", "2026-10-08 10:20:51Z"),
            pair("10:20:00Z", "10:20:51Z")
        );
        assert_eq!(
            w("2026-10-08T10:20:00", "2026-10-08T11:00:00"),
            pair("10:20:00", "11:00:00")
        );
        assert_eq!(
            w("2026-10-08 23:59", "2026-10-09 00:01"),
            pair("2026-10-08 23:59", "2026-10-09 00:01")
        );
        assert_eq!(w("T01", "T03"), pair("T01", "T03"));
        assert_eq!(w("North pump", "North fan"), pair("North pump", "North fan"));
        assert_eq!(w("same", "same"), pair("same", "same"));
    }

    #[test]
    fn fields_over_time() {
        // Newest first, as the store keeps them.
        let msgs = vec![
            msg(3, 20, r#"{"t":22,"rpm":9}"#, None),
            msg(2, 10, r#"{"t":21}"#, None),
            msg(1, 0, r#"{"t":20,"old":1}"#, None),
        ];
        let mut st = State::default();
        st.update((7, 3, None, 3), &msgs, 0);
        assert_eq!(names(&st.series), ["t"], "rpm and old are there once");
        let t = &st.series[0];
        assert!(t.over_time);
        assert_eq!(t.points, [(0.0, 20.0), (10.0, 21.0), (20.0, 22.0)]);
        assert_eq!(t.source, "3 messages");
        assert_eq!(t.ends.0.len(), "08:00:00".len(), "{:?}", t.ends);
        assert!(st.is_for((7, 3, None, 3)) && !st.is_for((7, 4, None, 4)));

        // A topic with a table and fields: its rows first.
        let msgs = vec![
            msg(5, 5, r#"{"v":[1,2,3],"t":2}"#, None),
            msg(4, 0, r#"{"v":[1],"t":1}"#, None),
        ];
        st.update((8, 5, None, 2), &msgs, 0);
        assert_eq!(names(&st.series), ["v", "t"]);
    }

    #[test]
    fn the_pick_follows_the_name() {
        let a = vec![msg(2, 1, r#"{"x":1,"y":2}"#, None), msg(1, 0, r#"{"x":3,"y":4}"#, None)];
        let b = vec![msg(4, 1, r#"{"y":5,"z":6}"#, None), msg(3, 0, r#"{"y":7,"z":8}"#, None)];
        let mut st = State::default();
        st.update((1, 2, None, 2), &a, 0);
        assert_eq!(st.current().unwrap().name, "x");
        st.step(1);
        assert_eq!(st.current().unwrap().name, "y");
        st.step(5);
        assert_eq!(st.sel, 1, "stops at the last");
        // Another topic with a `y`: shown; one without: the first.
        st.update((2, 4, None, 2), &b, 0);
        assert_eq!(st.current().unwrap().name, "y");
        st.update((3, 9, None, 2), &[msg(9, 1, "1", None), msg(8, 0, "2", None)], 0);
        assert_eq!(st.current().unwrap().name, "value");
        st.clear();
        assert!(st.current().is_none());
    }

    #[test]
    fn a_stored_topic_charts() {
        let mut store = Store::new(10, 64 * 1024);
        let c = store.add_conn("c", "mqtt://x", None, vec![]);
        for i in 0..4 {
            store.add(
                c,
                "plant/temp",
                format!("{}", 20 + i).as_bytes(),
                0,
                false,
                Props::default(),
            );
        }
        let n = store.find(c, "plant/temp").unwrap();
        let msgs = store.nodes[n].messages();
        let mut st = State::default();
        st.update((n, msgs[0].seq, None, 4), &msgs, 0);
        assert_eq!(names(&st.series), ["value"]);
        let ys: Vec<f64> = st.series[0].points.iter().map(|p| p.1).collect();
        assert_eq!(ys, [20.0, 21.0, 22.0, 23.0]);
    }
}
