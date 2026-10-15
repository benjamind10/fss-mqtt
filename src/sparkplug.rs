//! Sparkplug B (Eclipse Sparkplug 3.0): the protobuf payloads published on
//! `spBv1.0/{group}/{type}/{edge node}[/{device}]`, decoded with their schema
//! into metrics with names, types and values, plus data sets and templates.
//!
//! Data and command messages usually name their metrics by alias only: the
//! names (and types) come from the last birth certificate (NBIRTH/DBIRTH) of
//! the edge node or device. Each connection keeps them in an [`Aliases`] table
//! as births arrive. A birth sent before fss-mqtt connected can't be seen, so
//! until the next one those metrics show as `alias 7`.
//!
//! Runs on the full payload as it arrives: lengths are never trusted, depth,
//! counts and output are capped, and nothing panics. Payloads compressed the
//! way Eclipse Tahu does it (uuid `SPBV1.0_COMPRESSED`, gzip or deflate) are
//! unpacked first.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, UNIX_EPOCH};

use crate::fmt::{clock, commas, human_bytes, plural};
use crate::protobuf::{take, varint};
use crate::values::{clip, hex};
use crate::view::{Body, DocView, Fact};

const FORMAT: &str = "Sparkplug B";
const NAMESPACE: &str = "spBv1.0";
const COMPRESSED: &str = "SPBV1.0_COMPRESSED";

/// Rows of a table stop at about this many bytes of cells.
const ROW_BUDGET: usize = 4096;
const MAX_ROWS: usize = 200;
/// Data set and template sections shown (top-level metrics only).
const MAX_SECTIONS: usize = 4;
/// Templates hold metrics, which may hold templates.
const MAX_DEPTH: usize = 8;
/// Metrics read from one payload, at every level.
const MAX_METRICS: usize = 20_000;
/// Columns, parameters and properties read per data set, template or set.
const MAX_ITEMS: usize = 256;
/// Values shown of an array; bytes shown of a bytes value.
const MAX_LIST: usize = 16;
const MAX_HEX: usize = 8;
/// Bytes kept of a payload's `body` (like `--max-payload`), and the most a
/// compressed payload may unpack to.
const KEEP: usize = 4096;
const MAX_UNPACKED: usize = 16 << 20;
/// Metrics whose names and types are remembered from births, over all edge
/// nodes and devices.
const MAX_NAMES: usize = 100_000;

// DataType numbers (sparkplug_b.proto).
const INT8: u32 = 1;
const INT16: u32 = 2;
const INT32: u32 = 3;
const INT64: u32 = 4;
const DATETIME: u32 = 13;
const FILE: u32 = 18;
const INT8_ARRAY: u32 = 22;
const INT16_ARRAY: u32 = 23;
const INT32_ARRAY: u32 = 24;
const INT64_ARRAY: u32 = 25;
const UINT8_ARRAY: u32 = 26;
const UINT16_ARRAY: u32 = 27;
const UINT32_ARRAY: u32 = 28;
const UINT64_ARRAY: u32 = 29;
const FLOAT_ARRAY: u32 = 30;
const DOUBLE_ARRAY: u32 = 31;
const BOOLEAN_ARRAY: u32 = 32;
const STRING_ARRAY: u32 = 33;
const DATETIME_ARRAY: u32 = 34;

const TYPES: [&str; 35] = [
    "Unknown",
    "Int8",
    "Int16",
    "Int32",
    "Int64",
    "UInt8",
    "UInt16",
    "UInt32",
    "UInt64",
    "Float",
    "Double",
    "Boolean",
    "String",
    "DateTime",
    "Text",
    "UUID",
    "DataSet",
    "Bytes",
    "File",
    "Template",
    "PropertySet",
    "PropertySetList",
    "Int8Array",
    "Int16Array",
    "Int32Array",
    "Int64Array",
    "UInt8Array",
    "UInt16Array",
    "UInt32Array",
    "UInt64Array",
    "FloatArray",
    "DoubleArray",
    "BooleanArray",
    "StringArray",
    "DateTimeArray",
];

// ── Topics ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    NBirth,
    NDeath,
    DBirth,
    DDeath,
    NData,
    DData,
    NCmd,
    DCmd,
}

impl Kind {
    fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "NBIRTH" => Kind::NBirth,
            "NDEATH" => Kind::NDeath,
            "DBIRTH" => Kind::DBirth,
            "DDEATH" => Kind::DDeath,
            "NDATA" => Kind::NData,
            "DDATA" => Kind::DData,
            "NCMD" => Kind::NCmd,
            "DCMD" => Kind::DCmd,
            _ => return None,
        })
    }

    fn code(self) -> &'static str {
        match self {
            Kind::NBirth => "NBIRTH",
            Kind::NDeath => "NDEATH",
            Kind::DBirth => "DBIRTH",
            Kind::DDeath => "DDEATH",
            Kind::NData => "NDATA",
            Kind::DData => "DDATA",
            Kind::NCmd => "NCMD",
            Kind::DCmd => "DCMD",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::NBirth => "node birth",
            Kind::NDeath => "node death",
            Kind::DBirth => "device birth",
            Kind::DDeath => "device death",
            Kind::NData => "node data",
            Kind::DData => "device data",
            Kind::NCmd => "node command",
            Kind::DCmd => "device command",
        }
    }
}

struct Topic<'a> {
    group: &'a str,
    kind: Kind,
    node: &'a str,
    device: Option<&'a str>,
}

impl Topic<'_> {
    /// Where its aliases are kept: per edge node, and per device.
    fn key(&self) -> String {
        match self.device {
            Some(d) => format!("{}/{}/{d}", self.group, self.node),
            None => format!("{}/{}", self.group, self.node),
        }
    }
}

/// `spBv1.0/{group}/{type}/{edge node}[/{device}]` with a message type whose
/// payload is Sparkplug protobuf; `STATE` (JSON or text) and anything else is
/// `None`.
fn parse_topic(t: &str) -> Option<Topic<'_>> {
    let mut parts = t.split('/');
    if parts.next()? != NAMESPACE {
        return None;
    }
    let group = parts.next()?;
    let kind = Kind::parse(parts.next()?)?;
    let node = parts.next()?;
    let device = parts.next();
    if parts.next().is_some() || group.is_empty() || node.is_empty() || device == Some("") {
        return None;
    }
    Some(Topic {
        group,
        kind,
        node,
        device,
    })
}

// ── Aliases ────────────────────────────────────────────────────────────────

/// What a birth said about its metrics: name and type by alias, type by name.
#[derive(Default)]
struct Birth {
    names: HashMap<u64, (String, Option<u32>)>,
    types: HashMap<String, u32>,
    /// Metrics remembered.
    count: usize,
    /// It reached the limit: its later metrics aren't remembered.
    truncated: bool,
}

/// Metric names and types by alias, from the births a connection has seen,
/// per edge node and per device. Kept by the connection's thread.
pub struct Aliases {
    births: HashMap<String, Birth>,
    /// Metrics remembered over all births, and the most that may be.
    size: usize,
    limit: usize,
}

impl Default for Aliases {
    fn default() -> Aliases {
        Aliases {
            births: HashMap::new(),
            size: 0,
            limit: MAX_NAMES,
        }
    }
}

impl Aliases {
    /// The view of a Sparkplug B message on `topic`, or `None` if `topic`
    /// isn't one (it's left to the generic decoders). A birth updates the
    /// aliases first.
    pub fn decode(&mut self, topic: &str, payload: &[u8]) -> Option<DocView> {
        let t = parse_topic(topic)?;
        Some(
            self.view(&t, payload)
                .unwrap_or_else(|e| DocView::error(FORMAT, &e, payload)),
        )
    }

    /// Remembers what a complete birth says, replacing what the edge node or
    /// device said before; a node's birth also forgets its devices, which are
    /// born again after it.
    fn learn(&mut self, t: &Topic, metrics: &[Metric]) {
        let key = t.key();
        if t.kind == Kind::NBirth {
            let prefix = format!("{key}/");
            let mut gone = 0;
            self.births.retain(|k, b| {
                let keep = !k.starts_with(&prefix);
                if !keep {
                    gone += b.count;
                }
                keep
            });
            self.size = self.size.saturating_sub(gone);
        }
        if let Some(old) = self.births.remove(&key) {
            self.size = self.size.saturating_sub(old.count);
        }
        let mut b = Birth::default();
        for m in metrics {
            let Some(name) = &m.name else { continue };
            if m.alias.is_none() && m.datatype.is_none() {
                continue; // nothing to remember
            }
            if self.size + b.count >= self.limit {
                b.truncated = true;
                break;
            }
            if let Some(dt) = m.datatype {
                b.types.insert(name.to_string(), dt);
            }
            if let Some(a) = m.alias {
                b.names.insert(a, (name.to_string(), m.datatype));
            }
            b.count += 1;
        }
        self.size += b.count;
        self.births.insert(key, b);
    }

    fn view(&mut self, t: &Topic, b: &[u8]) -> Result<DocView, String> {
        let mut r = Reader::default();
        let (outer, outer_err) = r.payload(b)?;
        let unpacked;
        let mut packed = None;
        let (p, err) = if outer.uuid.as_deref() == Some(COMPRESSED) {
            let algorithm = outer
                .metrics
                .iter()
                .find(|m| m.name.as_deref() == Some("algorithm"))
                .and_then(|m| match &m.value {
                    Value::Str(s) => Some(s.to_ascii_uppercase()),
                    _ => None,
                })
                .unwrap_or_else(|| "DEFLATE".into());
            let body = outer.body.unwrap_or_default();
            unpacked = unpack(&algorithm, body)?;
            packed = Some(Fact::new(
                algorithm.to_ascii_lowercase(),
                format!("{} → {}", human_bytes(body.len()), human_bytes(unpacked.len())),
            ));
            r = Reader::default();
            r.payload(&unpacked).map_err(|e| format!("unpacked payload: {e}"))?
        } else {
            (outer, outer_err)
        };

        if matches!(t.kind, Kind::NBirth | Kind::DBirth) && err.is_none() {
            self.learn(t, &p.metrics);
        }
        let birth = self.births.get(&t.key());
        let mut d = DocView::new(FORMAT);
        d.summary = vec![
            Fact::new(t.kind.code(), t.kind.label()),
            Fact::new(commas(r.total as u64), if r.total == 1 { "metric" } else { "metrics" }),
        ];
        if let Some(s) = p.seq {
            d.summary.push(Fact::new("", format!("seq {s}")));
        }
        d.summary.extend(packed);
        d.meta.push(("group".into(), t.group.into()));
        d.meta.push(("edge node".into(), t.node.into()));
        if let Some(dev) = t.device {
            d.meta.push(("device".into(), dev.into()));
        }
        if let Some(ts) = p.timestamp {
            d.meta.push(("timestamp".into(), datetime(ts)));
        }
        if let Some(bd) = p.metrics.iter().find(|m| m.name.as_deref() == Some("bdSeq")) {
            d.meta
                .push(("bdSeq".into(), metric_value(bd, bd.datatype).unwrap_or_default()));
        }
        if let Some(u) = p.uuid.as_deref().filter(|&u| u != COMPRESSED) {
            d.meta.push(("uuid".into(), u.into()));
        }

        let named: Vec<Named> = p.metrics.iter().map(|m| Named::of(m, birth)).collect();
        if !named.is_empty() {
            metrics_table(&mut d, &named, p.timestamp, r.total);
        }
        let mut sections = 0;
        for n in &named {
            if sections == MAX_SECTIONS {
                break;
            }
            match &n.m.value {
                Value::DataSet(ds) => dataset_section(&mut d, &n.label, ds),
                Value::Template(tp) => template_sections(&mut d, &n.label, tp),
                _ => continue,
            }
            sections += 1;
        }
        if let Some(body) = p.body {
            d.section(
                "Body",
                format!("· {}", human_bytes(body.len())),
                Body::Payload {
                    bytes: body[..body.len().min(KEEP)].to_vec(),
                    size: body.len(),
                    content_type: String::new(),
                },
            );
        }

        // Why aliases have no name, from what's remembered now: room comes
        // back when births are replaced and a node's devices forgotten.
        if named.iter().any(|n| n.unknown) {
            let kind = if t.device.is_some() { "DBIRTH" } else { "NBIRTH" };
            let limit = format!(
                "fss-mqtt's limit of {} remembered metric names (over all edge nodes and devices)",
                commas(self.limit as u64)
            );
            match birth {
                None => {
                    d.notes.push(format!(
                        "names come from the birth certificate ({kind}), sent before fss-mqtt connected; \
                         they appear after the next birth"
                    ));
                    if self.size >= self.limit {
                        d.notes.push(format!("{limit} is reached: a new birth won't fit"));
                    }
                }
                Some(b) => d.notes.push(if b.truncated {
                    format!("only part of the {kind} is remembered: {limit} was reached")
                } else {
                    format!("aliases shown as numbers aren't defined in the last {kind}")
                }),
            }
        }
        if let Some(e) = err {
            d.notes.push(format!(
                "malformed after {}: {e}",
                plural(p.metrics.len(), "metric", "metrics")
            ));
        }
        if r.cut {
            d.notes.push(format!(
                "only the first {} metrics were read",
                commas(MAX_METRICS as u64)
            ));
        }
        Ok(d)
    }
}

/// Unpacks the body of a payload compressed the way Eclipse Tahu does it.
fn unpack(algorithm: &str, b: &[u8]) -> Result<Vec<u8>, String> {
    match algorithm {
        "GZIP" => {
            let mut out = Vec::new();
            if crate::compressed::gunzip(b, &mut out, &mut Vec::new())? || out.len() > MAX_UNPACKED {
                return Err(format!(
                    "the compressed payload unpacks to over {}",
                    human_bytes(MAX_UNPACKED)
                ));
            }
            Ok(out)
        }
        // zlib (Java's Deflater, Python's zlib), or raw deflate.
        "DEFLATE" => miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(b, MAX_UNPACKED)
            .or_else(|_| miniz_oxide::inflate::decompress_to_vec_with_limit(b, MAX_UNPACKED))
            .map_err(|_| {
                format!(
                    "deflate: corrupt data, or it unpacks to over {}",
                    human_bytes(MAX_UNPACKED)
                )
            }),
        other => Err(format!("compressed with {other}, which isn't supported")),
    }
}

// ── Wire format ────────────────────────────────────────────────────────────

/// A field's value by wire type.
enum W<'a> {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    Len(&'a [u8]),
}

struct Wire<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Wire<'a> {
    fn new(b: &'a [u8]) -> Wire<'a> {
        Wire { b, pos: 0 }
    }

    /// The next field number and value; `None` at the end. Groups aren't
    /// part of Sparkplug, so they're an error.
    fn next(&mut self) -> Result<Option<(u64, W<'a>)>, String> {
        let b = self.b;
        if self.pos >= b.len() {
            return Ok(None);
        }
        let at = self.pos;
        let tag = varint(b, &mut self.pos).map_err(|e| format!("{e} at byte {at}"))?;
        let num = tag >> 3;
        if num == 0 {
            return Err(format!("field number 0 at byte {at}"));
        }
        let v = match tag & 7 {
            0 => W::Varint(varint(b, &mut self.pos).map_err(|e| format!("{e} at byte {at}"))?),
            1 => W::Fixed64(u64::from_le_bytes(take(b, &mut self.pos, at)?)),
            5 => W::Fixed32(u32::from_le_bytes(take(b, &mut self.pos, at)?)),
            2 => {
                let len = varint(b, &mut self.pos).map_err(|e| format!("{e} at byte {at}"))?;
                let end = usize::try_from(len)
                    .ok()
                    .and_then(|l| self.pos.checked_add(l))
                    .filter(|&e| e <= b.len())
                    .ok_or_else(|| format!("field {num} at byte {at}: length {len} runs past the end"))?;
                let s = &b[self.pos..end];
                self.pos = end;
                W::Len(s)
            }
            w => return Err(format!("field {num} has wire type {w} at byte {at}")),
        };
        Ok(Some((num, v)))
    }
}

fn text(s: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(s)
}

/// One of the value fields every Sparkplug value message has, `k`-th from
/// its first: int, long, float, double, boolean, string, and (metrics only)
/// bytes. `None` for a wire type that doesn't fit.
fn simple(k: u64, v: W<'_>) -> Option<Value<'_>> {
    Some(match (k, v) {
        (0, W::Varint(n)) => Value::Int(n as u32),
        (1, W::Varint(n)) => Value::Long(n),
        (2, W::Fixed32(n)) => Value::Float(f32::from_bits(n)),
        (3, W::Fixed64(n)) => Value::Double(f64::from_bits(n)),
        (4, W::Varint(n)) => Value::Bool(n != 0),
        (5, W::Len(s)) => Value::Str(text(s)),
        (6, W::Len(s)) => Value::Bytes(s),
        _ => return None,
    })
}

// ── Payload ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Payload<'a> {
    timestamp: Option<u64>,
    metrics: Vec<Metric<'a>>,
    seq: Option<u64>,
    uuid: Option<Cow<'a, str>>,
    body: Option<&'a [u8]>,
}

#[derive(Default)]
struct Metric<'a> {
    name: Option<Cow<'a, str>>,
    alias: Option<u64>,
    timestamp: Option<u64>,
    datatype: Option<u32>,
    historical: bool,
    transient: bool,
    null: bool,
    /// From its metadata, for files.
    file_name: Option<Cow<'a, str>>,
    /// Its property set, rendered.
    props: Vec<(String, String)>,
    value: Value<'a>,
}

#[derive(Default)]
enum Value<'a> {
    #[default]
    None,
    Int(u32),
    Long(u64),
    Float(f32),
    Double(f64),
    Bool(bool),
    Str(Cow<'a, str>),
    Bytes(&'a [u8]),
    DataSet(DataSet<'a>),
    Template(Template<'a>),
    Extension,
}

#[derive(Default)]
struct DataSet<'a> {
    num_columns: u64,
    columns: Vec<Cow<'a, str>>,
    types: Vec<u32>,
    /// The first `MAX_ROWS` rows.
    rows: Vec<Vec<Value<'a>>>,
    total_rows: usize,
}

impl DataSet<'_> {
    fn width(&self) -> usize {
        let n = usize::try_from(self.num_columns).unwrap_or(usize::MAX);
        n.min(MAX_ITEMS).max(self.columns.len()).max(self.types.len())
    }
}

#[derive(Default)]
struct Template<'a> {
    version: Option<Cow<'a, str>>,
    metrics: Vec<Metric<'a>>,
    total: usize,
    params: Vec<Param<'a>>,
    template_ref: Option<Cow<'a, str>>,
    definition: bool,
}

#[derive(Default)]
struct Param<'a> {
    name: Option<Cow<'a, str>>,
    datatype: Option<u32>,
    value: Value<'a>,
}

/// Parses payloads, counting metrics at every level against `MAX_METRICS`.
#[derive(Default)]
struct Reader {
    read: usize,
    /// Top-level metrics in the payload, read or not.
    total: usize,
    cut: bool,
}

impl Reader {
    /// The payload, and the error that stopped it early if any. An error
    /// before anything could be read is the result.
    fn payload<'a>(&mut self, b: &'a [u8]) -> Result<(Payload<'a>, Option<String>), String> {
        let mut p = Payload::default();
        let mut w = Wire::new(b);
        let mut fields = 0;
        let err = loop {
            let (num, v) = match w.next() {
                Ok(Some(f)) => f,
                Ok(None) => break None,
                Err(e) => break Some(e),
            };
            fields += 1;
            match (num, v) {
                (1, W::Varint(n)) => p.timestamp = Some(n),
                (2, W::Len(s)) => {
                    self.total += 1;
                    if self.read >= MAX_METRICS {
                        self.cut = true;
                        continue;
                    }
                    match self.metric(s, 0) {
                        Ok(m) => p.metrics.push(m),
                        Err(e) => break Some(format!("metric {}: {e}", self.total)),
                    }
                }
                (3, W::Varint(n)) => p.seq = Some(n),
                (4, W::Len(s)) => p.uuid = Some(text(s)),
                (5, W::Len(s)) => p.body = Some(s),
                _ => {} // extensions
            }
        };
        match err {
            Some(e) if fields == 0 => Err(e),
            err => Ok((p, err)),
        }
    }

    fn metric<'a>(&mut self, b: &'a [u8], depth: usize) -> Result<Metric<'a>, String> {
        self.read += 1;
        let mut m = Metric::default();
        let mut w = Wire::new(b);
        while let Some((num, v)) = w.next()? {
            match (num, v) {
                (1, W::Len(s)) => m.name = Some(text(s)),
                (2, W::Varint(n)) => m.alias = Some(n),
                (3, W::Varint(n)) => m.timestamp = Some(n),
                (4, W::Varint(n)) => m.datatype = Some(n as u32),
                (5, W::Varint(n)) => m.historical = n != 0,
                (6, W::Varint(n)) => m.transient = n != 0,
                (7, W::Varint(n)) => m.null = n != 0,
                (8, W::Len(s)) => m.file_name = file_name(s)?,
                (9, W::Len(s)) => m.props = properties(s, depth)?,
                (17, W::Len(s)) => m.value = Value::DataSet(dataset(s)?),
                (18, W::Len(s)) => {
                    if depth >= MAX_DEPTH {
                        return Err(format!("templates nested deeper than {MAX_DEPTH}"));
                    }
                    m.value = Value::Template(self.template(s, depth + 1)?);
                }
                (19, _) => m.value = Value::Extension,
                (n @ 10..=16, v) => {
                    if let Some(val) = simple(n - 10, v) {
                        m.value = val;
                    }
                }
                _ => {}
            }
        }
        Ok(m)
    }

    fn template<'a>(&mut self, b: &'a [u8], depth: usize) -> Result<Template<'a>, String> {
        let mut t = Template::default();
        let mut w = Wire::new(b);
        while let Some((num, v)) = w.next()? {
            match (num, v) {
                (1, W::Len(s)) => t.version = Some(text(s)),
                (2, W::Len(s)) => {
                    t.total += 1;
                    if self.read >= MAX_METRICS {
                        self.cut = true;
                        continue;
                    }
                    let m = self
                        .metric(s, depth)
                        .map_err(|e| format!("template metric {}: {e}", t.total))?;
                    t.metrics.push(m);
                }
                (3, W::Len(s)) if t.params.len() < MAX_ITEMS => t.params.push(param(s)?),
                (4, W::Len(s)) => t.template_ref = Some(text(s)),
                (5, W::Varint(n)) => t.definition = n != 0,
                _ => {}
            }
        }
        Ok(t)
    }
}

fn param(b: &[u8]) -> Result<Param<'_>, String> {
    let mut p = Param::default();
    let mut w = Wire::new(b);
    while let Some((num, v)) = w.next()? {
        match (num, v) {
            (1, W::Len(s)) => p.name = Some(text(s)),
            (2, W::Varint(n)) => p.datatype = Some(n as u32),
            (9, _) => p.value = Value::Extension,
            (n @ 3..=8, v) => {
                if let Some(val) = simple(n - 3, v) {
                    p.value = val;
                }
            }
            _ => {}
        }
    }
    Ok(p)
}

fn dataset(b: &[u8]) -> Result<DataSet<'_>, String> {
    let mut d = DataSet::default();
    let mut w = Wire::new(b);
    while let Some((num, v)) = w.next()? {
        match (num, v) {
            (1, W::Varint(n)) => d.num_columns = n,
            (2, W::Len(s)) if d.columns.len() < MAX_ITEMS => d.columns.push(text(s)),
            (3, W::Varint(n)) if d.types.len() < MAX_ITEMS => d.types.push(n as u32),
            (3, W::Len(s)) => {
                // `[packed = true]`, as some encoders write repeated numbers.
                let mut pos = 0;
                while pos < s.len() && d.types.len() < MAX_ITEMS {
                    d.types
                        .push(varint(s, &mut pos).map_err(|e| format!("data set types: {e}"))? as u32);
                }
            }
            (4, W::Len(s)) => {
                d.total_rows += 1;
                if d.rows.len() < MAX_ROWS {
                    let row = dataset_row(s).map_err(|e| format!("data set row {}: {e}", d.total_rows))?;
                    d.rows.push(row);
                }
            }
            _ => {}
        }
    }
    Ok(d)
}

fn dataset_row(b: &[u8]) -> Result<Vec<Value<'_>>, String> {
    let mut row = Vec::new();
    let mut w = Wire::new(b);
    while let Some((num, v)) = w.next()? {
        if let (1, W::Len(s)) = (num, v) {
            if row.len() == MAX_ITEMS {
                continue;
            }
            let mut val = Value::None;
            let mut w = Wire::new(s);
            while let Some((num, v)) = w.next()? {
                match (num, v) {
                    (7, _) => val = Value::Extension,
                    (n @ 1..=6, v) => {
                        if let Some(x) = simple(n - 1, v) {
                            val = x;
                        }
                    }
                    _ => {}
                }
            }
            row.push(val);
        }
    }
    Ok(row)
}

/// The file name in a metric's metadata.
fn file_name(b: &[u8]) -> Result<Option<Cow<'_, str>>, String> {
    let mut name = None;
    let mut w = Wire::new(b);
    while let Some((num, v)) = w.next()? {
        if let (5, W::Len(s)) = (num, v) {
            name = Some(text(s));
        }
    }
    Ok(name)
}

/// A property set as `(key, value)` text; nested sets in braces.
fn properties(b: &[u8], depth: usize) -> Result<Vec<(String, String)>, String> {
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut w = Wire::new(b);
    while let Some((num, v)) = w.next()? {
        match (num, v) {
            (1, W::Len(s)) if keys.len() < MAX_ITEMS => keys.push(text(s).into_owned()),
            (2, W::Len(s)) if values.len() < MAX_ITEMS => values.push(property_value(s, depth)?),
            _ => {}
        }
    }
    let mut values = values.into_iter();
    Ok(keys
        .into_iter()
        .map(|k| (k, values.next().unwrap_or_default()))
        .collect())
}

fn property_value(b: &[u8], depth: usize) -> Result<String, String> {
    let (mut ty, mut null, mut val) = (None, false, Value::None);
    let mut nested = None;
    let mut w = Wire::new(b);
    while let Some((num, v)) = w.next()? {
        match (num, v) {
            (1, W::Varint(n)) => ty = Some(n as u32),
            (2, W::Varint(n)) => null = n != 0,
            (9, W::Len(s)) if depth < MAX_DEPTH => nested = Some(braces(&properties(s, depth + 1)?)),
            (10, W::Len(s)) if depth < MAX_DEPTH => {
                let mut sets = Vec::new();
                let mut w = Wire::new(s);
                while let Some((num, v)) = w.next()? {
                    if let (1, W::Len(s)) = (num, v)
                        && sets.len() < MAX_ITEMS
                    {
                        sets.push(braces(&properties(s, depth + 1)?));
                    }
                }
                nested = Some(format!("[{}]", sets.join(", ")));
            }
            (9..=10, _) => nested = Some("…".into()),
            (11, _) => val = Value::Extension,
            (n @ 3..=8, v) => {
                if let Some(x) = simple(n - 3, v) {
                    val = x;
                }
            }
            _ => {}
        }
    }
    Ok(if null {
        "null".into()
    } else {
        nested.unwrap_or_else(|| value(ty, &val))
    })
}

fn braces(props: &[(String, String)]) -> String {
    let inner: Vec<String> = props.iter().map(|(k, v)| format!("{k}={v}")).collect();
    format!("{{{}}}", inner.join(", "))
}

// ── Rendering ──────────────────────────────────────────────────────────────

/// A metric with its name and type, from itself or from the birth.
struct Named<'m, 'a> {
    m: &'m Metric<'a>,
    /// The name, or `alias 7`.
    label: String,
    datatype: Option<u32>,
    /// An alias the birth (as far as seen) doesn't name.
    unknown: bool,
}

impl<'m, 'a> Named<'m, 'a> {
    fn of(m: &'m Metric<'a>, birth: Option<&Birth>) -> Named<'m, 'a> {
        let known = m.alias.and_then(|a| birth?.names.get(&a));
        let name = m
            .name
            .as_deref()
            .map(str::to_string)
            .or_else(|| known.map(|(n, _)| n.clone()));
        let datatype = m
            .datatype
            .or_else(|| known.and_then(|(_, dt)| *dt))
            .or_else(|| birth?.types.get(name.as_deref()?).copied());
        let unknown = name.is_none() && m.alias.is_some();
        let label = match (name, m.alias) {
            (Some(n), _) => n,
            (None, Some(a)) => format!("alias {a}"),
            (None, None) => "(no name)".into(),
        };
        Named {
            m,
            label,
            datatype,
            unknown,
        }
    }
}

fn metrics_table(d: &mut DocView, named: &[Named], payload_ts: Option<u64>, total: usize) {
    let any_alias = named.iter().any(|n| n.m.alias.is_some());
    let time_of = |n: &Named| n.m.timestamp.filter(|&t| Some(t) != payload_ts);
    let any_time = named.iter().any(|n| time_of(n).is_some());
    let any_flags = named.iter().any(|n| n.m.historical || n.m.transient);
    let any_props = named.iter().any(|n| !n.m.props.is_empty());

    let mut header: Vec<String> = vec!["name".into()];
    if any_alias {
        header.push("alias".into());
    }
    header.extend(["type".into(), "value".into()]);
    if any_time {
        header.push("time".into());
    }
    if any_flags {
        header.push("flags".into());
    }
    if any_props {
        header.push("properties".into());
    }

    let mut rows = Vec::new();
    let mut used = 0;
    for n in named {
        let m = n.m;
        let mut row = vec![Some(clip(n.label.clone()))];
        if any_alias {
            row.push(Some(m.alias.map(|a| a.to_string()).unwrap_or_default()));
        }
        row.push(Some(type_label(n.datatype, &m.value)));
        row.push(metric_value(m, n.datatype).map(clip));
        if any_time {
            row.push(Some(time_of(n).map(|t| when(t, payload_ts)).unwrap_or_default()));
        }
        if any_flags {
            let flags: Vec<&str> = [(m.historical, "historical"), (m.transient, "transient")]
                .iter()
                .filter(|(on, _)| *on)
                .map(|(_, f)| *f)
                .collect();
            row.push(Some(flags.join(", ")));
        }
        if any_props {
            let props: Vec<String> = m.props.iter().map(|(k, v)| format!("{k}={v}")).collect();
            row.push(Some(clip(props.join(", "))));
        }
        let len: usize = row.iter().map(|c| c.as_ref().map_or(4, |s| s.len()) + 1).sum();
        if !rows.is_empty() && (used + len > ROW_BUDGET || rows.len() == MAX_ROWS) {
            break;
        }
        used += len;
        rows.push(row);
    }
    let detail = if rows.len() < total {
        format!("· first {} of {}", rows.len(), commas(total as u64))
    } else {
        String::new()
    };
    d.section("Metrics", detail, Body::Table { header, rows });
}

fn dataset_section(d: &mut DocView, name: &str, ds: &DataSet) {
    let n = ds.width();
    let header: Vec<String> = (0..n)
        .map(|i| match ds.columns.get(i) {
            Some(c) => c.to_string(),
            None => format!("c{i}"),
        })
        .collect();
    let mut rows = Vec::new();
    let mut used = 0;
    for r in &ds.rows {
        let row: Vec<Option<String>> = (0..n)
            .map(|i| match r.get(i) {
                None | Some(Value::None) => None,
                Some(v) => Some(clip(value(ds.types.get(i).copied(), v))),
            })
            .collect();
        let len: usize = row.iter().map(|c| c.as_ref().map_or(4, |s| s.len()) + 1).sum();
        if !rows.is_empty() && used + len > ROW_BUDGET {
            break;
        }
        used += len;
        rows.push(row);
    }
    let detail = if rows.len() < ds.total_rows {
        format!("· first {} of {} rows", rows.len(), commas(ds.total_rows as u64))
    } else {
        format!("· {}", plural(ds.total_rows, "row", "rows"))
    };
    d.section(format!("DataSet {name}"), detail, Body::Table { header, rows });
}

fn template_sections(d: &mut DocView, name: &str, t: &Template) {
    let version = t
        .version
        .as_deref()
        .map(|v| format!(", version {v}"))
        .unwrap_or_default();
    let detail = match (&t.template_ref, t.definition) {
        (_, true) => format!("· definition{version}"),
        (Some(r), false) => format!("· instance of {r}{version}"),
        (None, false) => format!("· instance{version}"),
    };
    let header: Vec<String> = vec!["name".into(), "type".into(), "value".into()];
    let mut rows = Vec::new();
    let mut used = 0;
    for m in &t.metrics {
        let n = Named::of(m, None);
        let row = vec![
            Some(clip(n.label.clone())),
            Some(type_label(n.datatype, &m.value)),
            metric_value(m, n.datatype).map(clip),
        ];
        let len: usize = row.iter().map(|c| c.as_ref().map_or(4, |s| s.len()) + 1).sum();
        if !rows.is_empty() && (used + len > ROW_BUDGET || rows.len() == MAX_ROWS) {
            break;
        }
        used += len;
        rows.push(row);
    }
    let detail = if rows.len() < t.total {
        format!("{detail} · first {} of {} metrics", rows.len(), commas(t.total as u64))
    } else {
        detail
    };
    d.section(format!("Template {name}"), detail, Body::Table { header, rows });
    if !t.params.is_empty() {
        let rows = t
            .params
            .iter()
            .take(MAX_ROWS)
            .map(|p| {
                vec![
                    Some(clip(p.name.as_deref().unwrap_or("(no name)").to_string())),
                    Some(type_label(p.datatype, &p.value)),
                    Some(clip(value(p.datatype, &p.value))),
                ]
            })
            .collect();
        d.section(
            "Parameters",
            format!("· template {name}"),
            Body::Table {
                header: vec!["name".into(), "type".into(), "value".into()],
                rows,
            },
        );
    }
}

/// Its type's name, or one the value field implies (integers are ambiguous).
fn type_label(dt: Option<u32>, v: &Value) -> String {
    match dt {
        Some(dt) => match TYPES.get(dt as usize) {
            Some(name) => (*name).to_string(),
            None => format!("type {dt}"),
        },
        None => match v {
            Value::Float(_) => "Float",
            Value::Double(_) => "Double",
            Value::Bool(_) => "Boolean",
            Value::Str(_) => "String",
            Value::Bytes(_) => "Bytes",
            Value::DataSet(_) => "DataSet",
            Value::Template(_) => "Template",
            Value::None => "",
            Value::Int(_) | Value::Long(_) | Value::Extension => "?",
        }
        .to_string(),
    }
}

/// A metric's value as text; `None` when it's null.
fn metric_value(m: &Metric, dt: Option<u32>) -> Option<String> {
    if m.null {
        return None;
    }
    Some(match (&m.value, &m.file_name) {
        (Value::Bytes(b), Some(f)) if dt == Some(FILE) => format!("{f} · {}", human_bytes(b.len())),
        (v, _) => value(dt, v),
    })
}

/// A value as text, read as type `dt` when known.
fn value(dt: Option<u32>, v: &Value) -> String {
    match v {
        Value::None => String::new(),
        Value::Int(n) => integer(dt, u64::from(*n)),
        Value::Long(n) => integer(dt, *n),
        Value::Float(f) => f.to_string(),
        Value::Double(f) => f.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Str(s) => s.to_string(),
        Value::Bytes(b) => match dt {
            Some(dt @ INT8_ARRAY..=DATETIME_ARRAY) => array(dt, b),
            _ => bytes(b),
        },
        Value::DataSet(d) => format!(
            "{} × {}",
            plural(d.width(), "column", "columns"),
            plural(d.total_rows, "row", "rows")
        ),
        Value::Template(t) => {
            let what = match (&t.template_ref, t.definition) {
                (_, true) => "definition".to_string(),
                (Some(r), false) => format!("{r} instance"),
                (None, false) => "instance".into(),
            };
            format!("{what} · {}", plural(t.total, "metric", "metrics"))
        }
        Value::Extension => "extension".into(),
    }
}

/// Signed types are two's complement in the low bits (Eclipse Tahu sign-
/// extends them to 32 bits, others don't).
fn integer(dt: Option<u32>, n: u64) -> String {
    match dt {
        Some(INT8) => (n as u8 as i8).to_string(),
        Some(INT16) => (n as u16 as i16).to_string(),
        Some(INT32) => (n as u32 as i32).to_string(),
        Some(INT64) => (n as i64).to_string(),
        Some(DATETIME) => datetime(n),
        _ => n.to_string(),
    }
}

/// Milliseconds since the epoch as local time, like the rest of the UI;
/// past the year 9999 as the number.
fn datetime(ms: u64) -> String {
    const MAX_MS: u64 = 253_402_300_799_999;
    if ms > MAX_MS {
        return format!("{ms} ms");
    }
    clock(UNIX_EPOCH + Duration::from_millis(ms), true)
}

/// A metric's own timestamp: the time of day if it's the payload's day.
fn when(ms: u64, payload_ts: Option<u64>) -> String {
    let full = datetime(ms);
    match payload_ts.map(datetime) {
        Some(p) if p.get(..10) == full.get(..10) => full.get(11..).unwrap_or(&full).to_string(),
        _ => full,
    }
}

/// Sparkplug 3.0 arrays: packed little-endian in `bytes_value`.
fn array(dt: u32, b: &[u8]) -> String {
    let fixed = |size: usize, f: &dyn Fn(&[u8]) -> String| {
        if !b.len().is_multiple_of(size) {
            return format!(
                "<{}, not a whole number of {size}-byte values>",
                plural(b.len(), "byte", "bytes")
            );
        }
        list(b.chunks_exact(size).map(f), b.len() / size)
    };
    let le4 = |c: &[u8]| <[u8; 4]>::try_from(c).unwrap_or_default();
    let le8 = |c: &[u8]| <[u8; 8]>::try_from(c).unwrap_or_default();
    match dt {
        INT8_ARRAY => fixed(1, &|c| (c[0] as i8).to_string()),
        INT16_ARRAY => fixed(2, &|c| i16::from_le_bytes([c[0], c[1]]).to_string()),
        INT32_ARRAY => fixed(4, &|c| i32::from_le_bytes(le4(c)).to_string()),
        INT64_ARRAY => fixed(8, &|c| i64::from_le_bytes(le8(c)).to_string()),
        UINT8_ARRAY => fixed(1, &|c| c[0].to_string()),
        UINT16_ARRAY => fixed(2, &|c| u16::from_le_bytes([c[0], c[1]]).to_string()),
        UINT32_ARRAY => fixed(4, &|c| u32::from_le_bytes(le4(c)).to_string()),
        UINT64_ARRAY => fixed(8, &|c| u64::from_le_bytes(le8(c)).to_string()),
        FLOAT_ARRAY => fixed(4, &|c| f32::from_le_bytes(le4(c)).to_string()),
        DOUBLE_ARRAY => fixed(8, &|c| f64::from_le_bytes(le8(c)).to_string()),
        DATETIME_ARRAY => fixed(8, &|c| datetime(u64::from_le_bytes(le8(c)))),
        BOOLEAN_ARRAY => bools(b),
        STRING_ARRAY => strings(b),
        _ => bytes(b),
    }
}

/// A little-endian uint32 count, then the values packed into bytes, the
/// first in the top bit.
fn bools(b: &[u8]) -> String {
    let Some((head, bits)) = b.split_first_chunk::<4>() else {
        return bytes(b);
    };
    let n = u32::from_le_bytes(*head) as usize;
    let have = n.min(bits.len().saturating_mul(8));
    let shown = list(
        (0..have).map(|i| (bits[i / 8] & (0x80 >> (i % 8)) != 0).to_string()),
        have,
    );
    if have < n { format!("{shown} (of {n})") } else { shown }
}

/// Zero-terminated UTF-8 strings.
fn strings(b: &[u8]) -> String {
    if b.is_empty() {
        return "[]".into();
    }
    let body = b.strip_suffix(&[0]).unwrap_or(b);
    let parts: Vec<&[u8]> = body.split(|&c| c == 0).collect();
    list(parts.iter().map(|s| format!("{:?}", text(s))), parts.len())
}

fn list(items: impl Iterator<Item = String>, n: usize) -> String {
    let mut out = String::from("[");
    for (i, s) in items.take(MAX_LIST).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&s);
    }
    if n > MAX_LIST {
        out.push_str(", …");
    }
    out.push(']');
    out
}

fn bytes(b: &[u8]) -> String {
    if b.is_empty() {
        return "<0 bytes>".into();
    }
    let mut h = hex(&b[..b.len().min(MAX_HEX)]);
    if b.len() > MAX_HEX {
        h.push('…');
    }
    format!("<{}: {h}>", plural(b.len(), "byte", "bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/sparkplug/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    const NBIRTH: &str = "spBv1.0/Plant1/NBIRTH/Gateway1";
    const NDATA: &str = "spBv1.0/Plant1/NDATA/Gateway1";
    const DBIRTH: &str = "spBv1.0/Plant1/DBIRTH/Gateway1/Meter3";
    const DDATA: &str = "spBv1.0/Plant1/DDATA/Gateway1/Meter3";
    const T0: u64 = 1_792_059_630_123;
    const NOTE: &str = "names come from the birth certificate (BIRTH), sent before fss-mqtt connected; they appear after the next birth";

    fn summary(v: &DocView) -> String {
        v.summary
            .iter()
            .map(|f| format!("{} {}", f.value, f.label).trim().to_string())
            .collect::<Vec<_>>()
            .join(" · ")
    }

    fn meta(v: &DocView, key: &str) -> Option<String> {
        v.meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    /// The table in section `title`: its header, and rows with nulls as "null".
    fn table(v: &DocView, title: &str) -> (Vec<String>, Vec<Vec<String>>) {
        let s = v
            .sections
            .iter()
            .find(|s| s.title == title)
            .unwrap_or_else(|| panic!("no section {title:?}"));
        match &s.body {
            Body::Table { header, rows } => (
                header.clone(),
                rows.iter()
                    .map(|r| r.iter().map(|c| c.clone().unwrap_or_else(|| "null".into())).collect())
                    .collect(),
            ),
            _ => panic!("{title} isn't a table"),
        }
    }

    /// Rows of the Metrics table by name: (type, value) and the other columns.
    fn metrics(v: &DocView) -> HashMap<String, Vec<String>> {
        let (header, rows) = table(v, "Metrics");
        rows.into_iter()
            .map(|r| {
                let cols: Vec<String> = header
                    .iter()
                    .skip(1)
                    .cloned()
                    .zip(r.iter().skip(1).cloned())
                    .map(|(h, c)| format!("{h}={c}"))
                    .collect();
                (r[0].clone(), cols)
            })
            .collect()
    }

    fn get(m: &HashMap<String, Vec<String>>, name: &str, col: &str) -> String {
        let prefix = format!("{col}=");
        m.get(name)
            .unwrap_or_else(|| panic!("no metric {name}"))
            .iter()
            .find_map(|c| c.strip_prefix(&prefix).map(str::to_string))
            .unwrap_or_else(|| panic!("no column {col}"))
    }

    fn local(ms: u64) -> String {
        datetime(ms)
    }

    #[test]
    fn topics() {
        let t = parse_topic(DDATA).unwrap();
        assert_eq!(
            (t.group, t.kind, t.node, t.device),
            ("Plant1", Kind::DData, "Gateway1", Some("Meter3"))
        );
        assert_eq!(t.key(), "Plant1/Gateway1/Meter3");
        let t = parse_topic(NBIRTH).unwrap();
        assert_eq!(
            (t.kind, t.device, t.key()),
            (Kind::NBirth, None, "Plant1/Gateway1".into())
        );
        for not in [
            "spBv1.0/STATE/scada1",
            "spBv1.0/Plant1/NDATA",
            "spBv1.0/Plant1/NDATA/",
            "spBv1.0//NDATA/Gateway1",
            "spBv1.0/Plant1/NDATA/Gateway1/Meter3/extra",
            "spBv1.0/Plant1/NOTE/Gateway1",
            "spAv1.0/Plant1/NDATA/Gateway1",
            "site/spBv1.0/Plant1/NDATA/Gateway1",
            "v1/plant/line1/temp",
        ] {
            assert!(parse_topic(not).is_none(), "{not}");
        }
        assert!(
            Aliases::default()
                .decode("spBv1.0/STATE/scada1", b"{\"online\":true}")
                .is_none()
        );
    }

    #[test]
    fn birth_shows_every_type() {
        let mut a = Aliases::default();
        let v = a.decode(NBIRTH, &fixture("nbirth.bin")).unwrap();
        assert_eq!(v.format, "Sparkplug B");
        assert_eq!(summary(&v), "NBIRTH node birth · 36 metrics · seq 0");
        assert_eq!(meta(&v, "group").as_deref(), Some("Plant1"));
        assert_eq!(meta(&v, "edge node").as_deref(), Some("Gateway1"));
        assert_eq!(meta(&v, "device"), None);
        assert_eq!(meta(&v, "timestamp"), Some(local(T0)));
        assert_eq!(meta(&v, "bdSeq").as_deref(), Some("3"));
        assert!(v.notes.is_empty(), "{:?}", v.notes);

        let (header, _) = table(&v, "Metrics");
        assert_eq!(header, ["name", "alias", "type", "value", "flags", "properties"]);
        let m = metrics(&v);
        let cases: [(&str, &str, &str); 34] = [
            ("bdSeq", "Int64", "3"),
            ("Node Control/Rebirth", "Boolean", "false"),
            ("Inputs/Int8", "Int8", "-23"),
            ("Inputs/Int16", "Int16", "-30000"),
            ("Inputs/Int32", "Int32", "-1"),
            ("Inputs/Int64", "Int64", "-4270929666821191986"),
            ("Inputs/UInt8", "UInt8", "250"),
            ("Inputs/UInt16", "UInt16", "52360"),
            ("Inputs/UInt32", "UInt32", "3293969225"),
            ("Inputs/UInt64", "UInt64", "16444743074749521625"),
            ("Inputs/Float", "Float", "1.23"),
            ("Inputs/Double", "Double", "12.354213"),
            ("Inputs/Boolean", "Boolean", "true"),
            ("Inputs/String", "String", "hello"),
            ("Inputs/Text", "Text", "longer text"),
            ("Inputs/UUID", "UUID", "1b4e28ba-2fa1-11d2-883f-0016d3cca427"),
            ("Inputs/Bytes", "Bytes", "<4 bytes: 0001feff>"),
            ("Inputs/File", "File", "setpoints.csv · 24 B"),
            ("Inputs/Null", "Int32", "null"),
            ("Arrays/Int8", "Int8Array", "[-23, 123]"),
            ("Arrays/Int16", "Int16Array", "[-30000, 30000]"),
            ("Arrays/Int32", "Int32Array", "[-1, 315338746]"),
            (
                "Arrays/Int64",
                "Int64Array",
                "[-4270929666821191986, -3601064768563266876]",
            ),
            ("Arrays/UInt8", "UInt8Array", "[23, 250]"),
            ("Arrays/UInt16", "UInt16Array", "[30, 52360]"),
            ("Arrays/UInt32", "UInt32Array", "[52, 3293969225]"),
            ("Arrays/UInt64", "UInt64Array", "[52, 16444743074749521625]"),
            ("Arrays/Float", "FloatArray", "[1.23, 89.341]"),
            ("Arrays/Double", "DoubleArray", "[12.354213, 1022.9123213]"),
            (
                "Arrays/Boolean",
                "BooleanArray",
                "[false, false, true, true, false, true, false, false, true, true, false, true]",
            ),
            ("Arrays/String", "StringArray", "[\"ABC\", \"hello\"]"),
            ("Recipes/Current", "DataSet", "4 columns × 3 rows"),
            ("_types_/Motor", "Template", "definition · 2 metrics"),
            ("Motors/Motor1", "Template", "Motor instance · 2 metrics"),
        ];
        for (name, ty, val) in cases {
            assert_eq!(get(&m, name, "type"), ty, "{name}");
            assert_eq!(get(&m, name, "value"), clip(val.to_string()), "{name}");
        }
        assert_eq!(get(&m, "Inputs/DateTime", "value"), local(1_256_102_875_335));
        assert_eq!(
            get(&m, "Arrays/DateTime", "value"),
            format!("[{}, {}]", local(1_256_102_875_335), local(1_656_107_875_000))
        );
        assert_eq!(get(&m, "Inputs/Int8", "alias"), "10");
        assert_eq!(get(&m, "bdSeq", "alias"), "");
        assert_eq!(get(&m, "Inputs/Text", "flags"), "transient");
        assert_eq!(get(&m, "Inputs/Float", "properties"), "engUnit=kW, Quality=192");

        let (header, rows) = table(&v, "DataSet Recipes/Current");
        assert_eq!(header, ["step", "temp", "ok", "label"]);
        assert_eq!(
            rows,
            [
                ["1", "20.5", "true", "heat"],
                ["2", "21.25", "true", "hold"],
                ["3", "-4.5", "false", "cool"]
            ]
        );
        let (header, rows) = table(&v, "Template _types_/Motor");
        assert_eq!(header, ["name", "type", "value"]);
        assert_eq!(rows, [["RPM", "Float", ""], ["Amps", "Int32", ""]]);
        let (_, rows) = table(&v, "Template Motors/Motor1");
        assert_eq!(rows, [["RPM", "Float", "1450.5"], ["Amps", "Int32", "-12"]]);
        let details: Vec<&str> = v.sections.iter().map(|s| s.detail.as_str()).collect();
        assert!(details.contains(&"· instance of Motor, version 1.0"), "{details:?}");
        assert!(details.contains(&"· definition, version 1.0"), "{details:?}");
        let params: Vec<_> = v.sections.iter().filter(|s| s.title == "Parameters").collect();
        assert_eq!(params.len(), 2);
        assert_eq!(params[1].detail, "· template Motors/Motor1");
        let (_, rows) = table(&v, "Parameters");
        assert_eq!(rows, [["Index", "Int32", "0"]]);
    }

    #[test]
    fn aliases_get_their_names_from_the_birth() {
        let mut a = Aliases::default();
        // Data before any birth: aliases only, explained.
        let v = a.decode(NDATA, &fixture("ndata.bin")).unwrap();
        let m = metrics(&v);
        assert!(m.contains_key("alias 10") && m.contains_key("alias 99"));
        assert_eq!(get(&m, "alias 10", "type"), "?");
        assert_eq!(get(&m, "alias 10", "value"), "4294967272");
        assert_eq!(get(&m, "alias 18", "type"), "Float");
        assert_eq!(v.notes, [NOTE.replace("BIRTH", "NBIRTH")]);

        // After the birth: names and types, so signed values read right.
        a.decode(NBIRTH, &fixture("nbirth.bin")).unwrap();
        let v = a.decode(NDATA, &fixture("ndata.bin")).unwrap();
        assert_eq!(summary(&v), "NDATA node data · 6 metrics · seq 1");
        let (header, rows) = table(&v, "Metrics");
        assert_eq!(header, ["name", "alias", "type", "value", "time", "flags"]);
        let names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(
            names,
            [
                "Inputs/Int8",
                "Inputs/Float",
                "Inputs/String",
                "Inputs/Int64",
                "Arrays/Int8",
                "alias 99"
            ]
        );
        let m = metrics(&v);
        assert_eq!(get(&m, "Inputs/Int8", "value"), "-24");
        assert_eq!(get(&m, "Inputs/Int8", "type"), "Int8");
        assert_eq!(get(&m, "Inputs/Float", "value"), "2.5");
        assert_eq!(get(&m, "Inputs/String", "value"), "world");
        assert_eq!(get(&m, "Inputs/Int64", "value"), "-5");
        assert_eq!(get(&m, "Inputs/Int64", "flags"), "historical");
        assert_eq!(get(&m, "Inputs/Int64", "time"), when(T0 - 60_000, Some(T0 + 1000)));
        assert_eq!(get(&m, "Inputs/Float", "time"), "");
        assert_eq!(get(&m, "Arrays/Int8", "value"), "[1, -1]");
        // An alias the birth didn't define stays a number, and the note says why.
        assert_eq!(get(&m, "alias 99", "type"), "?");
        assert_eq!(get(&m, "alias 99", "value"), "7");
        assert_eq!(v.notes, ["aliases shown as numbers aren't defined in the last NBIRTH"]);
    }

    #[test]
    fn devices_have_their_own_births() {
        let mut a = Aliases::default();
        a.decode(NBIRTH, &fixture("nbirth.bin")).unwrap();
        let v = a.decode(DDATA, &fixture("ddata.bin")).unwrap();
        assert!(metrics(&v).contains_key("alias 100"));
        assert_eq!(v.notes, [NOTE.replace("BIRTH", "DBIRTH")]);

        let v = a.decode(DBIRTH, &fixture("dbirth.bin")).unwrap();
        assert_eq!(summary(&v), "DBIRTH device birth · 3 metrics · seq 2");
        assert_eq!(meta(&v, "device").as_deref(), Some("Meter3"));
        let v = a.decode(DDATA, &fixture("ddata.bin")).unwrap();
        assert_eq!(summary(&v), "DDATA device data · 2 metrics · seq 3");
        let m = metrics(&v);
        assert_eq!(get(&m, "Power", "type"), "Double");
        assert_eq!(get(&m, "Power", "value"), "1250.25");
        assert_eq!(get(&m, "Status", "value"), "IDLE");
        assert!(v.notes.is_empty(), "{:?}", v.notes);
        // Another device of the node, or another node, doesn't share them.
        let other = a
            .decode("spBv1.0/Plant1/DDATA/Gateway1/Meter4", &fixture("ddata.bin"))
            .unwrap();
        assert!(metrics(&other).contains_key("alias 100"));
        let other = a
            .decode("spBv1.0/Plant2/DDATA/Gateway1/Meter3", &fixture("ddata.bin"))
            .unwrap();
        assert!(metrics(&other).contains_key("alias 100"));
        // A node's birth forgets its devices (they're born again after it),
        // not other nodes' devices.
        a.decode("spBv1.0/Plant1/DBIRTH/Gateway2/Meter3", &fixture("dbirth.bin"))
            .unwrap();
        a.decode(NBIRTH, &fixture("nbirth.bin")).unwrap();
        let v = a.decode(DDATA, &fixture("ddata.bin")).unwrap();
        assert!(metrics(&v).contains_key("alias 100"));
        let v = a
            .decode("spBv1.0/Plant1/DDATA/Gateway2/Meter3", &fixture("ddata.bin"))
            .unwrap();
        assert!(metrics(&v).contains_key("Power"));
        assert!(metrics(&a.decode(NDATA, &fixture("ndata.bin")).unwrap()).contains_key("Inputs/Int8"));
    }

    #[test]
    fn the_limit_note_follows_what_is_remembered() {
        use enc::*;
        // A birth naming metrics `aliases` (each with an alias and a type), and data with one alias.
        let birth = |aliases: std::ops::RangeInclusive<u64>| -> Vec<u8> {
            aliases
                .flat_map(|n| {
                    let m = [
                        bytes_field(1, format!("m{n}").as_bytes()),
                        varint_field(2, n),
                        varint_field(4, 7),
                    ];
                    bytes_field(2, &m.concat())
                })
                .collect()
        };
        let data = |alias: u64| bytes_field(2, &[varint_field(2, alias), varint_field(10, 1)].concat());
        let notes = |a: &mut Aliases, topic: &str, alias: u64| a.decode(topic, &data(alias)).unwrap().notes;
        let (d1, d2) = ("spBv1.0/Plant1/DBIRTH/Gateway1/D1", "spBv1.0/Plant1/DBIRTH/Gateway1/D2");
        let d2_data = "spBv1.0/Plant1/DDATA/Gateway1/D2";
        let limit = "fss-mqtt's limit of 4 remembered metric names (over all edge nodes and devices)";
        let before = NOTE.replace("BIRTH", "NBIRTH");

        // Fill it: D1's birth fits, D2's is cut after two metrics.
        let mut a = Aliases {
            limit: 4,
            ..Aliases::default()
        };
        a.decode(d1, &birth(1..=2)).unwrap();
        a.decode(d2, &birth(11..=13)).unwrap();
        assert_eq!(a.size, 4);
        assert!(notes(&mut a, d2_data, 12).is_empty());
        assert_eq!(
            notes(&mut a, d2_data, 13),
            [format!("only part of the DBIRTH is remembered: {limit} was reached")]
        );
        // An edge node born before fss-mqtt connected: its next birth won't fit now.
        assert_eq!(
            notes(&mut a, NDATA, 5),
            [before.clone(), format!("{limit} is reached: a new birth won't fit")]
        );

        // The node's birth forgets its devices: there's room again, and the notes say so.
        a.decode(NBIRTH, &birth(5..=5)).unwrap();
        assert_eq!(a.size, 1);
        assert_eq!(notes(&mut a, "spBv1.0/Plant1/NDATA/Gateway2", 7), [before]);
        assert_eq!(notes(&mut a, d2_data, 13), [NOTE.replace("BIRTH", "DBIRTH")]);
        // Born again, D2 fits whole.
        a.decode(d2, &birth(11..=13)).unwrap();
        assert!(notes(&mut a, d2_data, 13).is_empty());
        // A birth that was kept whole, used with an alias it doesn't define.
        assert_eq!(
            notes(&mut a, NDATA, 6),
            ["aliases shown as numbers aren't defined in the last NBIRTH"]
        );
    }

    #[test]
    fn death_and_command() {
        let mut a = Aliases::default();
        let v = a
            .decode("spBv1.0/Plant1/NDEATH/Gateway1", &fixture("ndeath.bin"))
            .unwrap();
        assert_eq!(summary(&v), "NDEATH node death · 1 metric");
        assert_eq!(meta(&v, "bdSeq").as_deref(), Some("3"));
        assert_eq!(meta(&v, "timestamp"), Some(local(T0 + 4000)));
        let v = a.decode("spBv1.0/Plant1/NCMD/Gateway1", &fixture("ncmd.bin")).unwrap();
        assert_eq!(summary(&v), "NCMD node command · 1 metric");
        let m = metrics(&v);
        assert_eq!(get(&m, "Node Control/Rebirth", "value"), "true");
        assert_eq!(get(&m, "Node Control/Rebirth", "type"), "Boolean");
        // A device death has no metrics.
        let v = a
            .decode("spBv1.0/Plant1/DDEATH/Gateway1/Meter3", &[0x08, 0x01, 0x18, 0x05])
            .unwrap();
        assert_eq!(summary(&v), "DDEATH device death · 0 metrics · seq 5");
        assert!(v.sections.is_empty());
    }

    #[test]
    fn compressed_payloads() {
        let mut a = Aliases::default();
        a.decode(NBIRTH, &fixture("nbirth.bin")).unwrap();
        let plain = a.decode(NDATA, &fixture("ndata.bin")).unwrap();
        let v = a.decode(NDATA, &fixture("ndata-gzip.bin")).unwrap();
        assert!(
            summary(&v).starts_with("NDATA node data · 6 metrics · seq 1 · gzip "),
            "{}",
            summary(&v)
        );
        assert_eq!(table(&v, "Metrics"), table(&plain, "Metrics"));
        assert!(!v.sections.iter().any(|s| s.title == "Body"));

        // DEFLATE: zlib, as Java's Deflater and Python's zlib write it.
        let inner = fixture("ndata.bin");
        let mut p = enc::varint_field(1, T0);
        p.extend(enc::bytes_field(4, COMPRESSED.as_bytes()));
        p.extend(enc::bytes_field(
            5,
            &miniz_oxide::deflate::compress_to_vec_zlib(&inner, 6),
        ));
        let mut algo = enc::bytes_field(1, b"algorithm");
        algo.extend(enc::varint_field(4, 12));
        algo.extend(enc::bytes_field(15, b"DEFLATE"));
        p.extend(enc::bytes_field(2, &algo));
        let v = a.decode(NDATA, &p).unwrap();
        assert!(summary(&v).contains("· deflate "), "{}", summary(&v));
        assert_eq!(table(&v, "Metrics"), table(&plain, "Metrics"));
        // Unknown algorithms and broken bodies are errors with the raw bytes.
        let mut lz = enc::bytes_field(4, COMPRESSED.as_bytes());
        lz.extend(enc::bytes_field(5, b"x"));
        lz.extend(enc::bytes_field(
            2,
            &[enc::bytes_field(1, b"algorithm"), enc::bytes_field(15, b"lz4")].concat(),
        ));
        let v = a.decode(NDATA, &lz).unwrap();
        assert_eq!(
            v.notes,
            ["could not read this Sparkplug B payload: compressed with LZ4, which isn't supported"]
        );
        let mut broken = enc::bytes_field(4, COMPRESSED.as_bytes());
        broken.extend(enc::bytes_field(5, b"not deflate"));
        let v = a.decode(NDATA, &broken).unwrap();
        assert_eq!(v.sections[0].title, "Raw bytes");
        assert!(v.notes[0].contains("deflate: corrupt data"), "{:?}", v.notes);
    }

    #[test]
    fn hand_built_values() {
        use enc::*;
        let metric = |fields: &[Vec<u8>]| bytes_field(2, &fields.concat());
        let mut p = varint_field(1, T0);
        // Int8 -23 the way pysparkplug writes it (8-bit two's complement), and
        // Int32 -2 as a sign-extended 64-bit varint.
        p.extend(metric(&[
            bytes_field(1, b"i8"),
            varint_field(4, 1),
            varint_field(10, 233),
        ]));
        p.extend(metric(&[
            bytes_field(1, b"i32"),
            varint_field(4, 3),
            varint_field(10, (-2i64) as u64),
        ]));
        p.extend(metric(&[
            bytes_field(1, b"i16"),
            varint_field(4, 2),
            varint_field(11, 0xffff_ffff_ffff_8ad0),
        ]));
        // An unknown type, an extension value and a value of the wrong wire type.
        p.extend(metric(&[
            bytes_field(1, b"new"),
            varint_field(4, 99),
            varint_field(10, 5),
        ]));
        p.extend(metric(&[bytes_field(1, b"ext"), bytes_field(19, b"\x08\x01")]));
        p.extend(metric(&[
            bytes_field(1, b"odd"),
            varint_field(4, 12),
            varint_field(15, 7),
        ]));
        // Arrays that don't add up.
        p.extend(metric(&[
            bytes_field(1, b"a16"),
            varint_field(4, 23),
            bytes_field(16, &[1, 2, 3]),
        ]));
        p.extend(metric(&[
            bytes_field(1, b"bools"),
            varint_field(4, 32),
            bytes_field(16, &[10, 0, 0, 0, 0xff]),
        ]));
        p.extend(metric(&[
            bytes_field(1, b"nobools"),
            varint_field(4, 32),
            bytes_field(16, &[1, 0]),
        ]));
        p.extend(metric(&[
            bytes_field(1, b"strs"),
            varint_field(4, 33),
            bytes_field(16, b""),
        ]));
        p.extend(metric(&[
            bytes_field(1, b"strs2"),
            varint_field(4, 33),
            bytes_field(16, b"a\0b"),
        ]));
        let many: Vec<u8> = (0..40).collect();
        p.extend(metric(&[
            bytes_field(1, b"many"),
            varint_field(4, 26),
            bytes_field(16, &many),
        ]));
        p.extend(metric(&[
            bytes_field(1, b"future"),
            varint_field(4, 13),
            varint_field(11, u64::MAX),
        ]));
        p.extend(metric(&[bytes_field(1, b"blob"), bytes_field(16, &[0xab; 20])]));
        p.extend(metric(&[bytes_field(1, b"empty")]));
        // Properties: null, nested sets, a list of sets.
        let pv = |fields: &[Vec<u8>]| bytes_field(2, &fields.concat());
        let inner = [bytes_field(1, b"x"), pv(&[varint_field(1, 7), varint_field(3, 1)])].concat();
        let props = [
            bytes_field(1, b"a"),
            bytes_field(1, b"b"),
            bytes_field(1, b"c"),
            bytes_field(1, b"d"),
            pv(&[varint_field(1, 12), varint_field(2, 1)]),
            pv(&[varint_field(1, 20), bytes_field(9, &inner)]),
            pv(&[varint_field(1, 21), bytes_field(10, &bytes_field(1, &inner))]),
        ]
        .concat();
        p.extend(metric(&[
            bytes_field(1, b"props"),
            bytes_field(9, &props),
            varint_field(14, 1),
        ]));

        let v = Aliases::default().decode(NDATA, &p).unwrap();
        assert!(v.notes.is_empty(), "{:?}", v.notes);
        let m = metrics(&v);
        let val = |n: &str| get(&m, n, "value");
        assert_eq!(val("i8"), "-23");
        assert_eq!(val("i32"), "-2");
        assert_eq!(val("i16"), "-30000");
        assert_eq!((get(&m, "new", "type"), val("new")), ("type 99".into(), "5".into()));
        assert_eq!((get(&m, "ext", "type"), val("ext")), ("?".into(), "extension".into()));
        assert_eq!(val("odd"), "");
        assert_eq!(val("a16"), "<3 bytes, not a whole number of 2-byte values>");
        assert_eq!(val("bools"), "[true, true, true, true, true, true, true, true] (of 10)");
        assert_eq!(val("nobools"), "<2 bytes: 0100>");
        assert_eq!(val("strs"), "[]");
        assert_eq!(val("strs2"), "[\"a\", \"b\"]");
        assert_eq!(
            val("many"),
            clip("[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, …]".into())
        );
        assert_eq!(val("future"), format!("{} ms", u64::MAX));
        assert_eq!(
            (get(&m, "blob", "type"), val("blob")),
            ("Bytes".into(), "<20 bytes: abababababababab…>".into())
        );
        assert_eq!((get(&m, "empty", "type"), val("empty")), ("".into(), "".into()));
        assert_eq!(get(&m, "props", "properties"), "a=null, b={x=1}, c=[{x=1}], d=");
    }

    #[test]
    fn damaged_payloads() {
        let mut a = Aliases::default();
        // Nothing readable: an error view with the raw bytes.
        let v = a.decode(NDATA, &[0xff]).unwrap();
        assert_eq!(v.sections[0].title, "Raw bytes");
        assert_eq!(
            v.notes,
            ["could not read this Sparkplug B payload: truncated varint at byte 0"]
        );
        // Metrics, then garbage: those metrics and a note; a broken birth isn't learned.
        let mut b = fixture("dbirth.bin");
        b.truncate(b.len() - 3);
        let v = a.decode(DBIRTH, &b).unwrap();
        assert_eq!(table(&v, "Metrics").1.len(), 2);
        assert!(
            v.notes[0].starts_with("malformed after 2 metrics: ") && v.notes[0].ends_with("runs past the end"),
            "{:?}",
            v.notes
        );
        assert!(metrics(&a.decode(DDATA, &fixture("ddata.bin")).unwrap()).contains_key("alias 100"));
        // An empty payload is a valid message without fields.
        let v = a.decode(NDATA, b"").unwrap();
        assert_eq!(summary(&v), "NDATA node data · 0 metrics");
        // Templates nested too deep: a metric holding a template holding a metric …
        let mut m = enc::bytes_field(1, b"leaf");
        for _ in 0..20 {
            m = enc::bytes_field(18, &enc::bytes_field(2, &m));
        }
        let v = a.decode(NDATA, &enc::bytes_field(2, &m)).unwrap();
        assert!(v.notes[0].contains("templates nested deeper than 8"), "{:?}", v.notes);
    }

    #[test]
    fn output_is_capped() {
        let mut p = Vec::new();
        for i in 0..5000u64 {
            let mut m = enc::bytes_field(1, format!("Folder/Metric {i:04}").as_bytes());
            m.extend(enc::varint_field(2, i));
            m.extend(enc::varint_field(4, 7));
            m.extend(enc::varint_field(10, i * 3));
            p.extend(enc::bytes_field(2, &m));
        }
        let mut a = Aliases::default();
        let v = a.decode(NBIRTH, &p).unwrap();
        assert_eq!(summary(&v), "NBIRTH node birth · 5,000 metrics");
        let s = &v.sections[0];
        assert!(
            s.detail.starts_with("· first ") && s.detail.ends_with(" of 5,000"),
            "{}",
            s.detail
        );
        let (_, rows) = table(&v, "Metrics");
        assert!(rows.len() > 50 && rows.len() <= MAX_ROWS, "{}", rows.len());
        // Every alias is remembered, not just the ones shown.
        let late = enc::bytes_field(2, &[enc::varint_field(2, 4999), enc::varint_field(10, 1)].concat());
        let v = a.decode(NDATA, &late).unwrap();
        assert!(metrics(&v).contains_key("Folder/Metric 4999"));
    }

    #[test]
    fn never_panics_on_damaged_input() {
        let goods = ["nbirth.bin", "ndata.bin", "ndata-gzip.bin"].map(fixture);
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut a = Aliases::default();
        for i in 0..3000 {
            let good = &goods[i % goods.len()];
            let mut f = good.clone();
            for _ in 0..(rnd() % 8 + 1) {
                let i = (rnd() as usize) % f.len();
                f[i] = rnd() as u8;
            }
            let _ = a.decode(NBIRTH, &f);
            let _ = a.decode(NDATA, &f);
            let cut = (rnd() as usize) % f.len();
            let _ = a.decode(DBIRTH, &f[..cut]);
            let _ = a.decode(DDATA, &f[..cut]);
            let noise: Vec<u8> = (0..rnd() % 300).map(|_| rnd() as u8).collect();
            let _ = a.decode(NDATA, &noise);
        }
        // Deep and wide nesting.
        let mut b = Vec::new();
        for _ in 0..200 {
            b = enc::bytes_field(2, &enc::bytes_field(18, &b));
        }
        let _ = a.decode(NDATA, &b);
        let mut props = Vec::new();
        for _ in 0..200 {
            props = [
                enc::bytes_field(1, b"k"),
                enc::bytes_field(2, &enc::bytes_field(9, &props)),
            ]
            .concat();
        }
        let _ = a.decode(NDATA, &enc::bytes_field(2, &enc::bytes_field(9, &props)));
        let _ = a.decode(NDATA, &[0x12; 10_000]);
    }

    /// Writes protobuf fields for hand-built payloads.
    mod enc {
        pub fn varint(mut n: u64, out: &mut Vec<u8>) {
            loop {
                let b = (n & 0x7f) as u8;
                n >>= 7;
                if n == 0 {
                    out.push(b);
                    return;
                }
                out.push(b | 0x80);
            }
        }

        pub fn varint_field(num: u64, n: u64) -> Vec<u8> {
            let mut out = Vec::new();
            varint(num << 3, &mut out);
            varint(n, &mut out);
            out
        }

        pub fn bytes_field(num: u64, b: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            varint(num << 3 | 2, &mut out);
            varint(b.len() as u64, &mut out);
            out.extend_from_slice(b);
            out
        }
    }
}
