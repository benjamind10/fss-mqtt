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
}

/// Raw bytes shown under a decoding error.
const RAW_BYTES: usize = 256;
/// Hex dumps are laid out for this many cells (16 bytes a row).
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

    pub fn section(&mut self, title: impl Into<String>, detail: impl Into<String>, body: Body) {
        self.sections.push(Section {
            title: title.into(),
            detail: detail.into(),
            body,
        });
    }

    /// Tables don't wrap in the full-screen viewer (it pans instead).
    pub fn has_table(&self) -> bool {
        self.sections.iter().any(|s| matches!(s.body, Body::Table { .. }))
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
