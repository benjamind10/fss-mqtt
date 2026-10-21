//! Numbers to chart for the topic in view. A message's own rows: the columns
//! of a table payload (Parquet, Arrow, Avro, SQLite, CSV, …) and the number
//! arrays and record lists in JSON. And, message by message, the numbers each
//! message carries once: a number payload, a JSON document's number fields,
//! Sparkplug metrics.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::fmt::commas;
use crate::json::Json;
use crate::pretty::{self, Format};
use crate::store::Message;
use crate::view::{Body, DocView};

/// A column, array or field worth a chart.
#[derive(Clone, Debug, PartialEq)]
pub struct Series {
    /// `ActivePower`, `power_kw`, `readings.v`, `turbines · rated_kw`.
    pub name: String,
    /// x counts rows (or items) from 0, or (over time) seconds from the
    /// first message.
    pub points: Vec<(f64, f64)>,
    pub axis: Axis,
    /// How many rows, items, records or messages the points come from…
    pub count: usize,
    /// …of what: "rows", "items", "records", "messages".
    pub unit: &'static str,
    /// How many rows there are, when more than `count`: a table cut to its
    /// first rows on arrival.
    pub total: Option<usize>,
    /// The size of a payload cut on arrival: a `--max-payload` of that would
    /// keep all of its rows.
    pub whole: Option<usize>,
    /// One value per message, rather than the rows of one.
    pub over_time: bool,
}

impl Series {
    /// Where the points come from: "60 rows", "first 52 of 500 rows", "10 messages".
    pub fn source(&self) -> String {
        match self.total {
            Some(t) if t > self.count => format!(
                "first {} of {} {}",
                commas(self.count as u64),
                commas(t as u64),
                self.unit
            ),
            _ => format!("{} {}", commas(self.count as u64), self.unit),
        }
    }
}

/// What the x axis goes by, to label any stretch of it.
#[derive(Clone, Debug, PartialEq)]
pub enum Axis {
    /// Rows (or items) by number, from 1.
    Rows,
    /// Rows by their first column: a time, a name.
    Named(Arc<Vec<String>>),
    /// Seconds from the first message, which arrived then.
    Time(SystemTime),
}

impl Axis {
    /// The labels for the two ends of the stretch from `x0` to `x1`.
    pub fn ends(&self, x0: f64, x1: f64) -> (String, String) {
        match self {
            Axis::Rows => (commas(x0 as u64 + 1), commas(x1 as u64 + 1)),
            Axis::Named(names) => {
                let at = |x: f64| names.get(x as usize).map_or("", String::as_str);
                without_common_day(at(x0), at(x1))
            }
            Axis::Time(t0) => {
                let at = |x: f64| *t0 + Duration::from_secs_f64(x.max(0.0));
                (clock(at(x0), at(x1)), clock(at(x1), at(x0)))
            }
        }
    }
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
    /// The picked message's own rows, and its seq: a big table is decoded
    /// once, not again for every message that arrives meanwhile.
    rows_of: Option<(u64, Vec<Series>)>,
    /// All the rows of that message's table, being read (by seq).
    reading: Option<(u64, Reading)>,
    /// The numbers its messages carry, one series each.
    over: Vec<Series>,
    /// Where the stretch in view starts (a point), once scrolled; at first,
    /// at a message's first rows, and at the newest messages.
    from: Option<usize>,
    /// How many points the chart had room for, the last time it was drawn.
    pub room: Cell<usize>,
}

/// (topic node, newest message's seq, picked message's seq, messages).
pub type Key = (usize, u64, Option<u64>, u64);

/// Where a thread puts the rows it read.
type Reading = Arc<Mutex<Option<Vec<Series>>>>;

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
            self.rows_of = None;
        }
        // Another topic or message: back to the start of what's shown.
        if self.key.map(|k| (k.0, k.2)) != Some((key.0, key.2)) {
            self.from = None;
        }
        self.key = Some(key);
        let kept: HashSet<u64> = msgs.iter().map(|m| m.seq).collect();
        self.fields.retain(|seq, _| kept.contains(seq));
        match msgs.get(focused) {
            Some(m) if self.rows_of.as_ref().is_some_and(|(seq, _)| *seq == m.seq) => {}
            Some(m) => {
                // What arrival kept, now; all of a big table, when read.
                self.rows_of = Some((m.seq, rows(m)));
                self.reading = has_more(m).then(|| {
                    let slot = Arc::new(Mutex::new(None));
                    let (whole, done) = (m.clone(), slot.clone());
                    std::thread::spawn(move || {
                        *done.lock().unwrap() = Some(rows_whole(&whole).unwrap_or_default());
                    });
                    (m.seq, slot)
                });
            }
            None => {
                self.rows_of = None;
                self.reading = None;
            }
        }
        self.over = over_time(msgs, &mut self.fields);
        self.combine();
    }

    /// Takes the rows of a table read in full, once they're there; true
    /// when that changed the series.
    pub fn poll(&mut self) -> bool {
        let Some((seq, slot)) = &self.reading else { return false };
        let Some(all) = slot.lock().unwrap().take() else {
            return false;
        };
        let seq = *seq;
        self.reading = None;
        if self.rows_of.as_ref().is_none_or(|(s, _)| *s != seq) || all.is_empty() {
            return false;
        }
        self.rows_of = Some((seq, all));
        self.combine();
        true
    }

    /// Whether all of a table's rows are still being read.
    pub fn reading(&self) -> bool {
        self.reading.is_some()
    }

    /// The series: the message's own rows, then the numbers its messages
    /// carry. The one picked stays picked, else the one shown before.
    fn combine(&mut self) {
        let shown = self.series.get(self.sel).map(|s| s.name.clone());
        let rows = self.rows_of.as_ref().map(|(_, r)| r.as_slice()).unwrap_or_default();
        self.series = rows.iter().chain(&self.over).cloned().collect();
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
        self.rows_of = None;
        self.reading = None;
        self.over.clear();
        self.from = None;
    }

    /// The current series' points in view, and where they start: all of
    /// them when they fit the room the chart had, else a stretch.
    pub fn in_view(&self) -> Option<(usize, &[(f64, f64)])> {
        let s = self.current()?;
        let room = self.room.get().max(2);
        let n = s.points.len();
        if n <= room {
            return Some((0, &s.points));
        }
        let last = n - room;
        let start = match self.from {
            Some(f) => f.min(last),
            None if s.over_time => last,
            None => 0,
        };
        Some((start, &s.points[start..start + room]))
    }

    /// Scrolls the stretch in view by `delta` points (back when negative).
    pub fn scroll(&mut self, delta: isize) {
        let Some((start, shown)) = self.in_view() else { return };
        let (len, over_time) = (shown.len(), self.current().is_some_and(|s| s.over_time));
        let last = self.current().map_or(0, |s| s.points.len()) - len;
        let to = start.saturating_add_signed(delta).min(last);
        // Back at the newest messages: following them again.
        self.from = if over_time && to == last { None } else { Some(to) };
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
    let Some(arrived) = m.decoded.as_deref() else {
        let mut out = Vec::new();
        text_rows(&m.payload, m.size, content_type(m), &mut out);
        return out;
    };
    let whole = (cut(arrived) && m.truncated()).then_some(m.size);
    view_rows(arrived, whole)
}

/// Whether a table in `v` was cut to its first rows on arrival.
fn cut(v: &DocView) -> bool {
    v.sections.iter().any(|s| match &s.body {
        Body::Table { rows, .. } => total_rows(&s.detail).is_some_and(|t| t > rows.len()),
        _ => false,
    })
}

/// Whether `rows_whole` has more to give than `rows`: a table cut on
/// arrival, from a payload that was kept whole (a raised --max-payload).
pub fn has_more(m: &Message) -> bool {
    m.decoded.as_deref().is_some_and(cut) && !m.truncated()
}

/// A message's own rows with its tables read again, all of them: slow for
/// a big one, so `State` does it on a thread of its own.
pub fn rows_whole(m: &Message) -> Option<Vec<Series>> {
    let arrived = m.decoded.as_deref()?;
    let v = pretty::decode_rows(arrived.format, m.props.content_type.as_deref(), &m.payload)?;
    Some(view_rows(&v, None))
}

/// The rows of a decoded payload's tables, JSON and unpacked payloads.
fn view_rows(v: &DocView, whole: Option<usize>) -> Vec<Series> {
    let mut out = Vec::new();
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
                out.extend(table(prefix, header, rows, &s.detail, whole));
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
        Format::Csv(d) => csv_rows(p, d, (p.len() < size).then_some(size), out),
        _ => {}
    }
}

/// A number, as a table shows one; not the empty string, NaN or infinity.
fn number(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// A series per column whose cells are all numbers (nulls and blanks
/// aside), at least two of them. `detail` is the section's ("· first 52 of
/// 500"); `whole`, the size of a payload that was cut on arrival.
fn table(
    prefix: &str,
    header: &[String],
    rows: &[Vec<Option<String>>],
    detail: &str,
    whole: Option<usize>,
) -> Vec<Series> {
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
    let axis = match (column(0), rows.first().and_then(|r| cell(r, 0))) {
        (None, Some(_)) => Axis::Named(Arc::new(
            rows.iter().map(|r| cell(r, 0).unwrap_or("").to_string()).collect(),
        )),
        _ => Axis::Rows,
    };
    let total = total_rows(detail);
    let cut = total.is_some_and(|t| t > rows.len());
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
                axis: axis.clone(),
                count: rows.len(),
                unit: "rows",
                total,
                whole: whole.filter(|_| cut || detail.is_empty()),
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
    let mut common = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    // Not into a character the two only start alike (Å, Ø).
    while !a.is_char_boundary(common) {
        common -= 1;
    }
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

/// How many rows a table has in all, from a section's detail ("· first
/// 52 of 1,500").
fn total_rows(detail: &str) -> Option<usize> {
    let (_, rest) = detail.split_once(" of ")?;
    let digits: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',')
        .filter(|c| *c != ',')
        .collect();
    digits.parse().ok()
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

/// `whole`: the size of a payload that was cut on arrival.
fn csv_rows(p: &[u8], d: u8, whole: Option<usize>, out: &mut Vec<Series>) {
    let text = String::from_utf8_lossy(p);
    let mut rows: Vec<Vec<String>> = text
        .replace("\r\n", "\n")
        .split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| pretty::split_row(l, d))
        .collect();
    if whole.is_some() && rows.len() > 1 {
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
    out.extend(table("", &header, &rows, "", whole));
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
            if items.iter().all(|i| i.num().is_some() || matches!(i, Json::Null)) {
                let points: Vec<(f64, f64)> = number_points(items.iter().map(Json::num));
                if points.len() >= 2 {
                    out.push(Series {
                        name: if path.is_empty() { "values".into() } else { path.into() },
                        points,
                        axis: Axis::Rows,
                        count: items.len(),
                        unit: "items",
                        total: None,
                        whole: None,
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
    let axis = if items.first().and_then(label).is_some() {
        Axis::Named(Arc::new(items.iter().map(|i| label(i).unwrap_or_default()).collect()))
    } else {
        Axis::Rows
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
                axis: axis.clone(),
                count: items.len(),
                unit: "records",
                total: None,
                whole: None,
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
    // Each message is read once; the names are borrowed from what was
    // read, not copied for every point (with a long --history, many).
    for m in msgs {
        cache.entry(m.seq).or_insert_with(|| Arc::new(fields(m)));
    }
    let cache = &*cache;
    let mut names: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    if let Some(newest) = msgs.first() {
        names.extend(
            cache[&newest.seq]
                .iter()
                .map(|(k, _)| k.as_str())
                .filter(|k| seen.insert(k)),
        );
    }
    let mut by: HashMap<&str, Vec<(SystemTime, f64)>> = HashMap::new();
    for m in msgs.iter().rev() {
        for (k, v) in cache[&m.seq].iter() {
            if seen.insert(k) {
                names.push(k);
            }
            by.entry(k).or_default().push((m.time, *v));
        }
    }
    let secs = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64();
    let mut out = Vec::new();
    for name in names {
        let Some(mut pts) = by.remove(name) else { continue };
        if pts.len() < 2 {
            continue;
        }
        // In time order, should the clock have been set back meanwhile.
        pts.sort_by_key(|p| p.0);
        let t0 = pts[0].0;
        let start = secs(t0);
        out.push(Series {
            name: name.to_string(),
            axis: Axis::Time(t0),
            count: pts.len(),
            unit: "messages",
            total: None,
            whole: None,
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

    /// The x axis labels of a whole series.
    fn ends(s: &Series) -> (String, String) {
        s.axis.ends(s.points[0].0, s.points[s.points.len() - 1].0)
    }

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
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
        assert_eq!((ends(&s[0]), s[0].source()), (pair("1", "4"), "4 items".into()));
        let s = r(r#"[{"ts":"08:00","v":1,"ok":true},{"ts":"08:01","v":2},{"ts":"08:02","v":null,"w":3}]"#);
        // `ok` isn't a number; `w` is there once.
        assert_eq!(names(&s), ["v"]);
        assert_eq!(s[0].points, [(0.0, 1.0), (1.0, 2.0)]);
        // The axis spans the points: `v` is null in the last record.
        assert_eq!(ends(&s[0]), pair("08:00", "08:01"));
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
        assert_eq!(ends(&s[0]), pair("T01", "T03"));
        assert_eq!(s[0].source(), "3 rows");
        // No header: numbered columns, rows by number.
        let s = rows(&msg(1, 0, "1,2\n3,4\n5,6\n", Some("text/csv")));
        assert_eq!(names(&s), ["column 1", "column 2"]);
        assert_eq!(ends(&s[1]), pair("1", "3"));
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
            assert!(x.source().ends_with("rows"), "{}", x.source());
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
    fn where_the_points_come_from() {
        assert_eq!(total_rows("· first 52 of 500"), Some(500));
        assert_eq!(total_rows("· first 10 of 1,234+ rows"), Some(1234));
        assert_eq!(total_rows("· 3 rows"), None);
        let s = |count, total| Series {
            name: "x".into(),
            points: vec![],
            axis: Axis::Rows,
            count,
            unit: "rows",
            total,
            whole: None,
            over_time: false,
        };
        assert_eq!(s(52, Some(500)).source(), "first 52 of 500 rows");
        assert_eq!(s(60, Some(60)).source(), "60 rows");
        assert_eq!(s(30_000, None).source(), "30,000 rows");
    }

    #[test]
    fn a_cut_table_is_read_whole_when_its_payload_was_kept() {
        let pq = std::fs::read(format!(
            "{}/tests/fixtures/turbine-snappy.parquet",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let decoded = crate::pretty::decode_on_arrival(Some("application/parquet"), &pq).unwrap();
        let arrived = decoded.sections.iter().find(|s| s.title == "Rows").unwrap();
        assert!(arrived.detail.contains("of 500"), "cut on arrival: {}", arrived.detail);
        let message = |kept: usize| Message {
            payload: pq[..kept].to_vec(),
            size: pq.len(),
            decoded: crate::pretty::decode_on_arrival(Some("application/parquet"), &pq).map(Box::new),
            ..message(1, 0, "", Some("application/parquet"))
        };
        // Kept whole (a raised --max-payload): the first rows at once, and
        // all 500 read again.
        let kept = message(pq.len());
        let s = rows(&kept);
        assert!(s[0].count < 500 && s[0].whole.is_none(), "{}", s[0].source());
        assert!(has_more(&kept));
        let s = rows_whole(&kept).unwrap();
        assert!(
            s.iter().all(|x| x.count == 500 && x.whole.is_none()),
            "{:?}",
            s[0].source()
        );
        assert_eq!(s[0].source(), "500 rows");
        // Cut to the default 4 KiB: the first rows, and how much would keep them all.
        let cut = message(4096);
        assert!(!has_more(&cut));
        let s = rows(&cut);
        assert!(s[0].count < 500 && s[0].total == Some(500), "{}", s[0].source());
        assert_eq!(s[0].whole, Some(pq.len()));

        // The state reads it on a thread of its own, then has all of it.
        let mut st = State::default();
        st.update((1, 1, None, 1), &[Arc::new(kept)], 0);
        assert!(st.reading() && st.current().unwrap().count < 500);
        let start = std::time::Instant::now();
        while !st.poll() {
            assert!(start.elapsed() < Duration::from_secs(10), "never read");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!st.reading() && st.current().unwrap().count == 500);
    }

    #[test]
    fn a_long_series_scrolls() {
        let mut st = State::default();
        let points: Vec<(f64, f64)> = (0..1000).map(|i| (i as f64, (i % 7) as f64)).collect();
        let series = |over_time| Series {
            name: "x".into(),
            points: points.clone(),
            axis: Axis::Rows,
            count: 1000,
            unit: "rows",
            total: None,
            whole: None,
            over_time,
        };
        st.series = vec![series(false)];
        st.room.set(100);
        // A message's rows start at the first; scrolling moves along, no further than the end.
        assert_eq!(st.in_view().map(|(a, v)| (a, v.len())), Some((0, 100)));
        st.scroll(25);
        assert_eq!(st.in_view().unwrap().0, 25);
        st.scroll(-1000);
        assert_eq!(st.in_view().unwrap().0, 0);
        st.scroll(5000);
        assert_eq!(st.in_view().unwrap().0, 900);
        // Room for all: all of it.
        st.room.set(2000);
        assert_eq!(st.in_view().map(|(a, v)| (a, v.len())), Some((0, 1000)));
        // Messages start at the newest and follow them, until scrolled back.
        st.room.set(100);
        st.series = vec![series(true)];
        st.from = None;
        assert_eq!(st.in_view().unwrap().0, 900);
        st.scroll(-50);
        assert_eq!((st.in_view().unwrap().0, st.from), (850, Some(850)));
        st.scroll(50);
        assert_eq!(st.from, None, "at the newest again: following");
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
        // Different letters that start with the same byte.
        assert_eq!(w("Ålborg", "Ørsted"), pair("Ålborg", "Ørsted"));
        assert_eq!(w("Pumpe ä", "Pumpe ö"), pair("Pumpe ä", "Pumpe ö"));
        assert_eq!(
            w("2026-10-08 10:20 Å", "2026-10-08 10:21 Ø"),
            pair("10:20 Å", "10:21 Ø")
        );
    }

    #[test]
    fn a_clock_set_back_keeps_time_in_order() {
        // Newest first by arrival; the second to arrive is stamped latest.
        let msgs = vec![msg(3, 5, "3", None), msg(2, 10, "2", None), msg(1, 0, "1", None)];
        let mut st = State::default();
        st.update((1, 3, None, 3), &msgs, 0);
        assert_eq!(st.series[0].points, [(0.0, 1.0), (5.0, 3.0), (10.0, 2.0)]);
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
        assert_eq!(t.source(), "3 messages");
        assert_eq!(ends(t).0.len(), "08:00:00".len(), "{:?}", ends(t));
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
