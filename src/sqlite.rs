//! A small, read-only SQLite reader: the database header, the schema, row
//! counts and the first rows of each table, rendered as text. Its own reader
//! of the file format (<https://www.sqlite.org/fileformat2.html>), no SQLite.
//!
//! It runs on the full payload as it arrives, before the store keeps only the
//! first few KiB, and must never panic on malformed input: everything is
//! bounds-checked, b-tree walks remember the pages they've been to and stop at
//! a depth and page budget, and errors become notes in the result.

use std::cell::Cell;
use std::collections::HashSet;

use crate::fmt::{self, commas, plural};
use crate::view::{Body, DocView, Fact};

/// Rows are decoded until the rendered cells of all tables reach about this many bytes.
const ROW_BUDGET: usize = 4096;
/// Tables after the first stop being shown once less than this is left.
const MIN_TABLE_BUDGET: usize = 160;
const MAX_ROWS: usize = 200;
const MAX_CELL: usize = 64;
/// Blobs up to this size are shown as hex, longer ones by their size.
const HEX_BLOB: usize = 16;
const MAX_DEPTH: usize = 40;
/// Pages read across the whole decode (row counts walk every leaf).
const MAX_VISITS: usize = 1 << 20;
/// Bytes of one record collected from overflow pages; values past it aren't shown.
const MAX_RECORD: usize = 1 << 20;
const MAX_SCHEMA: usize = 10_000;
const MAGIC: &[u8; 16] = b"SQLite format 3\0";

pub struct SqliteView {
    /// Database size in pages, from the header (or the payload size).
    pub pages: u64,
    pub page_size: usize,
    pub encoding: &'static str,
    pub wal: bool,
    pub user_version: u32,
    pub application_id: u32,
    /// The SQLite library that last wrote the file, e.g. "3.46.1".
    pub writer: Option<String>,
    pub tables: Vec<Table>,
    /// Indexes, views and triggers, in schema order.
    pub others: Vec<Other>,
    pub notes: Vec<String>,
}

pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// `None` for virtual tables, whose rows live elsewhere.
    pub rows: Option<u64>,
    /// The count stopped early (a damaged b-tree).
    pub rows_partial: bool,
    pub without_rowid: bool,
    /// First rows; `None` is a NULL cell.
    pub first: Vec<Vec<Option<String>>>,
}

pub struct Column {
    pub name: String,
    pub ty: String,
}

pub struct Other {
    pub kind: String,
    pub name: String,
    pub table: String,
    pub sql: String,
}

/// Decoder entry point (see `pretty::Decoder`): recognised by its header or content type.
pub fn sniff(ct: &str, b: &[u8]) -> Option<Result<DocView, String>> {
    let by_type = matches!(
        ct.split(';').next().unwrap_or("").trim(),
        "application/vnd.sqlite3" | "application/x-sqlite3"
    );
    if b.starts_with(MAGIC) {
        Some(decode(b).map(|v| v.to_view()))
    } else if by_type {
        Some(Err("no \"SQLite format 3\" header".into()))
    } else {
        None
    }
}

impl SqliteView {
    pub fn to_view(&self) -> DocView {
        let mut d = DocView::new("SQLite");
        let pages = if self.pages == 1 { "page" } else { "pages" };
        d.summary = vec![
            Fact::new(commas(self.pages), pages),
            Fact::new(format!("{} B", self.page_size), "page size"),
            Fact::new(self.encoding, ""),
        ];
        if self.wal {
            d.summary.push(Fact::new("WAL", "mode"));
        }
        if self.user_version != 0 {
            d.summary
                .push(Fact::new("", format!("user_version {}", self.user_version)));
        }
        let (n, word) = (
            self.tables.len(),
            if self.tables.len() == 1 { "table" } else { "tables" },
        );
        d.summary.push(Fact::new(n.to_string(), word));
        if let Some(w) = &self.writer {
            d.summary.push(Fact::new("", format!("SQLite {w}")));
        }
        if self.application_id != 0 {
            let id = self.application_id;
            d.meta.push(("application_id".into(), format!("{id} (0x{id:08x})")));
        }

        if !self.tables.is_empty() {
            let header = ["table", "rows", "columns"].map(String::from).to_vec();
            let rows = self
                .tables
                .iter()
                .map(|t| {
                    let count = t
                        .rows
                        .map(|n| format!("{}{}", commas(n), if t.rows_partial { "+" } else { "" }));
                    let cols = t
                        .columns
                        .iter()
                        .map(|c| {
                            if c.ty.is_empty() {
                                c.name.clone()
                            } else {
                                format!("{} {}", c.name, c.ty)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    vec![Some(t.name.clone()), count, Some(cols)]
                })
                .collect();
            d.section("Tables", "", Body::Table { header, rows });
        }

        if !self.others.is_empty() {
            let kinds: Vec<&str> = [("index", "indexes"), ("view", "views"), ("trigger", "triggers")]
                .into_iter()
                .filter(|(k, _)| self.others.iter().any(|o| o.kind == *k))
                .map(|(_, many)| many)
                .collect();
            let mut title = match kinds.as_slice() {
                [a] => a.to_string(),
                [a, b] => format!("{a} and {b}"),
                [a, b, c] => format!("{a}, {b} and {c}"),
                _ => "other objects".into(),
            };
            title[..1].make_ascii_uppercase();
            let mut lines = Vec::new();
            for o in &self.others {
                match o.kind.as_str() {
                    "index" => lines.push(format!("index {} on {}{}", o.name, o.table, index_columns(&o.sql))),
                    "view" => lines.push(format!("view {}", o.name)),
                    _ => lines.push(format!("{} {} on {}", o.kind, o.name, o.table)),
                }
                if o.kind != "index" && !o.sql.is_empty() {
                    lines.push(format!("  {}", clip(fmt::one_line(o.sql.as_bytes(), 640), 160)));
                }
            }
            d.section(title, "", Body::Lines(lines));
        }

        for t in self.tables.iter().filter(|t| !t.first.is_empty()) {
            let total = t.rows.unwrap_or(0);
            let detail = if t.first.len() as u64 == total && !t.rows_partial {
                format!("· {}", plural(t.first.len(), "row", "rows"))
            } else {
                let more = if t.rows_partial { "+" } else { "" };
                format!("· first {} of {}{more} rows", t.first.len(), commas(total))
            };
            d.section(
                t.name.clone(),
                detail,
                Body::Table {
                    header: t.columns.iter().map(|c| c.name.clone()).collect(),
                    rows: t.first.clone(),
                },
            );
        }
        d.notes = self.notes.clone();
        d
    }
}

pub fn decode(file: &[u8]) -> Result<SqliteView, String> {
    let db = Db::open(file)?;
    let mut view = SqliteView {
        pages: db.pages,
        page_size: db.page_size,
        encoding: match db.encoding {
            2 => "UTF-16le",
            3 => "UTF-16be",
            _ => "UTF-8",
        },
        wal: file[18] == 2 || file[19] == 2,
        user_version: be32(file, 60)?,
        application_id: be32(file, 68)?,
        writer: match be32(file, 96)? {
            0 => None,
            v => Some(format!("{}.{}.{}", v / 1_000_000, v / 1000 % 1000, v % 1000)),
        },
        tables: Vec::new(),
        others: Vec::new(),
        notes: Vec::new(),
    };
    if view.wal {
        view.notes
            .push("WAL mode: recent changes may still be in the -wal file, which isn't part of this payload".into());
    }
    let in_file = (file.len() / db.page_size) as u64;
    if in_file < db.pages {
        view.notes.push(format!(
            "the payload holds only {} of the {} pages",
            commas(in_file),
            commas(db.pages)
        ));
    }

    // The schema: a table b-tree on page 1 with rows (type, name, tbl_name, rootpage, sql).
    let mut schema = Vec::new();
    let res = db.rows(1, |_, rec| {
        let text = |i: usize| match rec.get(i) {
            Some(Value::Text(s)) => s.clone(),
            _ => String::new(),
        };
        let root = match rec.get(3) {
            Some(Value::Int(n)) => u32::try_from(*n).unwrap_or(0),
            _ => 0,
        };
        schema.push((text(0), text(1), text(2), root, text(4)));
        schema.len() < MAX_SCHEMA
    });
    if let Err(e) = res {
        if schema.is_empty() {
            return Err(format!("schema: {e}"));
        }
        view.notes.push(format!("schema: {e}; the rest of it isn't shown"));
    }

    let mut defs = Vec::new();
    for (kind, name, table, root, sql) in schema {
        if name.starts_with("sqlite_") {
            continue; // internal: sqlite_sequence, sqlite_stat1, automatic indexes
        }
        match kind.as_str() {
            "table" => {
                let def = parse_create_table(&sql);
                let mut t = Table {
                    name,
                    columns: def
                        .columns
                        .iter()
                        .map(|c| Column {
                            name: c.name.clone(),
                            ty: c.ty.clone(),
                        })
                        .collect(),
                    rows: None,
                    rows_partial: false,
                    without_rowid: false,
                    first: Vec::new(),
                };
                if def.is_virtual || root == 0 {
                    view.notes
                        .push(format!("{}: a virtual table, its rows are stored elsewhere", t.name));
                } else {
                    match db.count(root) {
                        Ok((n, index, err)) => {
                            t.rows = Some(n);
                            t.without_rowid = index;
                            if let Some(e) = err {
                                t.rows_partial = true;
                                view.notes.push(format!("{}: {e}; counted the rows before it", t.name));
                            }
                        }
                        Err(e) => view.notes.push(format!("{}: {e}", t.name)),
                    }
                }
                defs.push((root, def));
                view.tables.push(t);
            }
            "index" | "view" | "trigger" => view.others.push(Other {
                kind,
                name,
                table,
                sql: sql.split_whitespace().collect::<Vec<_>>().join(" "),
            }),
            _ => {}
        }
    }

    // First rows, table by table, until the budget is used. The first table
    // may take half of it, each later one half of what's left.
    let mut left = ROW_BUDGET;
    let mut not_shown = 0;
    let has_rows = |t: &Table| t.rows.is_some_and(|n| n > 0);
    let with_rows = view.tables.iter().filter(|t| has_rows(t)).count();
    let mut seen = 0;
    for (t, (root, def)) in view.tables.iter_mut().zip(&defs) {
        if !has_rows(t) {
            continue;
        }
        seen += 1;
        if left < MIN_TABLE_BUDGET && seen > 1 {
            not_shown += 1;
            continue;
        }
        let budget = if seen == with_rows {
            left
        } else {
            (left / 2).max(MIN_TABLE_BUDGET).min(left)
        };
        let order = def.storage_order(t.without_rowid);
        let alias = def.rowid_alias().filter(|_| !t.without_rowid);
        let reals: Vec<bool> = def.columns.iter().map(|c| real_affinity(&c.ty)).collect();
        let mut used = 0;
        let res = db.rows(*root, |rowid, rec| {
            let mut row: Vec<Option<String>> = vec![None; def.columns.len()];
            for (i, v) in rec.iter().enumerate() {
                match order.get(i) {
                    // REAL columns store whole numbers as integers.
                    Some(&c) => match v {
                        Value::Int(n) if reals[c] => row[c] = render(&Value::Real(*n as f64)),
                        _ => row[c] = render(v),
                    },
                    None => row.push(render(v)),
                }
            }
            if let (Some(c), Some(id)) = (alias, rowid)
                && row[c].is_none()
            {
                row[c] = Some(id.to_string());
            }
            let size = row.iter().map(|c| c.as_ref().map_or(4, |s| s.len()) + 1).sum::<usize>();
            if used + size > budget && !t.first.is_empty() {
                return false;
            }
            used += size;
            t.first.push(row);
            t.first.len() < MAX_ROWS
        });
        if let Err(e) = res {
            view.notes.push(format!("{}: {e}", t.name));
        }
        left = left.saturating_sub(used);
        if t.without_rowid && !t.first.is_empty() {
            view.notes.push(format!(
                "{} is a WITHOUT ROWID table: its rows are shown in primary key order",
                t.name
            ));
        }
        let extra = t.first.iter().map(|r| r.len()).max().unwrap_or(0);
        for i in t.columns.len()..extra {
            t.columns.push(Column {
                name: format!("column{}", i + 1),
                ty: String::new(),
            });
        }
    }
    if not_shown > 0 {
        let s = if not_shown == 1 { "table" } else { "tables" };
        view.notes.push(format!("rows of {not_shown} more {s} not shown"));
    }
    Ok(view)
}

// ── Values ─────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    /// The size, and the bytes when it's short enough to show.
    Blob(usize, Vec<u8>),
    /// Past the part of a long record that was read.
    Cut,
}

fn render(v: &Value) -> Option<String> {
    Some(match v {
        Value::Null => return None,
        Value::Int(i) => i.to_string(),
        Value::Real(f) => real(*f),
        Value::Text(s) => {
            let head: String = s.chars().take(MAX_CELL + 1).collect();
            clip(fmt::one_line(head.as_bytes(), head.len()), MAX_CELL)
        }
        Value::Blob(n, b) if *n <= HEX_BLOB && b.len() == *n => format!("x'{}'", hex(b)),
        Value::Blob(n, _) => format!("<{} B blob>", commas(*n as u64)),
        Value::Cut => "…".into(),
    })
}

/// The shortest text that reads back as `f`, always with a point or exponent.
fn real(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Inf" } else { "-Inf" }.into();
    }
    if f != 0.0 && !(1e-5..1e16).contains(&f.abs()) {
        return format!("{f:e}");
    }
    let s = f.to_string();
    if s.contains('.') { s } else { format!("{s}.0") }
}

fn clip(s: String, max: usize) -> String {
    if s.chars().count() <= max {
        return s;
    }
    let mut c: String = s.chars().take(max - 1).collect();
    c.push('…');
    c
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ── File and pages ─────────────────────────────────────────────────────────

struct Db<'a> {
    file: &'a [u8],
    page_size: usize,
    /// Page size less the reserved bytes at the end of each page.
    usable: usize,
    pages: u64,
    /// 1 UTF-8, 2 UTF-16le, 3 UTF-16be.
    encoding: u32,
    visits: Cell<usize>,
}

/// A b-tree page.
#[derive(Clone, Copy)]
struct Page<'a> {
    no: u32,
    /// The usable bytes of the page.
    data: &'a [u8],
    /// Where the b-tree header starts: after the file header on page 1.
    hdr: usize,
    kind: u8,
    cells: usize,
    /// Interior pages: the right-most child.
    right: u32,
}

const INDEX_INTERIOR: u8 = 0x02;
const TABLE_INTERIOR: u8 = 0x05;
const INDEX_LEAF: u8 = 0x0a;
const TABLE_LEAF: u8 = 0x0d;

impl Page<'_> {
    fn is_leaf(&self) -> bool {
        self.kind == INDEX_LEAF || self.kind == TABLE_LEAF
    }

    fn is_index(&self) -> bool {
        self.kind == INDEX_LEAF || self.kind == INDEX_INTERIOR
    }

    fn header_len(&self) -> usize {
        if self.is_leaf() { 8 } else { 12 }
    }

    /// Where cell `i` starts within the page.
    fn cell(&self, i: usize) -> Result<usize, String> {
        let ptrs = self.hdr + self.header_len();
        let off = be16(self.data, ptrs + 2 * i)? as usize;
        if off < ptrs + 2 * self.cells || off >= self.data.len() {
            return Err(format!("page {}: cell {i} out of range", self.no));
        }
        Ok(off)
    }

    /// Interior pages: the page left of cell `i`.
    fn child(&self, i: usize) -> Result<u32, String> {
        be32(self.data, self.cell(i)?)
    }
}

impl<'a> Db<'a> {
    fn open(file: &'a [u8]) -> Result<Db<'a>, String> {
        if file.len() < 100 {
            return Err("file header cut short".into());
        }
        let page_size = match be16(file, 16)? {
            1 => 65536,
            n if n >= 512 && n.is_power_of_two() => n as usize,
            n => return Err(format!("bad page size {n}")),
        };
        let usable = page_size - file[20] as usize;
        if usable < 480 {
            return Err(format!("usable page size {usable} is too small"));
        }
        let encoding = match be32(file, 56)? {
            0 | 1 => 1,
            e @ (2 | 3) => e,
            e => return Err(format!("unknown text encoding {e}")),
        };
        // The size in the header is only valid if written by SQLite 3.7.0 or
        // later, which also stamps the change counter into "version-valid-for".
        let size = be32(file, 28)? as u64;
        let pages = if size > 0 && be32(file, 24)? == be32(file, 92)? {
            size
        } else {
            (file.len() / page_size) as u64
        };
        Ok(Db {
            file,
            page_size,
            usable,
            pages,
            encoding,
            visits: Cell::new(0),
        })
    }

    /// The usable bytes of page `n` (pages are numbered from 1).
    fn page(&self, n: u32) -> Result<&'a [u8], String> {
        if n == 0 || n as u64 > self.pages {
            return Err(format!("page {n} is out of range"));
        }
        let visits = self.visits.get() + 1;
        if visits > MAX_VISITS {
            return Err("too many pages to read".into());
        }
        self.visits.set(visits);
        let start = (n as usize - 1).checked_mul(self.page_size);
        start
            .and_then(|s| self.file.get(s..s.checked_add(self.usable)?))
            .ok_or_else(|| format!("page {n} is past the end of the payload"))
    }

    fn btree(&self, n: u32) -> Result<Page<'a>, String> {
        let data = self.page(n)?;
        let hdr = if n == 1 { 100 } else { 0 };
        let kind = data[hdr];
        if !matches!(kind, INDEX_INTERIOR | TABLE_INTERIOR | INDEX_LEAF | TABLE_LEAF) {
            return Err(format!("page {n} isn't a b-tree page"));
        }
        let mut p = Page {
            no: n,
            data,
            hdr,
            kind,
            cells: be16(data, hdr + 3)? as usize,
            right: 0,
        };
        if hdr + p.header_len() + 2 * p.cells > data.len() {
            return Err(format!("page {n}: too many cells"));
        }
        if !p.is_leaf() {
            p.right = be32(data, hdr + 8)?;
        }
        Ok(p)
    }

    /// Rows in the b-tree rooted at `root`; whether it's an index b-tree (a
    /// WITHOUT ROWID table); and the error that stopped the count early.
    fn count(&self, root: u32) -> Result<(u64, bool, Option<String>), String> {
        let mut walk = Walk::new(self, root)?;
        let mut n = 0u64;
        loop {
            match walk.next(self) {
                Ok(Some(Item::Leaf(p))) => n += p.cells as u64,
                Ok(Some(Item::Interior(..))) => n += 1,
                Ok(None) => return Ok((n, walk.index, None)),
                Err(e) => return Ok((n, walk.index, Some(e))),
            }
        }
    }

    /// Calls `f` with the rowid (table b-trees) and record of each row in key
    /// order, until it returns false.
    fn rows(&self, root: u32, mut f: impl FnMut(Option<i64>, Vec<Value>) -> bool) -> Result<(), String> {
        let mut walk = Walk::new(self, root)?;
        while let Some(item) = walk.next(self)? {
            match item {
                Item::Leaf(p) => {
                    for i in 0..p.cells {
                        let (rowid, rec) = self.cell(&p, i)?;
                        if !f(rowid, rec) {
                            return Ok(());
                        }
                    }
                }
                Item::Interior(p, i) => {
                    let (rowid, rec) = self.cell(&p, i)?;
                    if !f(rowid, rec) {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    /// The rowid (table leaves) and decoded record of cell `i`.
    fn cell(&self, p: &Page<'a>, i: usize) -> Result<(Option<i64>, Vec<Value>), String> {
        let mut pos = p.cell(i)?;
        if p.kind == INDEX_INTERIOR {
            pos += 4; // left child
        }
        let (size, next) = varint(p.data, pos)?;
        pos = next;
        let mut rowid = None;
        if p.kind == TABLE_LEAF {
            let (id, next) = varint(p.data, pos)?;
            rowid = Some(id as i64);
            pos = next;
        } else if p.kind == TABLE_INTERIOR {
            return Err(format!("page {}: interior table pages hold no rows", p.no));
        }
        let payload = self.payload(p, pos, size)?;
        Ok((rowid, self.record(&payload)?))
    }

    /// The payload of `size` bytes starting at `pos`, following its overflow
    /// pages; at most `MAX_RECORD` bytes.
    fn payload(&self, p: &Page<'a>, pos: usize, size: u64) -> Result<Vec<u8>, String> {
        let u = self.usable as u64;
        // How much is stored on the page itself (see "Cell Payload Overflow Pages").
        let x = if p.kind == TABLE_LEAF {
            u - 35
        } else {
            (u - 12) * 64 / 255 - 23
        };
        let local = if size <= x {
            size
        } else {
            let m = (u - 12) * 32 / 255 - 23;
            let k = m + (size - m) % (u - 4);
            if k <= x { k } else { m }
        } as usize;
        let want = size.min(MAX_RECORD as u64) as usize;
        let short = || format!("page {}: cell runs past the page", p.no);
        let here = p.data.get(pos..pos + local).ok_or_else(short)?;
        let mut out = here[..local.min(want)].to_vec();
        if (local as u64) < size {
            let mut next = be32(p.data, pos + local).map_err(|_| short())?;
            let mut seen = HashSet::new();
            while out.len() < want {
                if next == 0 {
                    return Err(format!("page {}: overflow chain ends early", p.no));
                }
                if !seen.insert(next) {
                    return Err(format!("page {}: overflow chain loops", p.no));
                }
                let page = self.page(next)?;
                next = be32(page, 0)?;
                let take = (want - out.len()).min(page.len() - 4);
                out.extend_from_slice(&page[4..4 + take]);
            }
        }
        Ok(out)
    }

    /// Decodes a record: a header of serial types, then the values.
    fn record(&self, b: &[u8]) -> Result<Vec<Value>, String> {
        let (len, mut hp) = varint(b, 0)?;
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len < hp || len > b.len() {
            return Err("bad record header".into());
        }
        let mut body = len;
        let mut out = Vec::new();
        while hp < len {
            let (t, next) = varint(b, hp)?;
            hp = next;
            let size = match t {
                0 | 8 | 9 => 0,
                1..=4 => t as usize,
                5 => 6,
                6 | 7 => 8,
                10 | 11 => return Err(format!("reserved serial type {t}")),
                _ => usize::try_from((t - 12) / 2).unwrap_or(usize::MAX),
            };
            let end = body.saturating_add(size);
            let v = b.get(body..end);
            out.push(match (t, v) {
                (0, _) => Value::Null,
                (8, _) => Value::Int(0),
                (9, _) => Value::Int(1),
                (1..=6, Some(v)) => Value::Int(be_int(v)),
                (7, Some(v)) => Value::Real(f64::from_bits(be_int(v) as u64)),
                (_, v) if t >= 12 && t % 2 == 0 => {
                    Value::Blob(size, v.filter(|v| v.len() <= HEX_BLOB).unwrap_or(&[]).to_vec())
                }
                (_, Some(v)) if t >= 13 => Value::Text(self.text(v)),
                // A long text cut at MAX_RECORD: what's there.
                (_, None) if t >= 13 && body < b.len() => Value::Text(self.text(&b[body..])),
                _ => Value::Cut,
            });
            body = end;
        }
        Ok(out)
    }

    fn text(&self, b: &[u8]) -> String {
        match self.encoding {
            2 | 3 => {
                let le = self.encoding == 2;
                let units = b.as_chunks::<2>().0.iter().map(|&c| {
                    if le {
                        u16::from_le_bytes(c)
                    } else {
                        u16::from_be_bytes(c)
                    }
                });
                char::decode_utf16(units).map(|c| c.unwrap_or('\u{FFFD}')).collect()
            }
            _ => String::from_utf8_lossy(b).into_owned(),
        }
    }
}

/// What a b-tree walk yields, in key order.
enum Item<'a> {
    Leaf(Page<'a>),
    /// A cell of an interior index page: an entry of its own, unlike the
    /// keys on interior table pages.
    Interior(Page<'a>, usize),
}

/// A depth-first walk over a b-tree that reads each page once.
struct Walk<'a> {
    /// Interior pages from the root down, with the next step at each: step
    /// 2i goes down to child i, step 2i+1 yields cell i (index b-trees only).
    stack: Vec<(Page<'a>, usize)>,
    /// A root that is a leaf, not yet yielded.
    leaf: Option<Page<'a>>,
    seen: HashSet<u32>,
    index: bool,
}

impl<'a> Walk<'a> {
    fn new(db: &Db<'a>, root: u32) -> Result<Walk<'a>, String> {
        let p = db.btree(root)?;
        let mut w = Walk {
            stack: Vec::new(),
            leaf: None,
            seen: HashSet::from([root]),
            index: p.is_index(),
        };
        if p.is_leaf() {
            w.leaf = Some(p);
        } else {
            w.stack.push((p, 0));
        }
        Ok(w)
    }

    fn next(&mut self, db: &Db<'a>) -> Result<Option<Item<'a>>, String> {
        let r = self.step(db);
        if r.is_err() {
            self.stack.clear();
        }
        r
    }

    fn step(&mut self, db: &Db<'a>) -> Result<Option<Item<'a>>, String> {
        if let Some(p) = self.leaf.take() {
            return Ok(Some(Item::Leaf(p)));
        }
        loop {
            let Some((p, step)) = self.stack.last_mut() else {
                return Ok(None);
            };
            let (p, i, odd) = (*p, *step / 2, *step % 2 == 1);
            *step += 1;
            if odd {
                if self.index && i < p.cells {
                    return Ok(Some(Item::Interior(p, i)));
                }
                continue;
            }
            let child = match i.cmp(&p.cells) {
                std::cmp::Ordering::Less => p.child(i)?,
                std::cmp::Ordering::Equal => p.right,
                std::cmp::Ordering::Greater => {
                    self.stack.pop();
                    continue;
                }
            };
            if !self.seen.insert(child) {
                return Err(format!("page {child} is linked twice"));
            }
            if self.stack.len() >= MAX_DEPTH {
                return Err("b-tree too deep".into());
            }
            let c = db.btree(child)?;
            if c.is_index() != self.index {
                return Err(format!("page {child} belongs to another kind of b-tree"));
            }
            if c.is_leaf() {
                return Ok(Some(Item::Leaf(c)));
            }
            self.stack.push((c, 0));
        }
    }
}

fn be16(b: &[u8], p: usize) -> Result<u16, String> {
    let v = b.get(p..p + 2).ok_or("page ends early")?;
    Ok(u16::from_be_bytes([v[0], v[1]]))
}

fn be32(b: &[u8], p: usize) -> Result<u32, String> {
    let v = b.get(p..p + 4).ok_or("page ends early")?;
    Ok(u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
}

/// A big-endian two's complement integer of 1 to 8 bytes.
fn be_int(b: &[u8]) -> i64 {
    let neg = b.first().is_some_and(|x| x & 0x80 != 0);
    b.iter().fold(if neg { -1 } else { 0 }, |v, &x| (v << 8) | x as i64)
}

/// SQLite's varint: 1 to 9 bytes, big-endian, the 9th byte whole.
fn varint(b: &[u8], mut p: usize) -> Result<(u64, usize), String> {
    let mut v = 0u64;
    for i in 0..9 {
        let x = *b.get(p).ok_or("record ends early")?;
        p += 1;
        if i == 8 {
            return Ok(((v << 8) | x as u64, p));
        }
        v = (v << 7) | (x & 0x7f) as u64;
        if x & 0x80 == 0 {
            break;
        }
    }
    Ok((v, p))
}

// ── CREATE statements ──────────────────────────────────────────────────────

#[derive(Default)]
struct TableDef {
    columns: Vec<ColumnDef>,
    /// Primary key columns, in key order.
    pk: Vec<usize>,
    without_rowid: bool,
    is_virtual: bool,
}

struct ColumnDef {
    name: String,
    ty: String,
    pk_desc: bool,
    /// A VIRTUAL generated column: not stored in the record.
    generated: bool,
}

impl TableDef {
    /// The `INTEGER PRIMARY KEY` column, stored as NULL: its value is the rowid.
    fn rowid_alias(&self) -> Option<usize> {
        match self.pk[..] {
            [c] if !self.without_rowid
                && !self.columns[c].pk_desc
                && self.columns[c].ty.eq_ignore_ascii_case("INTEGER") =>
            {
                Some(c)
            }
            _ => None,
        }
    }

    /// The column shown for each value of a record. WITHOUT ROWID tables
    /// store the primary key columns first.
    fn storage_order(&self, without_rowid: bool) -> Vec<usize> {
        let stored = (0..self.columns.len()).filter(|&c| !self.columns[c].generated);
        if without_rowid {
            let mut order = self.pk.clone();
            order.extend(stored.filter(|c| !self.pk.contains(c)));
            order
        } else {
            stored.collect()
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tok {
    Word,
    /// A quoted identifier: "x", [x] or `x`.
    Quoted,
    /// A string literal: 'x'.
    Str,
    Punct,
}

/// SQL tokens as (kind, start, end) byte ranges; comments are dropped.
fn tokens(s: &str) -> Vec<(Tok, usize, usize)> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let kind = match c {
            _ if c.is_ascii_whitespace() => {
                i += 1;
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                continue;
            }
            b'"' | b'`' | b'\'' | b'[' => {
                let close = if c == b'[' { b']' } else { c };
                i += 1;
                while i < b.len() {
                    if b[i] == close {
                        // A doubled quote is an escaped one.
                        if close != b']' && b.get(i + 1) == Some(&close) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i = (i + 1).min(b.len());
                if c == b'\'' { Tok::Str } else { Tok::Quoted }
            }
            _ if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80 => {
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                    i += 1;
                }
                Tok::Word
            }
            _ => {
                i += 1;
                Tok::Punct
            }
        };
        out.push((kind, start, i));
    }
    out
}

/// An identifier without its quotes.
fn ident(s: &str, kind: Tok) -> String {
    let inner = |q: char| {
        let t = s.strip_prefix(q).unwrap_or(s);
        let close = if q == '[' { ']' } else { q };
        let t = t.strip_suffix(close).unwrap_or(t);
        if q == '[' {
            t.to_string()
        } else {
            t.replace(&format!("{q}{q}"), &q.to_string())
        }
    };
    match (kind, s.chars().next()) {
        (Tok::Quoted | Tok::Str, Some(q)) => inner(q),
        _ => s.to_string(),
    }
}

/// Keywords that end a column's type and start its constraints.
const COLUMN_CONSTRAINTS: &[&str] = &[
    "CONSTRAINT",
    "PRIMARY",
    "NOT",
    "NULL",
    "UNIQUE",
    "CHECK",
    "DEFAULT",
    "COLLATE",
    "REFERENCES",
    "GENERATED",
    "AS",
];
const TABLE_CONSTRAINTS: &[&str] = &["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"];

/// Columns and keys from a CREATE TABLE statement: a tolerant reading of the
/// column list between the outer parentheses.
fn parse_create_table(sql: &str) -> TableDef {
    let toks = tokens(sql);
    let text = |t: &(Tok, usize, usize)| &sql[t.1..t.2];
    let is = |t: &(Tok, usize, usize), kw: &str| t.0 == Tok::Word && text(t).eq_ignore_ascii_case(kw);
    let mut def = TableDef::default();
    let Some(open) = toks.iter().position(|t| t.0 == Tok::Punct && text(t) == "(") else {
        return def;
    };
    def.is_virtual = toks[..open].iter().any(|t| is(t, "VIRTUAL"));

    // Split at top-level commas up to the matching parenthesis.
    let mut parts: Vec<&[(Tok, usize, usize)]> = Vec::new();
    let mut depth = 0;
    let mut start = open + 1;
    let mut end = toks.len();
    for (i, t) in toks.iter().enumerate().skip(open) {
        match (t.0, text(t)) {
            (Tok::Punct, "(") => depth += 1,
            (Tok::Punct, ")") => {
                depth -= 1;
                if depth == 0 {
                    parts.push(&toks[start..i]);
                    end = i;
                    break;
                }
            }
            (Tok::Punct, ",") if depth == 1 => {
                parts.push(&toks[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if end == toks.len() && start < toks.len() {
        parts.push(&toks[start..]); // no closing parenthesis
    }
    let tail = toks.get(end + 1..).unwrap_or(&[]);
    def.without_rowid = tail.windows(2).any(|w| is(&w[0], "WITHOUT") && is(&w[1], "ROWID"));

    let mut table_pk: Vec<String> = Vec::new();
    let mut column_pk = Vec::new();
    for part in parts {
        let Some(first) = part.first() else { continue };
        if TABLE_CONSTRAINTS.iter().any(|k| is(first, k)) {
            // PRIMARY KEY (a, b DESC): the first name of each item.
            if let Some(p) = part.windows(2).position(|w| is(&w[0], "PRIMARY") && is(&w[1], "KEY")) {
                let mut depth = 0;
                let mut take = false;
                for t in &part[p + 2..] {
                    match (t.0, text(t)) {
                        (Tok::Punct, "(") => {
                            depth += 1;
                            take = depth == 1;
                        }
                        (Tok::Punct, ")") => depth -= 1,
                        (Tok::Punct, ",") if depth == 1 => take = true,
                        (Tok::Word | Tok::Quoted | Tok::Str, s) if take && depth == 1 => {
                            table_pk.push(ident(s, t.0));
                            take = false;
                        }
                        _ => {}
                    }
                }
            }
            continue;
        }
        let rest = &part[1..];
        let cut = rest
            .iter()
            .position(|t| COLUMN_CONSTRAINTS.iter().any(|k| is(t, k)))
            .unwrap_or(rest.len());
        let ty = match (rest.first(), cut.checked_sub(1).and_then(|l| rest.get(l))) {
            (Some(a), Some(z)) if cut > 0 => sql[a.1..z.2].split_whitespace().collect::<Vec<_>>().join(" "),
            _ => String::new(),
        };
        let cons = &rest[cut..];
        let pk = cons.windows(2).position(|w| is(&w[0], "PRIMARY") && is(&w[1], "KEY"));
        let generated = cons.iter().any(|t| is(t, "AS")) && !cons.iter().any(|t| is(t, "STORED"));
        if pk.is_some() {
            column_pk.push(def.columns.len());
        }
        def.columns.push(ColumnDef {
            name: ident(text(first), first.0),
            ty,
            pk_desc: pk.is_some_and(|p| cons.get(p + 2).is_some_and(|t| is(t, "DESC"))),
            generated,
        });
    }
    def.pk = if column_pk.is_empty() {
        table_pk
            .iter()
            .filter_map(|n| def.columns.iter().position(|c| c.name.eq_ignore_ascii_case(n)))
            .collect()
    } else {
        column_pk
    };
    def
}

/// Does a column declared as `ty` have REAL affinity (see "Determination Of Column Affinity")?
fn real_affinity(ty: &str) -> bool {
    let t = ty.to_ascii_uppercase();
    let has = |k: &str| t.contains(k);
    !has("INT")
        && !has("CHAR")
        && !has("CLOB")
        && !has("TEXT")
        && !has("BLOB")
        && (has("REAL") || has("FLOA") || has("DOUB"))
}

/// The column list of a CREATE INDEX statement: " (ts)".
fn index_columns(sql: &str) -> String {
    let toks = tokens(sql);
    let Some(open) = toks.iter().position(|t| t.0 == Tok::Punct && &sql[t.1..t.2] == "(") else {
        return String::new();
    };
    let mut depth = 0;
    for t in &toks[open..] {
        match &sql[t.1..t.2] {
            "(" if t.0 == Tok::Punct => depth += 1,
            ")" if t.0 == Tok::Punct => {
                depth -= 1;
                if depth == 0 {
                    return format!(" {}", &sql[toks[open].1..t.2]);
                }
            }
            _ => {}
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    /// Every row of `table` as stored: rowid and record values.
    fn stored_rows(file: &[u8], table: &str) -> Vec<(Option<i64>, Vec<Value>)> {
        let db = Db::open(file).unwrap();
        let mut root = 0;
        db.rows(1, |_, rec| {
            if rec.get(1) == Some(&Value::Text(table.into()))
                && let Some(Value::Int(n)) = rec.get(3)
            {
                root = *n as u32;
            }
            true
        })
        .unwrap();
        let mut out = Vec::new();
        db.rows(root, |id, rec| {
            out.push((id, rec));
            true
        })
        .unwrap();
        out
    }

    fn cells(row: &[Option<String>]) -> Vec<&str> {
        row.iter().map(|c| c.as_deref().unwrap_or("null")).collect()
    }

    fn table<'a>(v: &'a SqliteView, name: &str) -> &'a Table {
        v.tables.iter().find(|t| t.name == name).unwrap()
    }

    #[test]
    fn turbine_database() {
        let v = decode(&fixture("turbine.db")).unwrap();
        assert!(v.notes.is_empty(), "{:?}", v.notes);
        assert_eq!((v.pages, v.page_size, v.encoding, v.wal), (11, 4096, "UTF-8", false));
        assert_eq!((v.user_version, v.application_id), (3, 1179865421));
        assert_eq!(v.writer.as_deref(), Some("3.46.1"));
        let names: Vec<_> = v.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["turbines", "readings", "events", "site config"]);
        let counts: Vec<_> = v.tables.iter().map(|t| t.rows).collect();
        assert_eq!(counts, [Some(8), Some(200), Some(4), Some(2)]);
        let cols = |t: &str| -> Vec<String> {
            table(&v, t)
                .columns
                .iter()
                .map(|c| format!("{} {}", c.name, c.ty))
                .collect()
        };
        assert_eq!(
            cols("turbines"),
            [
                "id INTEGER",
                "name TEXT",
                "model TEXT",
                "rated_kw REAL",
                "commissioned TEXT",
                "photo BLOB"
            ]
        );
        assert_eq!(
            cols("events"),
            ["id INTEGER", "turbine_id INTEGER", "kind TEXT", "at TEXT"]
        );
        assert_eq!(cols("site config"), ["key TEXT", "value TEXT", "updated, at INTEGER"]);

        // The INTEGER PRIMARY KEY shows the rowid; REAL columns read whole numbers back as reals.
        let turbines = &table(&v, "turbines").first;
        assert_eq!(turbines.len(), 8);
        assert_eq!(
            cells(&turbines[0]),
            ["1", "T01", "V117", "3600.0", "2019-01-15", "x'89504e47'"]
        );
        assert_eq!(cells(&turbines[1])[5], "<40 B blob>");
        assert_eq!(turbines[4][3], None);
        assert_eq!(
            cells(&turbines[7]),
            ["8", "T08", "V136", "4200.5", "2019-08-15", "null"]
        );
        let readings = &table(&v, "readings").first;
        assert_eq!(cells(&readings[0]), ["1791454800", "T01", "-12.5", "3.0", "RUNNING"]);
        assert_eq!(readings[7][3], None);
        assert!(readings.len() > 20 && readings.len() < 200, "{} rows", readings.len());
        let events: Vec<_> = table(&v, "events")
            .first
            .iter()
            .map(|r| cells(r)[..3].join(" "))
            .collect();
        assert_eq!(events, ["1 6 FAULT", "2 8 FAULT", "3 2 FAULT", "4 4 FAULT"]);
        assert_eq!(cells(&table(&v, "site config").first[1]), ["owner", "null", "-1"]);

        let others: Vec<_> = v
            .others
            .iter()
            .map(|o| format!("{} {} {}", o.kind, o.name, o.table))
            .collect();
        assert_eq!(
            others,
            [
                "index idx_readings_ts readings",
                "view latest latest",
                "trigger log_fault readings"
            ]
        );
        let rendered: usize = v
            .tables
            .iter()
            .flat_map(|t| t.first.iter().flatten())
            .map(|c| c.as_ref().map_or(4, |s| s.len()) + 1)
            .sum();
        assert!(rendered <= ROW_BUDGET, "rows use {rendered} bytes");
    }

    #[test]
    fn multi_level_btree() {
        let file = fixture("deep.db");
        let v = decode(&file).unwrap();
        assert!(v.notes.is_empty(), "{:?}", v.notes);
        assert_eq!((v.pages, v.page_size), (96, 512));
        let t = table(&v, "samples");
        assert_eq!(t.rows, Some(4000));
        assert_eq!(t.first.len(), MAX_ROWS);
        assert_eq!(cells(&t.first[10]), ["11", "11", "s"]);
        assert_eq!(cells(&t.first[11]), ["12", "12", "null"]);
        // Three levels: the root and its first child are interior pages.
        let db = Db::open(&file).unwrap();
        let root = db.btree(2).unwrap();
        assert_eq!(root.kind, TABLE_INTERIOR);
        assert_eq!(db.btree(root.child(0).unwrap()).unwrap().kind, TABLE_INTERIOR);
        // Every row, in rowid order.
        let ids: Vec<_> = stored_rows(&file, "samples").iter().map(|r| r.0.unwrap()).collect();
        assert_eq!(ids, (1..=4000).collect::<Vec<_>>());
    }

    #[test]
    fn overflow_pages() {
        let file = fixture("overflow.db");
        let rows = stored_rows(&file, "notes");
        assert_eq!(rows.len(), 3);
        let Value::Text(body) = &rows[1].1[1] else {
            panic!("{:?}", rows[1].1[1]);
        };
        assert_eq!(body.len(), 9000);
        assert!(body.starts_with("line 0000 of the service log. line 0001"));
        assert!(body.ends_with("line 0299 of the service log. "));
        assert_eq!(rows[1].1[2], Value::Blob(5000, vec![]));
        assert_eq!(
            rows[2].1,
            [Value::Null, Value::Text("after".into()), Value::Blob(2, vec![0, 0xff])]
        );

        let v = decode(&file).unwrap();
        let row = &table(&v, "notes").first[1];
        let cell = row[1].as_deref().unwrap();
        assert_eq!(cell.chars().count(), MAX_CELL);
        assert!(cell.starts_with("line 0000 of the service log.") && cell.ends_with('…'));
        assert_eq!(cells(row)[2], "<5,000 B blob>");
        assert_eq!(cells(&table(&v, "notes").first[2]), ["3", "after", "x'00ff'"]);
    }

    #[test]
    fn without_rowid_table() {
        let file = fixture("without_rowid.db");
        let v = decode(&file).unwrap();
        let t = table(&v, "settings");
        assert!(t.without_rowid);
        assert_eq!(t.rows, Some(300));
        let names: Vec<_> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["scope", "value", "key"]);
        // Declared order, though the key (key, scope) is stored first.
        assert_eq!(cells(&t.first[0]), ["T01", "value 0", "key000"]);
        assert_eq!(cells(&t.first[4]), ["T02", "value 4", "key001"]);
        assert!(v.notes.iter().any(|n| n.contains("WITHOUT ROWID")), "{:?}", v.notes);
        // Entries on interior pages are rows too: all 300, in key order.
        assert_eq!(Db::open(&file).unwrap().btree(2).unwrap().kind, INDEX_INTERIOR);
        let keys: Vec<_> = stored_rows(&file, "settings")
            .iter()
            .map(|(_, r)| match (&r[0], &r[1]) {
                (Value::Text(k), Value::Text(s)) => format!("{k}/{s}"),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(keys.len(), 300);
        assert!(keys.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(keys[299], "key099/T03");
    }

    #[test]
    fn utf16_text() {
        let v = decode(&fixture("utf16.db")).unwrap();
        assert_eq!(v.encoding, "UTF-16le");
        let t = table(&v, "places");
        let names: Vec<_> = t.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["name", "kind"]);
        assert_eq!(cells(&t.first[0]), ["Ærøskøbing", "harbour"]);
        assert_eq!(cells(&t.first[1]), ["風車", "null"]);
    }

    #[test]
    fn wal_mode_and_empty() {
        let v = decode(&fixture("wal.db")).unwrap();
        assert!(v.wal);
        assert!(v.notes[0].contains("-wal file"), "{:?}", v.notes);
        assert_eq!(cells(&table(&v, "log").first[0]), ["1", "started"]);

        let v = decode(&fixture("empty.db")).unwrap();
        assert!(!v.wal && v.tables.is_empty() && v.others.is_empty() && v.notes.is_empty());
        let d = v.to_view();
        assert!(d.sections.is_empty());
        let facts: Vec<_> = d.summary.iter().map(|f| format!("{} {}", f.value, f.label)).collect();
        assert_eq!(
            facts,
            ["1 page", "4096 B page size", "UTF-8 ", "0 tables", " SQLite 3.46.1"]
        );
    }

    #[test]
    fn rendered_on_arrival() {
        use crate::theme::Theme;
        let file = fixture("turbine.db");
        let v = crate::pretty::decode_on_arrival(Some("application/vnd.sqlite3"), &file).unwrap();
        assert_eq!(v.format, "SQLite");
        let lines: Vec<String> = v
            .render(&Theme::new(false))
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            lines[..18],
            [
                "11 pages · 4096 B page size · UTF-8 · user_version 3 · 4 tables · SQLite 3.46.1",
                "application_id 1179865421 (0x4653514d)",
                "",
                "Tables",
                "table       │ rows │ columns                     ",
                "────────────┼──────┼─────────────────────────────",
                "turbines    │    8 │ id INTEGER, name TEXT, mode…",
                "readings    │  200 │ ts INTEGER, turbine TEXT, p…",
                "events      │    4 │ id INTEGER, turbine_id INTE…",
                "site config │    2 │ key TEXT, value TEXT, updat…",
                "",
                "Indexes, views and triggers",
                "index idx_readings_ts on readings (ts)",
                "view latest",
                "  CREATE VIEW latest AS SELECT turbine, max(ts) AS ts FROM readings GROUP BY turbine",
                "trigger log_fault on readings",
                "  CREATE TRIGGER log_fault AFTER INSERT ON readings WHEN NEW.status = 'FAULT' BEGIN INSERT INTO events (turbine_id, kind, at) VALUES (CAST(substr(NEW.turbine, 2)…",
                "",
            ]
        );
        assert_eq!(lines[18], "turbines · 8 rows");
        assert!(
            lines.contains(&"readings · first 56 of 200 rows".to_string()),
            "{lines:#?}"
        );
        assert!(lines.contains(&"site config · 2 rows".to_string()));

        // Found by its header alone; a content type without the header is an error.
        assert!(crate::pretty::decode_on_arrival(None, &file).is_some_and(|v| v.format == "SQLite"));
        let bad = crate::pretty::decode_on_arrival(Some("application/x-sqlite3"), b"not a database").unwrap();
        assert_eq!(
            bad.notes,
            ["could not read this SQLite payload: no \"SQLite format 3\" header"]
        );
        assert!(sniff("", b"plain text").is_none());
    }

    #[test]
    fn create_table_parsing() {
        let d = parse_create_table(
            "CREATE TABLE IF NOT EXISTS \"t(x\" (\n  [a b] DECIMAL( 10 , 2 ) NOT NULL, -- a comment, with a comma\n  \
             `c` /* inline */ VARCHAR(20) DEFAULT 'x,y', d, e TEXT AS (c || 'z'),\n  \
             CONSTRAINT pk PRIMARY KEY (\"d\" DESC, [a b]), FOREIGN KEY (c) REFERENCES u(c)\n) WITHOUT ROWID, STRICT",
        );
        let cols: Vec<_> = d.columns.iter().map(|c| format!("{}:{}", c.name, c.ty)).collect();
        assert_eq!(cols, ["a b:DECIMAL( 10 , 2 )", "c:VARCHAR(20)", "d:", "e:TEXT"]);
        assert_eq!(d.pk, [2, 0]);
        assert!(d.without_rowid && d.columns[3].generated);
        assert_eq!(d.storage_order(true), [2, 0, 1]);
        assert_eq!(d.rowid_alias(), None);

        let d = parse_create_table("create table t (id integer primary key autoincrement, v)");
        assert_eq!(d.rowid_alias(), Some(0));
        let d = parse_create_table("CREATE TABLE t (id INTEGER, v, PRIMARY KEY (id))");
        assert_eq!(d.rowid_alias(), Some(0));
        let d = parse_create_table("CREATE TABLE t (id INTEGER PRIMARY KEY DESC, v)");
        assert_eq!(d.rowid_alias(), None);
        let d = parse_create_table("CREATE VIRTUAL TABLE docs USING fts5(title, body)");
        assert!(d.is_virtual);
        assert!(parse_create_table("CREATE TABLE broken (a INT, b").columns.len() == 2);
        assert!(parse_create_table("").columns.is_empty());
    }

    #[test]
    fn values() {
        assert_eq!(varint(&[0x7f], 0), Ok((0x7f, 1)));
        assert_eq!(varint(&[0x81, 0x00], 0), Ok((0x80, 2)));
        assert_eq!(varint(&[0xff; 9], 0), Ok((u64::MAX, 9)));
        assert!(varint(&[0x81], 0).is_err());
        assert_eq!(be_int(&[0xff, 0xfe]), -2);
        assert_eq!(be_int(&[0x01, 0x00, 0x00]), 65536);
        assert_eq!(real(3600.0), "3600.0");
        assert_eq!(real(-12.5), "-12.5");
        assert_eq!(real(0.1), "0.1");
        assert_eq!(real(1e300), "1e300");
        assert_eq!(real(2.5e-7), "2.5e-7");
        assert!(real_affinity("DOUBLE PRECISION") && real_affinity("float") && !real_affinity("POINT INT"));
    }

    #[test]
    fn never_panics_on_damaged_input() {
        assert!(decode(MAGIC).is_err());
        assert!(sniff("", MAGIC).is_some_and(|r| r.is_err()));
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Cut, flip and scramble bytes all over the files; any result is fine, a panic or hang is not.
        for name in ["turbine.db", "deep.db", "without_rowid.db", "overflow.db", "utf16.db"] {
            let good = fixture(name);
            for _ in 0..300 {
                let mut f = good.clone();
                for _ in 0..(rnd() % 8 + 1) {
                    // Mostly the b-tree headers and cell pointers at the start of pages.
                    let i = if rnd() % 2 == 0 {
                        (rnd() as usize) % f.len()
                    } else {
                        let page = (rnd() as usize) % (f.len() / 512);
                        (page * 512 + (rnd() as usize) % 120).min(f.len() - 1)
                    };
                    f[i] = rnd() as u8;
                }
                let _ = decode(&f).map(|v| v.to_view());
                let cut = 16 + (rnd() as usize) % (f.len() - 16);
                let _ = decode(&f[..cut]).map(|v| v.to_view());
            }
        }
    }
}
