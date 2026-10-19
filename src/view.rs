//! A decoded payload, ready to render: what the binary-format decoders
//! (Parquet, MessagePack, CBOR, …) produce on arrival, while the whole payload
//! is still available. Kept with the message instead of the raw bytes, so it
//! stays small: a summary, metadata, and a few sections of rows or text.

use ratatui::text::{Line, Span};

use crate::fmt;
use crate::theme::Theme;

pub struct DocView {
    /// Shown in the payload header, e.g. "Parquet".
    pub format: &'static str,
    /// One line of facts: `500 rows · 3 row groups · parquet-cpp 17.0`.
    pub summary: Vec<Fact>,
    /// File-level key/value metadata.
    pub meta: Vec<(String, String)>,
    pub sections: Vec<Section>,
    pub notes: Vec<String>,
}

/// `value` is emphasised, `label` follows it dimmed; either may be empty.
pub struct Fact {
    pub value: String,
    pub label: String,
}

impl Fact {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Fact {
        Fact {
            value: value.into(),
            label: label.into(),
        }
    }
}

pub struct Section {
    /// Empty for an untitled section.
    pub title: String,
    /// Dimmed text after the title, e.g. "· first 52 of 500".
    pub detail: String,
    pub body: Body,
}

pub enum Body {
    /// An aligned table; `None` cells are nulls.
    Table {
        header: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
    },
    /// JSON text, shown indented and coloured.
    Json(String),
    /// Plain lines.
    Lines(Vec<String>),
    /// The first bytes of a `size`-byte payload (e.g. unpacked from gzip),
    /// formatted like a message payload: JSON, XML, CSV, text or hex.
    Payload {
        bytes: Vec<u8>,
        size: usize,
        content_type: String,
    },
}

/// A format name read back from a session as the `&'static str` views
/// carry; each distinct name is allocated once.
fn intern(name: &str) -> &'static str {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static NAMES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut names = NAMES.get_or_init(Default::default).lock().unwrap();
    if let Some(&n) = names.get(name) {
        return n;
    }
    let n: &'static str = Box::leak(name.to_string().into_boxed_str());
    names.insert(n);
    n
}

/// Raw bytes shown under a decoding error.
const RAW_BYTES: usize = 256;
/// Hex dumps (error views and `Body::Payload`) are laid out for this many cells (16 bytes a row).
const HEX_WIDTH: usize = 76;

impl DocView {
    pub fn new(format: &'static str) -> DocView {
        DocView {
            format,
            summary: Vec::new(),
            meta: Vec::new(),
            sections: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// A payload that looked like `format` but couldn't be read: the reason,
    /// and the start of the raw bytes so nothing is hidden.
    pub fn error(format: &'static str, err: &str, payload: &[u8]) -> DocView {
        let mut v = DocView::new(format);
        v.notes.push(format!("could not read this {format} payload: {err}"));
        if !payload.is_empty() {
            let shown = &payload[..payload.len().min(RAW_BYTES)];
            let detail = if shown.len() < payload.len() {
                format!(
                    "· first {} of {}",
                    fmt::human_bytes(shown.len()),
                    fmt::human_bytes(payload.len())
                )
            } else {
                format!("· {}", fmt::human_bytes(payload.len()))
            };
            let lines = fmt::hex_lines(shown, HEX_WIDTH, &Theme::new(false))
                .into_iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            v.section("Raw bytes", detail, Body::Lines(lines));
        }
        v
    }

    /// Appends the view as a JSON object (kept in sessions for payloads cut
    /// on arrival, whose view can't be decoded again).
    pub fn write_json(&self, out: &mut String) {
        use crate::json::write_str;
        let list = |out: &mut String, items: &[String]| {
            out.push('[');
            for (i, s) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_str(out, s);
            }
            out.push(']');
        };
        let pairs = |out: &mut String, items: &mut dyn Iterator<Item = (&str, &str)>| {
            out.push('[');
            for (i, (a, b)) in items.enumerate() {
                out.push_str(if i == 0 { "[" } else { ",[" });
                write_str(out, a);
                out.push(',');
                write_str(out, b);
                out.push(']');
            }
            out.push(']');
        };
        out.push_str("{\"format\":");
        write_str(out, self.format);
        out.push_str(",\"summary\":");
        pairs(
            out,
            &mut self.summary.iter().map(|f| (f.value.as_str(), f.label.as_str())),
        );
        out.push_str(",\"meta\":");
        pairs(out, &mut self.meta.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        out.push_str(",\"sections\":[");
        for (i, s) in self.sections.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"title\":");
            write_str(out, &s.title);
            out.push_str(",\"detail\":");
            write_str(out, &s.detail);
            match &s.body {
                Body::Table { header, rows } => {
                    out.push_str(",\"header\":");
                    list(out, header);
                    out.push_str(",\"rows\":[");
                    for (r, row) in rows.iter().enumerate() {
                        out.push_str(if r == 0 { "[" } else { ",[" });
                        for (c, cell) in row.iter().enumerate() {
                            if c > 0 {
                                out.push(',');
                            }
                            match cell {
                                Some(v) => write_str(out, v),
                                None => out.push_str("null"),
                            }
                        }
                        out.push(']');
                    }
                    out.push(']');
                }
                Body::Json(text) => {
                    out.push_str(",\"json\":");
                    write_str(out, text);
                }
                Body::Lines(lines) => {
                    out.push_str(",\"lines\":");
                    list(out, lines);
                }
                Body::Payload {
                    bytes,
                    size,
                    content_type,
                } => {
                    out.push_str(&format!(
                        ",\"payload_base64\":\"{}\",\"size\":{size},\"content_type\":",
                        crate::base64::encode(bytes)
                    ));
                    write_str(out, content_type);
                }
            }
            out.push('}');
        }
        out.push_str("],\"notes\":");
        list(out, &self.notes);
        out.push('}');
    }

    /// Reads what `write_json` wrote; `None` if it doesn't hold together.
    pub fn from_json(j: &crate::json::Json) -> Option<DocView> {
        use crate::json::Json;
        let strs =
            |v: Option<&Json>| -> Option<Vec<String>> { v?.arr()?.iter().map(|s| s.str().map(String::from)).collect() };
        let pairs = |v: Option<&Json>| -> Option<Vec<(String, String)>> {
            v?.arr()?
                .iter()
                .map(|p| match p.arr()? {
                    [a, b] => Some((a.str()?.to_string(), b.str()?.to_string())),
                    _ => None,
                })
                .collect()
        };
        let mut v = DocView::new(intern(j.get("format")?.str()?));
        v.summary = pairs(j.get("summary"))?
            .into_iter()
            .map(|(a, b)| Fact::new(a, b))
            .collect();
        v.meta = pairs(j.get("meta"))?;
        v.notes = strs(j.get("notes"))?;
        for s in j.get("sections")?.arr()? {
            let body = if let Some(header) = strs(s.get("header")) {
                let rows = s
                    .get("rows")?
                    .arr()?
                    .iter()
                    .map(|r| {
                        r.arr()?
                            .iter()
                            .map(|c| match c {
                                Json::Null => Some(None),
                                c => c.str().map(|s| Some(s.to_string())),
                            })
                            .collect::<Option<Vec<_>>>()
                    })
                    .collect::<Option<Vec<_>>>()?;
                Body::Table { header, rows }
            } else if let Some(text) = s.get("json") {
                Body::Json(text.str()?.to_string())
            } else if let Some(lines) = strs(s.get("lines")) {
                Body::Lines(lines)
            } else {
                Body::Payload {
                    bytes: crate::base64::decode(s.get("payload_base64")?.str()?)?,
                    size: s.get("size")?.u64()? as usize,
                    content_type: s.get("content_type")?.str()?.to_string(),
                }
            };
            v.section(s.get("title")?.str()?, s.get("detail")?.str()?, body);
        }
        Some(v)
    }

    pub fn section(&mut self, title: impl Into<String>, detail: impl Into<String>, body: Body) {
        self.sections.push(Section {
            title: title.into(),
            detail: detail.into(),
            body,
        });
    }

    /// Tables (CSV payloads too) don't wrap in the full-screen viewer (it pans instead).
    pub fn has_table(&self) -> bool {
        self.sections.iter().any(|s| match &s.body {
            Body::Table { .. } => true,
            Body::Payload {
                bytes,
                size,
                content_type,
            } => matches!(
                crate::pretty::detect_payload(bytes, *size, content_type),
                crate::pretty::Format::Csv(_)
            ),
            _ => false,
        })
    }

    pub fn render(&self, th: &Theme) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        if !self.summary.is_empty() {
            let mut spans = Vec::new();
            for (i, f) in self.summary.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" · ", th.dim));
                }
                if !f.value.is_empty() {
                    spans.push(Span::styled(f.value.clone(), th.text));
                }
                if !f.label.is_empty() {
                    let sep = if f.value.is_empty() { "" } else { " " };
                    spans.push(Span::styled(format!("{sep}{}", f.label), th.dim));
                }
            }
            out.push(Line::from(spans));
        }
        for (k, v) in &self.meta {
            out.push(Line::from(vec![
                Span::styled(format!("{k} "), th.purple),
                Span::styled(fmt::one_line(v.as_bytes(), 160), th.text),
            ]));
        }
        for s in &self.sections {
            if !out.is_empty() {
                out.push(Line::default());
            }
            if !s.title.is_empty() {
                let mut l = vec![Span::styled(s.title.clone(), th.section)];
                if !s.detail.is_empty() {
                    l.push(Span::styled(format!(" {}", s.detail), th.dim));
                }
                out.push(Line::from(l));
            }
            match &s.body {
                Body::Table { header, rows } => out.extend(crate::pretty::table(header, rows, th)),
                Body::Json(text) => out.extend(crate::pretty::json_lines(text, th)),
                Body::Lines(lines) => out.extend(lines.iter().map(|l| Line::styled(l.clone(), th.value))),
                Body::Payload {
                    bytes,
                    size,
                    content_type,
                } => out.extend(crate::pretty::payload_lines(bytes, *size, content_type, th, HEX_WIDTH)),
            }
        }
        for n in &self.notes {
            out.push(Line::styled(format!("note: {n}"), th.yellow));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    #[test]
    fn renders_every_part() {
        let mut v = DocView::new("Demo");
        v.summary = vec![Fact::new("3", "tables"), Fact::new("", "writer 1.0")];
        v.meta.push(("Version".into(), "1.0.0".into()));
        v.section(
            "Rows",
            "· first 1 of 3",
            Body::Table {
                header: vec!["id".into(), "name".into()],
                rows: vec![vec![Some("1".into()), None]],
            },
        );
        v.section("", "", Body::Lines(vec!["plain".into()]));
        v.section("Doc", "", Body::Json(r#"{"a":1}"#.into()));
        v.notes.push("partial".into());
        assert!(v.has_table());
        assert_eq!(
            text(&v.render(&Theme::new(false))),
            [
                "3 tables · writer 1.0",
                "Version 1.0.0",
                "",
                "Rows · first 1 of 3",
                "id │ name",
                "───┼─────",
                " 1 │ null",
                "",
                "plain",
                "",
                "Doc",
                "{",
                "  \"a\": 1",
                "}",
                "note: partial",
            ]
        );
    }

    #[test]
    fn error_view_keeps_the_raw_bytes() {
        let v = DocView::error("Avro", "bad sync marker", b"Obj\x01\x00");
        assert!(!v.has_table());
        assert_eq!(
            text(&v.render(&Theme::new(false))),
            [
                "Raw bytes · 5 B",
                "00000000  4f 62 6a 01 00                                    Obj..",
                "note: could not read this Avro payload: bad sync marker"
            ]
        );
        let big = vec![0u8; 1000];
        let v = DocView::error("Avro", "x", &big);
        assert_eq!(v.sections[0].detail, "· first 256 B of 1000 B");
    }
}
