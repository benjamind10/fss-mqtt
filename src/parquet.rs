//! A small, read-only Parquet decoder: schema, file metadata and the first
//! rows of flat (non-repeated) columns, rendered as text.
//!
//! It runs on the full payload as it arrives, before the store keeps only the
//! first few KiB, and must never panic on malformed input: everything is
//! bounds-checked and errors become notes in the result.

use std::io::Read;

use crate::fmt::commas;
use crate::view::{Body, DocView, Fact};

/// Rows are decoded until the rendered cells reach about this many bytes.
const ROW_BUDGET: usize = 4096;
const MAX_ROWS: usize = 200;
const MAX_CELL: usize = 64;
/// Refuse to inflate pages larger than this.
const MAX_PAGE: usize = 32 << 20;

pub struct ParquetView {
    pub num_rows: i64,
    pub row_groups: usize,
    pub created_by: Option<String>,
    pub kv: Vec<(String, String)>,
    pub columns: Vec<Column>,
    /// First rows; `None` is a null cell.
    pub rows: Vec<Vec<Option<String>>>,
    pub notes: Vec<String>,
}

pub struct Column {
    pub name: String,
    pub ty: String,
    pub min: Option<String>,
    pub max: Option<String>,
    pub nulls: Option<i64>,
}

/// Decoder entry point (see `pretty::Decoder`): recognised by its magic bytes.
pub fn sniff(_ct: &str, b: &[u8]) -> Option<Result<DocView, String>> {
    is_parquet(b).then(|| decode(b).map(|v| v.to_view()))
}

impl ParquetView {
    pub fn to_view(&self) -> DocView {
        let mut d = DocView::new("Parquet");
        d.summary = vec![
            Fact::new(commas(self.num_rows.max(0) as u64), "rows"),
            Fact::new(self.row_groups.to_string(), "row groups"),
            Fact::new(self.columns.len().to_string(), "columns"),
        ];
        if let Some(c) = &self.created_by {
            d.summary.push(Fact::new("", c.clone()));
        }
        // Arrow's embedded schema is long and repeats the one below.
        d.meta = self
            .kv
            .iter()
            .filter(|(k, _)| !k.starts_with("ARROW:"))
            .cloned()
            .collect();
        let header = ["column", "type", "nulls", "min", "max"].map(String::from).to_vec();
        let rows = self
            .columns
            .iter()
            .map(|c| {
                vec![
                    Some(c.name.clone()),
                    Some(c.ty.clone()),
                    c.nulls.map(|n| n.to_string()),
                    c.min.clone(),
                    c.max.clone(),
                ]
            })
            .collect();
        d.section("Schema", "", Body::Table { header, rows });
        if !self.rows.is_empty() {
            d.section(
                "Rows",
                format!("· first {} of {}", self.rows.len(), commas(self.num_rows.max(0) as u64)),
                Body::Table {
                    header: self.columns.iter().map(|c| c.name.clone()).collect(),
                    rows: self.rows.clone(),
                },
            );
        }
        d.notes = self.notes.clone();
        d
    }
}

pub fn is_parquet(b: &[u8]) -> bool {
    b.len() >= 12 && b.starts_with(b"PAR1") && b.ends_with(b"PAR1")
}

pub fn decode(file: &[u8]) -> Result<ParquetView, String> {
    if !is_parquet(file) {
        return Err("not a Parquet file".into());
    }
    let n = file.len();
    let meta_len = u32::from_le_bytes(file[n - 8..n - 4].try_into().unwrap()) as usize;
    let meta_start = n
        .checked_sub(8 + meta_len)
        .filter(|&s| s >= 4)
        .ok_or("footer length out of range")?;
    let meta = Thrift::new(&file[meta_start..n - 8]).read_struct()?;

    let schema = meta.list(2);
    let leaves = flatten_schema(&schema)?;
    let row_groups = meta.list(4);
    let mut view = ParquetView {
        num_rows: meta.int(3).unwrap_or(0),
        row_groups: row_groups.len(),
        created_by: meta.string(6),
        kv: meta
            .list(5)
            .iter()
            .filter_map(|kv| Some((kv.string(1)?, kv.string(2).unwrap_or_default())))
            .collect(),
        columns: Vec::new(),
        rows: Vec::new(),
        notes: Vec::new(),
    };

    // Statistics: combine min/max/null counts over all row groups.
    for (ci, leaf) in leaves.iter().enumerate() {
        let mut col = Column {
            name: leaf.path.join("."),
            ty: leaf.type_name(),
            min: None,
            max: None,
            nulls: None,
        };
        let (mut min, mut max): (Option<Vec<u8>>, Option<Vec<u8>>) = (None, None);
        let mut nulls = Some(0i64);
        for rg in &row_groups {
            let Some(md) = rg.list(1).get(ci).and_then(|cc| cc.field(3)) else {
                continue;
            };
            let Some(stats) = md.field(12) else {
                nulls = None;
                continue;
            };
            nulls = nulls.zip(stats.int(3)).map(|(a, b)| a + b);
            let lo = stats.bytes(6).or_else(|| stats.bytes(2));
            let hi = stats.bytes(5).or_else(|| stats.bytes(1));
            if let Some(lo) = lo
                && min.as_ref().is_none_or(|m| leaf.less(&lo, m))
            {
                min = Some(lo);
            }
            if let Some(hi) = hi
                && max.as_ref().is_none_or(|m| leaf.less(m, &hi))
            {
                max = Some(hi);
            }
        }
        col.min = min.and_then(|v| leaf.render_stat(&v));
        col.max = max.and_then(|v| leaf.render_stat(&v));
        col.nulls = nulls.filter(|_| !row_groups.is_empty());
        view.columns.push(col);
    }

    // First rows, column by column, then cut to the byte budget.
    let want = (view.num_rows.max(0) as usize).min(MAX_ROWS);
    let mut cols: Vec<Vec<Option<String>>> = Vec::with_capacity(leaves.len());
    for (ci, leaf) in leaves.iter().enumerate() {
        let mut vals = Vec::new();
        if leaf.max_rep > 0 {
            view.notes.push(format!(
                "{}: nested (repeated) column, values not shown",
                leaf.path.join(".")
            ));
        } else {
            for rg in &row_groups {
                if vals.len() >= want {
                    break;
                }
                let Some(md) = rg.list(1).get(ci).and_then(|cc| cc.field(3)) else {
                    break;
                };
                if let Err(e) = read_chunk(file, &md, leaf, want, &mut vals) {
                    view.notes.push(format!("{}: {e}", leaf.path.join(".")));
                    break;
                }
            }
        }
        cols.push(vals);
    }
    let rows_have = cols
        .iter()
        .filter(|c| !c.is_empty())
        .map(|c| c.len())
        .min()
        .unwrap_or(0);
    let mut used = 0;
    for r in 0..rows_have {
        let row: Vec<Option<String>> = cols.iter().map(|c| c.get(r).cloned().unwrap_or(None)).collect();
        used += row.iter().map(|c| c.as_ref().map_or(4, |s| s.len()) + 1).sum::<usize>();
        if used > ROW_BUDGET && !view.rows.is_empty() {
            break;
        }
        view.rows.push(row);
    }
    Ok(view)
}

// ── Schema ─────────────────────────────────────────────────────────────────

struct Leaf {
    path: Vec<String>,
    physical: i64,
    type_length: usize,
    converted: Option<i64>,
    logical: Option<T>,
    scale: i64,
    precision: i64,
    max_def: u32,
    max_rep: u32,
}

fn flatten_schema(schema: &[T]) -> Result<Vec<Leaf>, String> {
    if schema.is_empty() {
        return Err("empty schema".into());
    }
    // Depth-first over the flat element list; element 0 is the root.
    let mut out = Vec::new();
    let mut i = 1;
    let root_children = schema[0].int(5).unwrap_or(0).max(0) as usize;
    for _ in 0..root_children {
        walk(schema, &mut i, &mut Vec::new(), 0, 0, &mut out)?;
    }
    Ok(out)
}

fn walk(
    schema: &[T],
    i: &mut usize,
    path: &mut Vec<String>,
    def: u32,
    rep: u32,
    out: &mut Vec<Leaf>,
) -> Result<(), String> {
    let el = schema.get(*i).ok_or("schema ends early")?;
    *i += 1;
    let (def, rep) = match el.int(3).unwrap_or(0) {
        1 => (def + 1, rep),
        2 => (def + 1, rep + 1),
        _ => (def, rep),
    };
    path.push(el.string(4).unwrap_or_default());
    let children = el.int(5).unwrap_or(0).max(0) as usize;
    if children == 0 {
        out.push(Leaf {
            path: path.clone(),
            physical: el.int(1).unwrap_or(-1),
            type_length: el.int(2).unwrap_or(0).max(0) as usize,
            converted: el.int(6),
            logical: el.field(10),
            scale: el.int(7).unwrap_or(0),
            precision: el.int(8).unwrap_or(0),
            max_def: def,
            max_rep: rep,
        });
    } else {
        if out.len() > 10_000 || path.len() > 64 {
            return Err("schema too large".into());
        }
        for _ in 0..children {
            walk(schema, i, path, def, rep, out)?;
        }
    }
    path.pop();
    Ok(())
}

enum Kind {
    Str,
    Date,
    Time(u32),            // digits after seconds: 3, 6
    Timestamp(u32, bool), // 3/6/9 digits, UTC
    Decimal(i64),         // scale
    Uuid,
    Unsigned,
    Plain,
}

impl Leaf {
    fn kind(&self) -> Kind {
        if let Some(l) = &self.logical
            && let Some((id, v)) = l.fields().first()
        {
            {
                return match id {
                    1 | 4 | 12 => Kind::Str,
                    5 => Kind::Decimal(v.int(1).unwrap_or(self.scale)),
                    6 => Kind::Date,
                    7 => Kind::Time(unit_digits(v)),
                    8 => Kind::Timestamp(unit_digits(v), v.bool(1).unwrap_or(false)),
                    10 if v.bool(2) == Some(false) => Kind::Unsigned,
                    14 => Kind::Uuid,
                    _ => Kind::Plain,
                };
            }
        }
        match self.converted {
            Some(0) | Some(4) | Some(19) => Kind::Str,
            Some(5) => Kind::Decimal(self.scale),
            Some(6) => Kind::Date,
            Some(7) => Kind::Time(3),
            Some(8) => Kind::Time(6),
            Some(9) => Kind::Timestamp(3, true),
            Some(10) => Kind::Timestamp(6, true),
            Some(11..=14) => Kind::Unsigned,
            _ => Kind::Plain,
        }
    }

    fn type_name(&self) -> String {
        let phys = match self.physical {
            0 => "BOOLEAN",
            1 => "INT32",
            2 => "INT64",
            3 => "INT96",
            4 => "FLOAT",
            5 => "DOUBLE",
            6 => "BYTE_ARRAY",
            7 => "FIXED",
            _ => "?",
        };
        let unit = |d| match d {
            3 => "ms",
            6 => "us",
            _ => "ns",
        };
        match self.kind() {
            Kind::Str => "STRING".into(),
            Kind::Date => "DATE".into(),
            Kind::Time(d) => format!("TIME({})", unit(d)),
            Kind::Timestamp(d, utc) => format!("TIMESTAMP({}{})", unit(d), if utc { ", UTC" } else { "" }),
            Kind::Decimal(s) => format!("DECIMAL({},{s})", self.precision.max(self.logical_precision())),
            Kind::Uuid => "UUID".into(),
            Kind::Unsigned => format!("U{phys}"),
            Kind::Plain if self.physical == 3 => "TIMESTAMP(INT96)".into(),
            Kind::Plain => phys.into(),
        }
    }

    fn logical_precision(&self) -> i64 {
        self.logical
            .as_ref()
            .and_then(|l| l.field(5))
            .and_then(|d| d.int(2))
            .unwrap_or(0)
    }

    /// Compares two plain-encoded statistic values.
    fn less(&self, a: &[u8], b: &[u8]) -> bool {
        match (self.physical, a.len(), b.len()) {
            (1, 4, 4) => i32::from_le_bytes(a.try_into().unwrap()) < i32::from_le_bytes(b.try_into().unwrap()),
            (2, 8, 8) => i64::from_le_bytes(a.try_into().unwrap()) < i64::from_le_bytes(b.try_into().unwrap()),
            (4, 4, 4) => f32::from_le_bytes(a.try_into().unwrap()) < f32::from_le_bytes(b.try_into().unwrap()),
            (5, 8, 8) => f64::from_le_bytes(a.try_into().unwrap()) < f64::from_le_bytes(b.try_into().unwrap()),
            _ => a < b,
        }
    }

    fn render_stat(&self, v: &[u8]) -> Option<String> {
        let val = match self.physical {
            0 => Val::Bool(*v.first()? != 0),
            1 => Val::I32(i32::from_le_bytes(v.get(..4)?.try_into().ok()?)),
            2 => Val::I64(i64::from_le_bytes(v.get(..8)?.try_into().ok()?)),
            4 => Val::F32(f32::from_le_bytes(v.get(..4)?.try_into().ok()?)),
            5 => Val::F64(f64::from_le_bytes(v.get(..8)?.try_into().ok()?)),
            3 => Val::Int96(v.get(..12)?.try_into().ok()?),
            _ => Val::Bytes(v.to_vec()),
        };
        Some(self.render(&val))
    }

    fn render(&self, v: &Val) -> String {
        let s = match (self.kind(), v) {
            (_, Val::Bool(b)) => b.to_string(),
            (Kind::Date, Val::I32(d)) => date(*d as i64),
            (Kind::Time(digits), Val::I32(t)) => time_of_day(*t as i64, digits),
            (Kind::Time(digits), Val::I64(t)) => time_of_day(*t, digits),
            (Kind::Timestamp(digits, utc), Val::I64(t)) => timestamp(*t, digits, utc),
            (Kind::Decimal(scale), Val::I32(x)) => decimal(*x as i128, scale),
            (Kind::Decimal(scale), Val::I64(x)) => decimal(*x as i128, scale),
            (Kind::Decimal(scale), Val::Bytes(b)) => match be_i128(b) {
                Some(x) => decimal(x, scale),
                None => hex(b),
            },
            (Kind::Unsigned, Val::I32(x)) => (*x as u32).to_string(),
            (Kind::Unsigned, Val::I64(x)) => (*x as u64).to_string(),
            (Kind::Uuid, Val::Bytes(b)) if b.len() == 16 => uuid(b),
            (_, Val::Int96(b)) => {
                let nanos = i64::from_le_bytes(b[..8].try_into().unwrap());
                let julian = i32::from_le_bytes(b[8..].try_into().unwrap()) as i64;
                let days = julian - 2_440_588;
                timestamp(days * 86_400_000_000_000 + nanos, 9, true)
            }
            (_, Val::I32(x)) => x.to_string(),
            (_, Val::I64(x)) => x.to_string(),
            (_, Val::F32(x)) => x.to_string(),
            (_, Val::F64(x)) => x.to_string(),
            (Kind::Str, Val::Bytes(b)) => String::from_utf8_lossy(b).into_owned(),
            (_, Val::Bytes(b)) => match std::str::from_utf8(b) {
                Ok(s) if !s.chars().any(|c| c.is_control()) => s.to_string(),
                _ => hex(b),
            },
        };
        clip(s)
    }
}

fn unit_digits(t: &T) -> u32 {
    match t.field(2).and_then(|u| u.fields().first().map(|f| f.0)) {
        Some(1) => 3,
        Some(2) => 6,
        _ => 9,
    }
}

pub fn clip(s: String) -> String {
    if s.chars().count() <= MAX_CELL {
        return s;
    }
    let mut c: String = s.chars().take(MAX_CELL - 1).collect();
    c.push('…');
    c
}

// ── Pages ──────────────────────────────────────────────────────────────────

enum Val {
    Bool(bool),
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    Int96([u8; 12]),
    Bytes(Vec<u8>),
}

/// Appends up to `want` rendered values of one column chunk to `out`.
fn read_chunk(file: &[u8], md: &T, leaf: &Leaf, want: usize, out: &mut Vec<Option<String>>) -> Result<(), String> {
    let codec = md.int(4).unwrap_or(0);
    let start = md
        .int(11)
        .filter(|&o| o > 0)
        .or_else(|| md.int(9))
        .ok_or("no data page offset")? as usize;
    let end = start
        .saturating_add(md.int(7).unwrap_or(0).max(0) as usize)
        .min(file.len());
    let mut pos = start;
    let mut dict: Vec<Val> = Vec::new();
    let def_width = bit_width(leaf.max_def);

    while pos < end && out.len() < want {
        let mut th = Thrift::new(file.get(pos..end).ok_or("page offset out of range")?);
        let header = th.read_struct()?;
        pos += th.pos;
        let size = header.int(3).unwrap_or(-1);
        if size < 0 {
            return Err("bad page size".into());
        }
        let page_end = pos.checked_add(size as usize).ok_or("bad page size")?;
        let body = file.get(pos..page_end).ok_or("page runs past the column chunk")?;
        pos = page_end;
        let raw_len = header.int(2).unwrap_or(0).max(0) as usize;

        match header.int(1) {
            Some(2) => {
                // Dictionary page: PLAIN-encoded values.
                let dh = header.field(7).ok_or("dictionary page without header")?;
                let n = dh.int(1).unwrap_or(0).max(0) as usize;
                let data = decompress(codec, body, raw_len)?;
                dict = plain_values(&data, leaf, n.min(1 << 20))?.0;
            }
            Some(0) => {
                let dh = header.field(5).ok_or("data page without header")?;
                let n = dh.int(1).unwrap_or(0).max(0) as usize;
                let data = decompress(codec, body, raw_len)?;
                let mut p = 0;
                let defs = if leaf.max_def > 0 {
                    let len = u32_le(&data, p)? as usize;
                    p += 4;
                    let lv = rle_hybrid(data.get(p..p + len).ok_or("bad level length")?, def_width, n)?;
                    p += len;
                    lv
                } else {
                    vec![0; n]
                };
                let enc = dh.int(2).unwrap_or(0);
                decode_values(&data[p.min(data.len())..], enc, &defs, leaf, &dict, want, out)?;
            }
            Some(3) => {
                let dh = header.field(8).ok_or("data page v2 without header")?;
                let n = dh.int(1).unwrap_or(0).max(0) as usize;
                let def_len = dh.int(5).unwrap_or(0).max(0) as usize;
                let rep_len = dh.int(6).unwrap_or(0).max(0) as usize;
                let levels = body.get(..rep_len + def_len).ok_or("bad level lengths")?;
                let defs = if leaf.max_def > 0 {
                    rle_hybrid(&levels[rep_len..], def_width, n)?
                } else {
                    vec![0; n]
                };
                let values = &body[rep_len + def_len..];
                let compressed = dh.bool(7).unwrap_or(true);
                let data = if compressed {
                    decompress(codec, values, raw_len.saturating_sub(rep_len + def_len))?
                } else {
                    values.to_vec()
                };
                let enc = dh.int(4).unwrap_or(0);
                decode_values(&data, enc, &defs, leaf, &dict, want, out)?;
            }
            _ => {} // index pages etc.
        }
    }
    Ok(())
}

fn decode_values(
    data: &[u8],
    enc: i64,
    defs: &[u32],
    leaf: &Leaf,
    dict: &[Val],
    want: usize,
    out: &mut Vec<Option<String>>,
) -> Result<(), String> {
    let present = defs.iter().filter(|&&d| d == leaf.max_def).count();
    let take_rows = (want - out.len()).min(defs.len());
    // Only decode values for the rows we keep.
    let need = defs[..take_rows].iter().filter(|&&d| d == leaf.max_def).count();
    let render = |v: &Val| leaf.render(v);
    let values: Vec<String> = match enc {
        0 => plain_values(data, leaf, need)?.0.iter().map(render).collect(),
        3 if leaf.physical == 0 => {
            // RLE-encoded booleans: 4-byte length, then the hybrid with width 1.
            let len = u32_le(data, 0)? as usize;
            let bits = rle_hybrid(data.get(4..4 + len).ok_or("values end early")?, 1, need)?;
            bits.iter().map(|&b| render(&Val::Bool(b == 1))).collect()
        }
        2 | 8 => {
            let width = *data.first().ok_or("empty dictionary indices")? as u32;
            let idx = rle_hybrid(&data[1..], width, need.min(present))?;
            idx.iter()
                .map(|&i| dict.get(i as usize).map(render).ok_or("dictionary index out of range"))
                .collect::<Result<_, _>>()?
        }
        other => return Err(format!("encoding {} not supported", encoding_name(other))),
    };
    let mut vi = values.into_iter();
    for &d in &defs[..take_rows] {
        out.push(if d == leaf.max_def { vi.next() } else { None });
    }
    Ok(())
}

fn encoding_name(e: i64) -> &'static str {
    match e {
        3 => "RLE",
        4 => "BIT_PACKED",
        5 => "DELTA_BINARY_PACKED",
        6 => "DELTA_LENGTH_BYTE_ARRAY",
        7 => "DELTA_BYTE_ARRAY",
        9 => "BYTE_STREAM_SPLIT",
        _ => "unknown",
    }
}

/// Decodes up to `n` PLAIN values; returns them and the bytes used.
fn plain_values(data: &[u8], leaf: &Leaf, n: usize) -> Result<(Vec<Val>, usize), String> {
    let mut out = Vec::with_capacity(n.min(4096));
    let mut p = 0;
    let short = "values end early";
    for i in 0..n {
        let v = match leaf.physical {
            0 => {
                let byte = *data.get(i / 8).ok_or(short)?;
                p = i / 8 + 1;
                Val::Bool(byte >> (i % 8) & 1 == 1)
            }
            1 => Val::I32(u32_le(data, p).inspect(|_| p += 4)? as i32),
            2 => Val::I64(i64::from_le_bytes(take::<8>(data, &mut p)?)),
            3 => Val::Int96(take::<12>(data, &mut p)?),
            4 => Val::F32(f32::from_le_bytes(take::<4>(data, &mut p)?)),
            5 => Val::F64(f64::from_le_bytes(take::<8>(data, &mut p)?)),
            6 => {
                let len = u32_le(data, p)? as usize;
                p += 4;
                let b = data.get(p..p + len).ok_or(short)?.to_vec();
                p += len;
                Val::Bytes(b)
            }
            7 => {
                let b = data.get(p..p + leaf.type_length).ok_or(short)?.to_vec();
                p += leaf.type_length;
                Val::Bytes(b)
            }
            _ => return Err("unknown physical type".into()),
        };
        out.push(v);
    }
    Ok((out, p))
}

fn take<const N: usize>(data: &[u8], p: &mut usize) -> Result<[u8; N], String> {
    let b: [u8; N] = data.get(*p..*p + N).ok_or("values end early")?.try_into().unwrap();
    *p += N;
    Ok(b)
}

fn u32_le(data: &[u8], p: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(take::<4>(data, &mut { p })?))
}

fn bit_width(max: u32) -> u32 {
    32 - max.leading_zeros()
}

/// The RLE / bit-packing hybrid used for levels and dictionary indices.
fn rle_hybrid(data: &[u8], width: u32, n: usize) -> Result<Vec<u32>, String> {
    if width > 32 {
        return Err("bit width out of range".into());
    }
    let mut out = Vec::with_capacity(n.min(1 << 16));
    let mut p = 0;
    let bytes = width.div_ceil(8) as usize;
    while out.len() < n {
        let mut header: u64 = 0;
        let mut shift = 0;
        loop {
            let b = *data.get(p).ok_or("levels end early")?;
            p += 1;
            header |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift > 35 {
                return Err("bad run header".into());
            }
        }
        if header & 1 == 1 {
            // Bit-packed groups of 8.
            let count = (header >> 1) as usize * 8;
            let total_bits = count * width as usize;
            let chunk = data
                .get(p..p + total_bits.div_ceil(8))
                .ok_or("bit-packed run ends early")?;
            p += chunk.len();
            for i in 0..count {
                if out.len() == n {
                    break;
                }
                let mut v: u32 = 0;
                for b in 0..width as usize {
                    let bit = i * width as usize + b;
                    if chunk[bit / 8] >> (bit % 8) & 1 == 1 {
                        v |= 1 << b;
                    }
                }
                out.push(v);
            }
        } else {
            let count = (header >> 1) as usize;
            let raw = data.get(p..p + bytes).ok_or("RLE run ends early")?;
            p += bytes;
            let mut v: u32 = 0;
            for (i, b) in raw.iter().enumerate() {
                v |= (*b as u32) << (8 * i);
            }
            let k = count.min(n - out.len());
            out.extend(std::iter::repeat_n(v, k));
            if count == 0 {
                return Err("empty RLE run".into());
            }
        }
    }
    Ok(out)
}

fn decompress(codec: i64, data: &[u8], raw_len: usize) -> Result<Vec<u8>, String> {
    if raw_len > MAX_PAGE {
        return Err("page too large".into());
    }
    let out = match codec {
        0 => data.to_vec(),
        1 => snap::raw::Decoder::new()
            .decompress_vec(data)
            .map_err(|e| format!("snappy: {e}"))?,
        2 => gunzip(data)?,
        6 => {
            let mut d = ruzstd::decoding::StreamingDecoder::new(data).map_err(|e| format!("zstd: {e}"))?;
            let mut out = Vec::with_capacity(raw_len);
            (&mut d)
                .take(MAX_PAGE as u64)
                .read_to_end(&mut out)
                .map_err(|e| format!("zstd: {e}"))?;
            out
        }
        7 => lz4_block(data, raw_len)?,
        5 => {
            // Hadoop-framed LZ4: [u32 BE raw len][u32 BE compressed len][block]...
            let mut out = Vec::with_capacity(raw_len);
            let mut p = 0;
            while p + 8 <= data.len() && out.len() < raw_len {
                let raw = (u32::from_be_bytes(data[p..p + 4].try_into().unwrap()) as usize).min(MAX_PAGE);
                let len = u32::from_be_bytes(data[p + 4..p + 8].try_into().unwrap()) as usize;
                let block = data.get(p + 8..p + 8 + len).ok_or("lz4 frame ends early")?;
                out.extend(lz4_block(block, raw)?);
                p += 8 + len;
            }
            out
        }
        3 => return Err("LZO compression not supported".into()),
        4 => return Err("Brotli compression not supported".into()),
        c => return Err(format!("compression codec {c} not supported")),
    };
    Ok(out)
}

fn lz4_block(data: &[u8], raw_len: usize) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; raw_len];
    let n = lz4_flex::decompress_into(data, &mut out).map_err(|e| format!("lz4: {e}"))?;
    out.truncate(n);
    Ok(out)
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    // RFC 1952 header, then raw deflate.
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return Err("gzip: bad header".into());
    }
    let flags = data[3];
    let mut p = 10;
    if flags & 4 != 0 {
        let xlen = u16::from_le_bytes([data[p], *data.get(p + 1).ok_or("gzip: bad header")?]) as usize;
        p += 2 + xlen;
    }
    for bit in [8u8, 16] {
        if flags & bit != 0 {
            while *data.get(p).ok_or("gzip: bad header")? != 0 {
                p += 1;
            }
            p += 1;
        }
    }
    if flags & 2 != 0 {
        p += 2;
    }
    let body = data.get(p..).ok_or("gzip: bad header")?;
    miniz_oxide::inflate::decompress_to_vec_with_limit(body, MAX_PAGE).map_err(|e| format!("gzip: {e:?}"))
}

// ── Value formatting ───────────────────────────────────────────────────────

fn civil(days: i64) -> (i64, u32, u32) {
    // Howard Hinnant's days-from-civil, inverted.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

pub fn date(days: i64) -> String {
    let (y, m, d) = civil(days);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn time_of_day(t: i64, digits: u32) -> String {
    let per_sec = 10i64.pow(digits);
    let secs = t.div_euclid(per_sec);
    let frac = t.rem_euclid(per_sec);
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if frac == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{frac:0w$}", w = digits as usize)
    }
}

pub fn timestamp(t: i64, digits: u32, utc: bool) -> String {
    let per_day = 86_400 * 10i64.pow(digits);
    let days = t.div_euclid(per_day);
    let rest = t.rem_euclid(per_day);
    format!(
        "{} {}{}",
        date(days),
        time_of_day(rest, digits),
        if utc { "Z" } else { "" }
    )
}

pub fn decimal(x: i128, scale: i64) -> String {
    if scale <= 0 {
        return x.to_string();
    }
    let neg = x < 0;
    let digits = x.unsigned_abs().to_string();
    let scale = scale as usize;
    let padded = format!("{digits:0>w$}", w = scale + 1);
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{}{int}.{frac}", if neg { "-" } else { "" })
}

pub fn be_i128(b: &[u8]) -> Option<i128> {
    if b.is_empty() || b.len() > 16 {
        return None;
    }
    let fill = if b[0] & 0x80 != 0 { 0xff } else { 0 };
    let mut buf = [fill; 16];
    buf[16 - b.len()..].copy_from_slice(b);
    Some(i128::from_be_bytes(buf))
}

pub fn uuid(b: &[u8]) -> String {
    let h = hex(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ── Thrift compact protocol (just enough for Parquet metadata) ─────────────

#[derive(Clone, Debug)]
enum T {
    Bool(bool),
    Int(i64),
    Double,
    Bin(Vec<u8>),
    List(Vec<T>),
    Struct(Vec<(i16, T)>),
}

impl T {
    fn fields(&self) -> Vec<(i16, T)> {
        match self {
            T::Struct(f) => f.clone(),
            _ => vec![],
        }
    }
    fn field(&self, id: i16) -> Option<T> {
        match self {
            T::Struct(f) => f.iter().find(|(i, _)| *i == id).map(|(_, v)| v.clone()),
            _ => None,
        }
    }
    fn int(&self, id: i16) -> Option<i64> {
        match self.field(id)? {
            T::Int(v) => Some(v),
            _ => None,
        }
    }
    fn bool(&self, id: i16) -> Option<bool> {
        match self.field(id)? {
            T::Bool(v) => Some(v),
            _ => None,
        }
    }
    fn bytes(&self, id: i16) -> Option<Vec<u8>> {
        match self.field(id)? {
            T::Bin(v) => Some(v),
            _ => None,
        }
    }
    fn string(&self, id: i16) -> Option<String> {
        self.bytes(id).map(|b| String::from_utf8_lossy(&b).into_owned())
    }
    fn list(&self, id: i16) -> Vec<T> {
        match self.field(id) {
            Some(T::List(v)) => v,
            _ => vec![],
        }
    }
}

struct Thrift<'a> {
    b: &'a [u8],
    pos: usize,
    depth: u32,
}

impl<'a> Thrift<'a> {
    fn new(b: &'a [u8]) -> Self {
        Thrift { b, pos: 0, depth: 0 }
    }

    fn byte(&mut self) -> Result<u8, String> {
        let v = *self.b.get(self.pos).ok_or("metadata ends early")?;
        self.pos += 1;
        Ok(v)
    }

    fn varint(&mut self) -> Result<u64, String> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("bad varint".into())
    }

    fn zigzag(&mut self) -> Result<i64, String> {
        let v = self.varint()?;
        Ok((v >> 1) as i64 ^ -((v & 1) as i64))
    }

    fn value(&mut self, ty: u8) -> Result<T, String> {
        Ok(match ty {
            1 => T::Bool(true),
            2 => T::Bool(false),
            3 => T::Int(self.byte()? as i8 as i64),
            4..=6 => T::Int(self.zigzag()?),
            7 => {
                self.pos += 8;
                if self.pos > self.b.len() {
                    return Err("metadata ends early".into());
                }
                T::Double
            }
            8 => {
                let len = self.varint()? as usize;
                let v = self
                    .b
                    .get(self.pos..self.pos + len)
                    .ok_or("metadata ends early")?
                    .to_vec();
                self.pos += len;
                T::Bin(v)
            }
            9 | 10 => {
                self.depth += 1;
                if self.depth > 32 {
                    return Err("metadata nested too deeply".into());
                }
                let h = self.byte()?;
                let mut n = (h >> 4) as usize;
                if n == 15 {
                    n = self.varint()? as usize;
                }
                let et = h & 0x0f;
                if n > self.b.len() {
                    return Err("list too long".into());
                }
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(if et == 1 || et == 2 {
                        T::Bool(self.byte()? == 1)
                    } else {
                        self.value(et)?
                    });
                }
                self.depth -= 1;
                T::List(items)
            }
            11 => {
                let n = self.varint()? as usize;
                if n > self.b.len() - self.pos {
                    return Err("map too long".into());
                }
                if n > 0 {
                    let kv = self.byte()?;
                    for _ in 0..n {
                        self.value(kv >> 4)?;
                        self.value(kv & 0x0f)?;
                    }
                }
                T::List(vec![])
            }
            12 => self.read_struct()?,
            t => return Err(format!("bad metadata type {t}")),
        })
    }

    fn read_struct(&mut self) -> Result<T, String> {
        self.depth += 1;
        if self.depth > 32 {
            return Err("metadata nested too deeply".into());
        }
        let mut fields = Vec::new();
        let mut last: i16 = 0;
        loop {
            let h = self.byte()?;
            if h == 0 {
                break;
            }
            let delta = (h >> 4) as i16;
            let id = if delta == 0 {
                self.zigzag()? as i16
            } else {
                last.wrapping_add(delta)
            };
            last = id;
            fields.push((id, self.value(h & 0x0f)?));
        }
        self.depth -= 1;
        Ok(T::Struct(fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn decodes_every_codec() {
        for name in [
            "turbine-snappy.parquet",
            "turbine-gzip.parquet",
            "turbine-zstd.parquet",
            "turbine-lz4.parquet",
            "turbine-none.parquet",
            "turbine-plain-v2.parquet",
        ] {
            let v = decode(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(v.notes.is_empty(), "{name}: {:?}", v.notes);
            assert_eq!((v.num_rows, v.row_groups), (500, 3), "{name}");
            let names: Vec<_> = v.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(
                names,
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
                    "Price"
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
                    "0.00"
                ],
                "{name}"
            );
            assert_eq!(v.rows[7][3], None, "{name}: WindSpeed row 7 is null");
            assert_eq!(v.rows[1][9].as_deref(), Some("0.01"), "{name}");
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
    fn schema_types_and_stats() {
        let v = decode(&fixture("turbine-snappy.parquet")).unwrap();
        let col = |n: &str| v.columns.iter().find(|c| c.name == n).unwrap();
        assert_eq!(col("Timestamp").ty, "TIMESTAMP(ms, UTC)");
        assert_eq!(col("TurbineId").ty, "STRING");
        assert_eq!(col("Price").ty, "DECIMAL(9,2)");
        assert_eq!(col("Day").ty, "DATE");
        assert_eq!(col("Rpm").min.as_deref(), Some("9"));
        assert_eq!(col("Rpm").max.as_deref(), Some("15"));
        assert_eq!(col("WindSpeed").nulls, Some(10));
        assert_eq!(col("Timestamp").max.as_deref(), Some("2026-10-08 10:28:19Z"));
        assert_eq!(
            v.kv.iter().find(|(k, _)| k == "ProviderName").map(|(_, v)| v.as_str()),
            Some("TurbineFastlog")
        );
        assert!(v.created_by.as_deref().unwrap_or("").starts_with("parquet-cpp-arrow"));
    }

    #[test]
    fn nested_columns_show_schema_and_note() {
        let v = decode(&fixture("nested.parquet")).unwrap();
        assert_eq!(v.columns.len(), 2);
        assert_eq!(v.columns[1].name, "tags.list.element");
        assert!(v.notes.iter().any(|n| n.contains("nested")), "{:?}", v.notes);
        assert_eq!(v.rows[0][0].as_deref(), Some("1"));
    }

    #[test]
    fn never_panics_on_damaged_input() {
        let good = fixture("turbine-snappy.parquet");
        assert!(decode(b"PAR1").is_err());
        assert!(decode(b"PAR1xxxxxxxxPAR1").is_err());
        // Cut, flip and scramble bytes all over the file; any result is fine, a panic is not.
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..800 {
            let mut f = good.clone();
            for _ in 0..(rnd() % 8 + 1) {
                let i = (rnd() as usize) % f.len();
                f[i] = rnd() as u8;
            }
            let _ = decode(&f);
            let cut = 12 + (rnd() as usize) % (f.len() - 12);
            let mut t = f[..cut].to_vec();
            t.extend_from_slice(&good[good.len() - 8..]);
            let _ = decode(&t);
        }
    }

    #[test]
    fn calendar() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(20_734), "2026-10-08");
        assert_eq!(date(-1), "1969-12-31");
        assert_eq!(decimal(-5, 2), "-0.05");
        assert_eq!(decimal(12345, 2), "123.45");
    }
}
