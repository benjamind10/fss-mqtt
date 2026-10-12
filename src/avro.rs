//! A small, read-only Apache Avro decoder: object container files are read
//! with the writer schema in their header (record count, codec, schema and the
//! first records as a table). Avro without its schema (schema-registry framing,
//! single-object encoding, bare records) is recognised and explained instead.
//!
//! It runs on the full payload as it arrives and must never panic on malformed
//! input: lengths and counts are never trusted, recursion and work are capped,
//! and errors become notes in the result.

use std::borrow::Cow;
use std::io::Read;

use crate::fmt::commas;
use crate::parquet::{be_i128, clip, date, decimal, hex, time_of_day, timestamp, uuid};
use crate::view::{Body, DocView, Fact};

/// Records are decoded until the rendered cells reach about this many bytes.
const ROW_BUDGET: usize = 4096;
const MAX_ROWS: usize = 200;
/// Bytes of text collected per cell before it's clipped for display.
const CELL_BYTES: usize = 264;
/// Refuse to inflate blocks larger than this.
const MAX_BLOCK: usize = 32 << 20;
/// Nesting of schemas, values and JSON.
const MAX_DEPTH: usize = 32;
const MAX_TYPES: usize = 10_000;
/// Values decoded per record before giving up on it.
const MAX_WORK: usize = 100_000;

const MAGIC: &[u8] = b"Obj\x01";

/// Decoder entry point (see `pretty::Decoder`): container files by their magic
/// bytes; schema-less Avro by its framing or an `avro` content type.
pub fn sniff(ct: &str, b: &[u8]) -> Option<Result<DocView, String>> {
    if b.starts_with(MAGIC) {
        return Some(decode(b));
    }
    let avro_ct = ct.contains("avro");
    if avro_ct && ct.contains("json") {
        return None; // Avro's JSON encoding is shown as JSON
    }
    let mut d = DocView::new("Avro");
    // Two magic bytes are too weak to claim an unlabelled payload.
    if b.len() >= 10 && b.starts_with(b"\xc3\x01") && avro_ct {
        d.summary = vec![Fact::new(commas(b.len() as u64 - 10), "bytes of data")];
        d.notes.push(format!(
            "Avro single-object encoding, schema fingerprint {}: the schema isn't in the payload",
            hex(&b[2..10])
        ));
    } else if !avro_ct {
        return None;
    } else if b.len() >= 5 && b[0] == 0 {
        let id = u32::from_be_bytes([b[1], b[2], b[3], b[4]]);
        d.summary = vec![
            Fact::new(id.to_string(), "schema id"),
            Fact::new(commas(b.len() as u64 - 5), "bytes of data"),
        ];
        d.notes.push(format!(
            "Confluent schema-registry framing, schema id {id}: the schema isn't in the payload"
        ));
    } else {
        d.summary = vec![Fact::new(commas(b.len() as u64), "bytes")];
        d.notes
            .push("binary Avro without a container file header: decoding it needs the writer schema".into());
    }
    Some(Ok(d))
}

struct Block<'a> {
    count: i64,
    data: &'a [u8],
}

pub fn decode(file: &[u8]) -> Result<DocView, String> {
    let mut r = Reader::new(file.get(MAGIC.len()..).ok_or("not an Avro file")?);
    let meta = read_meta(&mut r)?;
    let sync = r.take(16).map_err(|_| "file ends in the header")?;
    let get = |k: &str| meta.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_slice());
    let codec = get("avro.codec").map_or(Cow::Borrowed("null"), String::from_utf8_lossy);
    let mut d = DocView::new("Avro");
    d.meta = meta
        .iter()
        .filter(|(k, _)| k != "avro.schema" && k != "avro.codec")
        .map(|(k, v)| (k.clone(), String::from_utf8_lossy(v).into_owned()))
        .collect();

    // Count records from the block headers; records are decoded further down.
    let mut blocks = Vec::new();
    while r.p < r.b.len() {
        let n = blocks.len() + 1;
        let (Ok(count), Ok(size)) = (r.long(), r.len()) else {
            d.notes.push(format!("the file ends inside block {n}"));
            break;
        };
        if count < 0 {
            d.notes.push(format!("block {n} has a negative record count"));
            break;
        }
        let data = r.take(size).unwrap_or_default();
        if r.take(16).ok() != Some(sync) {
            d.notes
                .push(format!("block {n} isn't followed by the file's sync marker"));
            break;
        }
        blocks.push(Block { count, data });
    }
    let total = blocks.iter().fold(0u64, |a, b| a.saturating_add(b.count as u64));

    let schema = match get("avro.schema") {
        None => Err("the header has no schema".to_string()),
        Some(s) => Json::parse(s).and_then(|j| Schema::parse(&j)),
    };
    d.summary = vec![
        Fact::new(commas(total), if total == 1 { "record" } else { "records" }),
        Fact::new(
            blocks.len().to_string(),
            if blocks.len() == 1 { "block" } else { "blocks" },
        ),
        Fact::new(
            "",
            if codec == "null" {
                "uncompressed".into()
            } else {
                codec.to_string()
            },
        ),
    ];
    let schema = match schema {
        Ok(s) => s,
        Err(e) => {
            d.notes.push(format!("could not read the schema: {e}"));
            return Ok(d);
        }
    };
    if let Some(name) = schema.name(schema.root) {
        d.summary.push(Fact::new(name, ""));
    }
    schema.describe(&mut d);

    if !matches!(&*codec, "null" | "deflate" | "snappy" | "zstandard") {
        d.notes
            .push(format!("{codec} compression isn't supported: records not shown"));
        return Ok(d);
    }
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut used = 0;
    'blocks: for (i, blk) in blocks.iter().enumerate() {
        let data = match inflate(&codec, blk.data) {
            Ok(data) => data,
            Err(e) => {
                d.notes.push(format!("block {}: {e}", i + 1));
                break;
            }
        };
        let mut r = Reader::new(&data);
        for _ in 0..blk.count {
            let row = match schema.row(&mut r) {
                Ok(row) => row,
                Err(e) => {
                    d.notes.push(format!("record {}: {e}", rows.len() + 1));
                    break 'blocks;
                }
            };
            let len: usize = row.iter().map(|c| c.as_ref().map_or(4, |s| s.len()) + 1).sum();
            if !rows.is_empty() && used + len > ROW_BUDGET {
                break 'blocks;
            }
            used += len;
            rows.push(row);
            if rows.len() >= MAX_ROWS {
                break 'blocks;
            }
        }
    }
    if !rows.is_empty() {
        let header = match &schema.types[schema.root] {
            Ty::Record { fields, .. } => fields.iter().map(|f| f.name.clone()).collect(),
            _ => vec!["value".to_string()],
        };
        d.section(
            "Rows",
            format!("· first {} of {}", rows.len(), commas(total)),
            Body::Table { header, rows },
        );
    }
    Ok(d)
}

/// The header's metadata map: string keys, bytes values.
fn read_meta(r: &mut Reader) -> Result<Vec<(String, Vec<u8>)>, String> {
    let bad = |_| "the header is damaged".to_string();
    let mut meta = Vec::new();
    loop {
        let n = r.long().map_err(bad)?;
        if n == 0 {
            return Ok(meta);
        }
        if n < 0 {
            r.long().map_err(bad)?; // block size in bytes
        }
        for _ in 0..n.unsigned_abs() {
            let k = r.bytes().map_err(bad)?;
            let v = r.bytes().map_err(bad)?;
            meta.push((String::from_utf8_lossy(k).into_owned(), v.to_vec()));
        }
    }
}

fn inflate<'a>(codec: &str, data: &'a [u8]) -> Result<Cow<'a, [u8]>, String> {
    Ok(Cow::Owned(match codec {
        "null" => return Ok(Cow::Borrowed(data)),
        "deflate" => miniz_oxide::inflate::decompress_to_vec_with_limit(data, MAX_BLOCK)
            .map_err(|e| format!("deflate: {e:?}"))?,
        "snappy" => {
            // A raw Snappy block, then the CRC-32 of the uncompressed data.
            let (body, crc) = data
                .split_at_checked(data.len().wrapping_sub(4))
                .ok_or("snappy: block too short")?;
            let len = snap::raw::decompress_len(body).map_err(|e| format!("snappy: {e}"))?;
            if len > MAX_BLOCK {
                return Err("block too large".into());
            }
            let out = snap::raw::Decoder::new()
                .decompress_vec(body)
                .map_err(|e| format!("snappy: {e}"))?;
            if crc32(&out).to_be_bytes() != crc {
                return Err("snappy: checksum mismatch".into());
            }
            out
        }
        "zstandard" => {
            let mut d = ruzstd::decoding::StreamingDecoder::new(data).map_err(|e| format!("zstd: {e}"))?;
            let mut out = Vec::new();
            (&mut d)
                .take(MAX_BLOCK as u64)
                .read_to_end(&mut out)
                .map_err(|e| format!("zstd: {e}"))?;
            out
        }
        c => return Err(format!("{c} compression isn't supported")),
    }))
}

fn crc32(b: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in b {
        c ^= x as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xedb8_8320 & (c & 1).wrapping_neg());
        }
    }
    !c
}

// ── Binary encoding ────────────────────────────────────────────────────────

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
    /// Values decoded in the current record.
    work: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Reader { b, p: 0, work: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .p
            .checked_add(n)
            .filter(|&e| e <= self.b.len())
            .ok_or("data ends early")?;
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }

    /// A zigzag varint (Avro `int` and `long`).
    fn long(&mut self) -> Result<i64, String> {
        let mut v = 0u64;
        for shift in (0..70).step_by(7) {
            let byte = self.take(1)?[0];
            v |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok((v >> 1) as i64 ^ -((v & 1) as i64));
            }
        }
        Err("varint too long".into())
    }

    /// A length that must fit in what's left.
    fn len(&mut self) -> Result<usize, String> {
        let n = self.long()?;
        usize::try_from(n)
            .ok()
            .filter(|&n| n <= self.b.len() - self.p)
            .ok_or_else(|| "length out of range".into())
    }

    fn bytes(&mut self) -> Result<&'a [u8], String> {
        let n = self.len()?;
        self.take(n)
    }

    /// Counts one decoded value against the per-record limit.
    fn tick(&mut self) -> Result<(), String> {
        self.work += 1;
        if self.work > MAX_WORK {
            return Err("too many values in one record".into());
        }
        Ok(())
    }
}

/// Collects a cell's text, keeping only its start.
#[derive(Default)]
struct Out(String);

impl Out {
    fn push(&mut self, t: &str) {
        for c in t.chars() {
            if self.0.len() >= CELL_BYTES {
                return;
            }
            self.0.push(if c.is_control() { ' ' } else { c });
        }
    }

    /// Text: quoted when nested inside a record, array or map.
    fn text(&mut self, t: &str, quote: bool) {
        if quote {
            self.push(&format!("{t:?}"));
        } else {
            self.push(t);
        }
    }
}

// ── Schemas ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Debug)]
enum Logical {
    None,
    Date,
    /// Time of day with 3 or 6 fractional digits.
    Time(u32),
    /// Fractional digits, and whether it's UTC (not `local-timestamp-*`).
    Timestamp(u32, bool),
    Decimal(u32, u32),
    Uuid,
}

enum Ty {
    Null,
    Boolean,
    Int(Logical),
    Long(Logical),
    Float,
    Double,
    Bytes(Logical),
    String(Logical),
    Record {
        name: String,
        fields: Vec<Field>,
    },
    Enum {
        name: String,
        symbols: Vec<String>,
    },
    Array(usize),
    Map(usize),
    Union(Vec<usize>),
    Fixed {
        name: String,
        size: usize,
        logical: Logical,
    },
}

struct Field {
    name: String,
    ty: usize,
    doc: Option<String>,
}

struct Schema {
    /// Types refer to each other by index, so named types can be recursive.
    types: Vec<Ty>,
    names: Vec<(String, usize)>,
    root: usize,
}

impl Schema {
    fn parse(j: &Json) -> Result<Schema, String> {
        let mut s = Schema {
            types: Vec::new(),
            names: Vec::new(),
            root: 0,
        };
        s.root = s.ty(j, "", 0)?;
        Ok(s)
    }

    fn add(&mut self, t: Ty) -> Result<usize, String> {
        if self.types.len() >= MAX_TYPES {
            return Err("schema too large".into());
        }
        self.types.push(t);
        Ok(self.types.len() - 1)
    }

    fn ty(&mut self, j: &Json, ns: &str, depth: usize) -> Result<usize, String> {
        if depth > MAX_DEPTH {
            return Err("schema nested too deeply".into());
        }
        let obj = match j {
            Json::Str(name) => return self.named(name, ns, Logical::None),
            Json::Arr(branches) => {
                let ids = branches
                    .iter()
                    .map(|b| self.ty(b, ns, depth + 1))
                    .collect::<Result<_, _>>()?;
                return self.add(Ty::Union(ids));
            }
            Json::Obj(_) => j,
            _ => return Err("unexpected value in the schema".into()),
        };
        let t = match obj.get("type") {
            Some(Json::Str(t)) => t.as_str(),
            Some(inner) => return self.ty(inner, ns, depth + 1),
            None => return Err("a schema object has no type".into()),
        };
        let logical = logical(obj, t);
        match t {
            "record" | "error" | "enum" | "fixed" => {
                let name = obj.get("name").and_then(Json::str).ok_or("a named type has no name")?;
                let ns = match obj.get("namespace").and_then(Json::str) {
                    Some(n) if !name.contains('.') => n,
                    _ => ns,
                };
                let full = full_name(name, ns);
                // Register the name first: fields may refer back to it.
                let id = self.add(Ty::Null)?;
                self.names.push((full.clone(), id));
                let inner_ns = full.rsplit_once('.').map_or("", |(n, _)| n).to_string();
                let ty = match t {
                    "enum" => Ty::Enum {
                        name: full,
                        symbols: match obj.get("symbols") {
                            Some(Json::Arr(s)) => s.iter().map(|s| s.str().unwrap_or("?").to_string()).collect(),
                            _ => Vec::new(),
                        },
                    },
                    "fixed" => Ty::Fixed {
                        name: full,
                        size: obj.get("size").and_then(Json::num).unwrap_or(0.0).max(0.0) as usize,
                        logical,
                    },
                    _ => {
                        let mut fields = Vec::new();
                        if let Some(Json::Arr(fs)) = obj.get("fields") {
                            for f in fs {
                                let ty = f.get("type").ok_or("a field has no type")?;
                                fields.push(Field {
                                    name: f.get("name").and_then(Json::str).unwrap_or("?").to_string(),
                                    ty: self.ty(ty, &inner_ns, depth + 1)?,
                                    doc: f.get("doc").and_then(Json::str).map(String::from),
                                });
                            }
                        }
                        Ty::Record { name: full, fields }
                    }
                };
                self.types[id] = ty;
                Ok(id)
            }
            "array" => {
                let items = self.ty(obj.get("items").ok_or("an array has no items")?, ns, depth + 1)?;
                self.add(Ty::Array(items))
            }
            "map" => {
                let values = self.ty(obj.get("values").ok_or("a map has no values")?, ns, depth + 1)?;
                self.add(Ty::Map(values))
            }
            _ => self.named(t, ns, logical),
        }
    }

    /// A primitive type or a reference to a named type defined earlier.
    fn named(&mut self, name: &str, ns: &str, logical: Logical) -> Result<usize, String> {
        let t = match name {
            "null" => Ty::Null,
            "boolean" => Ty::Boolean,
            "int" => Ty::Int(logical),
            "long" => Ty::Long(logical),
            "float" => Ty::Float,
            "double" => Ty::Double,
            "bytes" => Ty::Bytes(logical),
            "string" => Ty::String(logical),
            _ => {
                let full = full_name(name, ns);
                return self
                    .names
                    .iter()
                    .find(|(n, _)| *n == full)
                    .or_else(|| self.names.iter().find(|(n, _)| n == name))
                    .map(|(_, id)| *id)
                    .ok_or_else(|| format!("unknown type {name}"));
            }
        };
        self.add(t)
    }

    /// The full name of a named type.
    fn name(&self, id: usize) -> Option<&str> {
        match &self.types[id] {
            Ty::Record { name, .. } | Ty::Enum { name, .. } | Ty::Fixed { name, .. } => Some(name),
            _ => None,
        }
    }

    /// A compact type name: `long`, `string?`, `decimal(9,2)`, `array<string>`, `record Foo`.
    fn type_name(&self, id: usize) -> String {
        let short = |n: &str| n.rsplit('.').next().unwrap_or(n).to_string();
        match &self.types[id] {
            Ty::Null => "null".into(),
            Ty::Boolean => "boolean".into(),
            Ty::Int(l) => logical_name(*l).unwrap_or_else(|| "int".into()),
            Ty::Long(l) => logical_name(*l).unwrap_or_else(|| "long".into()),
            Ty::Float => "float".into(),
            Ty::Double => "double".into(),
            Ty::Bytes(l) => logical_name(*l).unwrap_or_else(|| "bytes".into()),
            Ty::String(l) => logical_name(*l).unwrap_or_else(|| "string".into()),
            Ty::Record { name, .. } => format!("record {}", short(name)),
            Ty::Enum { name, .. } => format!("enum {}", short(name)),
            Ty::Fixed { size, logical, .. } => logical_name(*logical).unwrap_or_else(|| format!("fixed({size})")),
            Ty::Array(t) => format!("array<{}>", self.type_name(*t)),
            Ty::Map(t) => format!("map<{}>", self.type_name(*t)),
            Ty::Union(bs) => {
                let null = |b: &usize| matches!(self.types[*b], Ty::Null);
                match bs.as_slice() {
                    [a, b] if null(a) && !null(b) => format!("{}?", self.type_name(*b)),
                    [a, b] if null(b) && !null(a) => format!("{}?", self.type_name(*a)),
                    _ => bs.iter().map(|b| self.type_name(*b)).collect::<Vec<_>>().join(" | "),
                }
            }
        }
    }

    /// The "Schema" section: fields of a top-level record, or the type.
    fn describe(&self, d: &mut DocView) {
        let Ty::Record { fields, .. } = &self.types[self.root] else {
            d.section("Schema", "", Body::Lines(vec![clip(self.type_name(self.root))]));
            return;
        };
        let docs = fields.iter().any(|f| f.doc.is_some());
        let mut header = vec!["field".to_string(), "type".to_string()];
        if docs {
            header.push("doc".into());
        }
        let rows = fields
            .iter()
            .map(|f| {
                let mut r = vec![Some(f.name.clone()), Some(clip(self.type_name(f.ty)))];
                if docs {
                    r.push(Some(f.doc.clone().map(clip).unwrap_or_default()));
                }
                r
            })
            .collect();
        d.section("Schema", "", Body::Table { header, rows });
    }

    /// One record as table cells: a column per top-level field.
    fn row(&self, r: &mut Reader) -> Result<Vec<Option<String>>, String> {
        r.work = 0;
        match &self.types[self.root] {
            Ty::Record { fields, .. } => fields.iter().map(|f| self.cell(r, f.ty)).collect(),
            _ => Ok(vec![self.cell(r, self.root)?]),
        }
    }

    fn cell(&self, r: &mut Reader, t: usize) -> Result<Option<String>, String> {
        let mut out = Out::default();
        let null = self.value(r, t, &mut out, 0)?;
        Ok((!null).then(|| clip(out.0)))
    }

    /// Decodes one value of type `t` into `out`; true when it was null.
    fn value(&self, r: &mut Reader, t: usize, out: &mut Out, depth: usize) -> Result<bool, String> {
        r.tick()?;
        if depth > MAX_DEPTH {
            return Err("values nested too deeply".into());
        }
        let quote = depth > 0;
        match &self.types[t] {
            Ty::Null => {
                out.push("null");
                return Ok(true);
            }
            Ty::Boolean => out.push(if r.take(1)?[0] != 0 { "true" } else { "false" }),
            Ty::Int(l) => {
                let v = i32::try_from(r.long()?).map_err(|_| "int out of range")? as i64;
                match l {
                    Logical::Date => out.text(&date(v), quote),
                    Logical::Time(digits) => out.text(&time_of_day(v, *digits), quote),
                    _ => out.push(&v.to_string()),
                }
            }
            Ty::Long(l) => {
                let v = r.long()?;
                match l {
                    Logical::Time(digits) => out.text(&time_of_day(v, *digits), quote),
                    Logical::Timestamp(digits, utc) => out.text(&timestamp(v, *digits, *utc), quote),
                    _ => out.push(&v.to_string()),
                }
            }
            Ty::Float => out.push(&f32::from_le_bytes(r.take(4)?.try_into().unwrap()).to_string()),
            Ty::Double => out.push(&f64::from_le_bytes(r.take(8)?.try_into().unwrap()).to_string()),
            Ty::Bytes(l) => {
                let b = r.bytes()?;
                bytes_value(b, *l, out, quote);
            }
            Ty::Fixed { size, logical, .. } => {
                let b = r.take(*size)?;
                bytes_value(b, *logical, out, quote);
            }
            Ty::String(_) => {
                let n = r.len()?;
                let b = r.take(n)?;
                out.text(&String::from_utf8_lossy(&b[..n.min(CELL_BYTES)]), quote);
            }
            Ty::Enum { symbols, .. } => {
                let i = r.long()?;
                let s = usize::try_from(i)
                    .ok()
                    .and_then(|i| symbols.get(i))
                    .ok_or("enum index out of range")?;
                out.text(s, quote);
            }
            Ty::Union(bs) => {
                let i = r.long()?;
                let b = usize::try_from(i)
                    .ok()
                    .and_then(|i| bs.get(i))
                    .ok_or("union index out of range")?;
                return self.value(r, *b, out, depth);
            }
            Ty::Record { fields, .. } => {
                out.push("{");
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(", ");
                    }
                    out.text(&f.name, true);
                    out.push(": ");
                    self.value(r, f.ty, out, depth + 1)?;
                }
                out.push("}");
            }
            Ty::Array(items) => {
                out.push("[");
                let mut first = true;
                while let Some(n) = block_count(r)? {
                    for _ in 0..n {
                        r.tick()?;
                        if !std::mem::take(&mut first) {
                            out.push(", ");
                        }
                        self.value(r, *items, out, depth + 1)?;
                    }
                }
                out.push("]");
            }
            Ty::Map(values) => {
                out.push("{");
                let mut first = true;
                while let Some(n) = block_count(r)? {
                    for _ in 0..n {
                        r.tick()?;
                        if !std::mem::take(&mut first) {
                            out.push(", ");
                        }
                        let k = r.bytes()?;
                        out.text(&String::from_utf8_lossy(&k[..k.len().min(CELL_BYTES)]), true);
                        out.push(": ");
                        self.value(r, *values, out, depth + 1)?;
                    }
                }
                out.push("}");
            }
        }
        Ok(false)
    }
}

/// The item count of the next array or map block; `None` after the last.
fn block_count(r: &mut Reader) -> Result<Option<u64>, String> {
    let n = r.long()?;
    if n < 0 {
        r.long()?; // block size in bytes
    }
    Ok((n != 0).then_some(n.unsigned_abs()))
}

fn bytes_value(b: &[u8], l: Logical, out: &mut Out, quote: bool) {
    match l {
        Logical::Decimal(_, scale) => match be_i128(b) {
            Some(x) => out.push(&decimal(x, scale as i64)),
            None if b.is_empty() => out.push("0"),
            None => out.text(&hex(&b[..b.len().min(CELL_BYTES)]), quote),
        },
        Logical::Uuid if b.len() == 16 => out.text(&uuid(b), quote),
        _ => match std::str::from_utf8(b) {
            Ok(s) if !s.chars().any(|c| c.is_control()) => out.text(s, quote),
            _ => out.text(&hex(&b[..b.len().min(CELL_BYTES)]), quote),
        },
    }
}

fn full_name(name: &str, ns: &str) -> String {
    if name.contains('.') || ns.is_empty() {
        name.to_string()
    } else {
        format!("{ns}.{name}")
    }
}

/// The logical type of a schema object over the primitive or fixed type `t`.
/// Unknown or invalid logical types are ignored, as the spec says.
fn logical(obj: &Json, t: &str) -> Logical {
    let Some(name) = obj.get("logicalType").and_then(Json::str) else {
        return Logical::None;
    };
    let int = |k: &str| {
        obj.get(k)
            .and_then(Json::num)
            .filter(|n| (0.0..=1e6).contains(n))
            .map(|n| n as u32)
    };
    match (name, t) {
        ("date", "int") => Logical::Date,
        ("time-millis", "int") => Logical::Time(3),
        ("time-micros", "long") => Logical::Time(6),
        ("timestamp-millis", "long") => Logical::Timestamp(3, true),
        ("timestamp-micros", "long") => Logical::Timestamp(6, true),
        ("timestamp-nanos", "long") => Logical::Timestamp(9, true),
        ("local-timestamp-millis", "long") => Logical::Timestamp(3, false),
        ("local-timestamp-micros", "long") => Logical::Timestamp(6, false),
        ("local-timestamp-nanos", "long") => Logical::Timestamp(9, false),
        ("decimal", "bytes" | "fixed") => match (int("precision"), int("scale").or(Some(0))) {
            (Some(p), Some(s)) if s <= 38 && s <= p => Logical::Decimal(p, s),
            _ => Logical::None,
        },
        ("uuid", "string" | "fixed") => Logical::Uuid,
        _ => Logical::None,
    }
}

fn logical_name(l: Logical) -> Option<String> {
    Some(match l {
        Logical::None => return None,
        Logical::Date => "date".into(),
        Logical::Time(3) => "time-millis".into(),
        Logical::Time(_) => "time-micros".into(),
        Logical::Timestamp(digits, utc) => format!(
            "{}timestamp-{}",
            if utc { "" } else { "local-" },
            match digits {
                3 => "millis",
                6 => "micros",
                _ => "nanos",
            }
        ),
        Logical::Decimal(p, s) => format!("decimal({p},{s})"),
        Logical::Uuid => "uuid".into(),
    })
}

// ── JSON (just enough for schemas) ─────────────────────────────────────────

enum Json {
    /// `true`, `false` or `null`: never needed in a schema.
    Literal,
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn parse(b: &[u8]) -> Result<Json, String> {
        let mut p = JsonParser { b, p: 0 };
        p.value(0)
    }

    fn get(&self, k: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.iter().find(|(n, _)| n == k).map(|(_, v)| v),
            _ => None,
        }
    }

    fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    fn num(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    b: &'a [u8],
    p: usize,
}

impl JsonParser<'_> {
    fn peek(&mut self) -> Option<u8> {
        while self.b.get(self.p).is_some_and(|c| c.is_ascii_whitespace()) {
            self.p += 1;
        }
        self.b.get(self.p).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        let ok = self.peek() == Some(c);
        if ok {
            self.p += 1;
        }
        ok
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > 2 * MAX_DEPTH {
            return Err("schema nested too deeply".into());
        }
        let bad = || Err("the schema isn't valid JSON".to_string());
        match self.peek() {
            Some(b'{') => {
                self.p += 1;
                let mut m = Vec::new();
                if self.eat(b'}') {
                    return Ok(Json::Obj(m));
                }
                loop {
                    if self.peek() != Some(b'"') {
                        return bad();
                    }
                    let k = self.string()?;
                    if !self.eat(b':') {
                        return bad();
                    }
                    m.push((k, self.value(depth + 1)?));
                    if self.eat(b'}') {
                        return Ok(Json::Obj(m));
                    }
                    if !self.eat(b',') {
                        return bad();
                    }
                }
            }
            Some(b'[') => {
                self.p += 1;
                let mut a = Vec::new();
                if self.eat(b']') {
                    return Ok(Json::Arr(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    if self.eat(b']') {
                        return Ok(Json::Arr(a));
                    }
                    if !self.eat(b',') {
                        return bad();
                    }
                }
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b'-' | b'0'..=b'9') => {
                let start = self.p;
                while self.b.get(self.p).is_some_and(|c| b"+-.eE0123456789".contains(c)) {
                    self.p += 1;
                }
                let s = std::str::from_utf8(&self.b[start..self.p]).unwrap_or("");
                s.parse().map(Json::Num).or_else(|_| bad())
            }
            _ => {
                for word in ["true", "false", "null"] {
                    if self.b[self.p..].starts_with(word.as_bytes()) {
                        self.p += word.len();
                        return Ok(Json::Literal);
                    }
                }
                bad()
            }
        }
    }

    /// A string; the cursor is on its opening quote.
    fn string(&mut self) -> Result<String, String> {
        self.p += 1;
        let mut s = Vec::new();
        loop {
            let c = *self.b.get(self.p).ok_or("unterminated string in the schema")?;
            self.p += 1;
            match c {
                b'"' => return Ok(String::from_utf8_lossy(&s).into_owned()),
                b'\\' => {
                    let e = *self.b.get(self.p).ok_or("unterminated string in the schema")?;
                    self.p += 1;
                    let ch = match e {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xd800..0xdc00).contains(&hi) && self.b[self.p..].starts_with(b"\\u") {
                                self.p += 2;
                                let lo = self.hex4()?;
                                0x10000 + ((hi - 0xd800) << 10) + lo.wrapping_sub(0xdc00).min(0x3ff)
                            } else {
                                hi
                            };
                            char::from_u32(code).unwrap_or('\u{fffd}')
                        }
                        other => other as char,
                    };
                    s.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                }
                _ => s.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.b.get(self.p..self.p + 4).ok_or("bad escape in the schema")?;
        self.p += 4;
        std::str::from_utf8(h)
            .ok()
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .ok_or_else(|| "bad escape in the schema".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn table(d: &DocView, title: &str) -> (Vec<String>, Vec<Vec<Option<String>>>) {
        let s = d.sections.iter().find(|s| s.title == title).expect(title);
        match &s.body {
            Body::Table { header, rows } => (header.clone(), rows.clone()),
            _ => panic!("{title} isn't a table"),
        }
    }

    fn facts(d: &DocView) -> Vec<String> {
        d.summary
            .iter()
            .map(|f| format!("{} {}", f.value, f.label).trim().to_string())
            .collect()
    }

    fn text(d: &DocView) -> Vec<String> {
        d.render(&Theme::new(false))
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    #[test]
    fn decodes_every_codec() {
        for (name, codec) in [
            ("turbine-null.avro", "uncompressed"),
            ("turbine-deflate.avro", "deflate"),
            ("turbine-snappy.avro", "snappy"),
            ("turbine-zstandard.avro", "zstandard"),
        ] {
            let f = fixture(name);
            assert!(f.len() < 64 << 10, "{name} is too large");
            let d = decode(&f).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(d.notes.is_empty(), "{name}: {:?}", d.notes);
            let blocks = facts(&d)[1].clone();
            assert_eq!(
                facts(&d),
                ["300 records", &blocks, codec, "energy.wind.TurbineReading"],
                "{name}"
            );
            let nblocks: usize = blocks.trim_end_matches(" blocks").parse().unwrap();
            assert!(nblocks > 5, "{name}: {blocks}");
            assert_eq!(
                d.meta,
                [
                    ("ProviderName".to_string(), "TurbineFastlog".to_string()),
                    ("Version".to_string(), "1.0.0".to_string())
                ],
                "{name}"
            );
            let (header, rows) = table(&d, "Rows");
            assert_eq!(
                header,
                [
                    "Timestamp",
                    "TurbineId",
                    "ActivePower",
                    "WindSpeed",
                    "Rpm",
                    "Running",
                    "Status",
                    "Price",
                    "Day",
                    "Tags",
                    "Readings",
                    "Location",
                    "Previous"
                ],
                "{name}"
            );
            let row0: Vec<_> = rows[0].iter().map(|c| c.as_deref().unwrap_or("None")).collect();
            assert_eq!(
                row0,
                [
                    "2026-10-08 10:20:00Z",
                    "T01",
                    "-12.5",
                    "2",
                    "9",
                    "false",
                    "IDLE",
                    "0.00",
                    "2026-10-08",
                    r#"["north", "row-a"]"#,
                    r#"{"temp": 21.5, "vib": 0.25}"#,
                    r#"{"lat": 59.91, "lon": 10.75}"#,
                    r#"{"lat": 1.5, "lon": -2}"#,
                ],
                "{name}"
            );
            let row1: Vec<_> = rows[1].iter().map(|c| c.as_deref().unwrap_or("None")).collect();
            assert_eq!(
                row1,
                [
                    "2026-10-08 10:20:01Z",
                    "T02",
                    "25",
                    "2.1",
                    "10",
                    "true",
                    "RUNNING",
                    "0.01",
                    "2026-10-08",
                    "[]",
                    "{}",
                    r#"{"lat": 59.91, "lon": 10.75}"#,
                    "None",
                ],
                "{name}"
            );
            assert_eq!(rows[7][3], None, "{name}: WindSpeed row 7 is null");
            assert_eq!(rows[12][7].as_deref(), Some("0.12"), "{name}");
            assert!(rows.len() > 10, "{name}: only {} rows", rows.len());
            let rendered: usize = rows
                .iter()
                .flatten()
                .map(|c| c.as_ref().map_or(4, |s| s.len()) + 1)
                .sum();
            assert!(rendered <= ROW_BUDGET, "{name}: rows use {rendered} bytes");
            let detail = &d.sections.iter().find(|s| s.title == "Rows").unwrap().detail;
            assert_eq!(*detail, format!("· first {} of 300", rows.len()), "{name}");
        }
    }

    #[test]
    fn schema_table() {
        let d = decode(&fixture("turbine-deflate.avro")).unwrap();
        let (header, rows) = table(&d, "Schema");
        assert_eq!(header, ["field", "type", "doc"]);
        let rows: Vec<Vec<&str>> = rows
            .iter()
            .map(|r| r.iter().map(|c| c.as_deref().unwrap_or("-")).collect())
            .collect();
        assert_eq!(
            rows,
            [
                ["Timestamp", "timestamp-millis", ""],
                ["TurbineId", "string", "Turbine tag"],
                ["ActivePower", "double", ""],
                ["WindSpeed", "float?", ""],
                ["Rpm", "int", ""],
                ["Running", "boolean", ""],
                ["Status", "enum Status", ""],
                ["Price", "decimal(9,2)", ""],
                ["Day", "date", ""],
                ["Tags", "array<string>", ""],
                ["Readings", "map<double>", ""],
                ["Location", "record Location", ""],
                ["Previous", "record Location?", ""],
            ]
        );
    }

    #[test]
    fn schemas_and_logical_types() {
        let s = |json: &str| Schema::parse(&Json::parse(json.as_bytes()).unwrap()).unwrap();
        let t = s(r#"{"type":"record","name":"a.b.Node","fields":[
            {"name":"next","type":["null","Node"]},
            {"name":"id","type":{"type":"fixed","name":"Id","size":16,"logicalType":"uuid"}},
            {"name":"amount","type":{"type":"fixed","name":"Amt","size":4,"logicalType":"decimal","precision":6,"scale":3}},
            {"name":"at","type":{"type":"long","logicalType":"local-timestamp-micros"}},
            {"name":"t","type":{"type":"int","logicalType":"time-millis"}},
            {"name":"u","type":{"type":"string","logicalType":"uuid"}},
            {"name":"both","type":["int","string","a.b.Id"]},
            {"name":"m","type":{"type":"map","values":{"type":"array","items":"long"}}}
        ]}"#);
        let types: Vec<String> = match &t.types[t.root] {
            Ty::Record { fields, .. } => fields.iter().map(|f| t.type_name(f.ty)).collect(),
            _ => panic!(),
        };
        assert_eq!(
            types,
            [
                "record Node?",
                "uuid",
                "decimal(6,3)",
                "local-timestamp-micros",
                "time-millis",
                "uuid",
                "int | string | uuid",
                "map<array<long>>"
            ]
        );
        // A linked list two deep, as Avro binary.
        let mut b = vec![2u8, 0]; // next: branch 1 (Node) → next: branch 0 (null)
        let rest = |b: &mut Vec<u8>| {
            b.extend([0x12; 16]); // id
            b.extend((-1234i32).to_be_bytes()); // amount
            b.push(0x02); // at = 1µs
            b.extend([0x80, 0xdd, 0xdb, 0x01]); // t = 1_800_000 ms
            b.extend(b"\x04ab"); // u
            b.extend(b"\x00\x0e"); // both: int 7
            b.extend(b"\x02\x02k\x04\x02\x04\x00\x00"); // m: {k: [1, 2]}
        };
        rest(&mut b);
        rest(&mut b);
        let mut r = Reader::new(&b);
        let row: Vec<_> = t.row(&mut r).unwrap().into_iter().map(|c| c.unwrap()).collect();
        assert_eq!(r.p, b.len());
        assert_eq!(
            row[0],
            r#"{"next": null, "id": "12121212-1212-1212-1212-121212121212", "a…"#
        );
        assert_eq!(
            row[1..],
            [
                "12121212-1212-1212-1212-121212121212",
                "-1.234",
                "1970-01-01 00:00:00.000001",
                "00:30:00",
                "ab",
                "7",
                r#"{"k": [1, 2]}"#
            ]
        );
        // A top-level non-record schema gets one "value" column.
        let json = br#"{"type":"array","items":"string"}"#;
        let mut f = b"Obj\x01\x02\x16avro.schema".to_vec();
        f.push(json.len() as u8 * 2);
        f.extend(json);
        f.push(0);
        f.extend([7; 16]); // sync marker
        f.extend(b"\x02\x08\x02\x02a\x00"); // one record, 4 bytes: ["a"]
        f.extend([7; 16]);
        let d = decode(&f).unwrap();
        assert_eq!(facts(&d), ["1 record", "1 block", "uncompressed"]);
        assert!(matches!(&d.sections[0].body, Body::Lines(l) if l == &["array<string>"]));
        assert_eq!(
            table(&d, "Rows"),
            (vec!["value".to_string()], vec![vec![Some(r#"["a"]"#.to_string())]])
        );
        assert!(
            Json::parse(br#"{"type":"record","name":"x","fields":[{"name":"a\u0041\"","type":"Nope"}]}"#)
                .and_then(|j| Schema::parse(&j))
                .is_err()
        );
    }

    #[test]
    fn schema_registry_framings() {
        let f = fixture("turbine-confluent.avro");
        let d = sniff("application/avro", &f).unwrap().unwrap();
        assert_eq!(
            d.notes,
            ["Confluent schema-registry framing, schema id 42: the schema isn't in the payload"]
        );
        assert_eq!(facts(&d)[0], "42 schema id");
        assert!(sniff("", &f).is_none(), "a leading zero alone isn't Avro");
        assert!(sniff("application/vnd.apache.avro+json", b"{}").is_none());

        let mut so = b"\xc3\x01\x01\x02\x03\x04\x05\x06\x07\x08".to_vec();
        so.extend(&f[5..]);
        assert!(sniff("", &so).is_none(), "two magic bytes alone don't claim a payload");
        let d = sniff("avro/binary", &so).unwrap().unwrap();
        assert_eq!(
            d.notes,
            ["Avro single-object encoding, schema fingerprint 0102030405060708: the schema isn't in the payload"]
        );
        let d = sniff("avro/binary", &f[5..]).unwrap().unwrap();
        assert!(d.notes[0].contains("needs the writer schema"), "{:?}", d.notes);
    }

    #[test]
    fn decoded_on_arrival() {
        let f = fixture("turbine-snappy.avro");
        for ct in [Some("avro/binary"), None] {
            let d = crate::pretty::decode_on_arrival(ct, &f).unwrap();
            assert_eq!(d.format, "Avro");
            let t = text(&d);
            assert!(t[0].starts_with("300 records · "), "{}", t[0]);
            assert!(t[0].ends_with(" · snappy · energy.wind.TurbineReading"), "{}", t[0]);
            assert!(t.iter().any(|l| l == "ProviderName TurbineFastlog"));
            assert!(
                t.iter()
                    .any(|l| l.starts_with("Timestamp ") && l.contains("timestamp-millis"))
            );
            assert!(t.iter().any(|l| l.starts_with("Rows · first ")));
            assert!(
                t.iter()
                    .any(|l| l.contains("2026-10-08 10:20:00Z") && l.contains("T01"))
            );
        }
        let c = fixture("turbine-confluent.avro");
        let d = crate::pretty::decode_on_arrival(Some("application/avro"), &c).unwrap();
        assert_eq!(
            text(&d),
            [
                "42 schema id · 105 bytes of data",
                "note: Confluent schema-registry framing, schema id 42: the schema isn't in the payload"
            ]
        );
        assert!(crate::pretty::decode_on_arrival(None, &c).is_none());
    }

    #[test]
    fn unsupported_codec_and_damaged_blocks() {
        let mut f = fixture("turbine-null.avro");
        let at = f.windows(4).position(|w| w == b"null").unwrap();
        f[at..at + 4].copy_from_slice(b"lzma");
        let d = decode(&f).unwrap();
        assert!(d.notes.iter().any(|n| n.contains("isn't supported")), "{:?}", d.notes);
        assert!(d.sections.iter().any(|s| s.title == "Schema"));

        let f = fixture("turbine-null.avro");
        let d = decode(&f[..f.len() - 100]).unwrap();
        assert!(facts(&d)[0].ends_with("records"));
        assert!(d.notes.iter().any(|n| n.contains("block")), "{:?}", d.notes);
    }

    #[test]
    fn never_panics_on_damaged_input() {
        assert!(decode(b"Obj\x01").is_err());
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for name in [
            "turbine-null.avro",
            "turbine-deflate.avro",
            "turbine-snappy.avro",
            "turbine-zstandard.avro",
        ] {
            let good = fixture(name);
            for _ in 0..400 {
                let mut f = good.clone();
                for _ in 0..(rnd() % 8 + 1) {
                    let i = (rnd() as usize) % f.len();
                    f[i] = rnd() as u8;
                }
                let _ = decode(&f);
                let cut = 4 + (rnd() as usize) % (f.len() - 4);
                let _ = decode(&f[..cut]);
            }
        }
        // Records that claim huge collections of nothing.
        let s = Schema::parse(&Json::parse(br#"{"type":"array","items":"null"}"#).unwrap()).unwrap();
        let b = [0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert!(s.row(&mut Reader::new(&b)).is_err());
        let s =
            Schema::parse(&Json::parse(br#"{"type":"record","name":"R","fields":[{"name":"r","type":"R"}]}"#).unwrap())
                .unwrap();
        assert!(s.row(&mut Reader::new(b"")).is_err());
        assert!(Json::parse(&[b'['; 10_000]).is_err());
    }
}
