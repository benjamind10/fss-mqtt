//! Sessions and opened files.
//!
//! When fss-mqtt quits it saves what it holds (the messages in memory, the
//! connections that were open, the view) to one file, and `--resume` brings
//! it back. The file is gzip-compressed JSON Lines: a header line, then one
//! line per message in arrival order, in the capture format (see `record`).
//! Lines are read back through the same path a capture opened with `--open`
//! takes, so new decoders apply to old messages.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::json::{self, Json};
use crate::record::{self, Record};
use crate::store::Store;

/// The session format; a newer one is refused rather than misread.
pub const FORMAT: u64 = 1;
const MAX_HEADER_DEPTH: usize = 32;
/// Progress of an opened file is published this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// Where the session is kept: the platform's place for application state.
pub fn default_path() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let dir = if cfg!(windows) {
        env("LOCALAPPDATA")?
    } else if cfg!(target_os = "macos") {
        env("HOME")?.join("Library").join("Application Support")
    } else {
        env("XDG_STATE_HOME").or_else(|| env("HOME").map(|h| h.join(".local").join("state")))?
    };
    Some(dir.join("fss-mqtt").join("session.jsonl.gz"))
}

/// The view to come back to.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct View {
    pub filter: String,
    /// Expanded topics, as (connection, topic path); "" is the connection.
    pub expanded: Vec<(String, String)>,
    pub selected: Option<(String, String)>,
    /// "tree", "messages" or "connections".
    pub focus: String,
    /// An unsent publish document, and the payload it may not send
    /// unchanged (cut on arrival) with the reason.
    pub draft: Option<(String, Option<(String, String)>)>,
}

/// A connection in a session.
#[derive(Debug, Clone, PartialEq)]
pub struct Conn {
    pub name: String,
    /// As shown (no credentials).
    pub url: String,
    pub connected: bool,
    /// Its `[[connection]]` block, when it wasn't in the connections file
    /// (a `--no-save` broker); saved ones come from the file.
    pub config: Option<String>,
    /// A file opened with `--open`.
    pub file: Option<PathBuf>,
}

pub struct Header {
    /// The fss-mqtt version that wrote it.
    pub version: String,
    pub saved: SystemTime,
    pub view: View,
    pub conns: Vec<Conn>,
    /// Sparkplug B names learned per connection (`sparkplug::Aliases` JSON).
    pub sparkplug: Vec<(String, Json)>,
}

/// Writes the session: `store`'s messages, oldest first, under a header.
/// Written to a temporary file first, so a failure leaves the last one.
/// Returns how many messages were saved.
pub fn save(path: &Path, view: &View, conns: &[Conn], store: &Store) -> Result<usize, String> {
    let dir = path.parent().ok_or("no directory for the session")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    let file = private_file(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let res = write(file, view, conns, store);
    match res {
        Ok(n) => {
            std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(n)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(format!("{}: {e}", path.display()))
        }
    }
}

fn private_file(path: &Path) -> std::io::Result<File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

fn write(file: File, view: &View, conns: &[Conn], store: &Store) -> std::io::Result<usize> {
    let mut z = crate::gzip::Writer::new(BufWriter::new(file))?;
    z.write_all(header_line(view, conns, store).as_bytes())?;
    let all = store.all_messages();
    for &(conn, topic, m) in &all {
        // Keep the view decoded on arrival where it can't be made again:
        // from a payload that was cut, or with Sparkplug names from births.
        let keep_view = m.truncated() || crate::sparkplug::is_topic(topic);
        let decoded = m.decoded.as_deref().filter(|_| keep_view);
        let line = record::line(
            Some(&store.conns[conn].name),
            m.time,
            topic,
            &m.payload,
            m.size,
            m.qos,
            m.retain,
            &m.props,
            decoded,
        );
        z.write_all(line.as_bytes())?;
    }
    let mut out = z.finish()?;
    out.flush()?;
    out.get_ref().sync_all()?;
    Ok(all.len())
}

fn header_line(view: &View, conns: &[Conn], store: &Store) -> String {
    let w = json::write_str;
    let pair = |s: &mut String, (c, p): &(String, String)| {
        s.push('[');
        w(s, c);
        s.push(',');
        w(s, p);
        s.push(']');
    };
    let mut s = format!(
        "{{\"fss_mqtt_session\":{FORMAT},\"version\":\"{}\",\"saved\":\"{}\",\"view\":{{\"filter\":",
        env!("CARGO_PKG_VERSION"),
        record::rfc3339(SystemTime::now())
    );
    w(&mut s, &view.filter);
    s.push_str(",\"expanded\":[");
    for (i, e) in view.expanded.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        pair(&mut s, e);
    }
    s.push_str("],\"selected\":");
    match &view.selected {
        Some(sel) => pair(&mut s, sel),
        None => s.push_str("null"),
    }
    s.push_str(",\"focus\":");
    w(&mut s, &view.focus);
    if let Some((text, guard)) = &view.draft {
        s.push_str(",\"draft\":{\"text\":");
        w(&mut s, text);
        if let Some((payload, why)) = guard {
            s.push_str(",\"guard\":");
            pair(&mut s, &(payload.clone(), why.clone()));
        }
        s.push('}');
    }
    s.push_str("},\"connections\":[");
    for (i, c) in conns.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"name\":");
        w(&mut s, &c.name);
        s.push_str(",\"url\":");
        w(&mut s, &c.url);
        s.push_str(&format!(",\"connected\":{}", c.connected));
        if let Some(cfg) = &c.config {
            s.push_str(",\"config\":");
            w(&mut s, cfg);
        }
        if let Some(f) = &c.file {
            s.push_str(",\"file\":");
            w(&mut s, &f.to_string_lossy());
        }
        // The names its births gave Sparkplug aliases, when it has any.
        if let Some(i) = store.conns.iter().position(|sc| sc.name == c.name && !sc.removed) {
            let mut names = String::new();
            store.conns[i].sparkplug.lock().unwrap().write_json(&mut names);
            if names != "{}" {
                s.push_str(",\"sparkplug\":");
                s.push_str(&names);
            }
        }
        s.push('}');
    }
    s.push_str("]}\n");
    s
}

/// A session read back.
pub struct Loaded {
    pub header: Header,
    pub records: Vec<Record>,
    /// Lines that couldn't be read, and the first reason.
    pub skipped: usize,
    pub first_skip: Option<String>,
}

/// Reads a session saved by `save`.
pub fn load(path: &Path) -> Result<Loaded, String> {
    let (mut lines, _, _) = open_lines(path)?;
    let mut first = Vec::new();
    lines
        .read_until(b'\n', &mut first)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let header = parse_header(&first).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut loaded = Loaded {
        header,
        records: Vec::new(),
        skipped: 0,
        first_skip: None,
    };
    for_each_record(&mut lines, 1, |r| match r {
        Ok(r) => loaded.records.push(r),
        Err(e) => {
            loaded.skipped += 1;
            loaded.first_skip.get_or_insert(e);
        }
    })
    .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(loaded)
}

fn parse_header(line: &[u8]) -> Result<Header, String> {
    let j = Json::parse(line.trim_ascii_end(), MAX_HEADER_DEPTH).map_err(|_| "not a session file")?;
    let format = j
        .get("fss_mqtt_session")
        .and_then(Json::u64)
        .ok_or("not a session file")?;
    let version = j.get("version").and_then(Json::str).unwrap_or("?").to_string();
    if format > FORMAT {
        return Err(format!(
            "saved by fss-mqtt {version}, in a newer session format; upgrade fss-mqtt to continue it"
        ));
    }
    let pair = |p: &Json| match p.arr()? {
        [a, b] => Some((a.str()?.to_string(), b.str()?.to_string())),
        _ => None,
    };
    let v = j.get("view");
    let get = |k: &str| v.and_then(|v| v.get(k));
    let draft = get("draft").and_then(|d| {
        let text = d.get("text")?.str()?.to_string();
        Some((text, d.get("guard").and_then(pair)))
    });
    let view = View {
        filter: get("filter").and_then(Json::str).unwrap_or("").to_string(),
        expanded: get("expanded")
            .and_then(Json::arr)
            .unwrap_or_default()
            .iter()
            .filter_map(pair)
            .collect(),
        selected: get("selected").and_then(pair),
        focus: get("focus").and_then(Json::str).unwrap_or("tree").to_string(),
        draft,
    };
    let mut conns = Vec::new();
    let mut sparkplug = Vec::new();
    for c in j.get("connections").and_then(Json::arr).unwrap_or_default() {
        let Some(name) = c.get("name").and_then(Json::str) else {
            continue;
        };
        conns.push(Conn {
            name: name.to_string(),
            url: c.get("url").and_then(Json::str).unwrap_or("").to_string(),
            connected: c.get("connected").and_then(Json::bool).unwrap_or(false),
            config: c.get("config").and_then(Json::str).map(String::from),
            file: c.get("file").and_then(Json::str).map(PathBuf::from),
        });
        if let Some(names) = c.get("sparkplug") {
            sparkplug.push((name.to_string(), names.clone()));
        }
    }
    let saved = j
        .get("saved")
        .and_then(Json::str)
        .and_then(record::parse_rfc3339)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    Ok(Header {
        version,
        saved,
        view,
        conns,
        sparkplug,
    })
}

/// Counts the bytes read from a file, for progress.
struct Counting<R> {
    inner: R,
    read: Arc<AtomicU64>,
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(b)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

/// The lines of a JSON Lines file, gzip-compressed or not, with a counter
/// of the (compressed) bytes read so far and the file's size.
fn open_lines(path: &Path) -> Result<(Box<dyn BufRead + Send>, Arc<AtomicU64>, u64), String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let file = File::open(path).map_err(err)?;
    let total = file.metadata().map_err(err)?.len();
    let read = Arc::new(AtomicU64::new(0));
    let mut raw = BufReader::with_capacity(
        256 << 10,
        Counting {
            inner: file,
            read: read.clone(),
        },
    );
    let gz = raw.fill_buf().map_err(err)?.starts_with(crate::gzip::MAGIC);
    let lines: Box<dyn BufRead + Send> = if gz {
        Box::new(BufReader::with_capacity(256 << 10, crate::gzip::Reader::new(raw)))
    } else {
        Box::new(raw)
    };
    Ok((lines, read, total))
}

/// Calls `f` with each line read as a message; blank lines are skipped and
/// a session's header line is passed over. `n` lines were read before.
fn for_each_record(
    lines: &mut dyn BufRead,
    mut n: usize,
    mut f: impl FnMut(Result<Record, String>),
) -> Result<(), String> {
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if lines.read_until(b'\n', &mut buf).map_err(|e| e.to_string())? == 0 {
            return Ok(());
        }
        n += 1;
        let line = buf.trim_ascii();
        if line.is_empty() || (n == 1 && line.starts_with(b"{\"fss_mqtt_session\"")) {
            continue;
        }
        f(record::parse(line).map_err(|e| format!("line {n}: {e}")));
    }
}

/// Adds one message read from a file to connection `conn`, decoding it as
/// on arrival: a view kept with the line is used as it is; a whole payload
/// is decoded again (Sparkplug B with the names its births gave); a payload
/// that was cut gets no view.
pub fn restore(store: &Mutex<Store>, conn: usize, r: Record) {
    let decoded = match r.decoded {
        Some(v) => Some(v),
        None if r.payload.len() < r.size => None,
        None => {
            let names = store.lock().unwrap().conns[conn].sparkplug.clone();
            let sparkplug = names.lock().unwrap().decode(&r.topic, &r.payload);
            sparkplug.or_else(|| crate::pretty::decode_on_arrival(r.props.content_type.as_deref(), &r.payload))
        }
    };
    store.lock().unwrap().add_restored(
        conn, &r.topic, r.time, &r.payload, r.size, r.qos, r.retain, r.props, decoded,
    );
}

/// Reads a capture (or session) file into connection `conn` on a thread of
/// its own, as if its messages were arriving, publishing progress in the
/// connection's `FileSource`.
pub fn open(path: PathBuf, store: Arc<Mutex<Store>>, conn: usize) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let set = |f: &dyn Fn(&mut crate::store::FileSource)| {
            if let Some(src) = store.lock().unwrap().conns[conn].file.as_mut() {
                f(src);
            }
        };
        let (mut lines, read, total) = match open_lines(&path) {
            Ok(l) => l,
            Err(e) => {
                set(&|s| {
                    s.err = Some(e.clone());
                    s.done = true;
                });
                store.lock().unwrap().notify(format!("couldn't open {e}"), true);
                return;
            }
        };
        set(&|s| s.total = total);
        let mut last = Instant::now();
        let mut skipped = 0usize;
        let mut first_skip = None;
        let res = for_each_record(&mut lines, 0, |r| {
            match r {
                Ok(r) => restore(&store, conn, r),
                Err(e) => {
                    skipped += 1;
                    first_skip.get_or_insert(e);
                }
            }
            if last.elapsed() >= PROGRESS_EVERY {
                last = Instant::now();
                let n = read.load(Ordering::Relaxed);
                set(&|s| s.read = n);
            }
        });
        let mut st = store.lock().unwrap();
        let name = st.conns[conn].name.clone(); // as the panel shows it
        let msgs = st.nodes[st.conns[conn].node].sub_msgs;
        if let Some(src) = st.conns[conn].file.as_mut() {
            src.read = total;
            src.done = true;
            src.skipped = skipped;
            src.first_skip = first_skip.clone();
            src.err = res.as_ref().err().cloned();
        }
        match res {
            Ok(()) if skipped > 0 => st.notify(
                format!(
                    "opened {name}: {} messages; {skipped} lines skipped ({})",
                    crate::fmt::commas(msgs),
                    first_skip.unwrap_or_default()
                ),
                true,
            ),
            Ok(()) => st.notify(format!("opened {name}: {} messages", crate::fmt::commas(msgs)), false),
            Err(e) => st.notify(format!("{name}: stopped reading: {e}"), true),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Props, Sub};
    use std::time::UNIX_EPOCH;

    pub fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("fss-mqtt-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn store_with(conns: &[&str]) -> Store {
        let mut st = Store::new(10, 64);
        for c in conns {
            let sub = Sub {
                topic: "#".into(),
                enabled: true,
                err: None,
            };
            st.add_conn(c, &format!("mqtt://{c}:1883"), None, vec![sub]);
        }
        st
    }

    #[test]
    fn save_and_load() {
        let mut st = store_with(&["oslo", "bergen"]);
        let props = Props {
            content_type: Some("application/json".into()),
            user: vec![("site".into(), "oslo".into())],
            ..Props::default()
        };
        st.add_decoded(0, "site/a", br#"{"t":1}"#, 1, true, props.clone(), None);
        st.add_decoded(1, "site/b", &[0, 1, 2, 0xff], 0, false, Props::default(), None);
        // Cut on arrival (max 64 here), with a view decoded from the whole.
        let mut view = crate::view::DocView::new("Parquet");
        view.notes.push("decoded whole".into());
        st.add_decoded(0, "site/big", &[7u8; 100], 0, false, Props::default(), Some(view));
        let v = View {
            filter: "site".into(),
            expanded: vec![("oslo".into(), "".into()), ("oslo".into(), "site".into())],
            selected: Some(("oslo".into(), "site/a".into())),
            focus: "messages".into(),
            draft: Some(("topic: x\n\nhi".into(), Some(("hi".into(), "cut".into())))),
        };
        let conns = vec![
            Conn {
                name: "oslo".into(),
                url: "mqtt://oslo:1883".into(),
                connected: true,
                config: None,
                file: None,
            },
            Conn {
                name: "bergen".into(),
                url: "mqtt://bergen:1883".into(),
                connected: false,
                config: Some("[[connection]]\nname = \"bergen\"\nurl = \"mqtt://bergen:1883\"\n".into()),
                file: None,
            },
        ];
        let path = tmp("session.jsonl.gz");
        assert_eq!(save(&path, &v, &conns, &st), Ok(3));
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).unwrap().permissions()) & 0o777,
            0o600
        );
        let l = load(&path).unwrap();
        assert_eq!(l.header.view, v);
        assert_eq!(l.header.conns, conns);
        assert_eq!(l.header.version, env!("CARGO_PKG_VERSION"));
        assert!(l.header.saved.elapsed().unwrap() < Duration::from_secs(60));
        assert_eq!(l.skipped, 0);
        let r: Vec<_> = l
            .records
            .iter()
            .map(|r| (r.conn.as_deref().unwrap(), r.topic.as_str()))
            .collect();
        assert_eq!(r, [("oslo", "site/a"), ("bergen", "site/b"), ("oslo", "site/big")]);
        let a = &l.records[0];
        assert_eq!((a.qos, a.retain, a.payload.as_slice()), (1, true, &br#"{"t":1}"#[..]));
        assert_eq!(a.props, props);
        assert!(a.decoded.is_none(), "whole payloads are decoded again");
        let big = &l.records[2];
        assert_eq!((big.payload.len(), big.size), (64, 100));
        assert_eq!(big.decoded.as_ref().unwrap().notes, ["decoded whole"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn refuses_what_it_cannot_read() {
        let path = tmp("not-a-session.jsonl");
        std::fs::write(&path, "{\"topic\":\"a\"}\n").unwrap();
        assert!(load(&path).err().unwrap().contains("not a session file"));
        std::fs::write(&path, "{\"fss_mqtt_session\":99,\"version\":\"9.0.0\"}\n").unwrap();
        let e = load(&path).err().unwrap();
        assert!(e.contains("fss-mqtt 9.0.0") && e.contains("upgrade"), "{e}");
        // Damaged lines are skipped and counted; unknown fields are fine.
        std::fs::write(
            &path,
            "{\"fss_mqtt_session\":1,\"version\":\"0.7.0\",\"later\":{\"x\":1}}\n{\"conn\":\"a\",\"topic\":\"t\",\"payload\":\"x\",\"new_field\":1}\n{oops\n\n",
        )
        .unwrap();
        let l = load(&path).unwrap();
        assert_eq!((l.records.len(), l.skipped), (1, 1));
        assert!(l.first_skip.unwrap().starts_with("line 3:"));
        assert_eq!(
            l.header.view,
            View {
                focus: "tree".into(),
                ..View::default()
            }
        );
        assert!(load(&tmp("missing")).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn open_reads_a_capture_like_arriving_messages() {
        // A capture as the capture writer makes it, gzip-compressed by hand.
        let t0 = UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        let mut text = String::new();
        for i in 0..30u64 {
            let payload = format!("{{\"n\":{i}}}");
            text += &crate::capture::encode(
                t0 + Duration::from_secs(i),
                "a/b",
                payload.as_bytes(),
                0,
                false,
                &Props::default(),
            );
        }
        text += "not json\n";
        text += &crate::capture::encode(t0, "big", &[1u8; 500], 1, true, &Props::default());
        let mut z = crate::gzip::Writer::new(Vec::new()).unwrap();
        z.write_all(text.as_bytes()).unwrap();
        let path = tmp("capture.jsonl.gz");
        std::fs::write(&path, z.finish().unwrap()).unwrap();

        let store = Arc::new(Mutex::new(store_with(&["capture.jsonl.gz"])));
        store.lock().unwrap().conns[0].file = Some(crate::store::FileSource {
            path: path.clone(),
            ..Default::default()
        });
        open(path.clone(), store.clone(), 0).join().unwrap();
        let mut st = store.lock().unwrap();
        let src = st.conns[0].file.as_ref().unwrap();
        assert!(src.done && src.err.is_none() && src.read == src.total && src.total > 0);
        assert_eq!(src.skipped, 1);
        let n = st.find(0, "a/b").unwrap();
        let node = &st.nodes[n];
        assert_eq!((node.msg_count, node.messages().len()), (30, 10), "history applies");
        let latest = node.latest().unwrap();
        assert_eq!(latest.payload, br#"{"n":29}"#);
        assert_eq!(latest.time, t0 + Duration::from_secs(29), "arrival times kept");
        assert_eq!(node.last_seen, t0 + Duration::from_secs(29));
        let big = st.nodes[st.find(0, "big").unwrap()].latest().unwrap().clone();
        assert_eq!((big.payload.len(), big.size, big.qos, big.retain), (64, 500, 1, true));
        let notices = st.take_notices();
        assert!(
            notices
                .iter()
                .any(|(t, err)| *err && t.contains("31 messages") && t.contains("1 lines skipped")),
            "{notices:?}"
        );
        drop(st);
        let _ = std::fs::remove_file(&path);
    }
}
