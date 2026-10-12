//! Payload formatting: JSON, XML, CSV/TSV and text are formatted from the
//! stored prefix of the payload and tolerate being cut off; binary formats
//! (see `DECODERS`) are decoded on arrival, with hex as the fallback.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::arrow;
use crate::avro;
use crate::compressed;
use crate::fmt;
use crate::parquet;
use crate::protobuf;
use crate::store::Message;
use crate::theme::Theme;
use crate::view::DocView;

/// What a binary payload was turned into when it arrived.
pub type Decoded = DocView;

/// Bytes of JSON kept for decoded documents (MessagePack, CBOR).
const DOC_BUDGET: usize = 4096;

/// Recognises and decodes one binary format from the whole payload.
/// `None`: not this format. `Some(Err)`: it is, but couldn't be read.
pub type Decoder = fn(content_type: &str, payload: &[u8]) -> Option<Result<DocView, String>>;

/// Tried in order; the first that recognises the payload wins.
const DECODERS: &[(&str, Decoder)] = &[
    ("compressed", compressed::sniff),
    ("Parquet", parquet::sniff),
    ("MessagePack", binval::sniff_msgpack),
    ("CBOR", binval::sniff_cbor),
    ("Protobuf", protobuf::sniff),
    ("Avro", avro::sniff),
    ("Arrow", arrow::sniff),
];

/// Decodes binary formats that can't be read from a truncated prefix.
pub fn decode_on_arrival(content_type: Option<&str>, payload: &[u8]) -> Option<Decoded> {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    decode(&ct, payload, "")
}

/// Decodes what a compressed payload unpacked to: every decoder but
/// `compressed` itself, so nesting stops after one level.
pub fn decode_unpacked(ct: &str, payload: &[u8]) -> Option<Decoded> {
    decode(ct, payload, "compressed")
}

fn decode(ct: &str, payload: &[u8], skip: &str) -> Option<Decoded> {
    DECODERS
        .iter()
        .filter(|(name, _)| *name != skip)
        .find_map(|(name, decode)| decode(ct, payload).map(|r| r.unwrap_or_else(|e| DocView::error(name, &e, payload))))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Empty,
    Json,
    Xml,
    Csv(u8),
    Text,
    Binary,
    /// Decoded on arrival; the flag says whether it holds a table.
    Decoded(&'static str, bool),
}

pub fn detect(m: &Message) -> Format {
    if m.size == 0 {
        return Format::Empty;
    }
    if let Some(v) = m.decoded.as_deref() {
        return Format::Decoded(v.format, v.has_table());
    }
    detect_payload(&m.payload, m.size, m.props.content_type.as_deref().unwrap_or(""))
}

/// The text format of `p`, the first bytes of a `size`-byte payload.
pub fn detect_payload(p: &[u8], size: usize, content_type: &str) -> Format {
    if size == 0 {
        return Format::Empty;
    }
    let ct = content_type.to_ascii_lowercase();
    if !fmt::is_text(p) {
        return Format::Binary;
    }
    let text = String::from_utf8_lossy(p);
    let t = text.trim_start();
    if ct.contains("json") || ((t.starts_with('{') || t.starts_with('[')) && json_like(t)) {
        return Format::Json;
    }
    if ct.contains("xml") || (t.starts_with('<') && xml_like(t)) {
        return Format::Xml;
    }
    if ct.contains("tab-separated") {
        return Format::Csv(b'\t');
    }
    if let Some(d) = csv_delimiter(&text) {
        return Format::Csv(d);
    }
    if ct.contains("csv") {
        return Format::Csv(b',');
    }
    Format::Text
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Empty => "empty",
            Format::Json => "JSON",
            Format::Xml => "XML",
            Format::Csv(b'\t') => "TSV",
            Format::Csv(_) => "CSV",
            Format::Text => "text",
            Format::Binary => "binary",
            Format::Decoded(name, _) => name,
        }
    }
}

/// The formatted payload, one `Line` per screen line (not wrapped). `width`
/// only sizes hex dumps.
pub fn render(m: &Message, th: &Theme, width: usize) -> Vec<Line<'static>> {
    // Decoded formats were read from the whole payload: nothing is cut.
    if let Some(v) = m.decoded.as_deref() {
        return v.render(th);
    }
    payload_lines(
        &m.payload,
        m.size,
        m.props.content_type.as_deref().unwrap_or(""),
        th,
        width,
    )
}

/// Formats `p`, the first bytes of a `size`-byte payload, and marks it when cut.
pub fn payload_lines(p: &[u8], size: usize, content_type: &str, th: &Theme, width: usize) -> Vec<Line<'static>> {
    let cut = p.len() < size;
    let mut out = match detect_payload(p, size, content_type) {
        Format::Empty => vec![Line::styled("(empty payload)", th.dim.add_modifier(Modifier::ITALIC))],
        Format::Decoded(..) => vec![],
        Format::Json => json_lines(&String::from_utf8_lossy(p), th),
        Format::Xml => xml_lines(&String::from_utf8_lossy(p), th),
        Format::Csv(d) => csv_lines(&String::from_utf8_lossy(p), d, cut, th),
        Format::Text => text_lines(p, th),
        Format::Binary => fmt::hex_lines(p, width, th),
    };
    if cut {
        out.push(Line::styled(
            format!("… first {} of {}", fmt::human_bytes(p.len()), fmt::human_bytes(size)),
            th.faint,
        ));
    }
    out
}

fn text_lines(b: &[u8], th: &Theme) -> Vec<Line<'static>> {
    String::from_utf8_lossy(b)
        .replace("\r\n", "\n")
        .split('\n')
        .map(|l| Line::styled(fmt::one_line(l.as_bytes(), l.len()), th.value))
        .collect()
}

/// Tables keep their rows on one line (the viewer pans sideways instead).
pub fn wraps(f: Format) -> bool {
    !matches!(f, Format::Csv(_) | Format::Decoded(_, true))
}

/// Drops the first `n` cells of a styled line.
pub fn skip_cells(line: &Line<'static>, n: usize) -> Line<'static> {
    let mut left = n;
    let mut spans = Vec::new();
    for s in &line.spans {
        if left == 0 {
            spans.push(s.clone());
            continue;
        }
        let mut kept = String::new();
        for ch in s.content.chars() {
            if left > 0 {
                left = left.saturating_sub(fmt::str_width(ch.encode_utf8(&mut [0; 4])));
            } else {
                kept.push(ch);
            }
        }
        if !kept.is_empty() {
            spans.push(Span::styled(kept, s.style));
        }
    }
    Line::from(spans)
}

/// Splits a styled line into lines of at most `w` cells.
pub fn wrap_line(line: Line<'static>, w: usize) -> Vec<Line<'static>> {
    if w == 0 || line.width() <= w {
        return vec![line];
    }
    let mut out = vec![Line::default()];
    let mut cur = 0;
    for span in line.spans {
        let mut buf = String::new();
        for ch in span.content.chars() {
            let cw = fmt::str_width(ch.encode_utf8(&mut [0; 4]));
            if cur + cw > w {
                if !buf.is_empty() {
                    out.last_mut()
                        .unwrap()
                        .spans
                        .push(Span::styled(std::mem::take(&mut buf), span.style));
                }
                out.push(Line::default());
                cur = 0;
            }
            buf.push(ch);
            cur += cw;
        }
        if !buf.is_empty() {
            out.last_mut().unwrap().spans.push(Span::styled(buf, span.style));
        }
    }
    out
}

// ── JSON ───────────────────────────────────────────────────────────────────

/// True when the text tokenizes as JSON (it may be cut off at the end).
fn json_like(t: &str) -> bool {
    let mut it = t.char_indices().peekable();
    let mut tokens = 0;
    while let Some((_, c)) = it.next() {
        match c {
            ' ' | '\t' | '\r' | '\n' => {}
            '{' | '}' | '[' | ']' | ',' | ':' => tokens += 1,
            '"' => {
                tokens += 1;
                let mut esc = false;
                for (_, c) in it.by_ref() {
                    if esc {
                        esc = false;
                    } else if c == '\\' {
                        esc = true;
                    } else if c == '"' {
                        break;
                    }
                }
            }
            '-' | '0'..='9' => {
                tokens += 1;
                while it.peek().is_some_and(|(_, c)| "+-.eE0123456789".contains(*c)) {
                    it.next();
                }
            }
            't' | 'f' | 'n' => {
                tokens += 1;
                while it.peek().is_some_and(|(_, c)| c.is_ascii_lowercase()) {
                    it.next();
                }
            }
            _ => return false,
        }
    }
    tokens > 1
}

pub fn json_lines(t: &str, th: &Theme) -> Vec<Line<'static>> {
    let b = t.as_bytes();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut depth: usize = 0;
    let newline = |out: &mut Vec<Line<'static>>, cur: &mut Vec<Span<'static>>, depth: usize| {
        if !cur.is_empty() {
            out.push(Line::from(std::mem::take(cur)));
        }
        cur.push(Span::raw("  ".repeat(depth.min(40))));
    };
    let next_non_ws = |from: usize| {
        b[from..]
            .iter()
            .position(|c| !c.is_ascii_whitespace())
            .map(|i| from + i)
    };
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => i += 1,
            b'{' | b'[' => {
                if depth == 0 && cur.iter().any(|s| !s.content.trim().is_empty()) {
                    newline(&mut out, &mut cur, 0); // next value of a JSON-lines stream
                }
                let close = if c == b'{' { b'}' } else { b']' };
                if let Some(j) = next_non_ws(i + 1)
                    && b[j] == close
                {
                    cur.push(Span::styled(format!("{}{}", c as char, close as char), th.dim));
                    i = j + 1;
                    continue;
                }
                cur.push(Span::styled((c as char).to_string(), th.dim));
                depth += 1;
                newline(&mut out, &mut cur, depth);
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, &mut cur, depth);
                cur.push(Span::styled((c as char).to_string(), th.dim));
                i += 1;
            }
            b',' => {
                cur.push(Span::styled(",", th.dim));
                newline(&mut out, &mut cur, depth);
                i += 1;
            }
            b':' => {
                cur.push(Span::styled(": ", th.dim));
                i += 1;
            }
            b'"' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                let end = (j + 1).min(b.len());
                let s = String::from_utf8_lossy(&b[i..end]).into_owned();
                let is_key = next_non_ws(end).is_some_and(|k| b[k] == b':');
                cur.push(Span::styled(s, if is_key { th.blue } else { th.green }));
                i = end;
            }
            _ => {
                let j = (i..b.len())
                    .find(|&k| b"{}[],:\" \t\r\n".contains(&b[k]))
                    .unwrap_or(b.len())
                    .max(i + 1);
                let word = String::from_utf8_lossy(&b[i..j]).into_owned();
                let style = if c == b'-' || c.is_ascii_digit() {
                    th.number
                } else if matches!(word.as_str(), "true" | "false" | "null") {
                    th.keyword
                } else {
                    th.text
                };
                cur.push(Span::styled(word, style));
                i = j;
            }
        }
    }
    if cur.iter().any(|s| !s.content.trim().is_empty()) {
        out.push(Line::from(cur));
    }
    out
}

// ── XML ────────────────────────────────────────────────────────────────────

fn xml_like(t: &str) -> bool {
    t.starts_with("<?xml")
        || t.starts_with("<!--")
        || t[1..].starts_with(|c: char| c.is_alphabetic() || c == '_' || c == '!')
}

enum Tok<'a> {
    Open(&'a str),        // `<a x="1">`
    SelfClosing(&'a str), // `<a/>`
    Close(&'a str),       // `</a>`
    Other(&'a str),       // `<?…?>`, `<!…>`, comments, CDATA
    Text(&'a str),
}

fn xml_tokens(t: &str) -> Vec<Tok<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    let find = |from: usize, pat: &str| t[from..].find(pat).map(|k| from + k + pat.len()).unwrap_or(t.len());
    while i < t.len() {
        let rest = &t[i..];
        let end;
        if !rest.starts_with('<') {
            end = rest.find('<').map_or(t.len(), |k| i + k);
            out.push(Tok::Text(&t[i..end]));
        } else if rest.starts_with("<!--") {
            end = find(i, "-->");
            out.push(Tok::Other(&t[i..end]));
        } else if rest.starts_with("<![CDATA[") {
            end = find(i, "]]>");
            out.push(Tok::Other(&t[i..end]));
        } else if rest.starts_with("<?") {
            end = find(i, "?>");
            out.push(Tok::Other(&t[i..end]));
        } else {
            // A tag: scan to `>` outside quotes.
            let mut q: Option<char> = None;
            let mut e = t.len();
            for (k, c) in rest.char_indices().skip(1) {
                match (q, c) {
                    (Some(qc), c) if c == qc => q = None,
                    (None, '"' | '\'') => q = Some(c),
                    (None, '>') => {
                        e = i + k + 1;
                        break;
                    }
                    _ => {}
                }
            }
            end = e;
            let tag = &t[i..end];
            out.push(if tag.starts_with("</") {
                Tok::Close(tag)
            } else if tag.starts_with("<!") {
                Tok::Other(tag)
            } else if tag.ends_with("/>") {
                Tok::SelfClosing(tag)
            } else {
                Tok::Open(tag)
            });
        }
        i = end.max(i + 1);
    }
    out
}

fn xml_tag(tag: &str, th: &Theme) -> Vec<Span<'static>> {
    // `<` `/`? name (attr="value")* `/`? `>`
    let mut spans = Vec::new();
    let body = tag.trim_start_matches('<');
    let (open, body) = match body.strip_prefix('/') {
        Some(b) => ("</", b),
        None => ("<", body),
    };
    let (body, close) = if let Some(b) = body.strip_suffix("/>") {
        (b, "/>")
    } else if let Some(b) = body.strip_suffix('>') {
        (b, ">")
    } else {
        (body, "") // cut off
    };
    spans.push(Span::styled(open, th.dim));
    let name_end = body.find(|c: char| c.is_whitespace()).unwrap_or(body.len());
    spans.push(Span::styled(body[..name_end].to_string(), th.blue));
    let mut rest = &body[name_end..];
    while !rest.is_empty() {
        let ws = rest.len() - rest.trim_start().len();
        if ws > 0 {
            spans.push(Span::raw(" "));
            rest = &rest[ws..];
            continue;
        }
        let eq = rest.find('=');
        let name_end = rest.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(rest.len());
        spans.push(Span::styled(rest[..name_end].to_string(), th.purple));
        rest = &rest[name_end..];
        if eq.is_some() && rest.trim_start().starts_with('=') {
            rest = rest.trim_start()[1..].trim_start();
            spans.push(Span::styled("=", th.dim));
            let q = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
            let vend = match q {
                Some(qc) => rest[1..].find(qc).map_or(rest.len(), |k| k + 2),
                None => rest.find(char::is_whitespace).unwrap_or(rest.len()),
            };
            spans.push(Span::styled(rest[..vend].to_string(), th.green));
            rest = &rest[vend..];
        }
    }
    spans.push(Span::styled(close, th.dim));
    spans
}

fn xml_lines(t: &str, th: &Theme) -> Vec<Line<'static>> {
    let toks = xml_tokens(t);
    let mut out = Vec::new();
    let mut depth: usize = 0;
    let indent = |d: usize| Span::raw("  ".repeat(d.min(40)));
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Open(tag) => {
                // `<a>short text</a>` stays on one line.
                if let (Some(Tok::Text(txt)), Some(Tok::Close(close))) = (toks.get(i + 1), toks.get(i + 2))
                    && !txt.contains('\n')
                    && txt.trim().chars().count() <= 80
                {
                    let mut spans = vec![indent(depth)];
                    spans.extend(xml_tag(tag, th));
                    spans.push(Span::styled(txt.trim().to_string(), th.text));
                    spans.extend(xml_tag(close, th));
                    out.push(Line::from(spans));
                    i += 3;
                    continue;
                }
                if let Some(Tok::Close(close)) = toks.get(i + 1) {
                    let mut spans = vec![indent(depth)];
                    spans.extend(xml_tag(tag, th));
                    spans.extend(xml_tag(close, th));
                    out.push(Line::from(spans));
                    i += 2;
                    continue;
                }
                let mut spans = vec![indent(depth)];
                spans.extend(xml_tag(tag, th));
                out.push(Line::from(spans));
                depth += 1;
            }
            Tok::Close(tag) => {
                depth = depth.saturating_sub(1);
                let mut spans = vec![indent(depth)];
                spans.extend(xml_tag(tag, th));
                out.push(Line::from(spans));
            }
            Tok::SelfClosing(tag) => {
                let mut spans = vec![indent(depth)];
                spans.extend(xml_tag(tag, th));
                out.push(Line::from(spans));
            }
            Tok::Other(s) => {
                for l in s.lines() {
                    out.push(Line::from(vec![
                        indent(depth),
                        Span::styled(l.trim().to_string(), th.faint),
                    ]));
                }
            }
            Tok::Text(s) => {
                for l in s.lines().map(str::trim).filter(|l| !l.is_empty()) {
                    out.push(Line::from(vec![indent(depth), Span::styled(l.to_string(), th.text)]));
                }
            }
        }
        i += 1;
    }
    out
}

// ── CSV and tables ─────────────────────────────────────────────────────────

/// The delimiter, if the first lines agree on one.
fn csv_delimiter(t: &str) -> Option<u8> {
    let mut lines: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty()).take(7).collect();
    if !t.ends_with('\n') && lines.len() > 2 {
        lines.pop(); // the last line may be cut off
    }
    if lines.len() < 2 {
        return None;
    }
    let mut best = None;
    for &d in b",;\t|" {
        let counts: Vec<usize> = lines.iter().map(|l| split_row(l, d).len() - 1).collect();
        if counts[0] >= 1 && counts.iter().all(|&c| c == counts[0]) && best.is_none_or(|(_, n)| counts[0] > n) {
            best = Some((d, counts[0]));
        }
    }
    best.map(|(d, _)| d)
}

fn split_row(line: &str, d: u8) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                out.last_mut().unwrap().push('"');
            }
            '"' => quoted = !quoted,
            c if c as u32 == d as u32 && !quoted => out.push(String::new()),
            c => out.last_mut().unwrap().push(c),
        }
    }
    out
}

fn csv_lines(t: &str, d: u8, cut: bool, th: &Theme) -> Vec<Line<'static>> {
    let mut rows: Vec<Vec<String>> = t
        .replace("\r\n", "\n")
        .split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| split_row(l, d))
        .collect();
    if cut && rows.len() > 1 {
        rows.pop(); // likely incomplete
    }
    if rows.is_empty() {
        return vec![];
    }
    let header = rows.remove(0);
    let rows: Vec<Vec<Option<String>>> = rows.into_iter().map(|r| r.into_iter().map(Some).collect()).collect();
    table(&header, &rows, th)
}

const MAX_COL: usize = 28;

/// An aligned table: bold header, a rule, `│` between columns, numbers right-aligned.
pub fn table(header: &[String], rows: &[Vec<Option<String>>], th: &Theme) -> Vec<Line<'static>> {
    let n = header.len().max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
    let cell = |r: &Vec<Option<String>>, i: usize| r.get(i).cloned().flatten();
    let width = |s: &str| fmt::str_width(s).min(MAX_COL);
    let widths: Vec<usize> = (0..n)
        .map(|i| {
            let h = header.get(i).map_or(0, |s| width(s));
            rows.iter()
                .map(|r| cell(r, i).map_or(4, |s| width(&s)))
                .max()
                .unwrap_or(0)
                .max(h)
        })
        .collect();
    let numeric: Vec<bool> = (0..n)
        .map(|i| {
            let mut any = false;
            let all = rows.iter().filter_map(|r| cell(r, i)).all(|s| {
                any = true;
                s.trim().parse::<f64>().is_ok()
            });
            any && all
        })
        .collect();
    let pad = |s: &str, w: usize, right: bool| {
        let s = fmt::pad(s, w);
        if right {
            let t = s.trim_end();
            format!("{}{t}", " ".repeat(w - fmt::str_width(t)))
        } else {
            s
        }
    };
    let sep = Span::styled(" │ ", th.faint);
    let mut out = Vec::new();
    let mut h = Vec::new();
    for i in 0..n {
        if i > 0 {
            h.push(sep.clone());
        }
        let name = header.get(i).cloned().unwrap_or_default();
        h.push(Span::styled(pad(&name, widths[i], numeric[i]), th.bold));
    }
    out.push(Line::from(h));
    let rule: Vec<String> = widths.iter().map(|&w| "─".repeat(w)).collect();
    out.push(Line::styled(rule.join("─┼─"), th.faint));
    for r in rows {
        let mut spans = Vec::new();
        for i in 0..n {
            if i > 0 {
                spans.push(sep.clone());
            }
            match cell(r, i) {
                Some(s) => spans.push(Span::styled(
                    pad(&s, widths[i], numeric[i]),
                    if numeric[i] { th.number } else { th.value },
                )),
                None => spans.push(Span::styled(
                    pad("null", widths[i], numeric[i]),
                    th.faint.add_modifier(Modifier::ITALIC),
                )),
            }
        }
        out.push(Line::from(spans));
    }
    out
}

// ── MessagePack and CBOR ───────────────────────────────────────────────────

pub(crate) mod binval {
    //! Just enough MessagePack and CBOR to show a document as JSON.

    use crate::view::{Body, DocView};

    pub enum V {
        Null,
        Bool(bool),
        Int(i128),
        Float(f64),
        Str(String),
        Bin(Vec<u8>),
        Arr(Vec<V>),
        Map(Vec<(V, V)>),
    }

    impl V {
        /// Appends JSON to `out`, stopping (with `…`) past `budget` bytes.
        pub fn write_json(&self, out: &mut String, budget: usize) {
            if out.len() > budget {
                return;
            }
            match self {
                V::Null => out.push_str("null"),
                V::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                V::Int(i) => out.push_str(&i.to_string()),
                V::Float(f) if f.is_finite() => out.push_str(&f.to_string()),
                V::Float(f) => out.push_str(&format!("\"{f}\"")),
                V::Str(s) => quote(out, s),
                V::Bin(b) => quote(out, &format!("<{} bytes: {}>", b.len(), hex(b))),
                V::Arr(items) => {
                    out.push('[');
                    for (i, v) in items.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        if out.len() > budget {
                            out.push_str("\"…\"");
                            break;
                        }
                        v.write_json(out, budget);
                    }
                    out.push(']');
                }
                V::Map(items) => {
                    out.push('{');
                    for (i, (k, v)) in items.iter().enumerate() {
                        if i > 0 {
                            out.push(',');
                        }
                        if out.len() > budget {
                            out.push_str("\"…\":\"…\"");
                            break;
                        }
                        match k {
                            V::Str(s) => quote(out, s),
                            other => {
                                let mut k = String::new();
                                other.write_json(&mut k, budget);
                                quote(out, &k);
                            }
                        }
                        out.push(':');
                        v.write_json(out, budget);
                    }
                    out.push('}');
                }
            }
        }
    }

    fn hex(b: &[u8]) -> String {
        let mut s: String = b.iter().take(32).map(|x| format!("{x:02x}")).collect();
        if b.len() > 32 {
            s.push('…');
        }
        s
    }

    fn quote(out: &mut String, s: &str) {
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
    }

    struct R<'a> {
        b: &'a [u8],
        p: usize,
        depth: u32,
    }

    impl R<'_> {
        fn take(&mut self, n: usize) -> Result<&[u8], ()> {
            let s = self.b.get(self.p..self.p.checked_add(n).ok_or(())?).ok_or(())?;
            self.p += n;
            Ok(s)
        }
        fn be(&mut self, n: usize) -> Result<u64, ()> {
            Ok(self.take(n)?.iter().fold(0u64, |a, &b| a << 8 | b as u64))
        }
        fn enter(&mut self) -> Result<(), ()> {
            self.depth += 1;
            if self.depth > 64 { Err(()) } else { Ok(()) }
        }
        fn malformed(&self) -> String {
            format!("malformed at byte {} of {}", self.p.min(self.b.len()), self.b.len())
        }
        /// A document must use the whole payload.
        fn trailing(&self, v: V) -> Result<V, String> {
            match self.b.len() - self.p {
                0 => Ok(v),
                n => Err(format!("{n} unexpected bytes after the document, at byte {}", self.p)),
            }
        }
        fn count(&self, n: u64) -> Result<usize, ()> {
            // Every element takes at least one byte.
            if n as usize > self.b.len() - self.p {
                Err(())
            } else {
                Ok(n as usize)
            }
        }
    }

    fn as_view(name: &'static str, v: Result<V, String>) -> Result<DocView, String> {
        let mut text = String::new();
        v?.write_json(&mut text, super::DOC_BUDGET);
        let mut view = DocView::new(name);
        view.section("", "", Body::Json(text));
        Ok(view)
    }

    pub fn sniff_msgpack(ct: &str, b: &[u8]) -> Option<Result<DocView, String>> {
        ct.contains("msgpack").then(|| as_view("MessagePack", msgpack(b)))
    }

    pub fn sniff_cbor(ct: &str, b: &[u8]) -> Option<Result<DocView, String>> {
        ct.contains("cbor").then(|| as_view("CBOR", cbor(b)))
    }

    pub fn msgpack(b: &[u8]) -> Result<V, String> {
        let mut r = R { b, p: 0, depth: 0 };
        let v = mp(&mut r).map_err(|_| r.malformed())?;
        r.trailing(v)
    }

    fn mp(r: &mut R) -> Result<V, ()> {
        let t = r.take(1)?[0];
        let s = |r: &mut R, n: usize| -> Result<V, ()> { Ok(V::Str(String::from_utf8_lossy(r.take(n)?).into_owned())) };
        Ok(match t {
            0x00..=0x7f => V::Int(t as i128),
            0xe0..=0xff => V::Int(t as i8 as i128),
            0x80..=0x8f => mp_map(r, (t & 0x0f) as u64)?,
            0x90..=0x9f => mp_arr(r, (t & 0x0f) as u64)?,
            0xa0..=0xbf => s(r, (t & 0x1f) as usize)?,
            0xc0 => V::Null,
            0xc2 => V::Bool(false),
            0xc3 => V::Bool(true),
            0xc4..=0xc6 => {
                let n = r.be(1 << (t - 0xc4))? as usize;
                V::Bin(r.take(n)?.to_vec())
            }
            0xca => V::Float(f32::from_bits(r.be(4)? as u32) as f64),
            0xcb => V::Float(f64::from_bits(r.be(8)?)),
            0xcc..=0xcf => V::Int(r.be(1 << (t - 0xcc))? as i128),
            0xd0 => V::Int(r.be(1)? as i8 as i128),
            0xd1 => V::Int(r.be(2)? as i16 as i128),
            0xd2 => V::Int(r.be(4)? as i32 as i128),
            0xd3 => V::Int(r.be(8)? as i64 as i128),
            0xd9..=0xdb => {
                let n = r.be(1 << (t - 0xd9))? as usize;
                s(r, n)?
            }
            0xdc | 0xdd => {
                let n = r.be(if t == 0xdc { 2 } else { 4 })?;
                mp_arr(r, n)?
            }
            0xde | 0xdf => {
                let n = r.be(if t == 0xde { 2 } else { 4 })?;
                mp_map(r, n)?
            }
            0xd4..=0xd8 | 0xc7..=0xc9 => {
                // Extension types: show as bytes.
                let n = match t {
                    0xd4..=0xd8 => 1usize << (t - 0xd4),
                    _ => r.be(1 << (t - 0xc7))? as usize,
                };
                let _ty = r.take(1)?;
                V::Bin(r.take(n)?.to_vec())
            }
            _ => return Err(()),
        })
    }

    fn mp_arr(r: &mut R, n: u64) -> Result<V, ()> {
        r.enter()?;
        let n = r.count(n)?;
        let v = (0..n).map(|_| mp(r)).collect::<Result<_, _>>()?;
        r.depth -= 1;
        Ok(V::Arr(v))
    }

    fn mp_map(r: &mut R, n: u64) -> Result<V, ()> {
        r.enter()?;
        let n = r.count(n)?;
        let v = (0..n).map(|_| Ok((mp(r)?, mp(r)?))).collect::<Result<_, _>>()?;
        r.depth -= 1;
        Ok(V::Map(v))
    }

    pub fn cbor(b: &[u8]) -> Result<V, String> {
        let mut r = R { b, p: 0, depth: 0 };
        let v = cb(&mut r).map_err(|_| r.malformed())?;
        r.trailing(v)
    }

    fn cb_arg(r: &mut R, info: u8) -> Result<u64, ()> {
        match info {
            0..=23 => Ok(info as u64),
            24..=27 => r.be(1 << (info - 24)),
            _ => Err(()),
        }
    }

    fn cb(r: &mut R) -> Result<V, ()> {
        let ib = r.take(1)?[0];
        let (major, info) = (ib >> 5, ib & 0x1f);
        Ok(match major {
            0 => V::Int(cb_arg(r, info)? as i128),
            1 => V::Int(-1 - cb_arg(r, info)? as i128),
            2 | 3 => {
                let n = cb_arg(r, info)? as usize;
                let bytes = r.take(n)?.to_vec();
                if major == 3 {
                    V::Str(String::from_utf8_lossy(&bytes).into_owned())
                } else {
                    V::Bin(bytes)
                }
            }
            4 => {
                r.enter()?;
                let n = cb_arg(r, info)?;
                let n = r.count(n)?;
                let v = (0..n).map(|_| cb(r)).collect::<Result<_, _>>()?;
                r.depth -= 1;
                V::Arr(v)
            }
            5 => {
                r.enter()?;
                let n = cb_arg(r, info)?;
                let n = r.count(n)?;
                let v = (0..n).map(|_| Ok((cb(r)?, cb(r)?))).collect::<Result<_, _>>()?;
                r.depth -= 1;
                V::Map(v)
            }
            6 => {
                cb_arg(r, info)?; // tag: show the tagged value
                r.enter()?;
                let v = cb(r)?;
                r.depth -= 1;
                v
            }
            7 => match info {
                20 => V::Bool(false),
                21 => V::Bool(true),
                22 | 23 => V::Null,
                25 => V::Float(half(r.be(2)? as u16)),
                26 => V::Float(f32::from_bits(r.be(4)? as u32) as f64),
                27 => V::Float(f64::from_bits(r.be(8)?)),
                _ => return Err(()),
            },
            _ => return Err(()),
        })
    }

    pub(crate) fn half(h: u16) -> f64 {
        let exp = (h >> 10) & 0x1f;
        let mant = (h & 0x3ff) as f64;
        let v = match exp {
            0 => mant * 2f64.powi(-24),
            31 => {
                if mant == 0.0 {
                    f64::INFINITY
                } else {
                    f64::NAN
                }
            }
            e => (1.0 + mant / 1024.0) * 2f64.powi(e as i32 - 15),
        };
        if h & 0x8000 != 0 { -v } else { v }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn json(v: Result<V, String>) -> String {
            let mut s = String::new();
            v.unwrap().write_json(&mut s, 4096);
            s
        }

        #[test]
        fn msgpack_and_cbor_to_json() {
            // {"t": 21.5, "ok": true, "tags": ["a"]}
            let mp = [
                0x83, 0xa1, b't', 0xcb, 0x40, 0x35, 0x80, 0, 0, 0, 0, 0, 0xa2, b'o', b'k', 0xc3, 0xa4, b't', b'a',
                b'g', b's', 0x91, 0xa1, b'a',
            ];
            assert_eq!(json(msgpack(&mp)), r#"{"t":21.5,"ok":true,"tags":["a"]}"#);
            // {"n": -2, "h": 1.5 (half float), "b": h'0102'}
            let cb = [
                0xa3, 0x61, b'n', 0x21, 0x61, b'h', 0xf9, 0x3e, 0x00, 0x61, b'b', 0x42, 1, 2,
            ];
            assert_eq!(json(cbor(&cb)), r#"{"n":-2,"h":1.5,"b":"<2 bytes: 0102>"}"#);
            assert_eq!(msgpack(&[0x92, 0x01]).err().unwrap(), "malformed at byte 1 of 2");
            assert_eq!(
                cbor(&[0x01, 0x02]).err().unwrap(),
                "1 unexpected bytes after the document, at byte 1"
            );
            assert!(msgpack(&[0xdd, 0xff, 0xff, 0xff, 0xff]).is_err());
            assert!(cbor(&[0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Props;
    use std::time::SystemTime;

    /// A stored message: `payload` is the kept prefix of a `size`-byte payload.
    fn msg(payload: &[u8], size: usize, ct: Option<&str>) -> Message {
        let mut m = arrived(payload, payload.len(), ct);
        m.size = size;
        m
    }

    /// As the store keeps it: decoded from the full payload, first `keep` bytes kept.
    fn arrived(full: &[u8], keep: usize, ct: Option<&str>) -> Message {
        let props = Props {
            content_type: ct.map(String::from),
            ..Default::default()
        };
        Message {
            seq: 1,
            time: SystemTime::now(),
            payload: full[..keep].to_vec(),
            size: full.len(),
            qos: 0,
            retain: false,
            decoded: decode_on_arrival(ct, full).map(Box::new),
            props,
        }
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    #[test]
    fn detects_formats() {
        let d = |p: &[u8], ct: Option<&str>| detect(&msg(p, p.len(), ct));
        assert_eq!(d(br#"{"a":1}"#, None), Format::Json);
        assert_eq!(d(b"[2026-10-08 11:34:10] started", None), Format::Text);
        assert_eq!(d(b"<a><b>1</b></a>", None), Format::Xml);
        assert_eq!(d(b"a;b;c\n1;2;3\n4;5;6\n", None), Format::Csv(b';'));
        assert_eq!(d(b"2026-10-08 11:34:10,472;H;1;7;8", None), Format::Text);
        assert_eq!(d(b"hello world", None), Format::Text);
        assert_eq!(d(&[0, 1, 2, 0xff], None), Format::Binary);
        assert_eq!(d(b"a,b\n", Some("text/csv")), Format::Csv(b','));
        assert_eq!(d(b"", None), Format::Empty);
        let pq = std::fs::read(format!(
            "{}/tests/fixtures/turbine-zstd.parquet",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        assert_eq!(d(&pq, Some("application/parquet")), Format::Decoded("Parquet", true));
    }

    #[test]
    fn json_pretty_and_truncated() {
        let th = Theme::new(false);
        let m = msg(br#"{"t":21.5,"ok":true,"tags":["a","b"],"e":{},"n":null}"#, 51, None);
        assert_eq!(
            text(&render(&m, &th, 100)),
            [
                "{",
                "  \"t\": 21.5,",
                "  \"ok\": true,",
                "  \"tags\": [",
                "    \"a\",",
                "    \"b\"",
                "  ],",
                "  \"e\": {},",
                "  \"n\": null",
                "}"
            ]
        );
        // Cut off mid-document: formatted as far as it goes, then marked.
        let m = msg(br#"{"site":{"name":"oslo","turb"#, 4000, None);
        let t = text(&render(&m, &th, 100));
        assert_eq!(t[..3], ["{", "  \"site\": {", "    \"name\": \"oslo\","]);
        assert!(t.last().unwrap().starts_with("… first"));
    }

    #[test]
    fn xml_pretty() {
        let th = Theme::new(false);
        let m = msg(
            br#"<?xml version="1.0"?><DataStatus><Station id="91" kind='grid'><Name>gridStation</Name><Empty/></Station></DataStatus>"#,
            100,
            Some("application/xml"),
        );
        assert_eq!(
            text(&render(&m, &th, 100)),
            [
                "<?xml version=\"1.0\"?>",
                "<DataStatus>",
                "  <Station id=\"91\" kind='grid'>",
                "    <Name>gridStation</Name>",
                "    <Empty/>",
                "  </Station>",
                "</DataStatus>"
            ]
        );
    }

    #[test]
    fn csv_table() {
        let th = Theme::new(false);
        let csv = b"turbine,power,status\nT01,1200.5,RUNNING\n\"T,02\",15,IDLE\n";
        let m = msg(csv, csv.len(), None);
        assert_eq!(
            text(&render(&m, &th, 100)),
            [
                "turbine │  power │ status ",
                "────────┼────────┼────────",
                "T01     │ 1200.5 │ RUNNING",
                "T,02    │     15 │ IDLE   "
            ]
        );
    }

    #[test]
    fn parquet_view() {
        let th = Theme::new(false);
        let pq = std::fs::read(format!(
            "{}/tests/fixtures/turbine-snappy.parquet",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let m = arrived(&pq, 4096, Some("application/parquet"));
        let t = text(&render(&m, &th, 100));
        assert!(t[0].starts_with("500 rows · 3 row groups · 10 columns"), "{}", t[0]);
        assert!(t.iter().any(|l| l.starts_with("ProviderName TurbineFastlog")));
        assert!(
            t.iter()
                .any(|l| l.contains("Timestamp") && l.contains("TIMESTAMP(ms, UTC)"))
        );
        assert!(t.iter().any(|l| l.starts_with("Rows · first")));
        assert!(
            t.iter()
                .any(|l| l.contains("2026-10-08 10:20:00Z") && l.contains("T01"))
        );
        assert!(
            !t.iter().any(|l| l.starts_with("… first")),
            "decoded payloads aren't marked as cut"
        );
    }

    #[test]
    fn wraps_styled_lines() {
        let l = Line::from(vec![Span::raw("abcd"), Span::raw("efgh")]);
        let w = wrap_line(l, 3);
        assert_eq!(text(&w), ["abc", "def", "gh"]);
    }
}
