//! The publish composer: a message as an editable text document (headers, a
//! blank line, the payload), the small editor for it, and parsing it back.
//!
//! ```text
//! topic: site/oslo/turbine/T01/telemetry
//! connection: oslo
//! qos: 1
//! retain: no
//! content-type: application/json
//! user: TurbineId = T01
//!
//! {"ts":"…","power_kw":2265.8}
//! ```

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

use crate::base64;
use crate::fmt::{clock, human_bytes, str_width};
use crate::mqtt::{self, Outgoing};
use crate::store::{Message, Props};

/// A message as the composer reads it back: where to send it, and what.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub conn: String,
    pub out: Outgoing,
}

/// A problem with the document, on a 1-based line.
#[derive(Clone, Debug, PartialEq)]
pub struct DocError {
    pub line: usize,
    pub msg: String,
}

const KEYS: &str = "topic, connection, qos, retain, content-type, response-topic, correlation-data, \
                    message-expiry, payload-format, user, encoding";

/// Bytes per line of a base64 payload (76 characters).
const B64_BYTES: usize = 57;

/// Whether `p` can be edited as text and still be sent back unchanged: UTF-8
/// without control characters other than newlines and tabs.
fn editable(p: &[u8]) -> bool {
    std::str::from_utf8(p).is_ok_and(|s| !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t'))
}

/// Header values are one line; a line break in one would end the header.
fn one_line(s: &str) -> String {
    s.replace(['\r', '\n'], " ")
}

/// The document for `d`: payloads that aren't plain text are written as base64.
pub fn document(d: &Draft) -> String {
    let o = &d.out;
    let p = &o.props;
    let mut h = vec![
        format!("topic: {}", one_line(&o.topic)),
        format!("connection: {}", d.conn),
        format!("qos: {}", o.qos),
        format!("retain: {}", if o.retain { "yes" } else { "no" }),
    ];
    if let Some(v) = &p.content_type {
        h.push(format!("content-type: {}", one_line(v)));
    }
    if let Some(v) = p.payload_format {
        h.push(format!("payload-format: {}", if v == 1 { "utf8" } else { "bytes" }));
    }
    if let Some(v) = p.message_expiry {
        h.push(format!("message-expiry: {v}"));
    }
    if let Some(v) = &p.response_topic {
        h.push(format!("response-topic: {}", one_line(v)));
    }
    if let Some(v) = &p.correlation_data {
        let plain = std::str::from_utf8(v)
            .ok()
            .filter(|s| s.trim() == *s && !s.starts_with("base64:") && !s.chars().any(char::is_control));
        match plain {
            Some(s) => h.push(format!("correlation-data: {s}")),
            None => h.push(format!("correlation-data: base64:{}", base64::encode(v))),
        }
    }
    for (k, v) in &p.user {
        h.push(format!("user: {} = {}", one_line(k), one_line(v)));
    }
    let body = if editable(&o.payload) {
        String::from_utf8_lossy(&o.payload).into_owned()
    } else {
        h.push("encoding: base64".into());
        o.payload
            .chunks(B64_BYTES)
            .map(base64::encode)
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!("{}\n\n{body}", h.join("\n"))
}

/// Splits a document at its first blank line: the header text, and the
/// payload with the 0-based line it starts on (none without a blank line).
fn split_doc(text: &str) -> (&str, Option<(usize, &str)>) {
    let mut at = 0;
    for (i, line) in text.split('\n').enumerate() {
        if line.trim().is_empty() {
            let rest = (at + line.len() + 1).min(text.len());
            return (&text[..at], Some((i + 1, &text[rest..])));
        }
        at += line.len() + 1;
    }
    (text, None)
}

fn set<T>(slot: &mut Option<(T, usize)>, v: T, n: usize, key: &str) -> Result<(), DocError> {
    if slot.is_some() {
        return Err(DocError {
            line: n,
            msg: format!("{key} is given twice"),
        });
    }
    *slot = Some((v, n));
    Ok(())
}

/// Reads a document back into the message it describes.
pub fn parse(text: &str) -> Result<Draft, DocError> {
    let (head, body) = split_doc(text);
    let mut topic = None;
    let mut conn = None;
    let mut qos = None;
    let mut retain = None;
    let mut encoding = None;
    let mut content_type = None;
    let mut response_topic = None;
    let mut correlation = None;
    let mut expiry = None;
    let mut format = None;
    let mut user = Vec::new();
    for (i, line) in head.split('\n').enumerate().filter(|(_, l)| !l.is_empty()) {
        let n = i + 1;
        let err = |msg: String| DocError { line: n, msg };
        let Some((k, v)) = line.split_once(':') else {
            return Err(err("expected key: value (a blank line starts the payload)".into()));
        };
        let key = k.trim().to_ascii_lowercase();
        let v = v.trim();
        match key.as_str() {
            "topic" => set(&mut topic, v.to_string(), n, "topic")?,
            "connection" => set(&mut conn, v.to_string(), n, "connection")?,
            "qos" => {
                let q = match v {
                    "0" | "1" | "2" => v.as_bytes()[0] - b'0',
                    _ => return Err(err("qos must be 0, 1 or 2".into())),
                };
                set(&mut qos, q, n, "qos")?
            }
            "retain" => {
                let r = match v.to_ascii_lowercase().as_str() {
                    "yes" | "true" | "on" | "1" => true,
                    "no" | "false" | "off" | "0" => false,
                    _ => return Err(err("retain must be yes or no".into())),
                };
                set(&mut retain, r, n, "retain")?
            }
            "encoding" => {
                let e = match v.to_ascii_lowercase().as_str() {
                    "text" => Encoding::Text,
                    "base64" => Encoding::Base64,
                    "hex" => Encoding::Hex,
                    _ => return Err(err("encoding must be text, base64 or hex".into())),
                };
                set(&mut encoding, e, n, "encoding")?
            }
            "content-type" => set(&mut content_type, v.to_string(), n, "content-type")?,
            "response-topic" => set(&mut response_topic, v.to_string(), n, "response-topic")?,
            "correlation-data" => {
                let data = match v.strip_prefix("base64:") {
                    Some(b) => base64::decode(b).ok_or_else(|| err("correlation-data isn't valid base64".into()))?,
                    None => v.as_bytes().to_vec(),
                };
                set(&mut correlation, data, n, "correlation-data")?
            }
            "message-expiry" => {
                let s = v
                    .parse::<u32>()
                    .map_err(|_| err("message-expiry is a number of seconds".into()))?;
                set(&mut expiry, s, n, "message-expiry")?
            }
            "payload-format" => {
                let f = match v.to_ascii_lowercase().as_str() {
                    "utf8" | "utf-8" | "1" => 1,
                    "bytes" | "0" => 0,
                    _ => return Err(err("payload-format must be utf8 or bytes".into())),
                };
                set(&mut format, f, n, "payload-format")?
            }
            "user" => {
                let (k, v) = v
                    .split_once(" = ")
                    .or_else(|| v.split_once('='))
                    .ok_or_else(|| err("expected user: key = value".into()))?;
                user.push((k.trim().to_string(), v.trim().to_string()));
            }
            _ => return Err(err(format!("unknown key {key:?}; known: {KEYS}"))),
        }
    }

    let (topic, topic_line) = topic.ok_or(DocError {
        line: 1,
        msg: "no topic (add a line topic: …)".into(),
    })?;
    if let Some(p) = mqtt::topic_problem(&topic) {
        return Err(DocError {
            line: topic_line,
            msg: p.into(),
        });
    }
    let (conn, _) = conn.ok_or(DocError {
        line: 1,
        msg: "no connection (add a line connection: <name>)".into(),
    })?;

    let (body_line, body) = body.map_or((head.split('\n').count() + 1, ""), |(i, b)| (i + 1, b));
    let bad = |what: &str| DocError {
        line: body_line,
        msg: format!("the payload isn't valid {what}"),
    };
    let payload = match encoding.map_or(Encoding::Text, |(e, _)| e) {
        Encoding::Text => body.as_bytes().to_vec(),
        Encoding::Base64 => base64::decode(body).ok_or_else(|| bad("base64"))?,
        Encoding::Hex => hex(body).ok_or_else(|| bad("hex"))?,
    };
    if let Some((1, n)) = format
        && std::str::from_utf8(&payload).is_err()
    {
        return Err(DocError {
            line: n,
            msg: "payload-format is utf8 but the payload isn't UTF-8".into(),
        });
    }
    Ok(Draft {
        conn,
        out: Outgoing {
            topic,
            payload,
            qos: qos.map_or(0, |q| q.0),
            retain: retain.is_some_and(|r| r.0),
            props: Props {
                content_type: content_type.map(|v| v.0),
                response_topic: response_topic.map(|v| v.0),
                correlation_data: correlation.map(|v| v.0),
                payload_format: format.map(|v| v.0),
                message_expiry: expiry.map(|v| v.0),
                subscription_id: None,
                user,
            },
        },
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Encoding {
    Text,
    Base64,
    Hex,
}

/// Hex digits (whitespace ignored) as bytes.
fn hex(s: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = s
        .bytes()
        .filter(|c| !c.is_ascii_whitespace())
        .map(|c| (c as char).to_digit(16).map(|d| d as u8))
        .collect::<Option<_>>()?;
    if digits.len() % 2 == 1 {
        return None;
    }
    Some(digits.chunks(2).map(|p| p[0] << 4 | p[1]).collect())
}

// ── JSON ───────────────────────────────────────────────────────────────────

/// JSON's tokens, strings and numbers exactly as written. None if something
/// in `s` isn't JSON.
fn json_tokens(s: &str) -> Option<Vec<&str>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
                continue;
            }
            b'{' | b'}' | b'[' | b']' | b',' | b':' => i += 1,
            b'"' => {
                i += 1;
                loop {
                    match *b.get(i)? {
                        b'\\' => i += 2,
                        b'"' => break,
                        _ => i += 1,
                    }
                }
                i += 1;
            }
            b'-' | b'0'..=b'9' => {
                while i < b.len() && matches!(b[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    i += 1;
                }
                s[start..i].parse::<f64>().ok()?;
            }
            b't' | b'f' | b'n' => {
                while i < b.len() && b[i].is_ascii_lowercase() {
                    i += 1;
                }
                if !matches!(&s[start..i], "true" | "false" | "null") {
                    return None;
                }
            }
            _ => return None,
        }
        out.push(&s[start..i]);
    }
    Some(out)
}

/// Deeper nesting than this isn't formatted (and can't exhaust the stack).
const JSON_DEPTH: usize = 256;

/// Checks one JSON value starting at token `i`; returns the index after it.
fn json_value(t: &[&str], i: usize, depth: usize) -> Option<usize> {
    let close = match *t.get(i)? {
        "{" => "}",
        "[" => "]",
        "}" | "]" | "," | ":" => return None,
        _ => return Some(i + 1),
    };
    if depth >= JSON_DEPTH {
        return None;
    }
    let mut i = i + 1;
    if *t.get(i)? == close {
        return Some(i + 1);
    }
    loop {
        if close == "}" {
            if !t.get(i)?.starts_with('"') || *t.get(i + 1)? != ":" {
                return None;
            }
            i += 2;
        }
        i = json_value(t, i, depth + 1)?;
        match *t.get(i)? {
            "," => i += 1,
            c if c == close => return Some(i + 1),
            _ => return None,
        }
    }
}

/// `s` re-spaced: indented two spaces per level, or with no spaces at all.
/// Strings and numbers stay exactly as written. None unless `s` is one JSON
/// value.
pub fn json_reformat(s: &str, pretty: bool) -> Option<String> {
    let t = json_tokens(s)?;
    if json_value(&t, 0, 0)? != t.len() {
        return None;
    }
    let mut out = String::with_capacity(s.len() + s.len() / 2);
    let mut depth = 0usize;
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.push_str(&"  ".repeat(depth));
    };
    let mut i = 0;
    while i < t.len() {
        match t[i] {
            open @ ("{" | "[") => {
                let close = if open == "{" { "}" } else { "]" };
                out.push_str(open);
                if t.get(i + 1) == Some(&close) {
                    out.push_str(close); // {} and [] stay on one line
                    i += 1;
                } else {
                    depth += 1;
                    if pretty {
                        newline(&mut out, depth);
                    }
                }
            }
            close @ ("}" | "]") => {
                depth -= 1;
                if pretty {
                    newline(&mut out, depth);
                }
                out.push_str(close);
            }
            "," => {
                out.push(',');
                if pretty {
                    newline(&mut out, depth);
                }
            }
            ":" => out.push_str(if pretty { ": " } else { ":" }),
            tok => out.push_str(tok),
        }
        i += 1;
    }
    Some(out)
}

// ── Editor ─────────────────────────────────────────────────────────────────

/// How a tab shows in the editor (it is kept, and sent, as a tab).
#[cfg(not(windows))]
pub const TAB: char = '→';
#[cfg(windows)]
pub const TAB: char = '»';

/// Cells a character takes in the editor.
pub fn cell_width(c: char) -> usize {
    if c == '\t' { 1 } else { c.width().unwrap_or(0) }
}

/// A plain text editor: lines of characters, a cursor, and the scroll
/// position that keeps the cursor in view. Long lines scroll sideways.
pub struct Editor {
    pub lines: Vec<Vec<char>>,
    pub row: usize,
    pub col: usize, // character index in the line
    /// The column to return to on up and down, across shorter lines.
    want: Option<usize>,
    pub top: usize,  // first line shown
    pub left: usize, // first cell shown
}

impl Editor {
    pub fn new(text: &str) -> Editor {
        Editor {
            lines: text.split('\n').map(|l| l.chars().collect()).collect(),
            row: 0,
            col: 0,
            want: None,
            top: 0,
            left: 0,
        }
    }

    pub fn text(&self) -> String {
        let mut s = String::new();
        for (i, l) in self.lines.iter().enumerate() {
            if i > 0 {
                s.push('\n');
            }
            s.extend(l);
        }
        s
    }

    pub fn set_cursor(&mut self, row: usize, col: usize) {
        self.row = row.min(self.lines.len() - 1);
        self.col = col.min(self.lines[self.row].len());
        self.want = None;
    }

    /// The cell where character `col` of line `row` starts.
    pub fn cells_to(&self, row: usize, col: usize) -> usize {
        self.lines[row][..col].iter().map(|&c| cell_width(c)).sum()
    }

    /// The character that covers cell `x` of line `row`, or the line's end.
    fn col_at(&self, row: usize, x: usize) -> usize {
        let mut cells = 0;
        for (i, &c) in self.lines[row].iter().enumerate() {
            let w = cell_width(c);
            if cells + w > x {
                return i;
            }
            cells += w;
        }
        self.lines[row].len()
    }

    pub fn insert(&mut self, c: char) {
        if c == '\n' {
            return self.split_line(false);
        }
        self.lines[self.row].insert(self.col, c);
        self.col += 1;
        self.want = None;
    }

    /// Inserts pasted text: CR LF and lone CR become line breaks; other
    /// control characters (except tabs) are dropped.
    pub fn insert_str(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        let mut tail: Vec<char> = self.lines[self.row].split_off(self.col);
        for c in s.chars() {
            if c == '\n' {
                self.row += 1;
                self.lines.insert(self.row, Vec::new());
                self.col = 0;
            } else if !c.is_control() || c == '\t' {
                self.lines[self.row].push(c);
                self.col += 1;
            }
        }
        self.lines[self.row].append(&mut tail);
        self.want = None;
    }

    /// Enter: splits the line, keeping its indentation when `indent`.
    pub fn split_line(&mut self, indent: bool) {
        let tail = self.lines[self.row].split_off(self.col);
        let mut next: Vec<char> = if indent {
            self.lines[self.row]
                .iter()
                .take_while(|c| **c == ' ' || **c == '\t')
                .copied()
                .collect()
        } else {
            Vec::new()
        };
        self.col = next.len();
        next.extend(tail);
        self.row += 1;
        self.lines.insert(self.row, next);
        self.want = None;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            self.lines[self.row].remove(self.col);
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].extend(line);
        }
        self.want = None;
    }

    pub fn delete(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.lines[self.row].remove(self.col);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].extend(next);
        }
        self.want = None;
    }

    pub fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
        self.want = None;
    }

    pub fn right(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
        self.want = None;
    }

    /// Moves `delta` lines, keeping the column where it was before a run
    /// of up and down moves.
    pub fn vertical(&mut self, delta: isize) {
        let want = *self.want.get_or_insert(self.cells_to(self.row, self.col));
        let last = self.lines.len() as isize - 1;
        self.row = (self.row as isize + delta).clamp(0, last) as usize;
        self.col = self.col_at(self.row, want);
    }

    pub fn home(&mut self) {
        self.col = 0;
        self.want = None;
    }

    pub fn end(&mut self) {
        self.col = self.lines[self.row].len();
        self.want = None;
    }

    /// To the start of this word, or of the previous one.
    pub fn word_left(&mut self) {
        if self.col == 0 {
            return self.left();
        }
        let l = &self.lines[self.row];
        let mut i = self.col;
        while i > 0 && !l[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && l[i - 1].is_alphanumeric() {
            i -= 1;
        }
        self.col = i;
        self.want = None;
    }

    /// To the end of this word, or of the next one.
    pub fn word_right(&mut self) {
        let l = &self.lines[self.row];
        if self.col == l.len() {
            return self.right();
        }
        let mut i = self.col;
        while i < l.len() && !l[i].is_alphanumeric() {
            i += 1;
        }
        while i < l.len() && l[i].is_alphanumeric() {
            i += 1;
        }
        self.col = i;
        self.want = None;
    }

    /// Scrolls so the cursor is inside a view of `rows` lines by `cols` cells.
    pub fn scroll_into_view(&mut self, rows: usize, cols: usize) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if self.row < self.top {
            self.top = self.row;
        } else if self.row >= self.top + rows {
            self.top = self.row + 1 - rows;
        }
        self.top = self.top.min(self.lines.len().saturating_sub(rows));
        let x = self.cells_to(self.row, self.col);
        if x < self.left {
            // Show some of what's before the cursor, not just the cursor.
            self.left = x.saturating_sub(cols / 4);
        } else if x >= self.left + cols {
            self.left = x + 1 - cols;
        }
    }
}

// ── The composer ───────────────────────────────────────────────────────────

/// `s` in lines of at most `w` cells, broken between words where it can be.
fn word_wrap(s: &str, w: usize) -> Vec<String> {
    let w = w.max(8);
    let mut out = vec![String::new()];
    for word in s.split(' ') {
        let cur = out.last_mut().unwrap();
        let used = str_width(cur);
        if used > 0 && used + 1 + str_width(word) > w {
            out.push(String::new());
        } else if used > 0 {
            cur.push(' ');
        }
        // A word longer than a line is cut where the line ends.
        for c in word.chars() {
            let cur = out.last_mut().unwrap();
            if str_width(cur) + c.width().unwrap_or(0) > w {
                out.push(String::new());
            }
            out.last_mut().unwrap().push(c);
        }
    }
    out
}

/// What the app should do after a key.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Send,
    Close,
}

/// The editor's place on screen: the box fills it but the bottom (hint)
/// line, the status lines come first, then a line-number gutter.
pub struct EditView {
    pub rows: usize,
    pub cols: usize,
    pub gutter: usize, // cells for line numbers, with a space after
}

pub struct Composer {
    pub ed: Editor,
    pub title: String,
    pub error: Option<String>,
    /// Shown above the document while it's open (a truncated payload).
    pub warning: Option<String>,
    /// A payload that can't be sent as it is (cut off on arrival), and why.
    guard: Option<(String, String)>,
    dirty: bool,
    /// Esc was pressed once on an edited document; again discards it.
    discard: bool,
}

/// A size worth suggesting for --max-payload to keep `size` bytes whole.
fn max_payload_for(size: usize) -> String {
    if size <= 1 << 20 {
        format!("{}KiB", size.div_ceil(1 << 10).next_power_of_two())
    } else {
        format!("{}MiB", size.div_ceil(1 << 20).next_power_of_two())
    }
}

impl Composer {
    fn with(title: String, text: &str) -> Composer {
        let mut ed = Editor::new(text);
        // Start on the payload, or after "topic: " when there's no topic yet.
        if text.starts_with("topic: \n") {
            ed.set_cursor(0, 7);
        } else if let Some(blank) = text.split('\n').position(|l| l.trim().is_empty()) {
            ed.set_cursor(blank + 1, 0);
        }
        Composer {
            ed,
            title,
            error: None,
            warning: None,
            guard: None,
            dirty: false,
            discard: false,
        }
    }

    /// Opens a copy of `m`, received on `conn` at `topic`, to edit and send.
    pub fn republish(conn: &str, topic: &str, m: &Message) -> Composer {
        let props = Props {
            subscription_id: None,
            // Brokers forward the time a message has left, not the expiry it
            // was published with, so the received value would cut the
            // copy's life short (to nothing, for an old retained message).
            message_expiry: None,
            ..m.props.clone()
        };
        let d = Draft {
            conn: conn.to_string(),
            out: Outgoing {
                topic: topic.to_string(),
                payload: m.payload.clone(),
                qos: m.qos,
                retain: m.retain,
                props,
            },
        };
        let text = document(&d);
        let mut c = Composer::with(
            format!("Publish · copy of the message from {}", clock(m.time, false)),
            &text,
        );
        if m.truncated() {
            let (kept, size) = (human_bytes(m.payload.len()), human_bytes(m.size));
            c.warning = Some(format!(
                "only the first {kept} of this {size} payload were kept (--max-payload), so it can't be sent unchanged"
            ));
            let why = format!(
                "not sent: edit the payload, or restart with --max-payload {} to republish it whole",
                max_payload_for(m.size)
            );
            c.guard = Some((split_doc(&text).1.map_or("", |b| b.1).to_string(), why));
        }
        c
    }

    /// A new, empty message for `topic` on `conn`.
    pub fn new_message(conn: &str, topic: &str) -> Composer {
        let d = Draft {
            conn: conn.to_string(),
            out: Outgoing {
                topic: topic.to_string(),
                payload: Vec::new(),
                qos: 0,
                retain: false,
                props: Props::default(),
            },
        };
        Composer::with("Publish · new message".into(), &document(&d))
    }

    /// Status lines over the document, wrapped to the box (`width` is the
    /// screen's), with whether each is an error.
    pub fn status(&self, width: u16) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        let mut add = |text: &str, error: bool| {
            out.extend(
                word_wrap(text, (width as usize).saturating_sub(4))
                    .into_iter()
                    .map(|l| (l, error)),
            );
        };
        if let Some(w) = &self.warning {
            add(w, false);
        }
        if self.discard {
            add("esc again discards your changes", false);
        } else if let Some(e) = &self.error {
            add(e, true);
        }
        out
    }

    pub fn view(&self, width: u16, height: u16) -> EditView {
        let gutter = self.ed.lines.len().to_string().len() + 1;
        EditView {
            // The box leaves the hint line; its border takes two rows.
            rows: (height as usize).saturating_sub(3 + self.status(width).len()).max(1),
            // Borders and padding take four cells, the gutter its own, and
            // one more marks lines that go on past the right edge.
            cols: (width as usize).saturating_sub(4 + gutter + 1).max(1),
            gutter,
        }
    }

    /// Keeps the cursor in view on a `width` × `height` screen.
    pub fn fit(&mut self, width: u16, height: u16) {
        let v = self.view(width, height);
        self.ed.scroll_into_view(v.rows, v.cols);
    }

    /// The index of the first payload line (after the blank one), if any.
    pub fn payload_row(&self) -> Option<usize> {
        self.ed
            .lines
            .iter()
            .position(|l| l.iter().all(|c| c.is_whitespace()))
            .map(|i| i + 1)
    }

    fn edited(&mut self) {
        self.dirty = true;
    }

    pub fn paste(&mut self, s: &str) {
        self.discard = false;
        self.ed.insert_str(s);
        self.edited();
    }

    /// Handles a key; `page` is how many lines pgup and pgdn move.
    pub fn key(&mut self, k: KeyEvent, page: usize) -> Action {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if k.code != KeyCode::Esc {
            self.discard = false;
        }
        let ed = &mut self.ed;
        match k.code {
            KeyCode::Esc if self.dirty && !self.discard => self.discard = true,
            KeyCode::Esc => return Action::Close,
            KeyCode::Char('s') if ctrl && !alt => return Action::Send,
            KeyCode::Char('f') if ctrl && !alt => self.toggle_json(),
            KeyCode::Char('b') if alt && !ctrl => ed.word_left(),
            KeyCode::Char('f') if alt && !ctrl => ed.word_right(),
            // AltGr (for { [ @ on many keyboards) comes as ctrl+alt on Windows.
            KeyCode::Char(c) if ctrl == alt => {
                ed.insert(c);
                self.edited();
            }
            KeyCode::Enter => {
                ed.split_line(true);
                self.edited();
            }
            KeyCode::Tab => {
                ed.insert_str("  ");
                self.edited();
            }
            KeyCode::Backspace => {
                ed.backspace();
                self.edited();
            }
            KeyCode::Delete => {
                ed.delete();
                self.edited();
            }
            KeyCode::Left if ctrl || alt => ed.word_left(),
            KeyCode::Right if ctrl || alt => ed.word_right(),
            KeyCode::Left => ed.left(),
            KeyCode::Right => ed.right(),
            KeyCode::Up => ed.vertical(-1),
            KeyCode::Down => ed.vertical(1),
            KeyCode::PageUp => ed.vertical(-(page.max(1) as isize)),
            KeyCode::PageDown => ed.vertical(page.max(1) as isize),
            KeyCode::Home if ctrl => ed.set_cursor(0, 0),
            KeyCode::End if ctrl => {
                let last = ed.lines.len() - 1;
                ed.set_cursor(last, usize::MAX);
            }
            KeyCode::Home => ed.home(),
            KeyCode::End => ed.end(),
            _ => {}
        }
        Action::None
    }

    /// Ctrl+F: the payload as indented JSON, or back to one compact line.
    fn toggle_json(&mut self) {
        let Some(row) = self.payload_row() else {
            self.error = Some("there's no payload (a blank line after the headers starts it)".into());
            return;
        };
        let text = self.ed.text();
        let payload = split_doc(&text).1.map_or("", |b| b.1);
        let Some(compact) = json_reformat(payload, false) else {
            self.error = Some("the payload isn't JSON".into());
            return;
        };
        let new = if payload == compact {
            json_reformat(payload, true).unwrap_or(compact)
        } else {
            compact
        };
        self.ed.lines.truncate(row);
        self.ed
            .lines
            .extend(new.split('\n').map(|l| l.chars().collect::<Vec<_>>()));
        self.ed.set_cursor(row, 0);
        self.error = None;
        self.edited();
    }

    /// Shows `msg` as a problem with the header line for `key`, and moves
    /// the cursor there.
    pub fn fail_at(&mut self, key: &str, msg: String) {
        let row = self.ed.lines.iter().position(|l| {
            let l: String = l.iter().collect();
            l.split_once(':')
                .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(key))
        });
        match row {
            Some(r) => {
                self.error = Some(format!("line {}: {msg}", r + 1));
                self.ed.set_cursor(r, usize::MAX);
            }
            None => self.error = Some(msg),
        }
    }

    /// The message to send, or None after showing why it can't be sent.
    pub fn draft(&mut self) -> Option<Draft> {
        self.discard = false;
        let text = self.ed.text();
        match parse(&text) {
            Err(e) => {
                self.error = Some(format!("line {}: {}", e.line, e.msg));
                self.ed.set_cursor(e.line - 1, usize::MAX);
                None
            }
            Ok(d) => {
                if let Some((kept, why)) = &self.guard
                    && split_doc(&text).1.map_or("", |b| b.1) == kept
                {
                    self.error = Some(why.clone());
                    return None;
                }
                self.error = None;
                Some(d)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::SystemTime;

    fn msg(payload: &[u8], size: usize, props: Props) -> Arc<Message> {
        Arc::new(Message {
            seq: 1,
            time: SystemTime::now(),
            payload: payload.to_vec(),
            size,
            qos: 1,
            retain: true,
            props,
            decoded: None,
        })
    }

    fn key(c: &mut Composer, code: KeyCode, mods: KeyModifiers) -> Action {
        c.key(KeyEvent::new(code, mods), 10)
    }

    fn typ(c: &mut Composer, s: &str) {
        for ch in s.chars() {
            key(c, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    fn all_props() -> Props {
        Props {
            content_type: Some("application/json".into()),
            response_topic: Some("site/reply".into()),
            correlation_data: Some(vec![0, 1, 2, 0xff]),
            payload_format: Some(1),
            message_expiry: Some(60),
            subscription_id: Some(7),
            user: vec![
                ("TurbineId".into(), "T01".into()),
                ("expr".into(), "a=b".into()),
                ("Quality".into(), "Good".into()),
            ],
        }
    }

    #[test]
    fn republish_roundtrips_text_and_props() {
        let m = msg(br#"{"t":21.5}"#, 10, all_props());
        let c = Composer::republish("oslo", "site/x", &m);
        let text = c.ed.text();
        assert!(
            text.starts_with("topic: site/x\nconnection: oslo\nqos: 1\nretain: yes\n"),
            "{text}"
        );
        assert!(text.contains("user: expr = a=b\n") && text.contains("correlation-data: base64:AAEC/w==\n"));
        assert!(text.ends_with("\n\n{\"t\":21.5}"));
        assert!(!text.contains("subscription"), "subscription ids aren't sent");
        assert!(
            !text.contains("message-expiry"),
            "the received expiry is what was left of it"
        );
        let d = parse(&text).unwrap();
        assert_eq!(d.conn, "oslo");
        assert_eq!(d.out.topic, "site/x");
        assert_eq!((d.out.qos, d.out.retain), (1, true));
        assert_eq!(d.out.payload, br#"{"t":21.5}"#);
        assert_eq!(
            d.out.props,
            Props {
                subscription_id: None,
                message_expiry: None,
                ..all_props()
            }
        );
        // The cursor starts on the payload.
        assert_eq!(c.ed.lines[c.ed.row].iter().collect::<String>(), r#"{"t":21.5}"#);
        // An expiry typed in is sent, and written back by document().
        let typed = parse("topic: a\nconnection: c\nmessage-expiry: 30").unwrap();
        assert_eq!(typed.out.props.message_expiry, Some(30));
        assert!(document(&typed).contains("\nmessage-expiry: 30\n"));
        assert_eq!(parse(&document(&typed)).unwrap(), typed);
    }

    #[test]
    fn binary_payloads_are_base64_and_exact() {
        let bin: Vec<u8> = (0..=255).collect();
        let d = Draft {
            conn: "c".into(),
            out: Outgoing {
                topic: "t".into(),
                payload: bin.clone(),
                qos: 0,
                retain: false,
                props: Props::default(),
            },
        };
        let text = document(&d);
        assert!(text.contains("encoding: base64\n\n"));
        assert!(text.split('\n').all(|l| l.len() <= 76), "base64 wraps at 76");
        assert_eq!(parse(&text).unwrap(), d);
        // CR LF text can't be edited as is, so it's base64 too.
        let crlf = Draft {
            out: Outgoing {
                payload: b"a\r\nb".to_vec(),
                ..d.out.clone()
            },
            ..d.clone()
        };
        assert!(document(&crlf).contains("encoding: base64"));
        assert_eq!(parse(&document(&crlf)).unwrap(), crlf);
        // Tabs, trailing newlines and blank lines at the start stay as they were.
        for p in ["a\tb\n", "\n\nx", "", "line1\n\nline3"] {
            let t = Draft {
                out: Outgoing {
                    payload: p.into(),
                    ..d.out.clone()
                },
                ..d.clone()
            };
            assert!(!document(&t).contains("encoding"), "{p:?}");
            assert_eq!(parse(&document(&t)).unwrap(), t, "{p:?}");
        }
    }

    #[test]
    fn hex_and_text_encodings() {
        let d = parse("topic: a/b\nconnection: c\nencoding: hex\n\n00 ff 10\n7f").unwrap();
        assert_eq!(d.out.payload, [0, 0xff, 0x10, 0x7f]);
        let d = parse("Topic: a/b\nCONNECTION: c\nencoding: TEXT\n\nhi").unwrap();
        assert_eq!(d.out.payload, b"hi");
        // No blank line: no payload.
        let d = parse("topic: a/b\nconnection: c").unwrap();
        assert!(d.out.payload.is_empty());
    }

    #[test]
    fn errors_name_the_line() {
        let cases = [
            ("connection: c\n\nx", 1, "no topic"),
            ("topic: a/+\nconnection: c", 1, "+ or #"),
            ("topic: a\nconnection: c\nqos: 3", 3, "qos must be"),
            ("topic: a\nconnection: c\nretain: maybe", 3, "retain must be"),
            ("topic: a\ntopic: b\nconnection: c", 2, "given twice"),
            ("topic: a\nconnection: c\nflavour: x", 3, "unknown key \"flavour\""),
            ("topic: a\nconnection: c\nno colon here", 3, "expected key: value"),
            ("topic: a\nconnection: c\nuser: novalue", 3, "user: key = value"),
            ("topic: a\nconnection: c\nmessage-expiry: soon", 3, "seconds"),
            ("topic: a\nconnection: c\nencoding: base64\n\n!!", 5, "valid base64"),
            ("topic: a\nconnection: c\nencoding: hex\n\nabc", 5, "valid hex"),
            (
                "topic: a\nconnection: c\npayload-format: utf8\nencoding: hex\n\nff",
                3,
                "isn't UTF-8",
            ),
            ("topic: a\n\nx", 1, "no connection"),
        ];
        for (text, line, msg) in cases {
            let e = parse(text).unwrap_err();
            assert_eq!(e.line, line, "{text:?}: {e:?}");
            assert!(e.msg.contains(msg), "{text:?}: {e:?}");
        }
    }

    #[test]
    fn truncated_payload_must_be_edited() {
        let m = msg(&[b'x'; 4096], 5 << 20, Props::default());
        let mut c = Composer::republish("oslo", "big", &m);
        assert!(c.warning.as_deref().unwrap().contains("4.0 KB of this 5.0 MB"));
        assert!(c.status(40).len() > 1, "long status lines wrap");
        assert!(c.status(40).iter().all(|(l, _)| str_width(l) <= 36));
        assert_eq!(c.draft(), None);
        let e = c.error.clone().unwrap();
        assert!(e.contains("--max-payload 8MiB"), "{e}");
        // Header edits don't count; changing the payload does.
        c.ed.set_cursor(2, usize::MAX);
        c.ed.backspace();
        c.ed.insert('0');
        assert_eq!(c.ed.lines[2].iter().collect::<String>(), "qos: 0");
        c.error = None;
        assert_eq!(c.draft(), None);
        assert_eq!(c.error, Some(e));
        let row = c.payload_row().unwrap();
        c.ed.set_cursor(row, 0);
        c.ed.insert('y');
        let d = c.draft().expect("an edited payload can be sent");
        assert_eq!((d.out.qos, d.out.payload.len()), (0, 4097));
        assert_eq!(max_payload_for(5000), "8KiB");
        assert_eq!(max_payload_for(1 << 20), "1024KiB");
    }

    #[test]
    fn editor_basics() {
        let mut e = Editor::new("ab\ncd");
        e.set_cursor(0, 1);
        e.insert('x');
        assert_eq!(e.text(), "axb\ncd");
        e.split_line(false);
        assert_eq!((e.text().as_str(), e.row, e.col), ("ax\nb\ncd", 1, 0));
        e.backspace();
        assert_eq!((e.text().as_str(), e.row, e.col), ("axb\ncd", 0, 2));
        e.end();
        e.delete(); // joins the next line
        assert_eq!(e.text(), "axbcd");
        e.home();
        e.left(); // stays at the very start
        assert_eq!((e.row, e.col), (0, 0));
        e.insert_str("1\r\n2\r3\u{7}\t");
        assert_eq!(e.text(), "1\n2\n3\taxbcd");
        assert_eq!((e.row, e.col), (2, 2));
        // Enter keeps the indentation.
        let mut e = Editor::new("  {\"a\": 1,");
        e.set_cursor(0, usize::MAX);
        e.split_line(true);
        assert_eq!((e.text().as_str(), e.col), ("  {\"a\": 1,\n  ", 2));
    }

    #[test]
    fn editor_moves_and_scrolls() {
        let mut e = Editor::new("short\na much longer line here\nx\n風力 turbine");
        e.set_cursor(1, 20);
        e.vertical(1); // to a shorter line: its end
        assert_eq!((e.row, e.col), (2, 1));
        e.vertical(-1); // back: the remembered column
        assert_eq!((e.row, e.col), (1, 20));
        e.set_cursor(3, 3); // after "風力 " (5 cells)
        assert_eq!(e.cells_to(3, 3), 5);
        e.vertical(-1);
        assert_eq!(e.col, 1, "column by cells, not characters");
        e.set_cursor(1, 2);
        e.word_right();
        assert_eq!(e.col, 6);
        e.word_right();
        assert_eq!(e.col, 13);
        e.word_left();
        assert_eq!(e.col, 7);
        // Scrolling keeps the cursor inside a 2 × 8 view.
        e.set_cursor(1, 20);
        e.scroll_into_view(2, 8);
        assert!(e.row >= e.top && e.row < e.top + 2);
        let x = e.cells_to(e.row, e.col);
        assert!(x >= e.left && x < e.left + 8, "{x} {}", e.left);
        e.set_cursor(0, 0);
        e.scroll_into_view(2, 8);
        assert_eq!((e.top, e.left), (0, 0));
    }

    #[test]
    fn keys_edit_and_esc_twice_discards() {
        let mut c = Composer::new_message("oslo", "");
        assert_eq!((c.ed.row, c.ed.col), (0, 7), "cursor after topic: ");
        assert_eq!(
            key(&mut c, KeyCode::Esc, KeyModifiers::NONE),
            Action::Close,
            "untouched closes at once"
        );
        typ(&mut c, "a/b");
        assert_eq!(c.ed.lines[0].iter().collect::<String>(), "topic: a/b");
        assert_eq!(key(&mut c, KeyCode::Esc, KeyModifiers::NONE), Action::None);
        assert!(c.status(80).iter().any(|(s, _)| s.contains("esc again")));
        key(&mut c, KeyCode::Left, KeyModifiers::NONE); // any other key cancels the warning
        assert!(c.status(80).is_empty());
        key(&mut c, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(key(&mut c, KeyCode::Esc, KeyModifiers::NONE), Action::Close);
        assert_eq!(key(&mut c, KeyCode::Char('s'), KeyModifiers::CONTROL), Action::Send);
        key(&mut c, KeyCode::End, KeyModifiers::CONTROL);
        typ(&mut c, "hello");
        let d = c.draft().unwrap();
        assert_eq!((d.out.topic.as_str(), d.out.payload.as_slice()), ("a/b", &b"hello"[..]));
    }

    #[test]
    fn json_pretty_and_compact() {
        let src = r#"{"a" : [1, 2.5e3, {}], "b":{"c":"x, y: {z}"},"d":[], "e": true}"#;
        let compact = json_reformat(src, false).unwrap();
        assert_eq!(compact, r#"{"a":[1,2.5e3,{}],"b":{"c":"x, y: {z}"},"d":[],"e":true}"#);
        let pretty = json_reformat(src, true).unwrap();
        assert_eq!(
            pretty,
            "{\n  \"a\": [\n    1,\n    2.5e3,\n    {}\n  ],\n  \"b\": {\n    \"c\": \"x, y: {z}\"\n  },\n  \"d\": [],\n  \"e\": true\n}"
        );
        assert_eq!(json_reformat(&pretty, false).unwrap(), compact);
        for bad in [
            r#"{"a":1"#,
            r#"{"a" 1}"#,
            "[1,]",
            "{1:2}",
            "nope",
            "1 2",
            r#"{"a":1}x"#,
            "",
            "[\"open",
        ] {
            assert_eq!(json_reformat(bad, false), None, "{bad:?}");
        }
        assert_eq!(json_reformat(" 42 ", true).as_deref(), Some("42"));

        // In the composer, ctrl+f flips the payload between the two.
        let m = msg(src.as_bytes(), src.len(), Props::default());
        let mut c = Composer::republish("oslo", "t", &m);
        key(&mut c, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(c.draft().unwrap().out.payload, compact.as_bytes());
        key(&mut c, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(c.draft().unwrap().out.payload, pretty.as_bytes());
        let mut c = Composer::new_message("oslo", "t");
        key(&mut c, KeyCode::Char('f'), KeyModifiers::CONTROL);
        assert_eq!(c.error.as_deref(), Some("the payload isn't JSON"));
    }
}
