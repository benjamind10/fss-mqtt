//! A small, read-only Apache Arrow IPC decoder (file and stream format): the
//! schema, record batch and row counts, and the first rows rendered as text.
//!
//! Arrow metadata is FlatBuffers; there's a minimal, bounds-checked reader for
//! it below. Like the Parquet decoder it runs on the full payload as it
//! arrives and must never panic on malformed input: everything is checked and
//! errors become notes in the result.

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::io::Read;

use crate::fmt::commas;
use crate::values::{clip, date, hex, time_of_day, timestamp};
use crate::view::{Body, DocView, Fact};

/// Rows are decoded until the rendered cells reach about this many bytes.
const ROW_BUDGET: usize = 4096;
const MAX_ROWS: usize = 200;
const MAX_CELL: usize = 64;
/// Refuse to inflate a buffer larger than this, or more than `MAX_INFLATE` in all.
const MAX_BUFFER: usize = 32 << 20;
const MAX_INFLATE: usize = 64 << 20;
/// Nesting limit for schemas, values and dictionaries.
const MAX_DEPTH: usize = 32;
const MAX_FIELDS: usize = 10_000;

type R<T> = Result<T, String>;

/// Decoder entry point (see `pretty::Decoder`): the file format by its magic
/// bytes, the stream format by content type or a well-formed leading schema.
pub fn sniff(ct: &str, b: &[u8]) -> Option<Result<DocView, String>> {
    read(ct, b, MAX_ROWS, ROW_BUDGET).map(|r| r.map(|v| v.to_view()))
}

fn read(ct: &str, b: &[u8], max_rows: usize, budget: usize) -> Option<R<ArrowView>> {
    if is_file(b) {
        return Some(decode_file(b, max_rows, budget));
    }
    let claimed = ct.contains("vnd.apache.arrow.stream") || ct.contains("vnd.apache.arrow.file");
    (claimed || looks_like_stream(b)).then(|| decode_stream(b, max_rows, budget))
}

fn is_file(b: &[u8]) -> bool {
    b.len() >= 18 && b.starts_with(b"ARROW1") && b.ends_with(b"ARROW1")
}

/// Strict check for a stream without a content type: an encapsulated Schema
/// message, padded to 8 bytes, with a known metadata version and no body.
fn looks_like_stream(b: &[u8]) -> bool {
    let Ok(Some((m, _))) = message(b, 0) else {
        return false;
    };
    m.kind == 1
        && m.body.is_empty()
        && m.meta_len.is_multiple_of(8)
        && (0..=4).contains(&m.version)
        && parse_schema(m.header).is_ok_and(|s| !s.fields.is_empty())
}

struct ArrowView {
    kind: &'static str,
    batches: usize,
    num_rows: u64,
    codec: Option<&'static str>,
    meta: Vec<(String, String)>,
    /// name, type, nullable
    columns: Vec<(String, String, bool)>,
    /// Columns shown in `rows` (those whose values could be read).
    header: Vec<String>,
    /// First rows; `None` is a null cell.
    rows: Vec<Vec<Option<String>>>,
    notes: Vec<String>,
}

impl ArrowView {
    fn to_view(&self) -> DocView {
        let mut d = DocView::new("Arrow");
        let n = |k: u64, one: &str, many: &str| Fact::new(commas(k), if k == 1 { one } else { many });
        // Same order as Parquet's: rows, groups, columns, then how it's stored.
        d.summary = vec![
            n(self.num_rows, "row", "rows"),
            n(self.batches as u64, "record batch", "record batches"),
            n(self.columns.len() as u64, "column", "columns"),
            Fact::new("", self.kind),
        ];
        if let Some(c) = self.codec {
            d.summary.push(Fact::new("", c));
        }
        d.meta = self.meta.clone();
        let header = ["column", "type", "nullable"].map(String::from).to_vec();
        let rows = self
            .columns
            .iter()
            .map(|(name, ty, nullable)| {
                vec![
                    Some(clip(name.clone())),
                    Some(clip(ty.clone())),
                    Some(if *nullable { "yes" } else { "no" }.into()),
                ]
            })
            .collect();
        d.section("Schema", "", Body::Table { header, rows });
        if !self.rows.is_empty() {
            d.section(
                "Rows",
                format!("· first {} of {}", self.rows.len(), commas(self.num_rows)),
                Body::Table {
                    header: self.header.clone(),
                    rows: self.rows.clone(),
                },
            );
        }
        d.notes = self.notes.clone();
        d
    }
}

// ── Containers ─────────────────────────────────────────────────────────────

/// One encapsulated IPC message: flatbuffer `Message` and its body.
struct Msg<'a> {
    version: i16,
    /// `MessageHeader` union type: 1 Schema, 2 DictionaryBatch, 3 RecordBatch.
    kind: u8,
    header: Table<'a>,
    body: &'a [u8],
    meta_len: usize,
}

/// Reads the message at `pos`: `[0xFFFFFFFF] len:i32 Message body`, or the
/// legacy form without the continuation marker. `None` at end-of-stream.
fn message(b: &[u8], pos: usize) -> R<Option<(Msg<'_>, usize)>> {
    let mut p = pos;
    if u32_at(b, p)? == u32::MAX {
        p += 4;
    }
    let len = i32_at(b, p)?;
    p += 4;
    if len == 0 {
        return Ok(None);
    }
    let len = usize::try_from(len).map_err(|_| "bad message length")?;
    let meta = slice(b, p, len).ok_or("message runs past the end")?;
    let m = Table::root(meta)?;
    let body_len = usize::try_from(m.i64(3, 0)?).map_err(|_| "bad body length")?;
    let body = slice(b, p + len, body_len).ok_or("message body runs past the end")?;
    let header = m.table(2)?.ok_or("message without header")?;
    let msg = Msg {
        version: m.i16(0, 0)?,
        kind: m.u8(1, 0)?,
        header,
        body,
        meta_len: len,
    };
    Ok(Some((msg, p + len + body_len)))
}

fn decode_stream(b: &[u8], max_rows: usize, budget: usize) -> R<ArrowView> {
    let (first, mut pos) = message(b, 0)?.ok_or("empty stream")?;
    if first.kind != 1 {
        return Err("stream does not start with a schema".into());
    }
    let schema = parse_schema(first.header)?;
    let mut msgs = Vec::new();
    let mut notes = Vec::new();
    while pos < b.len() {
        match message(b, pos) {
            Ok(Some((m, end))) => {
                msgs.push(m);
                pos = end;
            }
            Ok(None) => break,
            Err(e) => {
                notes.push(format!("stream ends early: {e}"));
                break;
            }
        }
    }
    Ok(build("stream", &schema, &msgs, notes, max_rows, budget))
}

fn decode_file(b: &[u8], max_rows: usize, budget: usize) -> R<ArrowView> {
    if !is_file(b) {
        return Err("not an Arrow file".into());
    }
    let end = b.len() - 10;
    let footer_len = usize::try_from(i32_at(b, end)?).map_err(|_| "bad footer length")?;
    let start = end
        .checked_sub(footer_len)
        .filter(|&s| s >= 8)
        .ok_or("footer length out of range")?;
    let footer = Table::root(&b[start..end])?;
    let schema = parse_schema(footer.table(1)?.ok_or("footer without schema")?)?;
    let mut msgs = Vec::new();
    let mut notes = Vec::new();
    // Dictionaries first: record batches refer to them.
    for (slot, what) in [(2, "dictionary"), (3, "record batch")] {
        for (i, block) in footer.vector(slot, 24)?.as_chunks::<24>().0.iter().enumerate() {
            let at = usize::try_from(i64_at(block, 0)?).ok().filter(|&o| o < start);
            match at.ok_or_else(|| "bad offset".to_string()).and_then(|o| message(b, o)) {
                Ok(Some((m, _))) => msgs.push(m),
                Ok(None) => notes.push(format!("{what} {i}: empty message")),
                Err(e) => notes.push(format!("{what} {i}: {e}")),
            }
        }
    }
    Ok(build("file", &schema, &msgs, notes, max_rows, budget))
}

/// Counts batches and rows, then decodes the first rows.
fn build(
    kind: &'static str,
    schema: &Schema,
    msgs: &[Msg],
    mut notes: Vec<String>,
    max_rows: usize,
    budget: usize,
) -> ArrowView {
    let batches: Vec<&Msg> = msgs.iter().filter(|m| m.kind == 3).collect();
    let num_rows = batches
        .iter()
        .map(|m| m.header.i64(0, 0).unwrap_or(0).max(0) as u64)
        .fold(0u64, u64::saturating_add);
    let codec = msgs.iter().find_map(|m| {
        let rb = if m.kind == 2 {
            m.header.table(1).ok()??
        } else {
            m.header
        };
        let c = rb.table(3).ok()??;
        Some(match c.u8(0, 0) {
            Ok(0) => "lz4",
            Ok(1) => "zstd",
            _ => "unknown compression",
        })
    });
    let mut view = ArrowView {
        kind,
        batches: batches.len(),
        num_rows,
        codec,
        meta: schema
            .meta
            .iter()
            .filter(|(k, _)| k != "pandas" && !k.starts_with("ARROW:"))
            .map(|(k, v)| (clip(k.clone()), clip_long(v)))
            .collect(),
        columns: schema
            .fields
            .iter()
            .map(|f| (f.name.clone(), type_name(f, 0), f.nullable))
            .collect(),
        header: Vec::new(),
        rows: Vec::new(),
        notes: Vec::new(),
    };
    if schema.big_endian {
        notes.push("big-endian data: values not shown".into());
    } else {
        let want = (num_rows.min(max_rows as u64)) as usize;
        let (cols, failed) = first_rows(schema, msgs, want, &mut notes);
        let shown: Vec<usize> = (0..cols.len()).filter(|&c| !failed[c]).collect();
        let have = shown.iter().map(|&c| cols[c].len()).min().unwrap_or(0);
        view.header = shown.iter().map(|&c| clip(schema.fields[c].name.clone())).collect();
        let mut used = 0;
        let rows = (0..have).map(|r| shown.iter().map(|&c| cols[c][r].clone()).collect::<Vec<_>>());
        for row in rows {
            used += row.iter().map(|c| c.as_ref().map_or(4, |s| s.len()) + 1).sum::<usize>();
            if used > budget && !view.rows.is_empty() {
                break;
            }
            view.rows.push(row);
        }
    }
    view.notes = notes;
    view
}

/// Decodes up to `want` rows of every top-level column; also says which
/// columns couldn't be read (they get a note).
fn first_rows(
    schema: &Schema,
    msgs: &[Msg],
    want: usize,
    notes: &mut Vec<String>,
) -> (Vec<Vec<Option<String>>>, Vec<bool>) {
    let fields = &schema.fields;
    let mut cols: Vec<Vec<Option<String>>> = vec![Vec::new(); fields.len()];
    let mut failed = vec![false; fields.len()];
    let dict_fields = dictionary_fields(fields);
    let mut dicts: Dicts = HashMap::new();
    let inflate = Cell::new(MAX_INFLATE);
    let mut have = 0;
    for m in msgs {
        if have >= want {
            break;
        }
        match m.kind {
            2 => {
                if let Err((id, e)) = read_dictionary(m, &dict_fields, &inflate, &mut dicts) {
                    notes.push(format!("dictionary {id}: {e}"));
                }
            }
            3 => {
                let batch = match Batch::new(m.header, m.body, m.version) {
                    Ok(b) => b,
                    Err(e) => {
                        notes.push(format!("record batch: {e}"));
                        break;
                    }
                };
                let take = batch.len.min(want - have);
                let mut pos = Pos::default();
                for (c, f) in fields.iter().enumerate() {
                    let start = pos;
                    if let Err(e) = batch.skip(f, &mut pos, 0) {
                        // The layout of the columns after this one is unknown.
                        for (k, g) in fields.iter().enumerate().skip(c) {
                            if !failed[k] {
                                failed[k] = true;
                                notes.push(format!("{}: {e}", g.name));
                            }
                        }
                        break;
                    }
                    if failed[c] {
                        continue;
                    }
                    let res = batch.array(f, &mut { start }, &inflate, 0).and_then(|a| {
                        (0..take).try_for_each(|i| {
                            cols[c].push(a.cell(i, &dicts, 0)?);
                            Ok(())
                        })
                    });
                    if let Err(e) = res {
                        failed[c] = true;
                        notes.push(format!("{}: {e}", f.name));
                    }
                }
                have += take;
            }
            _ => {}
        }
    }
    (cols, failed)
}

fn read_dictionary<'a>(
    m: &Msg<'a>,
    dict_fields: &'a [(i64, Field)],
    inflate: &Cell<usize>,
    dicts: &mut Dicts<'a>,
) -> Result<(), (i64, String)> {
    let id = m.header.i64(0, 0).map_err(|e| (0, e))?;
    let err = |e: String| (id, e);
    let data = m.header.table(1).map_err(err)?.ok_or((id, "no data".into()))?;
    let delta = m.header.bool(2, false).map_err(err)?;
    let Some((_, f)) = dict_fields.iter().find(|(i, _)| *i == id) else {
        return Err((id, "not used by any column".into()));
    };
    let batch = Batch::new(data, m.body, m.version).map_err(err)?;
    let arr = batch.array(f, &mut Pos::default(), inflate, 0).map_err(err)?;
    let chunks = dicts.entry(id).or_default();
    if !delta {
        chunks.clear();
    }
    chunks.push(arr);
    Ok(())
}

/// The value fields of every dictionary-encoded field, by dictionary id.
fn dictionary_fields(fields: &[Field]) -> Vec<(i64, Field)> {
    let mut out = Vec::new();
    let mut stack: Vec<&Field> = fields.iter().collect();
    while let Some(f) = stack.pop() {
        if let Some((id, _)) = f.dict {
            out.push((
                id,
                Field {
                    dict: None,
                    ..f.clone()
                },
            ));
        }
        stack.extend(&f.children);
    }
    out
}

fn clip_long(v: &str) -> String {
    if v.len() <= 256 {
        return v.to_string();
    }
    let mut end = 256;
    while !v.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &v[..end])
}

// ── Schema ─────────────────────────────────────────────────────────────────

struct Schema {
    big_endian: bool,
    fields: Vec<Field>,
    meta: Vec<(String, String)>,
}

#[derive(Clone)]
struct Field {
    name: String,
    nullable: bool,
    ty: Ty,
    /// Dictionary id and index type.
    dict: Option<(i64, Ty)>,
    children: Vec<Field>,
}

/// Time units: 0 s, 1 ms, 2 us, 3 ns.
#[derive(Clone)]
enum Ty {
    Null,
    Int(u8, bool), // bit width, signed
    Float(i16),    // 0 half, 1 single, 2 double
    Bool,
    Utf8,
    LargeUtf8,
    Binary,
    LargeBinary,
    FixedBinary(usize),
    Date(bool), // milliseconds (DATE64) rather than days
    Time(i16, i32),
    Timestamp(i16, Option<String>),
    Decimal(i32, i32, i32), // precision, scale, bit width
    Duration(i16),
    Interval(i16),
    List,
    LargeList,
    FixedList(usize),
    Struct,
    Map,
    Union(i16), // 0 sparse, 1 dense
    RunEnd,
    View(bool), // Utf8View, BinaryView
    ListView(bool),
    Unknown(u8),
}

fn parse_schema(t: Table) -> R<Schema> {
    let mut count = 0;
    Ok(Schema {
        big_endian: t.i16(0, 0)? == 1,
        fields: t
            .tables(1)?
            .into_iter()
            .map(|f| parse_field(f, 0, &mut count))
            .collect::<R<_>>()?,
        meta: key_values(&t, 2)?,
    })
}

fn key_values(t: &Table, slot: usize) -> R<Vec<(String, String)>> {
    t.tables(slot)?
        .into_iter()
        .map(|kv| Ok((kv.string(0)?.unwrap_or_default(), kv.string(1)?.unwrap_or_default())))
        .collect()
}

fn parse_field(t: Table, depth: usize, count: &mut usize) -> R<Field> {
    *count += 1;
    if depth > MAX_DEPTH || *count > MAX_FIELDS {
        return Err("schema too large".into());
    }
    let dict = match t.table(4)? {
        Some(d) => {
            let index = match d.table(1)? {
                Some(i) => parse_type(2, Some(i))?,
                None => Ty::Int(32, true),
            };
            if !matches!(index, Ty::Int(..)) {
                return Err("bad dictionary index type".into());
            }
            Some((d.i64(0, 0)?, index))
        }
        None => None,
    };
    Ok(Field {
        name: t.string(0)?.unwrap_or_default(),
        nullable: t.bool(1, false)?,
        ty: parse_type(t.u8(2, 0)?, t.table(3)?)?,
        dict,
        children: t
            .tables(5)?
            .into_iter()
            .map(|c| parse_field(c, depth + 1, count))
            .collect::<R<_>>()?,
    })
}

fn parse_type(id: u8, t: Option<Table>) -> R<Ty> {
    let i16f = |slot, d| t.as_ref().map_or(Ok(d), |t| t.i16(slot, d));
    let i32f = |slot, d| t.as_ref().map_or(Ok(d), |t| t.i32(slot, d));
    let unit = |slot, d| {
        let u = i16f(slot, d)?;
        if (0..=3).contains(&u) {
            Ok(u)
        } else {
            Err("bad time unit".to_string())
        }
    };
    let width = |w: i32| usize::try_from(w).map_err(|_| "bad width".to_string());
    Ok(match id {
        0 => return Err("field without a type".into()),
        1 => Ty::Null,
        2 => {
            let bits = i32f(0, 0)?;
            if ![8, 16, 32, 64].contains(&bits) {
                return Err("bad integer width".into());
            }
            Ty::Int(bits as u8, t.as_ref().map_or(Ok(false), |t| t.bool(1, false))?)
        }
        3 => match i16f(0, 0)? {
            p @ 0..=2 => Ty::Float(p),
            _ => return Err("bad float precision".into()),
        },
        4 => Ty::Binary,
        5 => Ty::Utf8,
        6 => Ty::Bool,
        7 => {
            let bits = i32f(2, 128)?;
            if ![32, 64, 128, 256].contains(&bits) {
                return Err("bad decimal width".into());
            }
            Ty::Decimal(i32f(0, 0)?, i32f(1, 0)?, bits)
        }
        8 => match i16f(0, 1)? {
            u @ 0..=1 => Ty::Date(u == 1),
            _ => return Err("bad date unit".into()),
        },
        9 => {
            let bits = i32f(1, 32)?;
            if bits != 32 && bits != 64 {
                return Err("bad time width".into());
            }
            Ty::Time(unit(0, 1)?, bits)
        }
        10 => Ty::Timestamp(unit(0, 0)?, t.as_ref().map_or(Ok(None), |t| t.string(1))?),
        11 => match i16f(0, 0)? {
            u @ 0..=2 => Ty::Interval(u),
            _ => return Err("bad interval unit".into()),
        },
        12 => Ty::List,
        13 => Ty::Struct,
        14 => Ty::Union(i16f(0, 0)?),
        15 => Ty::FixedBinary(width(i32f(0, 0)?)?),
        16 => Ty::FixedList(width(i32f(0, 0)?)?),
        17 => Ty::Map,
        18 => Ty::Duration(unit(0, 1)?),
        19 => Ty::LargeBinary,
        20 => Ty::LargeUtf8,
        21 => Ty::LargeList,
        22 => Ty::RunEnd,
        23 => Ty::View(false),
        24 => Ty::View(true),
        25 => Ty::ListView(false),
        26 => Ty::ListView(true),
        other => Ty::Unknown(other),
    })
}

const UNITS: [&str; 4] = ["s", "ms", "us", "ns"];

fn unit_name(u: i16) -> &'static str {
    UNITS.get(u as usize).copied().unwrap_or("?")
}

fn type_name(f: &Field, depth: usize) -> String {
    let name = base_type_name(f, depth);
    if f.dict.is_some() {
        format!("DICTIONARY({name})")
    } else {
        name
    }
}

fn base_type_name(f: &Field, depth: usize) -> String {
    if depth > MAX_DEPTH {
        return "…".into();
    }
    let child = |i: usize| f.children.get(i).map_or("?".into(), |c| type_name(c, depth + 1));
    let members = || {
        f.children
            .iter()
            .map(|c| format!("{}: {}", c.name, type_name(c, depth + 1)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match &f.ty {
        Ty::Null => "NULL".into(),
        Ty::Int(bits, true) => format!("INT{bits}"),
        Ty::Int(bits, false) => format!("UINT{bits}"),
        Ty::Float(0) => "FLOAT16".into(),
        Ty::Float(1) => "FLOAT".into(),
        Ty::Float(_) => "DOUBLE".into(),
        Ty::Bool => "BOOLEAN".into(),
        Ty::Utf8 => "STRING".into(),
        Ty::LargeUtf8 => "LARGE_STRING".into(),
        Ty::Binary => "BINARY".into(),
        Ty::LargeBinary => "LARGE_BINARY".into(),
        Ty::FixedBinary(w) => format!("FIXED_BINARY({w})"),
        Ty::Date(false) => "DATE".into(),
        Ty::Date(true) => "DATE(ms)".into(),
        Ty::Time(u, _) => format!("TIME({})", unit_name(*u)),
        Ty::Timestamp(u, None) => format!("TIMESTAMP({})", unit_name(*u)),
        Ty::Timestamp(u, Some(tz)) => {
            let tz = if is_utc(tz) { "UTC" } else { tz };
            format!("TIMESTAMP({}, {tz})", unit_name(*u))
        }
        Ty::Decimal(p, s, 128) => format!("DECIMAL({p},{s})"),
        Ty::Decimal(p, s, bits) => format!("DECIMAL{bits}({p},{s})"),
        Ty::Duration(u) => format!("DURATION({})", unit_name(*u)),
        Ty::Interval(u) => format!(
            "INTERVAL({})",
            ["YEAR_MONTH", "DAY_TIME", "MONTH_DAY_NANO"][*u as usize % 3]
        ),
        Ty::List => format!("LIST<{}>", child(0)),
        Ty::LargeList => format!("LARGE_LIST<{}>", child(0)),
        Ty::FixedList(n) => format!("FIXED_LIST({n})<{}>", child(0)),
        Ty::Struct => format!("STRUCT<{}>", members()),
        Ty::Map => {
            let entries = f.children.first();
            let kv = |i: usize| {
                entries
                    .and_then(|e| e.children.get(i))
                    .map_or("?".into(), |c| type_name(c, depth + 2))
            };
            format!("MAP<{}, {}>", kv(0), kv(1))
        }
        Ty::Union(_) => format!("UNION<{}>", members()),
        Ty::RunEnd => format!("RUN_END_ENCODED<{}>", child(1)),
        Ty::View(true) => "STRING_VIEW".into(),
        Ty::View(false) => "BINARY_VIEW".into(),
        Ty::ListView(false) => format!("LIST_VIEW<{}>", child(0)),
        Ty::ListView(true) => format!("LARGE_LIST_VIEW<{}>", child(0)),
        Ty::Unknown(id) => format!("TYPE {id}"),
    }
}

fn is_utc(tz: &str) -> bool {
    matches!(tz, "UTC" | "utc" | "Z" | "+00:00" | "Etc/UTC" | "GMT")
}

// ── Record batches ─────────────────────────────────────────────────────────

struct Batch<'a> {
    len: usize,
    /// `FieldNode` structs: length, null_count.
    nodes: &'a [u8],
    /// `Buffer` structs: offset, length (into `body`).
    buffers: &'a [u8],
    /// Data buffer counts of the view-type columns.
    variadic: &'a [u8],
    body: &'a [u8],
    /// 0 LZ4 frame, 1 Zstd.
    codec: Option<u8>,
    v5: bool,
}

/// Where the next field's node and buffers are.
#[derive(Clone, Copy, Default)]
struct Pos {
    node: usize,
    buf: usize,
    var: usize,
}

impl<'a> Batch<'a> {
    fn new(rb: Table<'a>, body: &'a [u8], version: i16) -> R<Batch<'a>> {
        let codec = match rb.table(3)? {
            Some(c) => {
                if c.u8(1, 0)? != 0 {
                    return Err("compression method not supported".into());
                }
                match c.u8(0, 0)? {
                    k @ 0..=1 => Some(k),
                    k => return Err(format!("compression codec {k} not supported")),
                }
            }
            None => None,
        };
        Ok(Batch {
            len: usize::try_from(rb.i64(0, 0)?).map_err(|_| "bad length")?,
            nodes: rb.vector(1, 16)?,
            buffers: rb.vector(2, 16)?,
            variadic: rb.vector(4, 8)?,
            body,
            codec,
            v5: version >= 4,
        })
    }

    /// How many buffers an array of `f` has (not counting its children).
    fn buffer_count(&self, f: &Field, p: &mut Pos) -> R<usize> {
        if f.dict.is_some() {
            return Ok(2);
        }
        Ok(match f.ty {
            Ty::Null | Ty::RunEnd => 0,
            Ty::Struct | Ty::FixedList(_) => 1,
            Ty::Utf8 | Ty::LargeUtf8 | Ty::Binary | Ty::LargeBinary | Ty::ListView(_) => 3,
            Ty::Union(mode) => 1 + usize::from(mode == 1) + usize::from(!self.v5),
            Ty::View(_) => {
                let extra = usize::try_from(i64_at(self.variadic, p.var * 8)?)
                    .ok()
                    .filter(|&n| n <= self.buffers.len() / 16)
                    .ok_or("bad buffer count")?;
                p.var += 1;
                2 + extra
            }
            Ty::Unknown(id) => return Err(format!("unknown type {id}, layout unknown")),
            _ => 2,
        })
    }

    /// Steps over the nodes and buffers of `f` and its children.
    fn skip(&self, f: &Field, p: &mut Pos, depth: usize) -> R<()> {
        if depth > MAX_DEPTH {
            return Err("nested too deeply".into());
        }
        p.node += 1;
        p.buf = p.buf.saturating_add(self.buffer_count(f, p)?);
        if f.dict.is_none() {
            for c in &f.children {
                self.skip(c, p, depth + 1)?;
            }
        }
        Ok(())
    }

    fn node(&self, i: usize) -> R<(usize, i64)> {
        let at = i.checked_mul(16).ok_or("bad node")?;
        let len = usize::try_from(i64_at(self.nodes, at)?).map_err(|_| "bad node length")?;
        Ok((len, i64_at(self.nodes, at + 8)?))
    }

    fn buffer(&self, i: usize, inflate: &Cell<usize>) -> R<Cow<'a, [u8]>> {
        let at = i.checked_mul(16).ok_or("bad buffer")?;
        let off = usize::try_from(i64_at(self.buffers, at)?).ok();
        let len = usize::try_from(i64_at(self.buffers, at + 8)?).ok();
        let raw = off
            .zip(len)
            .and_then(|(o, l)| slice(self.body, o, l))
            .ok_or("buffer out of range")?;
        match self.codec {
            None => Ok(Cow::Borrowed(raw)),
            Some(codec) => decompress(codec, raw, inflate),
        }
    }

    /// The array of `f` (and its children) in this batch.
    fn array<'f>(&self, f: &'f Field, p: &mut Pos, inflate: &Cell<usize>, depth: usize) -> R<Arr<'f>>
    where
        'a: 'f,
    {
        if depth > MAX_DEPTH {
            return Err("nested too deeply".into());
        }
        if f.dict.is_none() {
            match f.ty {
                Ty::Union(_) | Ty::RunEnd | Ty::View(_) | Ty::ListView(_) | Ty::Unknown(_) => {
                    return Err(format!("{} values not shown", type_name(f, 0)));
                }
                _ => {}
            }
        }
        let (len, nulls) = self.node(p.node)?;
        p.node += 1;
        let n = self.buffer_count(f, p)?;
        let first = p.buf;
        p.buf = p.buf.checked_add(n).ok_or("bad buffer count")?;
        let has_validity = f.dict.is_some() || !matches!(f.ty, Ty::Null);
        let valid = if has_validity && nulls != 0 {
            Some(self.buffer(first, inflate)?).filter(|v| !v.is_empty())
        } else {
            None
        };
        let bufs = (first + usize::from(has_validity)..first + n)
            .map(|i| self.buffer(i, inflate))
            .collect::<R<Vec<_>>>()?;
        let kids = if f.dict.is_none() {
            f.children
                .iter()
                .map(|c| self.array(c, p, inflate, depth + 1))
                .collect::<R<Vec<_>>>()?
        } else {
            Vec::new()
        };
        Ok(Arr {
            f,
            len,
            valid,
            bufs,
            kids,
        })
    }
}

/// A buffer of a compressed body: its uncompressed length (`-1`: stored as
/// is), then LZ4 frame or Zstd data.
fn decompress<'a>(codec: u8, raw: &'a [u8], inflate: &Cell<usize>) -> R<Cow<'a, [u8]>> {
    if raw.is_empty() {
        return Ok(Cow::Borrowed(raw));
    }
    let n = i64_at(raw, 0)?;
    let data = &raw[8..];
    if n == -1 {
        return Ok(Cow::Borrowed(data));
    }
    let n = usize::try_from(n)
        .ok()
        .filter(|&n| n <= MAX_BUFFER && n <= inflate.get())
        .ok_or("buffer too large")?;
    inflate.set(inflate.get() - n);
    let out = if codec == 0 {
        lz4_frame(data, n)?
    } else {
        let mut d = ruzstd::decoding::StreamingDecoder::new(data).map_err(|e| format!("zstd: {e}"))?;
        let mut out = Vec::new();
        (&mut d)
            .take(n as u64)
            .read_to_end(&mut out)
            .map_err(|e| format!("zstd: {e}"))?;
        out
    };
    if out.len() != n {
        return Err("buffer has the wrong length".into());
    }
    Ok(Cow::Owned(out))
}

/// The LZ4 frame format, on top of lz4_flex's block decoder. Checksums are
/// skipped.
fn lz4_frame(data: &[u8], n: usize) -> R<Vec<u8>> {
    // LZ4 expands at most ~255x: don't allocate for more than the input can hold.
    if n > data.len().saturating_mul(255).saturating_add(64) {
        return Err("lz4: bad length".into());
    }
    let bad = || "lz4: bad frame".to_string();
    let mut out = vec![0u8; n];
    let (mut p, mut w) = (0, 0);
    while p < data.len() && w < n {
        if get::<4>(data, p)? != [0x04, 0x22, 0x4d, 0x18] {
            return Err(bad());
        }
        let flags = *data.get(p + 4).ok_or_else(bad)?;
        if flags >> 6 != 1 {
            return Err(bad());
        }
        p += 7 + if flags & 8 != 0 { 8 } else { 0 } + if flags & 1 != 0 { 4 } else { 0 };
        let linked = flags & 0x20 == 0;
        loop {
            let size = u32_at(data, p)?;
            p += 4;
            if size == 0 {
                break;
            }
            let len = (size & 0x7fff_ffff) as usize;
            let block = slice(data, p, len).ok_or_else(bad)?;
            p += len + if flags & 0x10 != 0 { 4 } else { 0 };
            let (prev, rest) = out.split_at_mut(w);
            if size & 0x8000_0000 != 0 {
                rest.get_mut(..len).ok_or_else(bad)?.copy_from_slice(block);
                w += len;
            } else {
                let dict = if linked {
                    &prev[prev.len().saturating_sub(65536)..]
                } else {
                    &[]
                };
                w += lz4_flex::block::decompress_into_with_dict(block, rest, dict).map_err(|e| format!("lz4: {e}"))?;
            }
        }
        if flags & 4 != 0 {
            p += 4;
        }
    }
    out.truncate(w);
    Ok(out)
}

// ── Values ─────────────────────────────────────────────────────────────────

type Dicts<'a> = HashMap<i64, Vec<Arr<'a>>>;

/// One column of a record batch (or a dictionary), ready to render.
struct Arr<'a> {
    f: &'a Field,
    len: usize,
    /// Validity bitmap, LSB first; `None`: no nulls.
    valid: Option<Cow<'a, [u8]>>,
    /// The other buffers, in layout order.
    bufs: Vec<Cow<'a, [u8]>>,
    kids: Vec<Arr<'a>>,
}

impl Arr<'_> {
    fn buf(&self, k: usize) -> &[u8] {
        self.bufs.get(k).map_or(&[], |b| b)
    }

    fn cell(&self, i: usize, dicts: &Dicts, depth: usize) -> R<Option<String>> {
        if depth > MAX_DEPTH {
            return Err("nested too deeply".into());
        }
        if i >= self.len {
            return Err("column has fewer rows than the batch".into());
        }
        if self.f.dict.is_none() && matches!(self.f.ty, Ty::Null) {
            return Ok(None);
        }
        if let Some(v) = &self.valid
            && !bit(v, i)?
        {
            return Ok(None);
        }
        if let Some((id, Ty::Int(bits, signed))) = &self.f.dict {
            let k = int(self.buf(0), *bits, *signed, i)?;
            return dict_cell(dicts, *id, k, depth + 1);
        }
        Ok(Some(clip(self.value(i, dicts, depth)?)))
    }

    fn value(&self, i: usize, dicts: &Dicts, depth: usize) -> R<String> {
        let b = self.buf(0);
        let digits = |u: i16| [0, 3, 6, 9][u as usize % 4];
        let kid = |k: usize, j: usize| -> R<String> {
            let a = self.kids.get(k).ok_or("missing child")?;
            Ok(a.cell(j, dicts, depth + 1)?.unwrap_or_else(|| "null".into()))
        };
        Ok(match &self.f.ty {
            Ty::Null => String::new(),
            Ty::Int(bits, signed) => int(b, *bits, *signed, i)?.to_string(),
            Ty::Float(0) => crate::pretty::binval::half(u16::from_le_bytes(get(b, i * 2)?)).to_string(),
            Ty::Float(1) => f32::from_le_bytes(get(b, i * 4)?).to_string(),
            Ty::Float(_) => f64::from_le_bytes(get(b, i * 8)?).to_string(),
            Ty::Bool => bit(b, i)?.to_string(),
            Ty::Utf8 | Ty::LargeUtf8 => {
                let large = matches!(self.f.ty, Ty::LargeUtf8);
                let s = self.var_bytes(large, i)?;
                String::from_utf8_lossy(&s[..s.len().min(MAX_CELL * 4)]).into_owned()
            }
            Ty::Binary | Ty::LargeBinary => bytes(self.var_bytes(matches!(self.f.ty, Ty::LargeBinary), i)?),
            Ty::FixedBinary(w) => bytes(fixed(b, i, *w)?),
            Ty::Date(false) => date(i32::from_le_bytes(get(b, i * 4)?) as i64),
            Ty::Date(true) => date(i64::from_le_bytes(get(b, i * 8)?).div_euclid(86_400_000)),
            Ty::Time(u, 32) => time_of_day(i32::from_le_bytes(get(b, i * 4)?) as i64, digits(*u)),
            Ty::Time(u, _) => time_of_day(i64::from_le_bytes(get(b, i * 8)?), digits(*u)),
            Ty::Timestamp(u, tz) => timestamp(i64::from_le_bytes(get(b, i * 8)?), digits(*u), tz.is_some()),
            Ty::Decimal(_, scale, bits) => decimal(fixed(b, i, *bits as usize / 8)?, *scale),
            Ty::Duration(u) => format!("{}{}", i64::from_le_bytes(get(b, i * 8)?), unit_name(*u)),
            Ty::Interval(0) => format!("{}mo", i32::from_le_bytes(get(b, i * 4)?)),
            Ty::Interval(1) => {
                let v = fixed(b, i, 8)?;
                let (d, ms) = (i32_at(v, 0)?, i32_at(v, 4)?);
                format!("{d}d {ms}ms")
            }
            Ty::Interval(_) => {
                let v = fixed(b, i, 16)?;
                format!("{}mo {}d {}ns", i32_at(v, 0)?, i32_at(v, 4)?, i64_at(v, 8)?)
            }
            Ty::List | Ty::LargeList | Ty::Map => {
                let (start, end) = offsets(b, matches!(self.f.ty, Ty::LargeList), i)?;
                if matches!(self.f.ty, Ty::Map) {
                    let entries = self.kids.first().ok_or("missing child")?;
                    let pair = |j| -> R<String> {
                        let kv = |k: usize| -> R<String> {
                            let a = entries.kids.get(k).ok_or("missing child")?;
                            Ok(a.cell(j, dicts, depth + 2)?.unwrap_or_else(|| "null".into()))
                        };
                        Ok(format!("{}: {}", kv(0)?, kv(1)?))
                    };
                    join("{", "}", (start..end).map(pair))?
                } else {
                    join("[", "]", (start..end).map(|j| kid(0, j)))?
                }
            }
            Ty::FixedList(n) => {
                let start = i.checked_mul(*n).ok_or("bad list size")?;
                let end = start.checked_add(*n).ok_or("bad list size")?;
                join("[", "]", (start..end).map(|j| kid(0, j)))?
            }
            Ty::Struct => {
                let members = self.f.children.iter().enumerate();
                join(
                    "{",
                    "}",
                    members.map(|(k, c)| Ok(format!("{}: {}", c.name, kid(k, i)?))),
                )?
            }
            Ty::Union(_) | Ty::RunEnd | Ty::View(_) | Ty::ListView(_) | Ty::Unknown(_) => {
                return Err(format!("{} values not shown", type_name(self.f, 0)));
            }
        })
    }

    /// The bytes of a variable-width value: offsets in buffer 0, data in buffer 1.
    fn var_bytes(&self, large: bool, i: usize) -> R<&[u8]> {
        let (start, end) = offsets(self.buf(0), large, i)?;
        self.buf(1).get(start..end).ok_or_else(|| "value out of range".into())
    }
}

fn dict_cell(dicts: &Dicts, id: i64, k: i128, depth: usize) -> R<Option<String>> {
    let chunks = dicts.get(&id).ok_or_else(|| format!("dictionary {id} not available"))?;
    let mut k = usize::try_from(k).map_err(|_| "bad dictionary index")?;
    for c in chunks {
        if k < c.len {
            return c.cell(k, dicts, depth);
        }
        k -= c.len;
    }
    Err("dictionary index out of range".into())
}

/// Items joined with ", ", stopping once the text is longer than a cell can show.
fn join(open: &str, close: &str, items: impl Iterator<Item = R<String>>) -> R<String> {
    let mut s = String::from(open);
    for (k, item) in items.enumerate() {
        if s.len() > MAX_CELL * 4 {
            s.push('…');
            break;
        }
        if k > 0 {
            s.push_str(", ");
        }
        s.push_str(&item?);
    }
    s.push_str(close);
    Ok(s)
}

/// Text if it's printable UTF-8, else hex.
fn bytes(b: &[u8]) -> String {
    let b = &b[..b.len().min(MAX_CELL * 4)];
    match std::str::from_utf8(b) {
        Ok(s) if !s.chars().any(|c| c.is_control()) => s.to_string(),
        _ => hex(&b[..b.len().min(MAX_CELL)]),
    }
}

fn bit(b: &[u8], i: usize) -> R<bool> {
    let byte = b.get(i / 8).ok_or("bitmap too short")?;
    Ok(byte >> (i % 8) & 1 == 1)
}

/// The `i`-th `w`-byte slot of a buffer.
fn fixed(b: &[u8], i: usize, w: usize) -> R<&[u8]> {
    i.checked_mul(w)
        .and_then(|p| slice(b, p, w))
        .ok_or_else(|| "values end early".into())
}

fn int(b: &[u8], bits: u8, signed: bool, i: usize) -> R<i128> {
    let v = fixed(b, i, bits as usize / 8)?;
    let mut a = [0u8; 16];
    a[..v.len()].copy_from_slice(v);
    if signed && v.last().is_some_and(|x| x & 0x80 != 0) {
        a[v.len()..].fill(0xff);
    }
    Ok(i128::from_le_bytes(a))
}

fn offsets(b: &[u8], large: bool, i: usize) -> R<(usize, usize)> {
    let at = |j: usize| -> R<usize> {
        let v = if large {
            i64_at(b, j.checked_mul(8).ok_or("bad offset")?)?
        } else {
            i32_at(b, j.checked_mul(4).ok_or("bad offset")?)? as i64
        };
        usize::try_from(v).map_err(|_| "bad offset".into())
    };
    let (start, end) = (at(i)?, at(i + 1)?);
    if start > end {
        return Err("bad offsets".into());
    }
    Ok((start, end))
}

/// A little-endian two's complement decimal of any width, with `scale` digits
/// after the point.
fn decimal(b: &[u8], scale: i32) -> String {
    let neg = b.last().is_some_and(|x| x & 0x80 != 0);
    // Magnitude as little-endian 32-bit limbs.
    let mut bytes = b.to_vec();
    if neg {
        let mut carry = true;
        for x in &mut bytes {
            *x = !*x;
            if carry {
                let (v, c) = x.overflowing_add(1);
                *x = v;
                carry = c;
            }
        }
    }
    let mut limbs: Vec<u32> = bytes
        .chunks(4)
        .map(|c| c.iter().rev().fold(0u32, |a, &x| a << 8 | x as u32))
        .collect();
    let mut digits = Vec::new();
    while limbs.iter().any(|&l| l != 0) {
        let mut rem = 0u64;
        for l in limbs.iter_mut().rev() {
            let cur = rem << 32 | *l as u64;
            *l = (cur / 10) as u32;
            rem = cur % 10;
        }
        digits.push(b'0' + rem as u8);
    }
    digits.reverse();
    let digits = String::from_utf8(digits).unwrap_or_default();
    let sign = if neg { "-" } else { "" };
    if !(1..=76).contains(&scale) {
        return match scale {
            0 => format!("{sign}{}", if digits.is_empty() { "0" } else { &digits }),
            _ => format!("{sign}{}e{}", if digits.is_empty() { "0" } else { &digits }, -scale),
        };
    }
    let scale = scale as usize;
    let padded = format!("{digits:0>w$}", w = scale + 1);
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{sign}{int}.{frac}")
}

// ── FlatBuffers ────────────────────────────────────────────────────────────

fn slice(b: &[u8], p: usize, len: usize) -> Option<&[u8]> {
    b.get(p..p.checked_add(len)?)
}

fn get<const N: usize>(b: &[u8], p: usize) -> R<[u8; N]> {
    let s = slice(b, p, N).ok_or("data ends early")?;
    let mut a = [0u8; N];
    a.copy_from_slice(s);
    Ok(a)
}

fn u16_at(b: &[u8], p: usize) -> R<u16> {
    get(b, p).map(u16::from_le_bytes)
}

fn u32_at(b: &[u8], p: usize) -> R<u32> {
    get(b, p).map(u32::from_le_bytes)
}

fn i32_at(b: &[u8], p: usize) -> R<i32> {
    get(b, p).map(i32::from_le_bytes)
}

fn i64_at(b: &[u8], p: usize) -> R<i64> {
    get(b, p).map(i64::from_le_bytes)
}

/// A flatbuffer table: fields are found through its vtable by slot number
/// (the field's position in the schema, unions taking two slots).
#[derive(Clone, Copy)]
struct Table<'a> {
    b: &'a [u8],
    pos: usize,
    vt: usize,
    vt_len: usize,
}

impl<'a> Table<'a> {
    fn root(b: &'a [u8]) -> R<Table<'a>> {
        Table::at(b, u32_at(b, 0)? as usize)
    }

    fn at(b: &'a [u8], pos: usize) -> R<Table<'a>> {
        let vt = (pos as i64)
            .checked_sub(i32_at(b, pos)? as i64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or("bad vtable")?;
        let vt_len = u16_at(b, vt)? as usize;
        if vt_len < 4 || !vt_len.is_multiple_of(2) || vt + vt_len > b.len() {
            return Err("bad vtable".into());
        }
        Ok(Table { b, pos, vt, vt_len })
    }

    fn slot(&self, id: usize) -> R<Option<usize>> {
        let o = 4 + 2 * id;
        if o + 2 > self.vt_len {
            return Ok(None);
        }
        let off = u16_at(self.b, self.vt + o)? as usize;
        Ok((off != 0).then_some(self.pos + off))
    }

    fn scalar<const N: usize>(&self, id: usize) -> R<Option<[u8; N]>> {
        self.slot(id)?.map(|p| get::<N>(self.b, p)).transpose()
    }

    fn i64(&self, id: usize, d: i64) -> R<i64> {
        Ok(self.scalar(id)?.map_or(d, i64::from_le_bytes))
    }

    fn i32(&self, id: usize, d: i32) -> R<i32> {
        Ok(self.scalar(id)?.map_or(d, i32::from_le_bytes))
    }

    fn i16(&self, id: usize, d: i16) -> R<i16> {
        Ok(self.scalar(id)?.map_or(d, i16::from_le_bytes))
    }

    fn u8(&self, id: usize, d: u8) -> R<u8> {
        Ok(self.scalar::<1>(id)?.map_or(d, |[x]| x))
    }

    fn bool(&self, id: usize, d: bool) -> R<bool> {
        Ok(self.scalar::<1>(id)?.map_or(d, |[x]| x != 0))
    }

    /// Follows an offset field (to a table, vector or string).
    fn target(&self, id: usize) -> R<Option<usize>> {
        let Some(p) = self.slot(id)? else {
            return Ok(None);
        };
        let off = u32_at(self.b, p)? as usize;
        Ok(Some(p.checked_add(off).ok_or("bad offset")?))
    }

    fn table(&self, id: usize) -> R<Option<Table<'a>>> {
        self.target(id)?.map(|p| Table::at(self.b, p)).transpose()
    }

    /// The bytes of a vector of `size`-byte elements; empty if absent.
    fn vector(&self, id: usize, size: usize) -> R<&'a [u8]> {
        let Some(p) = self.target(id)? else {
            return Ok(&[]);
        };
        let n = u32_at(self.b, p)? as usize;
        n.checked_mul(size)
            .and_then(|len| slice(self.b, p + 4, len))
            .ok_or_else(|| "vector out of range".into())
    }

    fn tables(&self, id: usize) -> R<Vec<Table<'a>>> {
        let Some(p) = self.target(id)? else {
            return Ok(Vec::new());
        };
        let n = self.vector(id, 4)?.len() / 4;
        (0..n)
            .map(|i| {
                let e = p + 4 + 4 * i;
                let target = e.checked_add(u32_at(self.b, e)? as usize).ok_or("bad offset")?;
                Table::at(self.b, target)
            })
            .collect()
    }

    fn string(&self, id: usize) -> R<Option<String>> {
        if self.slot(id)?.is_none() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(self.vector(id, 1)?).into_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    const TURBINE: [&str; 6] = [
        "turbine-none.arrow",
        "turbine-lz4.arrow",
        "turbine-zstd.arrow",
        "turbine-none.arrows",
        "turbine-lz4.arrows",
        "turbine-zstd.arrows",
    ];

    fn decode(name: &str, max_rows: usize, budget: usize) -> ArrowView {
        read("", &fixture(name), max_rows, budget)
            .unwrap_or_else(|| panic!("{name}: not recognised"))
            .unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    #[test]
    fn decodes_every_variant() {
        for name in TURBINE {
            let v = decode(name, MAX_ROWS, ROW_BUDGET);
            assert!(v.notes.is_empty(), "{name}: {:?}", v.notes);
            let file = name.ends_with(".arrow");
            assert_eq!(v.kind, if file { "file" } else { "stream" }, "{name}");
            assert_eq!((v.num_rows, v.batches, v.columns.len()), (500, 3, 11), "{name}");
            let codec = ["lz4", "zstd"].into_iter().find(|c| name.contains(c));
            assert_eq!(v.codec, codec, "{name}");
            assert_eq!(
                v.header,
                [
                    "Timestamp",
                    "TurbineId",
                    "ActivePower",
                    "WindSpeed",
                    "Rpm",
                    "Counter",
                    "Running",
                    "Status",
                    "Day",
                    "Price",
                    "Tags"
                ],
                "{name}"
            );
            let row0: Vec<_> = v.rows[0].iter().map(|c| c.as_deref().unwrap_or("null")).collect();
            assert_eq!(
                row0,
                [
                    "2026-10-08 10:20:00Z",
                    "T01",
                    "-12.5",
                    "2",
                    "9",
                    "10000000000",
                    "false",
                    "IDLE",
                    "2026-10-08",
                    "0.00",
                    "[idle, check]"
                ],
                "{name}"
            );
            assert_eq!(v.rows[7][3], None, "{name}: WindSpeed row 7 is null");
            assert_eq!(v.rows[1][7].as_deref(), Some("RUNNING"), "{name}");
            assert_eq!(v.rows[1][9].as_deref(), Some("0.01"), "{name}");
            assert_eq!(v.rows[1][10].as_deref(), Some("[ok]"), "{name}");
            assert!(v.rows.len() > 20, "{name}: only {} rows", v.rows.len());
            let rendered: usize = v
                .rows
                .iter()
                .flatten()
                .map(|c| c.as_ref().map_or(4, |s| s.len()) + 1)
                .sum();
            assert!(rendered <= ROW_BUDGET + 200, "{name}: rows use {rendered} bytes");
        }
    }

    #[test]
    fn reads_rows_across_batches() {
        for name in TURBINE {
            let v = decode(name, 500, usize::MAX);
            assert_eq!(v.rows.len(), 500, "{name}");
            // Row 257 is in the second batch of 200, row 499 in the third.
            assert_eq!(v.rows[257][0].as_deref(), Some("2026-10-08 10:24:17Z"), "{name}");
            assert_eq!(v.rows[257][3], None, "{name}");
            assert_eq!(v.rows[257][7].as_deref(), Some("RUNNING"), "{name}");
            assert_eq!(v.rows[499][9].as_deref(), Some("4.99"), "{name}");
            assert_eq!(v.rows[495][7].as_deref(), Some("IDLE"), "{name}");
        }
    }

    #[test]
    fn schema_and_metadata() {
        let v = decode("turbine-zstd.arrows", MAX_ROWS, ROW_BUDGET);
        let ty = |n: &str| v.columns.iter().find(|c| c.0 == n).unwrap().1.as_str();
        assert_eq!(ty("Timestamp"), "TIMESTAMP(ms, UTC)");
        assert_eq!(ty("TurbineId"), "STRING");
        assert_eq!(ty("ActivePower"), "DOUBLE");
        assert_eq!(ty("WindSpeed"), "FLOAT");
        assert_eq!(ty("Rpm"), "INT32");
        assert_eq!(ty("Counter"), "INT64");
        assert_eq!(ty("Running"), "BOOLEAN");
        assert_eq!(ty("Status"), "DICTIONARY(STRING)");
        assert_eq!(ty("Day"), "DATE");
        assert_eq!(ty("Price"), "DECIMAL(9,2)");
        assert_eq!(ty("Tags"), "LIST<STRING>");
        assert!(v.columns.iter().all(|c| c.2), "pyarrow fields are nullable");
        assert_eq!(
            v.meta,
            [
                ("ProviderName".to_string(), "TurbineFastlog".to_string()),
                ("Version".to_string(), "1.0.0".to_string())
            ]
        );
    }

    #[test]
    fn rarer_types() {
        let v = decode("types.arrow", MAX_ROWS, ROW_BUDGET);
        assert_eq!(v.notes, ["either: UNION<n: INT64, s: STRING> values not shown"]);
        assert_eq!(
            v.header.len(),
            v.columns.len() - 1,
            "the union column isn't in the rows"
        );
        let types: Vec<_> = v.columns.iter().map(|c| c.1.as_str()).collect();
        assert_eq!(
            types,
            [
                "NULL",
                "UINT8",
                "INT16",
                "UINT64",
                "FLOAT16",
                "TIME(s)",
                "TIME(us)",
                "TIMESTAMP(ns)",
                "DATE(ms)",
                "LARGE_STRING",
                "BINARY",
                "LARGE_BINARY",
                "FIXED_BINARY(4)",
                "DURATION(ms)",
                "DECIMAL256(40,5)",
                "STRUCT<x: INT8, y: INT8>",
                "MAP<STRING, INT32>",
                "INTERVAL(MONTH_DAY_NANO)",
                "UNION<n: INT64, s: STRING>",
            ]
        );
        let cells = |r: usize| -> Vec<String> {
            v.rows[r]
                .iter()
                .map(|c| c.clone().unwrap_or_else(|| "null".into()))
                .collect()
        };
        assert_eq!(
            cells(0),
            [
                "null",
                "255",
                "-300",
                "18446744073709551615",
                "1.5",
                "01:02:03",
                "01:02:03.000001",
                "2026-10-08 10:30:00.123456789",
                "2026-10-08",
                "large",
                "0001ff",
                "02",
                "deadbeef",
                "1500ms",
                "-12345678901234567890.12345",
                "{x: 1, y: 2}",
                "{k: 1}",
                "1mo 2d 3ns",
            ]
        );
        assert_eq!(
            cells(1),
            [
                "null",
                "null",
                "7",
                "0",
                "-2",
                "00:00:00",
                "00:00:00",
                "1970-01-01 00:00:00",
                "1970-01-01",
                "null",
                "ok",
                "",
                "abcd",
                "-1ms",
                "null",
                "null",
                "{}",
                "null",
            ]
        );
    }

    /// The stream's messages again, in the legacy framing (no continuation
    /// marker), leaving out those `keep` rejects.
    fn legacy(stream: &[u8], keep: impl Fn(&Msg) -> bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some((m, end)) = message(stream, pos).unwrap() {
            if keep(&m) {
                out.extend_from_slice(&stream[pos + 4..end]);
            }
            pos = end;
        }
        out.extend_from_slice(&[0; 4]);
        out
    }

    #[test]
    fn legacy_stream_and_missing_dictionary() {
        let stream = fixture("turbine-zstd.arrows");
        let all = legacy(&stream, |_| true);
        assert_ne!(all[..4], [0xff; 4]);
        let v = read("", &all, MAX_ROWS, ROW_BUDGET).unwrap().unwrap();
        assert!(v.notes.is_empty(), "{:?}", v.notes);
        assert_eq!((v.num_rows, v.batches), (500, 3));
        assert_eq!(v.rows[0][7].as_deref(), Some("IDLE"));

        let no_dict = legacy(&stream, |m| m.kind != 2);
        let v = read("", &no_dict, MAX_ROWS, ROW_BUDGET).unwrap().unwrap();
        assert_eq!(v.notes, ["Status: dictionary 0 not available"]);
        assert!(!v.header.iter().any(|h| h == "Status"));
        assert_eq!(v.rows[0][0].as_deref(), Some("2026-10-08 10:20:00Z"));
    }

    /// Hand-written flatbuffer fields: (slot, value).
    enum Fb {
        U8(u8),
        I16(i16),
        I32(i32),
        Str(&'static str),
        Table(Vec<(usize, Fb)>),
        Tables(Vec<Vec<(usize, Fb)>>),
    }

    /// Writes vtable, table (each field in an 8-byte slot), then the children;
    /// returns the table's position.
    fn put(buf: &mut Vec<u8>, fields: &[(usize, Fb)]) -> usize {
        let slots = fields.iter().map(|f| f.0 + 1).max().unwrap_or(0);
        let vt = buf.len();
        buf.extend((4 + 2 * slots as u16).to_le_bytes());
        buf.extend((4 + 8 * fields.len() as u16).to_le_bytes());
        let mut entries = vec![0u16; slots];
        for (k, (slot, _)) in fields.iter().enumerate() {
            entries[*slot] = 4 + 8 * k as u16;
        }
        entries.iter().for_each(|e| buf.extend(e.to_le_bytes()));
        let pos = buf.len();
        buf.extend(((pos - vt) as i32).to_le_bytes());
        for (_, f) in fields {
            let mut v = [0u8; 8];
            match f {
                Fb::U8(x) => v[0] = *x,
                Fb::I16(x) => v[..2].copy_from_slice(&x.to_le_bytes()),
                Fb::I32(x) => v[..4].copy_from_slice(&x.to_le_bytes()),
                _ => {}
            }
            buf.extend(v);
        }
        let patch = |buf: &mut Vec<u8>, at: usize, target: usize| {
            buf[at..at + 4].copy_from_slice(&((target - at) as u32).to_le_bytes());
        };
        for (k, (_, f)) in fields.iter().enumerate() {
            let at = pos + 4 + 8 * k;
            let target = match f {
                Fb::Str(s) => {
                    let p = buf.len();
                    buf.extend((s.len() as u32).to_le_bytes());
                    buf.extend(s.as_bytes());
                    p
                }
                Fb::Table(t) => put(buf, t),
                Fb::Tables(ts) => {
                    let p = buf.len();
                    buf.extend((ts.len() as u32).to_le_bytes());
                    buf.extend(vec![0u8; 4 * ts.len()]);
                    for (i, t) in ts.iter().enumerate() {
                        let tp = put(buf, t);
                        patch(buf, p + 4 + 4 * i, tp);
                    }
                    p
                }
                _ => continue,
            };
            patch(buf, at, target);
        }
        pos
    }

    /// A stream holding just a schema message.
    fn schema_stream(schema: Vec<(usize, Fb)>) -> Vec<u8> {
        let mut meta = vec![0u8; 4];
        let root = put(&mut meta, &[(0, Fb::I16(4)), (1, Fb::U8(1)), (2, Fb::Table(schema))]);
        meta[..4].copy_from_slice(&(root as u32).to_le_bytes());
        meta.resize(meta.len().next_multiple_of(8), 0);
        let mut out = vec![0xff; 4];
        out.extend((meta.len() as u32).to_le_bytes());
        out.extend(meta);
        out.extend([0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0]);
        out
    }

    #[test]
    fn hand_made_schema_messages() {
        let int_field = || {
            vec![
                (0, Fb::Str("a")),
                (1, Fb::U8(1)),
                (2, Fb::U8(2)),
                (3, Fb::Table(vec![(0, Fb::I32(16)), (1, Fb::U8(0))])),
            ]
        };
        let ok = schema_stream(vec![(1, Fb::Tables(vec![int_field()]))]);
        let v = read("", &ok, MAX_ROWS, ROW_BUDGET).unwrap().unwrap();
        assert_eq!((v.kind, v.num_rows, v.batches), ("stream", 0, 0));
        assert_eq!(v.columns, [("a".to_string(), "UINT16".to_string(), true)]);

        let big = schema_stream(vec![(0, Fb::I16(1)), (1, Fb::Tables(vec![int_field()]))]);
        let v = read("", &big, MAX_ROWS, ROW_BUDGET).unwrap().unwrap();
        assert_eq!(v.notes, ["big-endian data: values not shown"]);

        // An integer can't be 12 bits wide: not taken for a stream.
        let mut bad = int_field();
        bad[3] = (3, Fb::Table(vec![(0, Fb::I32(12))]));
        assert!(sniff("", &schema_stream(vec![(1, Fb::Tables(vec![bad]))])).is_none());
        // No fields: only with a content type.
        let empty = schema_stream(vec![]);
        assert!(sniff("", &empty).is_none());
        assert!(matches!(
            sniff("application/vnd.apache.arrow.stream", &empty),
            Some(Ok(_))
        ));
    }

    #[test]
    fn detection() {
        let file = fixture("turbine-none.arrow");
        let stream = fixture("turbine-none.arrows");
        assert!(sniff("", &file).is_some());
        assert!(sniff("", &stream).is_some(), "stream found without a content type");
        assert!(sniff("application/vnd.apache.arrow.stream", &stream).is_some());
        // A file mislabelled as a stream is still read as a file.
        assert!(matches!(
            sniff("application/vnd.apache.arrow.stream", &file),
            Some(Ok(_))
        ));
        // Claimed but broken: recognised, with an error.
        assert!(matches!(
            sniff("application/vnd.apache.arrow.stream", b"nope"),
            Some(Err(_))
        ));
        for pq in ["turbine-snappy.parquet", "nested.parquet", "turbine-zstd.parquet"] {
            assert!(sniff("", &fixture(pq)).is_none(), "{pq}");
        }
        for text in [
            &b"{\"a\":1}"[..],
            b"",
            b"\xff\xff\xff\xff",
            b"\xff\xff\xff\xff\x08\0\0\0\0\0\0\0\0\0\0\0",
        ] {
            assert!(sniff("", text).is_none());
        }
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for i in 0..2000 {
            let mut b: Vec<u8> = (0..(rnd() % 600 + 8)).map(|_| rnd() as u8).collect();
            if i % 2 == 0 {
                // Continuation marker and a plausible length.
                b[..4].copy_from_slice(&[0xff; 4]);
                b[4..8].copy_from_slice(&((rnd() % 64) as u32 * 8).to_le_bytes());
            }
            assert!(sniff("", &b).is_none(), "{b:02x?}");
        }
    }

    #[test]
    fn view_through_decode_on_arrival() {
        use crate::theme::Theme;
        let b = fixture("turbine-lz4.arrows");
        let d = crate::pretty::decode_on_arrival(None, &b).unwrap();
        assert_eq!(d.format, "Arrow");
        assert!(d.has_table());
        let t: Vec<String> = d
            .render(&Theme::new(false))
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(t[0], "500 rows · 3 record batches · 11 columns · stream · lz4");
        assert_eq!(t[1], "ProviderName TurbineFastlog");
        assert_eq!(t[2], "Version 1.0.0");
        assert_eq!(t[4], "Schema");
        assert!(
            t.iter()
                .any(|l| l.contains("Timestamp") && l.contains("TIMESTAMP(ms, UTC)"))
        );
        let rows = t.iter().position(|l| l.starts_with("Rows · first ")).unwrap();
        assert!(t[rows].ends_with(" of 500"), "{}", t[rows]);
        assert!(t[rows + 3].contains("2026-10-08 10:20:00Z") && t[rows + 3].contains("[idle, check]"));
        assert!(!t.iter().any(|l| l.starts_with("note:")));
    }

    #[test]
    fn claimed_buffer_sizes_are_checked_before_allocating() {
        let budget = Cell::new(64 << 20);
        let claim = |n: i64, data: &[u8]| {
            let mut raw = n.to_le_bytes().to_vec();
            raw.extend_from_slice(data);
            raw
        };
        // A few bytes claiming 2^63, or over the per-buffer cap: refused, nothing allocated.
        for codec in [0, 1] {
            assert!(decompress(codec, &claim(i64::MAX, b"\x04\x22\x4d\x18"), &budget).is_err());
            assert!(decompress(codec, &claim((MAX_BUFFER + 1) as i64, b"x"), &budget).is_err());
        }
        // Within the cap but far more than 8 bytes of LZ4 can expand to.
        assert!(lz4_frame(b"\x04\x22\x4d\x18\x60\x40\x82\0", MAX_BUFFER).is_err());
        // The per-payload budget runs out across buffers.
        let small = Cell::new(10);
        assert!(decompress(1, &claim(11, b""), &small).is_err());
        assert_eq!(budget.get(), 64 << 20, "refused claims don't use the budget");
    }

    #[test]
    fn decimals() {
        let le = |x: i128| x.to_le_bytes();
        assert_eq!(decimal(&le(-5), 2), "-0.05");
        assert_eq!(decimal(&le(12345), 2), "123.45");
        assert_eq!(decimal(&le(0), 0), "0");
        assert_eq!(decimal(&le(7), -3), "7e3");
        assert_eq!(decimal(&(-12i32).to_le_bytes(), 1), "-1.2");
        let mut big = [0u8; 32];
        big[16] = 1; // 2^128
        assert_eq!(decimal(&big, 0), "340282366920938463463374607431768211456");
        let mut min = [0u8; 32];
        min[31] = 0x80; // -2^255
        assert!(
            decimal(&min, 76)
                .starts_with("-5.789604461865809771178549250434395392663499233282028201972879200395656481996")
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
        for name in TURBINE.into_iter().chain(["types.arrow"]) {
            let good = fixture(name);
            // Cut, flip and scramble bytes all over the payload; any result is fine, a panic is not.
            for _ in 0..300 {
                let mut f = good.clone();
                for _ in 0..(rnd() % 8 + 1) {
                    let i = (rnd() as usize) % f.len();
                    f[i] = rnd() as u8;
                }
                let _ = sniff("application/vnd.apache.arrow.stream", &f);
                let cut = 18 + (rnd() as usize) % (f.len() - 18);
                let mut t = f[..cut].to_vec();
                if name.ends_with(".arrow") {
                    t.extend_from_slice(&good[good.len() - 10..]);
                }
                let _ = sniff("", &t);
            }
        }
    }
}
