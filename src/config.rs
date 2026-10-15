//! The connections file: a small TOML subset, edited by hand or written when
//! you connect from the command line.
//!
//! ```toml
//! [[connection]]
//! name         = "local"
//! url          = "mqtts://127.0.0.1:8883"
//! cafile       = "~/secrets/ca.pem"
//! username     = "backend"
//! password_env = "BROKER_PW"        # read from this environment variable
//! topics       = ["devices/+/up/#", "$SYS/#"]
//! paused       = ["$SYS/#"]         # listed but not subscribed
//! capture      = ["devices/d1/#"]   # written to disk (opt-in, see capture.rs)
//! autoconnect  = true
//! ```
//!
//! Passwords are never written to the file. Saving an entry rewrites only that
//! entry's `[[connection]]` block, so comments elsewhere are kept.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ConnCfg {
    pub name: String,
    pub url: String,
    pub cafile: Option<String>,
    pub cert: Option<String>,
    pub key: Option<String>,
    pub insecure: bool,
    pub username: Option<String>,
    pub password_env: Option<String>,
    pub key_password_env: Option<String>,
    pub client_id: Option<String>,
    pub qos: u8,
    pub topics: Vec<String>,
    pub paused: Vec<String>,
    /// Topic filters captured to disk, and those of them turned off.
    pub capture: Vec<String>,
    pub capture_paused: Vec<String>,
    pub autoconnect: bool,
}

/// Default location: $XDG_CONFIG_HOME or ~/.config on Unix, %APPDATA% on Windows.
pub fn default_path() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|h| h.join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("fss-mqtt")
        .join("connections.toml")
}

fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// Expands a leading `~/` to the home directory.
pub fn expand(p: &str) -> String {
    match (p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")), home()) {
        (Some(rest), Some(h)) => h.join(rest).to_string_lossy().into_owned(),
        _ => p.to_string(),
    }
}

/// `p` with the home directory written as `~`, for display.
pub fn tilde(p: &Path) -> String {
    match home().and_then(|h| p.strip_prefix(h).ok().map(Path::to_path_buf)) {
        Some(rest) if !rest.as_os_str().is_empty() => {
            format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
        }
        _ => p.display().to_string(),
    }
}

/// Loads the file; a missing file is an empty list.
pub fn load(path: &Path) -> Result<Vec<ConnCfg>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Adds the entry, or replaces the existing one with the same name.
pub fn save(path: &Path, c: &ConnCfg) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let out = upsert(&text, c).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
}

const HEADER: &str = "\
# fss-mqtt connections. Edit freely; see `fss-mqtt --help`.
# Passwords are not stored: set password_env (or key_password_env, for an
# encrypted key) to an environment variable name, or fss-mqtt asks when it connects.
";

/// Returns `text` with `c`'s block replaced (by name) or appended.
fn upsert(text: &str, c: &ConnCfg) -> Result<String, String> {
    let blocks = parse_blocks(text)?;
    let lines: Vec<&str> = text.lines().collect();
    let rendered = render(c);
    if let Some(b) = blocks.iter().find(|b| b.cfg.name == c.name) {
        let mut out: Vec<String> = lines[..b.start].iter().map(|s| s.to_string()).collect();
        out.push(rendered.trim_end().to_string());
        out.extend(lines[b.end..].iter().map(|s| s.to_string()));
        return Ok(out.join("\n") + "\n");
    }
    let mut out = if text.trim().is_empty() {
        HEADER.to_string()
    } else {
        text.to_string()
    };
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&rendered);
    Ok(out)
}

fn render(c: &ConnCfg) -> String {
    let mut s = String::from("[[connection]]\n");
    let mut kv = |k: &str, v: String| s.push_str(&format!("{k:<12} = {v}\n"));
    kv("name", quote(&c.name));
    kv("url", quote(&c.url));
    for (k, v) in [
        ("cafile", &c.cafile),
        ("cert", &c.cert),
        ("key", &c.key),
        ("username", &c.username),
        ("password_env", &c.password_env),
        ("key_password_env", &c.key_password_env),
        ("client_id", &c.client_id),
    ] {
        if let Some(v) = v {
            kv(k, quote(v));
        }
    }
    if c.insecure {
        kv("insecure", "true".into());
    }
    if c.qos != 0 {
        kv("qos", c.qos.to_string());
    }
    kv("topics", list(&c.topics));
    if !c.paused.is_empty() {
        kv("paused", list(&c.paused));
    }
    if !c.capture.is_empty() {
        kv("capture", list(&c.capture));
    }
    if !c.capture_paused.is_empty() {
        kv("capture_paused", list(&c.capture_paused));
    }
    kv("autoconnect", c.autoconnect.to_string());
    s
}

fn quote(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn list(v: &[String]) -> String {
    format!("[{}]", v.iter().map(|s| quote(s)).collect::<Vec<_>>().join(", "))
}

// ── Parsing ────────────────────────────────────────────────────────────────

pub fn parse(text: &str) -> Result<Vec<ConnCfg>, String> {
    let blocks = parse_blocks(text)?;
    let mut names = std::collections::HashSet::new();
    for b in &blocks {
        if !names.insert(b.cfg.name.clone()) {
            return Err(format!(
                "line {}: duplicate connection name {:?}",
                b.start + 1,
                b.cfg.name
            ));
        }
    }
    Ok(blocks.into_iter().map(|b| b.cfg).collect())
}

struct Block {
    cfg: ConnCfg,
    start: usize, // line index of [[connection]]
    end: usize,   // one past the last line belonging to the block
}

enum Value {
    Str(String),
    Bool(bool),
    Int(i64),
    List(Vec<String>),
}

fn parse_blocks(text: &str) -> Result<Vec<Block>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks: Vec<Block> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = strip_comment(lines[i]).trim().to_string();
        let lineno = i + 1;
        i += 1;
        if line.is_empty() {
            continue;
        }
        if line == "[[connection]]" {
            if let Some(b) = blocks.last_mut() {
                b.end = trim_trailing_blank(&lines, b.start, lineno - 1);
            }
            blocks.push(Block {
                cfg: ConnCfg::default(),
                start: lineno - 1,
                end: lines.len(),
            });
            continue;
        }
        if line.starts_with('[') {
            return Err(format!("line {lineno}: unknown section {line}"));
        }
        let Some(b) = blocks.last_mut() else {
            return Err(format!("line {lineno}: expected [[connection]] before settings"));
        };
        let (key, rest) = line
            .split_once('=')
            .ok_or_else(|| format!("line {lineno}: expected key = value"))?;
        let key = key.trim();
        let mut raw = rest.trim().to_string();
        // Lists may span lines.
        while raw.starts_with('[') && !list_closed(&raw) {
            let Some(next) = lines.get(i) else {
                return Err(format!("line {lineno}: unterminated list"));
            };
            raw.push(' ');
            raw.push_str(strip_comment(next).trim());
            i += 1;
        }
        let v = parse_value(&raw).map_err(|e| format!("line {lineno}: {key}: {e}"))?;
        set(&mut b.cfg, key, v).map_err(|e| format!("line {lineno}: {e}"))?;
    }
    if let Some(b) = blocks.last_mut() {
        b.end = trim_trailing_blank(&lines, b.start, lines.len());
    }
    for b in &blocks {
        if b.cfg.name.is_empty() {
            return Err(format!("line {}: connection has no name", b.start + 1));
        }
        if b.cfg.url.is_empty() {
            return Err(format!("line {}: connection {:?} has no url", b.start + 1, b.cfg.name));
        }
    }
    Ok(blocks)
}

/// Blank lines and comments at the end of a block belong to whatever follows.
fn trim_trailing_blank(lines: &[&str], start: usize, mut end: usize) -> usize {
    while end > start + 1 {
        let l = lines[end - 1].trim();
        if l.is_empty() || l.starts_with('#') {
            end -= 1;
        } else {
            break;
        }
    }
    end
}

/// Removes a trailing `# comment`, ignoring `#` inside strings.
fn strip_comment(line: &str) -> &str {
    let mut in_str: Option<char> = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match in_str {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' && q == '"' {
                    escaped = true;
                } else if c == q {
                    in_str = None;
                }
            }
            None if c == '"' || c == '\'' => in_str = Some(c),
            None if c == '#' => return &line[..i],
            None => {}
        }
    }
    line
}

fn list_closed(raw: &str) -> bool {
    strip_comment(raw).trim_end().ends_with(']')
}

fn parse_value(raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if raw == "true" || raw == "false" {
        return Ok(Value::Bool(raw == "true"));
    }
    if raw.starts_with('"') || raw.starts_with('\'') {
        let (s, rest) = parse_string(raw)?;
        if !rest.trim().is_empty() {
            return Err(format!("unexpected text after string: {rest}"));
        }
        return Ok(Value::Str(s));
    }
    if let Some(inner) = raw.strip_prefix('[') {
        let mut items = Vec::new();
        let mut rest = inner.trim_start();
        loop {
            if let Some(r) = rest.strip_prefix(']') {
                if !r.trim().is_empty() {
                    return Err("unexpected text after list".into());
                }
                return Ok(Value::List(items));
            }
            let (s, r) = parse_string(rest)?;
            items.push(s);
            rest = r.trim_start();
            if let Some(r) = rest.strip_prefix(',') {
                rest = r.trim_start();
            } else if !rest.starts_with(']') {
                return Err("expected , or ] in list".into());
            }
        }
    }
    raw.parse::<i64>()
        .map(Value::Int)
        .map_err(|_| format!("can't read value {raw:?} (strings need quotes)"))
}

/// Parses a leading "basic" or 'literal' string; returns it and the remainder.
fn parse_string(s: &str) -> Result<(String, &str), String> {
    let mut chars = s.char_indices();
    let quote = match chars.next() {
        Some((_, q @ ('"' | '\''))) => q,
        _ => return Err(format!("expected a quoted string at {s:?}")),
    };
    let mut out = String::new();
    while let Some((i, c)) = chars.next() {
        if c == quote {
            return Ok((out, &s[i + 1..]));
        }
        if c == '\\' && quote == '"' {
            match chars.next().map(|(_, c)| c) {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(o) => return Err(format!("unsupported escape \\{o}")),
                None => break,
            }
        } else {
            out.push(c);
        }
    }
    Err("unterminated string".into())
}

fn set(c: &mut ConnCfg, key: &str, v: Value) -> Result<(), String> {
    let s = |v: Value| match v {
        Value::Str(s) => Ok(s),
        _ => Err(format!("{key} must be a string")),
    };
    match key {
        "name" => c.name = s(v)?,
        "url" => c.url = s(v)?,
        "cafile" => c.cafile = Some(s(v)?),
        "cert" => c.cert = Some(s(v)?),
        "key" => c.key = Some(s(v)?),
        "username" => c.username = Some(s(v)?),
        "password_env" => c.password_env = Some(s(v)?),
        "key_password_env" => c.key_password_env = Some(s(v)?),
        "client_id" => c.client_id = Some(s(v)?),
        "insecure" | "autoconnect" => {
            let Value::Bool(b) = v else {
                return Err(format!("{key} must be true or false"));
            };
            if key == "insecure" {
                c.insecure = b;
            } else {
                c.autoconnect = b;
            }
        }
        "qos" => match v {
            Value::Int(q @ 0..=2) => c.qos = q as u8,
            _ => return Err("qos must be 0, 1 or 2".into()),
        },
        "topics" | "paused" | "capture" | "capture_paused" => {
            let Value::List(l) = v else {
                return Err(format!("{key} must be a list of strings"));
            };
            if key.starts_with("capture")
                && let Some(bad) = l.iter().find(|f| !crate::capture::valid_filter(f))
            {
                return Err(format!("{key}: {bad:?} is not a topic filter"));
            }
            match key {
                "topics" => c.topics = l,
                "paused" => c.paused = l,
                "capture" => c.capture = l,
                _ => c.capture_paused = l,
            }
        }
        "password" => {
            return Err("passwords are not read from the file; use password_env = \"VAR\"".into());
        }
        "key_password" => {
            return Err("passphrases are not read from the file; use key_password_env = \"VAR\"".into());
        }
        other => return Err(format!("unknown setting {other:?}")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"
# my brokers
[[connection]]
name = "local"   # the dev broker
url = "mqtts://127.0.0.1:8883"
cafile = '~/secrets/ca.pem'
username = "backend"
password_env = "BROKER_PW"
topics = [
    "devices/+/up/#",  # devices
    "$SYS/#",
]
paused = ["$SYS/#"]
capture = ["devices/d1/#", "#"]
capture_paused = ["#"]
autoconnect = true

# staging, off by default
[[connection]]
name = "staging"
url = "mqtt://staging:1883"
qos = 1
topics = ["#"]
"##;

    #[test]
    fn parses_sample() {
        let c = parse(SAMPLE).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].name, "local");
        assert_eq!(c[0].cafile.as_deref(), Some("~/secrets/ca.pem"));
        assert_eq!(c[0].topics, vec!["devices/+/up/#", "$SYS/#"]);
        assert_eq!(c[0].paused, vec!["$SYS/#"]);
        assert_eq!(c[0].capture, vec!["devices/d1/#", "#"]);
        assert_eq!(c[0].capture_paused, vec!["#"]);
        assert!(c[0].autoconnect);
        assert!(c[1].capture.is_empty());
        assert_eq!((c[1].qos, c[1].autoconnect), (1, false));
    }

    #[test]
    fn errors_name_the_line() {
        assert!(parse("name = \"x\"").unwrap_err().contains("line 1"));
        let e = parse("[[connection]]\nname = \"a\"\nurl = \"mqtt://x\"\npassword = \"secret\"").unwrap_err();
        assert!(e.contains("line 4") && e.contains("password_env"), "{e}");
        let e =
            parse("[[connection]]\nname = \"a\"\nurl = \"mqtt://x\"\n[[connection]]\nname = \"a\"\nurl = \"mqtt://y\"")
                .unwrap_err();
        assert!(e.contains("duplicate"), "{e}");
        assert!(parse("[[connection]]\nname = \"a\"").unwrap_err().contains("no url"));
        let e = parse("[[connection]]\nname = \"a\"\nurl = \"mqtt://x\"\ncapture = [\"a/#/b\"]").unwrap_err();
        assert!(e.contains("line 4") && e.contains("not a topic filter"), "{e}");
    }

    #[test]
    fn upsert_replaces_one_block_and_keeps_comments() {
        let mut c = parse(SAMPLE).unwrap()[0].clone();
        c.paused.clear();
        c.topics.push("alarms/#".into());
        c.capture_paused.clear();
        c.capture.push("alarms/#".into());
        let out = upsert(SAMPLE, &c).unwrap();
        assert!(out.contains("# my brokers"));
        assert!(out.contains("# staging, off by default"));
        let back = parse(&out).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], c);
        assert_eq!(back[1], parse(SAMPLE).unwrap()[1]);
    }

    #[test]
    fn upsert_appends_new_and_roundtrips_escapes() {
        let c = ConnCfg {
            name: "odd \"name\"".into(),
            url: "mqtt://h:1883".into(),
            topics: vec!["a/#".into(), "b\\c".into()],
            autoconnect: true,
            ..Default::default()
        };
        let out = upsert("", &c).unwrap();
        assert!(out.starts_with("# fss-mqtt connections"));
        assert_eq!(parse(&out).unwrap(), vec![c.clone()]);
        let out2 = upsert(
            &out,
            &ConnCfg {
                name: "two".into(),
                ..c.clone()
            },
        )
        .unwrap();
        assert_eq!(parse(&out2).unwrap().len(), 2);
    }
}
