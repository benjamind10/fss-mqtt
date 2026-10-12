//! SQLite session extension changesets and patchsets: per table, the rows
//! inserted, updated (old → new) and deleted.
//!
//! The format (see the comment at the top of sqlite's `ext/session/sqlite3session.c`)
//! is a run of table headers, each followed by its changes:
//!
//! - header: `T` (changeset) or `P` (patchset), the column count as a varint,
//!   one byte per column that is non-zero for primary key columns (their
//!   position in the key), then the NUL-terminated table name;
//! - change: the operation (INSERT 18, UPDATE 23, DELETE 9), an "indirect"
//!   byte, then one or two records of values. Column names aren't stored.
//!
//! A changeset's delete carries the old row, an insert the new one, and an
//! update the old values (the key and the changed columns) followed by the
//! new ones (the changed columns); the rest are "undefined". A patchset's
//! delete carries only the key, and its update the key followed by the other
//! columns' new values.
//!
//! It runs on the full payload as it arrives and must never panic on
//! malformed input: every read is bounds-checked.

use crate::fmt::{self, commas};
use crate::view::{Body, DocView, Fact};

/// Rows are rendered until the cells reach about this many bytes.
const CELL_BUDGET: usize = 4096;
const MAX_CELL: usize = 64;
/// Tables shown as sections; the rest are only counted.
const MAX_TABLES: usize = 64;
/// SQLite's hard limit on columns per table.
const MAX_COLUMNS: u64 = 32767;
/// Recognised without a content type only below SQLite's default limit.
const SANE_COLUMNS: usize = 2000;

const INSERT: u8 = 18;
const UPDATE: u8 = 23;
const DELETE: u8 = 9;

pub fn sniff(ct: &str, b: &[u8]) -> Option<Result<DocView, String>> {
    if ct.contains("changeset") || ct.contains("patchset") {
        return Some(decode(b).map(|c| c.to_view()));
    }
    // No content type: only a payload that parses cleanly from end to end,
    // so that other binary payloads keep their hex dump.
    if !matches!(b.first(), Some(b'T' | b'P')) {
        return None;
    }
    let c = decode(b).ok()?;
    c.plausible().then(|| Ok(c.to_view()))
}

pub struct Changeset {
    pub tables: Vec<Table>,
    /// Changes flagged indirect (made by a trigger, a foreign key action, or
    /// while the session's indirect flag was set).
    pub indirect: usize,
    /// Values missing where the format requires one.
    pub undefined: usize,
    /// Anything else out of the ordinary; also rules out the sniffing.
    pub odd: Vec<String>,
    /// Changes not rendered because the byte budget ran out.
    pub omitted: usize,
}

pub struct Table {
    pub name: String,
    /// Built from `P` headers.
    pub patchset: bool,
    /// Per column: its position in the primary key, 0 if not part of it.
    pub pk: Vec<u8>,
    pub inserts: usize,
    pub updates: usize,
    pub deletes: usize,
    /// First changes: the operation, then one cell per column. `None` is a
    /// NULL value, an empty cell a column an update left alone.
    pub rows: Vec<Vec<Option<String>>>,
}

impl Table {
    fn changes(&self) -> usize {
        self.inserts + self.updates + self.deletes
    }
}

impl Changeset {
    fn patchset(&self) -> bool {
        self.tables.iter().any(|t| t.patchset)
    }

    /// Good enough to claim a payload that came without a content type.
    fn plausible(&self) -> bool {
        !self.tables.is_empty()
            && self.odd.is_empty()
            && self.undefined == 0
            && self.tables.iter().all(|t| {
                t.changes() > 0
                    && t.pk.len() <= SANE_COLUMNS
                    && t.pk.iter().any(|&k| k != 0)
                    && t.pk.iter().all(|&k| k as usize <= t.pk.len())
                    && printable_name(&t.name)
            })
    }

    pub fn to_view(&self) -> DocView {
        let patchset = self.patchset();
        let mut d = DocView::new(if patchset {
            "SQLite patchset"
        } else {
            "SQLite changeset"
        });
        let sum = |f: fn(&Table) -> usize| self.tables.iter().map(f).sum::<usize>();
        let counts = [
            (self.tables.len(), "table", "tables"),
            (sum(|t| t.inserts), "insert", "inserts"),
            (sum(|t| t.updates), "update", "updates"),
            (sum(|t| t.deletes), "delete", "deletes"),
        ];
        for (i, (n, one, many)) in counts.into_iter().enumerate() {
            if n > 0 || i == 0 {
                d.summary
                    .push(Fact::new(commas(n as u64), if n == 1 { one } else { many }));
            }
        }
        if patchset {
            d.summary.push(Fact::new("", "patchset"));
        }

        for t in self.tables.iter().take(MAX_TABLES) {
            let total = t.changes();
            let shown = t.rows.len();
            let s = |n: usize| if n == 1 { "change" } else { "changes" };
            let detail = if shown == total {
                format!("· {} {}", commas(total as u64), s(total))
            } else if shown > 0 {
                format!("· first {shown} of {} {}", commas(total as u64), s(total))
            } else {
                format!("· {} {}, not shown", commas(total as u64), s(total))
            };
            let body = if shown == 0 {
                Body::Lines(Vec::new())
            } else {
                let mut header = vec!["op".to_string()];
                header.extend(t.pk.iter().enumerate().map(
                    |(i, &k)| {
                        if k != 0 { format!("pk c{i}") } else { format!("c{i}") }
                    },
                ));
                Body::Table {
                    header,
                    rows: t.rows.clone(),
                }
            };
            d.section(t.name.clone(), detail, body);
        }
        if self.tables.len() > MAX_TABLES {
            d.notes
                .push(format!("{} more tables not shown", self.tables.len() - MAX_TABLES));
        }
        if self.omitted > 0 {
            d.notes.push(format!(
                "{} {} not shown",
                commas(self.omitted as u64),
                if self.omitted == 1 { "change" } else { "changes" }
            ));
        }
        if self.indirect > 0 {
            d.notes.push(format!(
                "{} {} flagged indirect",
                commas(self.indirect as u64),
                if self.indirect == 1 { "change" } else { "changes" }
            ));
        }
        if self.undefined > 0 {
            d.notes.push(format!(
                "{} undefined {} where the format expects one (shown blank)",
                commas(self.undefined as u64),
                if self.undefined == 1 { "value" } else { "values" }
            ));
        }
        d.notes.extend(self.odd.iter().cloned());
        if self.tables.is_empty() {
            d.notes.push("no changes".into());
        }
        d
    }
}

fn printable_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && !s.starts_with(' ') && s.chars().all(|c| !c.is_control())
}

// ── Decoding ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum V<'a> {
    Undefined,
    Null,
    Int(i64),
    Real(f64),
    Text(&'a [u8]),
    Blob(&'a [u8]),
}

struct R<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> R<'a> {
    fn byte(&mut self) -> Result<u8, String> {
        let v = *self.b.get(self.p).ok_or("changeset ends early")?;
        self.p += 1;
        Ok(v)
    }

    fn take(&mut self, n: u64) -> Result<&'a [u8], String> {
        let end = usize::try_from(n)
            .ok()
            .and_then(|n| self.p.checked_add(n))
            .filter(|&e| e <= self.b.len())
            .ok_or("changeset ends early")?;
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }

    /// SQLite's varint: big-endian groups of 7 bits, the 9th byte all 8.
    fn varint(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for _ in 0..8 {
            let b = self.byte()?;
            v = v << 7 | (b & 0x7f) as u64;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Ok(v << 8 | self.byte()? as u64)
    }

    fn value(&mut self) -> Result<V<'a>, String> {
        Ok(match self.byte()? {
            0 => V::Undefined,
            1 => V::Int(i64::from_be_bytes(self.take(8)?.try_into().unwrap_or_default())),
            2 => V::Real(f64::from_be_bytes(self.take(8)?.try_into().unwrap_or_default())),
            3 => {
                let n = self.varint()?;
                V::Text(self.take(n)?)
            }
            4 => {
                let n = self.varint()?;
                V::Blob(self.take(n)?)
            }
            5 => V::Null,
            t => return Err(format!("bad value type {t} at byte {}", self.p - 1)),
        })
    }

    fn record(&mut self, n: usize) -> Result<Vec<V<'a>>, String> {
        (0..n).map(|_| self.value()).collect()
    }
}

pub fn decode(b: &[u8]) -> Result<Changeset, String> {
    let mut c = Changeset {
        tables: Vec::new(),
        indirect: 0,
        undefined: 0,
        odd: Vec::new(),
        omitted: 0,
    };
    let mut r = R { b, p: 0 };
    let mut cur: Option<usize> = None;
    let mut used = 0;
    let mut full = false;
    while r.p < b.len() {
        let at = r.p;
        let op = r.byte()?;
        if op == b'T' || op == b'P' {
            let ncol = r.varint()?;
            if ncol == 0 || ncol > MAX_COLUMNS {
                return Err(format!("bad column count {ncol} at byte {at}"));
            }
            let pk = r.take(ncol)?.to_vec();
            let len = r.b[r.p..]
                .iter()
                .position(|&x| x == 0)
                .ok_or("table name not terminated")?;
            let name = String::from_utf8_lossy(r.take(len as u64)?).into_owned();
            r.p += 1; // the NUL
            let patchset = op == b'P';
            // The same table may come back after another one.
            let i = match c
                .tables
                .iter()
                .position(|t| t.name == name && t.pk == pk && t.patchset == patchset)
            {
                Some(i) => i,
                None => {
                    c.tables.push(Table {
                        name,
                        patchset,
                        pk,
                        inserts: 0,
                        updates: 0,
                        deletes: 0,
                        rows: Vec::new(),
                    });
                    c.tables.len() - 1
                }
            };
            cur = Some(i);
            continue;
        }
        let t = &mut c.tables[cur.ok_or("change before any table header")?];
        if !matches!(op, INSERT | UPDATE | DELETE) {
            return Err(format!("bad operation {op} at byte {at}"));
        }
        match r.byte()? {
            0 => {}
            1 => c.indirect += 1,
            x => {
                c.indirect += 1;
                note(&mut c.odd, format!("unusual indirect flag {x}"));
            }
        }
        let ncol = t.pk.len();
        let (old, new) = match op {
            INSERT => (Vec::new(), r.record(ncol)?),
            DELETE if t.patchset => (some_columns(&mut r, &t.pk, true)?, Vec::new()),
            DELETE => (r.record(ncol)?, Vec::new()),
            _ if t.patchset => {
                let old = some_columns(&mut r, &t.pk, true)?;
                (old, some_columns(&mut r, &t.pk, false)?)
            }
            _ => {
                let old = r.record(ncol)?;
                (old, r.record(ncol)?)
            }
        };
        // Check the values against what the operation allows.
        for (i, &k) in t.pk.iter().enumerate() {
            let is_pk = k != 0;
            let o = old.get(i).copied().unwrap_or(V::Undefined);
            let n = new.get(i).copied().unwrap_or(V::Undefined);
            let missing = match op {
                INSERT => matches!(n, V::Undefined),
                DELETE => matches!(o, V::Undefined) && (is_pk || !t.patchset),
                // An update names its row by the old key; a changeset also
                // keeps the old value of every column that changed.
                _ => {
                    (is_pk && matches!(o, V::Undefined))
                        || (!t.patchset && !is_pk && matches!(o, V::Undefined) != matches!(n, V::Undefined))
                }
            };
            if missing {
                c.undefined += 1;
            }
        }
        match op {
            INSERT => t.inserts += 1,
            DELETE => t.deletes += 1,
            _ => t.updates += 1,
        }
        if full {
            c.omitted += 1;
            continue;
        }
        let row = row(op, &t.pk, &old, &new);
        let cost: usize = row.iter().map(|c| c.as_ref().map_or(4, |s| s.len()) + 1).sum();
        if used + cost > CELL_BUDGET && used > 0 {
            full = true;
            c.omitted += 1;
            continue;
        }
        used += cost;
        t.rows.push(row);
    }
    Ok(c)
}

/// Values for only the primary key columns (`keys`) or only the others,
/// spread over all columns with the rest undefined. Patchsets store just the
/// key as the old values of deletes and updates, and just the other columns
/// as the new values of updates.
fn some_columns<'a>(r: &mut R<'a>, pk: &[u8], keys: bool) -> Result<Vec<V<'a>>, String> {
    let mut vals = r.record(pk.iter().filter(|&&k| (k != 0) == keys).count())?.into_iter();
    Ok(pk
        .iter()
        .map(|&k| {
            if (k != 0) == keys {
                vals.next().unwrap_or(V::Undefined)
            } else {
                V::Undefined
            }
        })
        .collect())
}

fn note(notes: &mut Vec<String>, n: String) {
    if !notes.contains(&n) {
        notes.push(n);
    }
}

fn row(op: u8, pk: &[u8], old: &[V], new: &[V]) -> Vec<Option<String>> {
    let mut row = vec![Some(
        match op {
            INSERT => "insert",
            DELETE => "delete",
            _ => "update",
        }
        .to_string(),
    )];
    fn get<'a>(vs: &[V<'a>], i: usize) -> V<'a> {
        vs.get(i).copied().unwrap_or(V::Undefined)
    }
    for (i, &k) in pk.iter().enumerate() {
        let (o, n) = (get(old, i), get(new, i));
        row.push(match op {
            INSERT => cell(n),
            DELETE => cell(o),
            _ if k != 0 => cell(o),
            _ => match (o, n) {
                (_, V::Undefined) => Some(String::new()),
                (V::Undefined, n) => Some(clip(format!("→ {}", text(n, MAX_CELL - 2)))),
                (o, n) => {
                    let half = (MAX_CELL - 3) / 2;
                    Some(clip(format!("{} → {}", text(o, half), text(n, half))))
                }
            },
        });
    }
    row
}

/// A table cell: `None` for NULL, blank for undefined.
fn cell(v: V) -> Option<String> {
    match v {
        V::Null => None,
        V::Undefined => Some(String::new()),
        v => Some(text(v, MAX_CELL)),
    }
}

fn text(v: V, max: usize) -> String {
    let s = match v {
        V::Undefined => String::new(),
        V::Null => "null".into(),
        V::Int(i) => i.to_string(),
        V::Real(x) => real(x),
        V::Text(t) => fmt::one_line(t, max * 4),
        V::Blob(b) => {
            let hex: String = b.iter().take(max / 2).map(|x| format!("{x:02x}")).collect();
            format!("x'{hex}'")
        }
    };
    clip_to(s, max)
}

/// As SQLite prints a REAL: always with a decimal point or exponent.
fn real(x: f64) -> String {
    if x.is_finite() && x.fract() == 0.0 && x.abs() < 1e16 {
        format!("{x:.1}")
    } else if x.is_finite() && x != 0.0 && (x.abs() >= 1e16 || x.abs() < 1e-4) {
        format!("{x:e}")
    } else {
        x.to_string()
    }
}

fn clip(s: String) -> String {
    clip_to(s, MAX_CELL)
}

fn clip_to(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut c: String = s.chars().take(max.saturating_sub(1)).collect();
    c.push('…');
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn cells(t: &Table, row: usize) -> Vec<&str> {
        t.rows[row].iter().map(|c| c.as_deref().unwrap_or("null")).collect()
    }

    fn table<'a>(c: &'a Changeset, name: &str) -> &'a Table {
        c.tables.iter().find(|t| t.name == name).unwrap()
    }

    fn text_lines(v: &DocView) -> Vec<String> {
        v.render(&Theme::new(false))
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    #[test]
    fn changeset_across_two_tables() {
        let c = decode(&fixture("sensors.changeset")).unwrap();
        assert!(c.plausible());
        assert_eq!((c.indirect, c.undefined, c.omitted), (0, 0, 0));
        assert!(c.odd.is_empty(), "{:?}", c.odd);
        let names: Vec<_> = c.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["sensors", "events"]);

        let s = table(&c, "sensors");
        assert_eq!(s.pk, [1, 0, 0, 0, 0]);
        assert_eq!((s.inserts, s.updates, s.deletes), (1, 1, 1));
        // SQLite orders changes by its hash table; find them by operation.
        let row = |op: &str| {
            (0..s.rows.len())
                .find(|&i| s.rows[i][0].as_deref() == Some(op))
                .unwrap()
        };
        assert_eq!(
            cells(s, row("update")),
            ["update", "1", "", "71.5 → 72.0", "", "null → hot"]
        );
        assert_eq!(
            cells(s, row("delete")),
            ["delete", "3", "fan", "-1.0", "x'cafe'", "old"]
        );
        assert_eq!(
            cells(s, row("insert")),
            ["insert", "4", "valve", "0.5", "x'0102030405'", "null"]
        );

        let e = table(&c, "events");
        assert_eq!((e.inserts, e.updates, e.deletes), (1, 1, 0));
        let all: Vec<Vec<&str>> = (0..e.rows.len()).map(|i| cells(e, i)).collect();
        assert!(all.contains(&vec!["insert", "11", "alarm", "-9000000000"]), "{all:?}");
        assert!(all.contains(&vec!["update", "10", "start → started", ""]), "{all:?}");

        let v = c.to_view();
        assert_eq!(v.format, "SQLite changeset");
        let facts: Vec<_> = v.summary.iter().map(|f| format!("{} {}", f.value, f.label)).collect();
        assert_eq!(facts, ["2 tables", "2 inserts", "2 updates", "1 delete"]);
        assert_eq!(v.sections[0].title, "sensors");
        assert_eq!(v.sections[0].detail, "· 3 changes");
        let Body::Table { header, .. } = &v.sections[0].body else {
            panic!("not a table")
        };
        assert_eq!(header, &["op", "pk c0", "c1", "c2", "c3", "c4"]);
        assert!(v.notes.is_empty(), "{:?}", v.notes);
    }

    #[test]
    fn patchset_carries_keys_and_new_values() {
        let c = decode(&fixture("sensors.patchset")).unwrap();
        assert!(c.plausible());
        assert_eq!(c.undefined, 0);
        let s = table(&c, "sensors");
        assert!(s.patchset);
        assert_eq!((s.inserts, s.updates, s.deletes), (1, 1, 1));
        let row = |op: &str| {
            (0..s.rows.len())
                .find(|&i| s.rows[i][0].as_deref() == Some(op))
                .unwrap()
        };
        assert_eq!(cells(s, row("update")), ["update", "1", "", "→ 72.0", "", "→ hot"]);
        assert_eq!(cells(s, row("delete")), ["delete", "3", "", "", "", ""]);
        let e = table(&c, "events");
        let all: Vec<Vec<&str>> = (0..e.rows.len()).map(|i| cells(e, i)).collect();
        assert!(all.contains(&vec!["update", "10", "→ started", ""]), "{all:?}");
        assert!(all.contains(&vec!["insert", "11", "alarm", "-9000000000"]), "{all:?}");
        let v = c.to_view();
        assert_eq!(v.format, "SQLite patchset");
        assert_eq!(v.summary.last().map(|f| f.label.as_str()), Some("patchset"));
    }

    #[test]
    fn composite_primary_key() {
        let c = decode(&fixture("composite-pk.changeset")).unwrap();
        assert!(c.plausible());
        let t = table(&c, "readings");
        assert_eq!(t.pk, [1, 2, 0]);
        assert_eq!((t.inserts, t.updates, t.deletes), (1, 1, 1));
        let all: Vec<Vec<&str>> = (0..t.rows.len()).map(|i| cells(t, i)).collect();
        assert!(all.contains(&vec!["insert", "bergen", "100", "9.75"]), "{all:?}");
        assert!(all.contains(&vec!["update", "oslo", "200", "2.5 → 3.0"]), "{all:?}");
        assert!(all.contains(&vec!["delete", "oslo", "100", "1.5"]), "{all:?}");
        let Body::Table { header, .. } = &c.to_view().sections[0].body else {
            panic!("not a table")
        };
        assert_eq!(header, &["op", "pk c0", "pk c1", "c2"]);
    }

    #[test]
    fn empty_changeset_by_content_type() {
        // SQLite writes nothing at all for a session without changes.
        let v = sniff("application/x-sqlite-changeset", b"").unwrap().unwrap();
        assert!(v.notes.iter().any(|n| n == "no changes"));
        assert!(sniff("", b"").is_none());
        // With the content type, a damaged payload is an error, not a hex dump.
        assert!(matches!(
            sniff("application/x-sqlite-changeset", b"T\x02\x01"),
            Some(Err(_))
        ));
        assert!(sniff("", b"T\x02\x01").is_none());
    }

    #[test]
    fn cuts_long_changesets_to_the_budget() {
        // 400 inserts of a long text value into t(id INTEGER PRIMARY KEY, v TEXT).
        let mut b = b"T\x02\x01\x00t\x00".to_vec();
        for i in 0..400u64 {
            b.extend_from_slice(&[INSERT, 0, 1]);
            b.extend_from_slice(&i.to_be_bytes());
            b.extend_from_slice(&[3, 100]);
            b.extend_from_slice(&[b'x'; 100]);
        }
        let c = decode(&b).unwrap();
        assert!(c.plausible());
        let t = &c.tables[0];
        assert_eq!(t.inserts, 400);
        assert!(t.rows.len() > 20 && t.rows.len() < 400, "{} rows", t.rows.len());
        assert_eq!(c.omitted, 400 - t.rows.len());
        assert!(t.rows.iter().flatten().flatten().all(|s| s.chars().count() <= MAX_CELL));
        let used: usize = t
            .rows
            .iter()
            .flatten()
            .map(|c| c.as_ref().map_or(4, |s| s.len()) + 1)
            .sum();
        assert!(used <= CELL_BUDGET, "{used}");
        let v = c.to_view();
        assert_eq!(v.sections[0].detail, format!("· first {} of 400 changes", t.rows.len()));
        assert!(
            v.notes.iter().any(|n| n.ends_with("changes not shown")),
            "{:?}",
            v.notes
        );
    }

    #[test]
    fn notes_indirect_and_undefined_values() {
        // An indirect insert with an undefined value: readable, but not sniffed.
        let mut b = b"T\x02\x01\x00t\x00".to_vec();
        b.extend_from_slice(&[INSERT, 1, 1, 0, 0, 0, 0, 0, 0, 0, 7, 0]);
        let c = decode(&b).unwrap();
        assert_eq!((c.indirect, c.undefined), (1, 1));
        assert!(!c.plausible());
        assert!(sniff("", &b).is_none());
        let v = sniff("application/x-sqlite-changeset", &b).unwrap().unwrap();
        assert!(v.notes.iter().any(|n| n.contains("indirect")), "{:?}", v.notes);
        assert!(v.notes.iter().any(|n| n.contains("undefined")), "{:?}", v.notes);
    }

    #[test]
    fn varints() {
        let v = |b: &[u8]| R { b, p: 0 }.varint();
        assert_eq!(v(&[0x05]), Ok(5));
        assert_eq!(v(&[0x81, 0x00]), Ok(128));
        assert_eq!(v(&[0xff; 9]), Ok(u64::MAX));
        assert!(v(&[0x81]).is_err());
        assert_eq!(real(72.0), "72.0");
        assert_eq!(real(1e20), "1e20");
    }

    #[test]
    fn sniffs_changesets_without_content_type() {
        for name in ["sensors.changeset", "sensors.patchset", "composite-pk.changeset"] {
            assert!(matches!(sniff("", &fixture(name)), Some(Ok(_))), "{name}");
        }
    }

    #[test]
    fn leaves_other_payloads_alone() {
        let dir = format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR"));
        for e in std::fs::read_dir(&dir).unwrap() {
            let path = e.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if name.ends_with(".changeset") || name.ends_with(".patchset") {
                continue;
            }
            assert!(sniff("", &std::fs::read(&path).unwrap()).is_none(), "{name}");
        }
        // A table header with no changes: too little to claim a payload without a content type.
        assert!(sniff("", b"T\x01\x01t\x00").is_none());
        assert!(sniff("", b"P\x01\x01t\x00").is_none());
        // With one insert it is a (tiny) changeset; with the content type, the empty one is too.
        assert!(sniff("", b"T\x01\x01t\x00\x12\x00\x05").is_some_and(|r| r.is_ok()));
        assert!(sniff("application/x-sqlite-changeset", b"T\x01\x01t\x00").is_some_and(|r| r.is_ok()));
        for text in [
            &b"Temperature: 21.5"[..],
            b"PING",
            b"T",
            b"P\x00",
            b"T\x01\x01t\x00",
            br#"Tx{"a":1}"#,
            b"PK\x03\x04\x14\x00\x00\x00\x08\x00",
        ] {
            assert!(sniff("", text).is_none(), "{text:?}");
        }
        // Random payloads that start like a header.
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut found = 0;
        for i in 0..20_000 {
            let len = 2 + (rnd() % 64) as usize;
            let mut b: Vec<u8> = (0..len).map(|_| rnd() as u8).collect();
            b[0] = if i % 2 == 0 { b'T' } else { b'P' };
            if i % 3 == 0 {
                b[1] = 1 + (rnd() % 4) as u8; // a sane column count
            }
            if sniff("", &b).is_some() {
                found += 1;
            }
        }
        assert_eq!(found, 0);
    }

    #[test]
    fn decoded_on_arrival() {
        let v = crate::pretty::decode_on_arrival(None, &fixture("composite-pk.changeset")).unwrap();
        assert_eq!(v.format, "SQLite changeset");
        let lines = text_lines(&v);
        assert_eq!(lines[0], "1 table · 1 insert · 1 update · 1 delete");
        assert_eq!(lines[2], "readings · 3 changes");
        assert!(lines[3].starts_with("op"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("2.5 → 3.0")), "{lines:?}");
        let v = crate::pretty::decode_on_arrival(Some("application/x-sqlite-patchset"), &fixture("sensors.patchset"));
        assert_eq!(v.unwrap().format, "SQLite patchset");
        let v = crate::pretty::decode_on_arrival(Some("application/x-sqlite-changeset"), b"Tgarbage").unwrap();
        let t = text_lines(&v);
        assert!(t[0].starts_with("Raw bytes · 8 B"), "{t:?}");
        assert!(
            t.last()
                .unwrap()
                .starts_with("note: could not read this SQLite changeset payload"),
            "{t:?}"
        );
    }

    #[test]
    fn never_panics_on_damaged_input() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for name in ["sensors.changeset", "sensors.patchset", "composite-pk.changeset"] {
            let good = fixture(name);
            for _ in 0..2000 {
                let mut f = good.clone();
                for _ in 0..(rnd() % 6 + 1) {
                    let i = (rnd() as usize) % f.len();
                    f[i] = rnd() as u8;
                }
                for ct in ["", "application/x-sqlite-changeset"] {
                    if let Some(Ok(v)) = sniff(ct, &f) {
                        text_lines(&v);
                    }
                    let cut = (rnd() as usize) % f.len();
                    let _ = sniff(ct, &f[..cut]);
                }
            }
        }
    }
}
