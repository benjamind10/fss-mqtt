//! Opt-in capture to disk. Each connection has capture rules (MQTT topic
//! filters); messages matching an enabled rule are appended, whole, to one
//! JSON Lines file per connection per run. Nothing is written until a rule
//! matches, and the file is created then.
//!
//! The connection thread checks the rules under the store lock (cheap),
//! encodes the message after releasing it and queues the line for the
//! connection's [`Writer`], a thread that does all the disk I/O. A slow or
//! stuck disk never holds up the MQTT connection or the UI: when more than
//! [`MAX_PENDING`] bytes are waiting to be written, capture stops for that
//! connection and says why.
//!
//! Locks: a writer's report may be locked while holding the store lock, never
//! the other way round, and nothing waits for a writer while holding the
//! store lock.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::store::{Props, Store};

/// Buffered lines reach the file at least this often.
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// Bytes a connection may have waiting to be written. More means the disk
/// can't keep up, and capture stops rather than use ever more memory.
pub const MAX_PENDING: usize = 64 << 20;
/// How long quitting waits for captured messages to reach the disk.
pub const QUIT_WAIT: Duration = Duration::from_secs(5);

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
    /// Messages this rule had written to the file in this run, counted by the
    /// writer once they're written.
    pub count: Arc<AtomicU64>,
}

impl Rule {
    pub fn written(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

/// One connection's capture rules, and its writer once a message was captured.
#[derive(Default)]
pub struct Capture {
    pub rules: Vec<Rule>,
    writer: Option<Writer>,
}

impl Capture {
    /// Takes the rules from the connections file (`capture`, and
    /// `capture_paused` for those turned off), keeping the counts of rules
    /// that stay.
    pub fn set_rules(&mut self, filters: &[String], paused: &[String]) {
        let old = std::mem::take(&mut self.rules);
        for f in filters {
            if self.rules.iter().any(|r| &r.filter == f) {
                continue;
            }
            let count = old
                .iter()
                .find(|r| &r.filter == f)
                .map(|r| r.count.clone())
                .unwrap_or_default();
            self.rules.push(Rule {
                filter: f.clone(),
                enabled: !paused.contains(f),
                count,
            });
        }
    }

    /// The file being written, once the first message created it.
    pub fn file(&self) -> Option<PathBuf> {
        self.writer.as_ref().and_then(|w| w.report().file.clone())
    }

    /// Why capture stopped, if it did; nothing more is captured until a rule
    /// is toggled.
    pub fn err(&self) -> Option<String> {
        self.writer.as_ref().and_then(|w| w.report().err.clone())
    }

    fn stopped(&self) -> bool {
        self.writer.as_ref().is_some_and(|w| w.report().err.is_some())
    }

    /// Whether anything is being captured.
    pub fn active(&self) -> bool {
        !self.stopped() && self.rules.iter().any(|r| r.enabled)
    }

    /// Whether messages on `topic` are being captured.
    pub fn captures(&self, topic: &str) -> bool {
        !self.stopped() && self.rules.iter().any(|r| r.enabled && matches(&r.filter, topic))
    }

    /// An enabled rule that already captures all of `sub` (see [`covers`]).
    pub fn covering(&self, sub: &str) -> Option<&str> {
        self.rules
            .iter()
            .find(|r| r.enabled && covers(&r.filter, sub))
            .map(|r| r.filter.as_str())
    }

    /// Where a message on `topic` goes, if it is captured. The writer starts
    /// with the first captured message (and makes the file when it writes).
    pub fn target(&mut self, dir: &Path, conn: &str, topic: &str) -> Option<Target> {
        if self.stopped() {
            return None;
        }
        let counts: Vec<_> = self
            .rules
            .iter()
            .filter(|r| r.enabled && matches(&r.filter, topic))
            .map(|r| r.count.clone())
            .collect();
        if counts.is_empty() {
            return None;
        }
        let writer = self
            .writer
            .get_or_insert_with(|| Writer::spawn(dir.to_path_buf(), conn, MAX_PENDING))
            .clone();
        Some(Target { writer, counts })
    }

    /// The writer, to flush it (without holding the store lock).
    pub fn writer(&self) -> Option<Writer> {
        self.writer.clone()
    }

    /// Captures again after it stopped: into the same file when the disk was
    /// only too slow, into a new one after the file failed.
    pub fn retry(&mut self) {
        let dead = self.writer.as_ref().is_some_and(|w| {
            let mut r = w.report();
            r.err.take().is_some() && r.dead
        });
        if dead {
            self.writer = None; // its thread ends once it has let go of the queue
        }
    }

    /// The writer's messages for the status line.
    pub fn take_notices(&self) -> Vec<(String, bool)> {
        self.writer
            .as_ref()
            .map(|w| std::mem::take(&mut w.report().notices))
            .unwrap_or_default()
    }
}

/// A connection's capture file, written by a thread of its own. Handles are
/// cheap to clone; the thread ends once they're all gone and it has written
/// what was queued.
#[derive(Clone)]
pub struct Writer {
    tx: Sender<Job>,
    shared: Arc<Shared>,
}

struct Shared {
    conn: String,
    /// Bytes queued and not yet written.
    pending: AtomicUsize,
    max_pending: usize,
    report: Mutex<Report>,
}

/// What the writer has to tell the UI.
#[derive(Default)]
struct Report {
    file: Option<PathBuf>,
    /// Why capture stopped.
    err: Option<String>,
    /// The file failed: the thread writes nothing more.
    dead: bool,
    notices: Vec<(String, bool)>,
}

enum Job {
    Line(String, Vec<Arc<AtomicU64>>),
    /// Write out what's buffered, then reply, if asked to: everything queued
    /// before this is on disk by then (or has failed).
    Flush(Option<Sender<()>>),
    /// Tests: a disk that hangs until told to go on.
    #[cfg(test)]
    Stall(Receiver<()>),
}

impl Writer {
    pub fn spawn(dir: PathBuf, conn: &str, max_pending: usize) -> Writer {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            conn: conn.to_string(),
            pending: AtomicUsize::new(0),
            max_pending,
            report: Mutex::default(),
        });
        let s = shared.clone();
        std::thread::spawn(move || write_loop(&rx, &s, dir));
        Writer { tx, shared }
    }

    fn report(&self) -> MutexGuard<'_, Report> {
        self.shared.report.lock().unwrap()
    }

    /// Bytes waiting to be written.
    pub fn pending(&self) -> usize {
        self.shared.pending.load(Ordering::SeqCst)
    }

    /// Queues `line` without waiting. When more than the limit would be
    /// waiting, the disk isn't keeping up and capture stops instead; a
    /// single larger line still goes when nothing else is waiting.
    fn send_line(&self, line: String, counts: Vec<Arc<AtomicU64>>) {
        let n = line.len();
        let before = self.shared.pending.fetch_add(n, Ordering::SeqCst);
        if before > 0 && before + n > self.shared.max_pending {
            self.shared.pending.fetch_sub(n, Ordering::SeqCst);
            let waiting = crate::fmt::human_bytes(before);
            self.shared.stop(format!("disk too slow, {waiting} waiting"), false);
            return;
        }
        if self.tx.send(Job::Line(line, counts)).is_err() {
            self.shared.pending.fetch_sub(n, Ordering::SeqCst); // the thread is gone
        }
    }

    /// Asks for what's buffered to be written out, without waiting.
    pub fn flush(&self) {
        let _ = self.tx.send(Job::Flush(None));
    }

    /// Asks for everything queued so far to be written out; the receiver
    /// hears when it has been (or has failed).
    pub fn flush_ack(&self) -> Receiver<()> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(Job::Flush(Some(tx)));
        rx
    }

    /// [`Writer::flush_ack`], waiting up to `wait`; whether it was done in time.
    #[cfg(test)]
    pub fn flush_wait(&self, wait: Duration) -> bool {
        self.flush_ack().recv_timeout(wait).is_ok()
    }

    /// Tests: the "disk" hangs until the returned sender is dropped.
    #[cfg(test)]
    pub fn stall(&self) -> Sender<()> {
        let (go_on, hang) = mpsc::channel();
        self.tx.send(Job::Stall(hang)).unwrap();
        go_on
    }
}

impl Shared {
    /// Stops capture, keeping the first reason, and says so. `dead`: the file
    /// failed, so capturing again needs a new one.
    fn stop(&self, err: String, dead: bool) {
        let mut r = self.report.lock().unwrap();
        r.dead |= dead;
        if r.err.is_none() {
            r.notices
                .push((format!("capture stopped for {}: {err}", self.conn), true));
            r.err = Some(err);
        }
    }
}

/// The writer thread: writes queued lines, counting each for the rules that
/// matched it once it's written, and flushes at least once a second.
fn write_loop(rx: &Receiver<Job>, shared: &Shared, dir: PathBuf) {
    let mut sink = Sink::new(dir, &shared.conn);
    let mut dead = false;
    let flush = |sink: &mut Sink, dead: &mut bool| {
        if !*dead && let Err(e) = sink.flush() {
            shared.stop(e, true);
            *dead = true;
        }
    };
    loop {
        let job = if sink.dirty {
            match rx.recv_timeout(sink.due_in()) {
                Ok(job) => Some(job),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(job) => Some(job),
                Err(_) => break,
            }
        };
        match job {
            Some(Job::Line(line, counts)) => {
                if !dead {
                    match sink.write(&line) {
                        Ok(created) => {
                            for c in &counts {
                                c.fetch_add(1, Ordering::Relaxed);
                            }
                            if let Some(path) = created {
                                let mut r = shared.report.lock().unwrap();
                                let text = format!("capturing {} to {}", shared.conn, path.display());
                                r.notices.push((text, false));
                                r.file = Some(path);
                            }
                        }
                        Err(e) => {
                            shared.stop(e, true);
                            dead = true;
                        }
                    }
                }
                shared.pending.fetch_sub(line.len(), Ordering::SeqCst);
            }
            Some(Job::Flush(ack)) => {
                flush(&mut sink, &mut dead);
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
            None => flush(&mut sink, &mut dead), // a second since the first unwritten line
            #[cfg(test)]
            Some(Job::Stall(hang)) => {
                let _ = hang.recv();
            }
        }
    }
    flush(&mut sink, &mut dead);
}

/// Where a captured message goes: the connection's writer, and the counts of
/// the rules that matched it.
pub struct Target {
    writer: Writer,
    counts: Vec<Arc<AtomicU64>>,
}

impl Target {
    /// Encodes the message and queues it for the writer; never waits for the
    /// disk. Call it without holding the store lock.
    pub fn record(self, topic: &str, payload: &[u8], qos: u8, retain: bool, props: &Props) {
        let line = encode(SystemTime::now(), topic, payload, qos, retain, props);
        self.writer.send_line(line, self.counts);
    }
}

/// Asks connection `conn`'s writer, if it has one, to write out what it has
/// buffered, without waiting for it (or holding the store lock meanwhile).
pub fn flush(store: &Mutex<Store>, conn: usize) {
    let writer = store.lock().unwrap().conns[conn].capture.writer();
    if let Some(w) = writer {
        w.flush();
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

    /// How long until buffered lines are due to be written out.
    fn due_in(&self) -> Duration {
        FLUSH_EVERY.saturating_sub(self.last_flush.elapsed())
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

    /// Long enough for a writer on a working disk.
    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn rules_match_and_keep_their_counts() {
        let dir = tmp("rules");
        let mut c = Capture::default();
        c.set_rules(&["a/#".into(), "b/#".into(), "a/#".into()], &["b/#".into()]);
        assert_eq!(c.rules.len(), 2, "duplicates are listed once");
        let t = c.target(&dir, "x", "a/1").expect("captured");
        assert!(t.counts.len() == 1 && Arc::ptr_eq(&t.counts[0], &c.rules[0].count));
        assert!(c.target(&dir, "x", "b/1").is_none(), "paused");
        assert!(c.target(&dir, "x", "c").is_none());
        assert!(c.captures("a/2") && !c.captures("b/2") && c.active());
        assert_eq!(c.covering("a/z/#"), Some("a/#"));
        assert_eq!(c.covering("b/#"), None, "a paused rule doesn't cover");
        c.rules[0].count.store(5, Ordering::Relaxed);
        c.set_rules(&["a/#".into(), "c/#".into()], &[]);
        assert_eq!((c.rules[0].written(), c.rules[1].written()), (5, 0), "kept, and new");
        assert!(!dir.exists(), "nothing on disk without a message");
    }

    #[test]
    fn records_are_written_counted_and_announced() {
        let dir = tmp("record");
        let mut st = Store::new(10, 4);
        st.add_conn("lab", "mqtt://x:1883", None, vec![]);
        st.capture_dir = dir.clone();
        st.conns[0].capture.set_rules(&["a/#".into(), "+/x".into()], &[]);
        assert!(st.capture_target(0, "b").is_none());
        let props = Props::default();
        st.capture_target(0, "a/x")
            .unwrap()
            .record("a/x", b"0123456789", 1, false, &props);
        st.capture_target(0, "a/y")
            .unwrap()
            .record("a/y", b"z", 0, false, &props);
        let w = st.conns[0].capture.writer().unwrap();
        assert!(w.flush_wait(WAIT));
        let path = st.conns[0].capture.file().expect("file noted");
        let notices = st.take_notices();
        assert_eq!(
            notices,
            [(format!("capturing lab to {}", path.display()), false)],
            "once"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "whole, beyond the store's 4 bytes");
        assert!(
            lines[0].contains("\"topic\":\"a/x\"") && lines[0].ends_with("\"size\":10,\"payload\":\"0123456789\"}")
        );
        let counts: Vec<u64> = st.conns[0].capture.rules.iter().map(Rule::written).collect();
        assert_eq!(counts, [2, 1], "each rule counts what it had written");
        assert_eq!(w.pending(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stuck_disk_stops_capture_instead_of_holding_up_the_connection() {
        let dir = tmp("stuck");
        let mut c = Capture::default();
        c.set_rules(&["#".into()], &[]);
        // A writer allowed 1 KB of backlog, on a disk that hangs.
        c.writer = Some(Writer::spawn(dir.clone(), "slow", 1000));
        let go_on = c.writer().unwrap().stall();
        let start = Instant::now();
        let mut sent = 0;
        while let Some(t) = c.target(&dir, "slow", "a/b") {
            t.record("a/b", &[b'x'; 100], 0, false, &Props::default());
            sent += 1;
            assert!(sent < 100, "never stopped");
        }
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "queuing never waits for the disk"
        );
        let err = c.err().expect("stopped");
        assert!(err.starts_with("disk too slow, ") && err.ends_with(" waiting"), "{err}");
        assert_eq!(c.take_notices(), [(format!("capture stopped for slow: {err}"), true)]);
        assert!(!c.active() && !c.captures("a/b"));
        let w = c.writer().unwrap();
        assert!(
            !w.flush_wait(Duration::from_millis(50)) && w.pending() > 0,
            "still stuck"
        );

        // The disk comes back: what was queued gets written and counted, the
        // line that didn't fit doesn't.
        drop(go_on);
        assert!(w.flush_wait(WAIT));
        let queued = sent - 1;
        let file = c.file().expect("written");
        assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), queued);
        assert_eq!(c.rules[0].written(), queued as u64);
        assert_eq!(w.pending(), 0);

        // Turned on again, it goes on in the same file.
        c.retry();
        assert!(c.err().is_none() && c.active());
        c.target(&dir, "slow", "a/b")
            .unwrap()
            .record("a/b", b"again", 0, false, &Props::default());
        assert!(c.writer().unwrap().flush_wait(WAIT));
        assert_eq!(c.file(), Some(file.clone()));
        assert_eq!(c.rules[0].written(), queued as u64 + 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lines_wait_at_most_a_second_and_a_flush_waits_for_the_queue() {
        let dir = tmp("timer");
        let w = Writer::spawn(dir.clone(), "t", MAX_PENDING);
        let count = Arc::new(AtomicU64::new(0));
        let target = |w: &Writer| Target {
            writer: w.clone(),
            counts: vec![count.clone()],
        };
        target(&w).record("a", b"1", 0, false, &Props::default());
        // Nobody asks: the writer's own timer gets it to the file.
        let start = Instant::now();
        let file = loop {
            let file = w.report().file.clone();
            if let Some(f) = file.filter(|f| std::fs::read_to_string(f).is_ok_and(|t| t.lines().count() == 1)) {
                break f;
            }
            assert!(start.elapsed() < WAIT, "not written");
            std::thread::sleep(Duration::from_millis(20));
        };
        // About a second; the slack is for busy CI machines.
        assert!(start.elapsed() < Duration::from_millis(2500), "{:?}", start.elapsed());
        // A flush asked for (as on quit) covers everything queued before it,
        // and waiting for it is bounded.
        let go_on = w.stall();
        target(&w).record("a", b"2", 0, false, &Props::default());
        let ack = w.flush_ack();
        assert!(ack.recv_timeout(Duration::from_millis(50)).is_err(), "bounded");
        drop(go_on);
        assert!(ack.recv_timeout(WAIT).is_ok());
        assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 2);
        assert_eq!(count.load(Ordering::Relaxed), 2);
        let _ = std::fs::remove_dir_all(&dir);
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
    fn a_failed_write_counts_nothing_and_stops_capture() {
        let dir = tmp("fail");
        std::fs::create_dir_all(&dir).unwrap();
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();

        let mut st = Store::new(10, 1024);
        st.add_conn("c", "mqtt://x:1883", None, vec![]);
        st.capture_dir = blocker.join("captures");
        st.conns[0].capture.set_rules(&["#".into()], &[]);
        let props = Props::default();
        st.capture_target(0, "a/b")
            .expect("captured")
            .record("a/b", b"1", 0, false, &props);
        assert!(st.conns[0].capture.writer().unwrap().flush_wait(WAIT));
        let err = st.conns[0].capture.err().expect("failure kept");
        assert!(err.contains("not-a-dir"), "{err}");
        assert_eq!(st.conns[0].capture.rules[0].written(), 0, "it never reached the file");
        let notices = st.take_notices();
        assert!(
            notices
                .iter()
                .any(|(t, e)| *e && t.starts_with("capture stopped for c: ")),
            "{notices:?}"
        );
        assert!(st.capture_target(0, "a/b").is_none(), "stopped");
        assert!(!st.conns[0].capture.active());

        // Turned on again once the problem is gone: a new file, counted again.
        std::fs::remove_file(&blocker).unwrap();
        st.conns[0].capture.retry();
        assert!(st.conns[0].capture.err().is_none() && st.conns[0].capture.file().is_none());
        st.capture_target(0, "a/b")
            .expect("captured")
            .record("a/b", b"2", 0, false, &props);
        assert!(st.conns[0].capture.writer().unwrap().flush_wait(WAIT));
        assert_eq!(st.conns[0].capture.rules[0].written(), 1);
        let path = st.conns[0].capture.file().expect("a new file");
        assert!(std::fs::read_to_string(path).unwrap().ends_with("\"payload\":\"2\"}\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
