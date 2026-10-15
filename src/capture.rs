//! Opt-in capture to disk. Each connection has capture rules (MQTT topic
//! filters); messages matching an enabled rule are appended, whole, to one
//! JSON Lines file per connection per run. Nothing is written until a rule
//! matches, and the file is created then.
//!
//! The connection thread checks the rules under the store lock (cheap) and
//! writes after releasing it, through the connection's [`Sink`], which has
//! its own lock; disk I/O never holds up the UI.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::store::{Props, Store};

/// Buffered lines reach the file at least this often.
const FLUSH_EVERY: Duration = Duration::from_secs(1);

/// Whether `topic` matches the MQTT filter `filter` (`+` one level, `#` the
/// rest, including the level above it). As in MQTT, a filter starting with a
/// wildcard doesn't match `$` topics such as `$SYS/…`.
pub fn matches(filter: &str, topic: &str) -> bool {
    if topic.starts_with('$') && filter.starts_with(['+', '#']) {
        return false;
    }
    let (mut f, mut t) = (filter.split('/'), topic.split('/'));
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// Whether every topic in the subtree `sub` (a filter ending in `#`, such as
/// `site/oslo/#`) also matches `rule`.
pub fn covers(rule: &str, sub: &str) -> bool {
    let levels: Vec<&str> = sub.split('/').collect();
    let lits = &levels[..levels.len() - 1];
    if lits.first().is_some_and(|l| l.starts_with('$')) && rule.starts_with(['+', '#']) {
        return false;
    }
    let mut r = rule.split('/');
    for l in lits {
        match r.next() {
            Some("#") => return true,
            Some("+") => {}
            Some(x) if x == *l => {}
            _ => return false,
        }
    }
    r.next() == Some("#")
}

/// A well-formed MQTT topic filter: `#` only as the whole last level, `+`
/// only as a whole level.
pub fn valid_filter(f: &str) -> bool {
    let levels: Vec<&str> = f.split('/').collect();
    !f.is_empty()
        && !f.contains('\0')
        && levels
            .iter()
            .enumerate()
            .all(|(i, l)| (!l.contains('#') || (*l == "#" && i == levels.len() - 1)) && (!l.contains('+') || *l == "+"))
}

/// The rule that captures a topic node's subtree: `path/#`, or `#` for a
/// whole connection (`path` empty).
pub fn subtree(path: &str) -> String {
    if path.is_empty() {
        "#".into()
    } else {
        format!("{path}/#")
    }
}

pub struct Rule {
    pub filter: String,
    pub enabled: bool,
    pub msgs: u64, // captured by this rule in this run
}

/// One connection's capture rules and file.
#[derive(Default)]
pub struct Capture {
    pub rules: Vec<Rule>,
    /// The file, once the first message created it.
    pub file: Option<PathBuf>,
    /// Writing failed; nothing more is captured until a rule is toggled.
    pub err: Option<String>,
    sink: Option<Arc<Mutex<Sink>>>,
}

impl Capture {
    /// Takes the rules from the connections file (`capture`, and
    /// `capture_paused` for those turned off), keeping the counts of rules
    /// that stay.
    pub fn set_rules(&mut self, filters: &[String], paused: &[String]) {
        let mut old = std::mem::take(&mut self.rules);
        for f in filters {
            if self.rules.iter().any(|r| &r.filter == f) {
                continue;
            }
            let msgs = old
                .iter_mut()
                .find(|r| &r.filter == f)
                .map_or(0, |r| std::mem::take(&mut r.msgs));
            self.rules.push(Rule {
                filter: f.clone(),
                enabled: !paused.contains(f),
                msgs,
            });
        }
    }

    /// Whether anything is being captured.
    pub fn active(&self) -> bool {
        self.err.is_none() && self.rules.iter().any(|r| r.enabled)
    }

    /// Whether messages on `topic` are being captured.
    pub fn captures(&self, topic: &str) -> bool {
        self.err.is_none() && self.rules.iter().any(|r| r.enabled && matches(&r.filter, topic))
    }

    /// An enabled rule that already captures all of `sub` (see [`covers`]).
    pub fn covering(&self, sub: &str) -> Option<&str> {
        self.rules
            .iter()
            .find(|r| r.enabled && covers(&r.filter, sub))
            .map(|r| r.filter.as_str())
    }

    /// Where a message on `topic` goes, if anywhere; counts it per rule.
    /// The sink is made on first use (the file only when written to).
    pub fn target(&mut self, dir: &Path, conn: &str, topic: &str) -> Option<Arc<Mutex<Sink>>> {
        if self.err.is_some() {
            return None;
        }
        let mut hit = false;
        for r in self.rules.iter_mut().filter(|r| r.enabled && matches(&r.filter, topic)) {
            r.msgs += 1;
            hit = true;
        }
        if !hit {
            return None;
        }
        let sink = self
            .sink
            .get_or_insert_with(|| Arc::new(Mutex::new(Sink::new(dir.to_path_buf(), conn))));
        Some(sink.clone())
    }

    /// The sink, to flush it without holding the store lock.
    pub fn sink(&self) -> Option<Arc<Mutex<Sink>>> {
        self.sink.clone()
    }

    /// After a failure: forget it and start over with a new file.
    pub fn retry(&mut self) {
        if self.err.take().is_some() {
            self.sink = None;
            self.file = None;
        }
    }
}

/// Appends lines to a connection's capture file, created on the first write.
pub struct Sink {
    dir: PathBuf,
    conn: String,
    out: Option<BufWriter<File>>,
    path: Option<PathBuf>,
    dirty: bool,
    last_flush: Instant,
}

impl Sink {
    pub fn new(dir: PathBuf, conn: &str) -> Sink {
        Sink {
            dir,
            conn: conn.to_string(),
            out: None,
            path: None,
            dirty: false,
            last_flush: Instant::now(),
        }
    }

    /// Appends `line`; returns the file's path when this write created it.
    pub fn write(&mut self, line: &str) -> Result<Option<PathBuf>, String> {
        let mut created = None;
        if self.out.is_none() {
            let (file, path) = create(&self.dir, &self.conn, SystemTime::now())?;
            self.out = Some(BufWriter::with_capacity(64 * 1024, file));
            self.path = Some(path.clone());
            self.last_flush = Instant::now();
            created = Some(path);
        }
        let res = self.out.as_mut().map_or(Ok(()), |o| o.write_all(line.as_bytes()));
        if let Err(e) = res {
            return Err(self.failed(e));
        }
        self.dirty = true;
        self.flush_if_due()?;
        Ok(created)
    }

    pub fn flush(&mut self) -> Result<(), String> {
        if self.dirty {
            let res = self.out.as_mut().map_or(Ok(()), |o| o.flush());
            if let Err(e) = res {
                return Err(self.failed(e));
            }
        }
        self.dirty = false;
        self.last_flush = Instant::now();
        Ok(())
    }

    /// Flushes if lines have waited [`FLUSH_EVERY`] or longer.
    pub fn flush_if_due(&mut self) -> Result<(), String> {
        if self.dirty && self.last_flush.elapsed() >= FLUSH_EVERY {
            self.flush()
        } else {
            Ok(())
        }
    }

    /// Gives up on the file after an I/O error; returns the message.
    fn failed(&mut self, e: std::io::Error) -> String {
        self.out = None;
        self.dirty = false;
        match &self.path {
            Some(p) => format!("{}: {e}", p.display()),
            None => e.to_string(),
        }
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// Creates `fss-mqtt-<connection>-<local time>.jsonl` in `dir` (and `dir`),
/// numbering it if that name is taken. Readable by you only on Unix:
/// payloads can be sensitive.
fn create(dir: &Path, conn: &str, t: SystemTime) -> Result<(File, PathBuf), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let base = format!("fss-mqtt-{}-{}", safe_name(conn), local_stamp(t));
    let mut n = 1;
    loop {
        let path = match n {
            1 => dir.join(format!("{base}.jsonl")),
            _ => dir.join(format!("{base}-{n}.jsonl")),
        };
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        match opts.open(&path) {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 100 => n += 1,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
}

/// The connection name with anything but letters, digits, `.`, `_` and `-`
/// replaced, for a file name.
fn safe_name(conn: &str) -> String {
    let s: String = conn
        .chars()
        .take(64)
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => c,
            _ => '_',
        })
        .collect();
    if s.trim_matches(['.', '_']).is_empty() {
        "connection".into()
    } else {
        s
    }
}

/// `YYYYMMDD-HHMMSS` in local time.
fn local_stamp(t: SystemTime) -> String {
    let digits: String = crate::fmt::clock(t, true)
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    format!("{}-{}", &digits[..8], &digits[8..14])
}

/// RFC 3339 in UTC with milliseconds: `2026-10-15T08:20:30.123Z`.
fn rfc3339(t: SystemTime) -> String {
    let ms = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
    let (days, ms) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let s = ms / 1000;
    format!(
        "{}T{:02}:{:02}:{:02}.{:03}Z",
        crate::values::date(days),
        s / 3600,
        s / 60 % 60,
        s % 60,
        ms % 1000
    )
}

fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// One message as a line of JSON (with the newline): its time, topic, QoS,
/// retain flag, the properties it has, its size and the whole payload, as
/// text when it is UTF-8 and as base64 otherwise.
pub fn encode(t: SystemTime, topic: &str, payload: &[u8], qos: u8, retain: bool, p: &Props) -> String {
    let mut s = String::with_capacity(payload.len() * 4 / 3 + topic.len() + 128);
    s.push_str("{\"time\":\"");
    s.push_str(&rfc3339(t));
    s.push_str("\",\"topic\":");
    json_str(&mut s, topic);
    s.push_str(&format!(",\"qos\":{qos},\"retain\":{retain}"));
    if let Some(v) = &p.content_type {
        s.push_str(",\"content_type\":");
        json_str(&mut s, v);
    }
    if let Some(v) = &p.response_topic {
        s.push_str(",\"response_topic\":");
        json_str(&mut s, v);
    }
    if let Some(v) = &p.correlation_data {
        s.push_str(&format!(",\"correlation_data\":\"{}\"", crate::base64::encode(v)));
    }
    if let Some(v) = p.message_expiry {
        s.push_str(&format!(",\"message_expiry\":{v}"));
    }
    if let Some(v) = p.payload_format {
        s.push_str(&format!(",\"payload_format\":{v}"));
    }
    if !p.user.is_empty() {
        s.push_str(",\"user\":[");
        for (i, (k, v)) in p.user.iter().enumerate() {
            s.push_str(if i == 0 { "[" } else { ",[" });
            json_str(&mut s, k);
            s.push(',');
            json_str(&mut s, v);
            s.push(']');
        }
        s.push(']');
    }
    s.push_str(&format!(",\"size\":{}", payload.len()));
    match std::str::from_utf8(payload) {
        Ok(text) => {
            s.push_str(",\"payload\":");
            json_str(&mut s, text);
        }
        Err(_) => s.push_str(&format!(",\"payload_base64\":\"{}\"", crate::base64::encode(payload))),
    }
    s.push_str("}\n");
    s
}

/// Writes a captured message to `sink` (without the store lock). A new file
/// is announced on the status line; a failure stops capture for the
/// connection and is shown too.
#[allow(clippy::too_many_arguments)]
pub fn record(
    store: &Mutex<Store>,
    conn: usize,
    sink: &Mutex<Sink>,
    topic: &str,
    payload: &[u8],
    qos: u8,
    retain: bool,
    props: &Props,
) {
    let line = encode(SystemTime::now(), topic, payload, qos, retain, props);
    let res = sink.lock().unwrap().write(&line);
    match res {
        Ok(None) => {}
        Ok(Some(path)) => {
            let mut st = store.lock().unwrap();
            let name = st.conns[conn].name.clone();
            st.conns[conn].capture.file = Some(path.clone());
            st.notify(format!("capturing {name} to {}", path.display()), false);
        }
        Err(e) => fail(store, conn, e),
    }
}

/// Stops capture for connection `conn` after a write failed, and says so.
pub fn fail(store: &Mutex<Store>, conn: usize, err: String) {
    let mut st = store.lock().unwrap();
    let c = &mut st.conns[conn];
    if c.capture.err.is_none() {
        let name = c.name.clone();
        c.capture.err = Some(err.clone());
        st.notify(format!("capture stopped for {name}: {err}"), true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_match_like_mqtt() {
        for (f, t, want) in [
            ("a/b", "a/b", true),
            ("a/b", "a/c", false),
            ("a/+", "a/b", true),
            ("a/+", "a", false),
            ("a/+", "a/b/c", false),
            ("a/#", "a", true),
            ("a/#", "a/b/c", true),
            ("a/#", "ab", false),
            ("+/+", "a/b", true),
            ("+/b/#", "x/b", true),
            ("#", "a/b", true),
            ("#", "$SYS/load", false),
            ("+/load", "$SYS/load", false),
            ("$SYS/#", "$SYS/load", true),
            ("a/+/c", "a//c", true),
        ] {
            assert_eq!(matches(f, t), want, "{f} vs {t}");
        }
    }

    #[test]
    fn coverage_and_validity() {
        assert!(covers("site/#", "site/oslo/#"));
        assert!(covers("site/oslo/#", "site/oslo/#"));
        assert!(covers("+/oslo/#", "site/oslo/#"));
        assert!(covers("#", "site/oslo/#"));
        assert!(!covers("site/oslo/#", "site/#"), "narrower");
        assert!(!covers("site/+/temp", "site/oslo/#"), "only some topics below");
        assert!(!covers("site/oslo/+/#", "site/oslo/#"), "not site/oslo itself");
        assert!(!covers("#", "$SYS/#"));
        assert!(covers("#", "#"));
        assert!(!covers("a/#", "#"));
        for f in ["#", "a/#", "+/b", "a/+/c/#", "$SYS/#", "a b/c"] {
            assert!(valid_filter(f), "{f}");
        }
        for f in ["", "a/#/b", "a#", "a/b+", "+a/b", "a/\0"] {
            assert!(!valid_filter(f), "{f:?}");
        }
        assert_eq!(subtree(""), "#");
        assert_eq!(subtree("v1/plant"), "v1/plant/#");
    }

    #[test]
    fn rules_count_and_keep_counts() {
        let mut c = Capture::default();
        let dir = std::env::temp_dir();
        c.set_rules(&["a/#".into(), "b/#".into(), "a/#".into()], &["b/#".into()]);
        assert_eq!(c.rules.len(), 2, "duplicates are listed once");
        assert!(c.target(&dir, "x", "a/1").is_some());
        assert!(c.target(&dir, "x", "b/1").is_none(), "paused");
        assert!(c.target(&dir, "x", "c").is_none());
        assert!(c.captures("a/2") && !c.captures("b/2") && c.active());
        assert_eq!(c.covering("a/z/#"), Some("a/#"));
        assert_eq!(c.covering("b/#"), None, "a paused rule doesn't cover");
        c.set_rules(&["a/#".into(), "c/#".into()], &[]);
        assert_eq!(c.rules[0].msgs, 1, "count kept");
        assert_eq!(c.rules[1].msgs, 0);
        c.err = Some("disk full".into());
        assert!(c.target(&dir, "x", "a/1").is_none() && !c.active() && !c.captures("a/1"));
        c.retry();
        assert!(c.err.is_none() && c.active());
    }

    #[test]
    fn lines_are_json_with_the_whole_payload() {
        let t = UNIX_EPOCH + Duration::from_millis(1_791_454_800_123);
        let props = Props {
            content_type: Some("text/plain".into()),
            response_topic: Some("reply/x".into()),
            correlation_data: Some(vec![1, 2, 3]),
            message_expiry: Some(60),
            payload_format: Some(1),
            user: vec![("k\"1".into(), "v\\1".into()), ("é".into(), "🌬".into())],
            subscription_id: Some(7),
        };
        let line = encode(t, "a/\"b\"", "x\"y\\z\n\t\r\u{1}é🌬".as_bytes(), 1, true, &props);
        assert_eq!(
            line,
            "{\"time\":\"2026-10-08T10:20:00.123Z\",\"topic\":\"a/\\\"b\\\"\",\"qos\":1,\"retain\":true,\
             \"content_type\":\"text/plain\",\"response_topic\":\"reply/x\",\"correlation_data\":\"AQID\",\
             \"message_expiry\":60,\"payload_format\":1,\"user\":[[\"k\\\"1\",\"v\\\\1\"],[\"é\",\"🌬\"]],\
             \"size\":15,\"payload\":\"x\\\"y\\\\z\\n\\t\\r\\u0001é🌬\"}\n"
        );
        // Binary goes as base64; no properties, no keys for them.
        let line = encode(t, "bin", &[0, 0xff, 0xfe], 0, false, &Props::default());
        assert_eq!(
            line,
            "{\"time\":\"2026-10-08T10:20:00.123Z\",\"topic\":\"bin\",\"qos\":0,\"retain\":false,\
             \"size\":3,\"payload_base64\":\"AP/+\"}\n"
        );
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("fss-mqtt-capture-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn file_is_made_on_the_first_write() {
        let dir = tmp("lazy");
        let mut s = Sink::new(dir.join("sub"), "backend@10.0.0.5:8883");
        assert!(!dir.exists(), "nothing on disk before a message");
        let path = s.write("{\"a\":1}\n").unwrap().expect("created");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with("fss-mqtt-backend_10.0.0.5_8883-") && name.ends_with(".jsonl"),
            "{name}"
        );
        assert_eq!(s.write("{\"a\":2}\n").unwrap(), None);
        s.flush().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}\n{\"a\":2}\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // The same second again: numbered rather than appended to.
        let t = SystemTime::now();
        let (_, a) = create(&dir, "x", t).unwrap();
        let (_, b) = create(&dir, "x", t).unwrap();
        assert!(b.to_string_lossy().ends_with("-2.jsonl") && a != b, "{b:?}");
        assert_eq!(safe_name("../.."), "connection");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_writes_and_announces_the_file() {
        let dir = tmp("record");
        let mut st = Store::new(10, 4);
        st.add_conn("lab", "mqtt://x:1883", None, vec![]);
        st.capture_dir = dir.clone();
        st.conns[0].capture.set_rules(&["a/#".into()], &[]);
        let store = Mutex::new(st);
        assert!(store.lock().unwrap().capture_target(0, "b").is_none());
        let sink = store.lock().unwrap().capture_target(0, "a/x").unwrap();
        record(&store, 0, &sink, "a/x", b"0123456789", 1, false, &Props::default());
        record(&store, 0, &sink, "a/y", b"z", 0, false, &Props::default());
        sink.lock().unwrap().flush().unwrap();
        let mut st = store.lock().unwrap();
        let path = st.conns[0].capture.file.clone().expect("file noted");
        let notices = st.take_notices();
        assert_eq!(
            notices,
            [(format!("capturing lab to {}", path.display()), false)],
            "once"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].contains("\"topic\":\"a/x\"") && lines[0].ends_with("\"size\":10,\"payload\":\"0123456789\"}")
        );
        assert_eq!(st.conns[0].capture.rules[0].msgs, 1, "counted by capture_target");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failures_stop_capture_and_are_reported() {
        let dir = tmp("fail");
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();

        let mut st = Store::new(10, 1024);
        st.add_conn("c", "mqtt://x:1883", None, vec![]);
        st.capture_dir = blocker.join("captures");
        st.conns[0].capture.set_rules(&["#".into()], &[]);
        let store = Mutex::new(st);
        let sink = store.lock().unwrap().capture_target(0, "a/b").expect("captured");
        record(&store, 0, &sink, "a/b", b"1", 0, false, &Props::default());
        let mut st = store.lock().unwrap();
        let err = st.conns[0].capture.err.clone().expect("failure kept");
        assert!(err.contains("not-a-dir"), "{err}");
        let notices = st.take_notices();
        assert!(
            notices
                .iter()
                .any(|(t, e)| *e && t.starts_with("capture stopped for c")),
            "{notices:?}"
        );
        assert!(st.capture_target(0, "a/b").is_none(), "stopped");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
