//! The live topic tree and the last N messages per topic.
//!
//! Written by the MQTT threads, read by the UI, always behind one Mutex.
//! Nodes live in an arena and are never removed, so node ids stay valid.
//! Each connection owns one top-level node; topic paths below it don't
//! include the connection name.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Default, Clone, Debug, PartialEq)]
pub struct Props {
    pub content_type: Option<String>,
    pub response_topic: Option<String>,
    pub correlation_data: Option<Vec<u8>>,
    pub payload_format: Option<u8>,
    pub message_expiry: Option<u32>,
    pub subscription_id: Option<u32>,
    pub user: Vec<(String, String)>,
}

pub struct Message {
    pub seq: u64,
    pub time: SystemTime,
    pub payload: Vec<u8>, // at most Store::max_payload bytes
    pub size: usize,      // size on the wire
    pub qos: u8,
    pub retain: bool,
    pub props: Props,
    /// Binary formats decoded on arrival (Parquet, MessagePack, CBOR, Protobuf).
    pub decoded: Option<Box<crate::pretty::Decoded>>,
}

impl Message {
    pub fn truncated(&self) -> bool {
        self.payload.len() < self.size
    }
}

pub const ROOT: usize = 0;

pub struct Node {
    pub name: String,
    pub path: String,
    pub parent: Option<usize>,
    pub depth: usize, // root 0, connections 1, top-level topics 2
    pub conn: usize,  // index into Store::conns
    lower_name: String,
    lower_path: String,
    children: HashMap<String, usize>,
    sorted: Vec<usize>,
    sorted_dirty: bool,
    msgs: VecDeque<Arc<Message>>, // oldest first
    pub msg_count: u64,
    pub sub_msgs: u64,
    pub topics: u32,
    pub last_seen: SystemTime,
}

impl Node {
    fn new(name: &str, path: &str, parent: Option<usize>, depth: usize, conn: usize) -> Node {
        Node {
            name: name.to_string(),
            path: path.to_string(),
            parent,
            depth,
            conn,
            lower_name: name.to_lowercase(),
            lower_path: path.to_lowercase(),
            children: HashMap::new(),
            sorted: Vec::new(),
            sorted_dirty: false,
            msgs: VecDeque::new(),
            msg_count: 0,
            sub_msgs: 0,
            topics: 0,
            last_seen: SystemTime::UNIX_EPOCH,
        }
    }
    pub fn is_conn(&self) -> bool {
        self.depth == 1
    }
    pub fn has_children(&self) -> bool {
        !self.children.is_empty()
    }
    pub fn num_children(&self) -> usize {
        self.children.len()
    }
    pub fn has_messages(&self) -> bool {
        !self.msgs.is_empty()
    }
    pub fn children(&self) -> &[usize] {
        &self.sorted
    }
    /// Retained history, newest first.
    pub fn messages(&self) -> Vec<Arc<Message>> {
        self.msgs.iter().rev().cloned().collect()
    }
    pub fn latest(&self) -> Option<&Arc<Message>> {
        self.msgs.back()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnState {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Clone)]
pub struct Status {
    pub state: ConnState,
    pub err: Option<String>,
}

#[derive(Clone)]
pub struct Sub {
    pub topic: String,
    pub enabled: bool,
    pub err: Option<String>, // why the broker refused it
}

pub struct ConnInfo {
    pub name: String,
    pub url: String, // for display; no credentials
    pub username: Option<String>,
    pub node: usize,
    pub status: Status,
    pub subs: Vec<Sub>,
    /// Gone from the connections file. Kept so indices stay valid; its
    /// topics are detached from the tree and it takes no more messages.
    pub removed: bool,
}

pub struct Store {
    pub nodes: Vec<Node>,
    pub conns: Vec<ConnInfo>,
    history: usize,
    max_payload: usize,
    seq: u64,
    pub struct_ver: u64,
    /// Messages for the status line from the connection threads (a publish
    /// acknowledged, a capture file that failed), shown on the next tick.
    notices: Vec<(String, bool)>,
}

impl Store {
    pub fn new(history: usize, max_payload: usize) -> Store {
        Store {
            nodes: vec![Node::new("", "", None, 0, 0)],
            conns: Vec::new(),
            history: history.max(1),
            max_payload,
            seq: 0,
            struct_ver: 0,
            notices: Vec::new(),
        }
    }

    /// Queues a status-line message (`error` shows it in red).
    pub fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notices.push((text.into(), error));
    }

    pub fn take_notices(&mut self) -> Vec<(String, bool)> {
        std::mem::take(&mut self.notices)
    }

    /// Registers a connection and its top-level node; returns its index.
    pub fn add_conn(&mut self, name: &str, url: &str, username: Option<String>, subs: Vec<Sub>) -> usize {
        let idx = self.conns.len();
        let id = self.nodes.len();
        self.nodes.push(Node::new(name, "", Some(ROOT), 1, idx));
        let root = &mut self.nodes[ROOT];
        root.children.insert(name.to_string(), id);
        root.sorted_dirty = true;
        self.struct_ver += 1;
        self.conns.push(ConnInfo {
            name: name.to_string(),
            url: url.to_string(),
            username,
            node: id,
            status: Status {
                state: ConnState::Disconnected,
                err: None,
            },
            subs,
            removed: false,
        });
        idx
    }

    /// The connections still in use (not removed).
    pub fn live(&self) -> impl Iterator<Item = &ConnInfo> {
        self.conns.iter().filter(|c| !c.removed)
    }

    /// Retires connection `i`: its topics leave the tree (and the totals).
    /// Returns how many messages it had, by which `total()` went down.
    pub fn remove_conn(&mut self, i: usize) -> u64 {
        let c = &mut self.conns[i];
        if c.removed {
            return 0;
        }
        c.removed = true;
        c.subs.clear();
        c.status = Status {
            state: ConnState::Disconnected,
            err: None,
        };
        let (node, name) = (c.node, c.name.clone());
        let (msgs, topics) = (self.nodes[node].sub_msgs, self.nodes[node].topics);
        let root = &mut self.nodes[ROOT];
        if root.children.get(&name) == Some(&node) {
            root.children.remove(&name);
        }
        root.sorted_dirty = true;
        root.sub_msgs -= msgs;
        root.topics -= topics;
        self.struct_ver += 1;
        msgs
    }

    pub fn total(&self) -> u64 {
        self.nodes[ROOT].sub_msgs
    }
    pub fn topic_count(&self) -> u32 {
        self.nodes[ROOT].topics
    }

    #[cfg(test)]
    pub fn add(&mut self, conn: usize, topic: &str, payload: &[u8], qos: u8, retain: bool, props: Props) {
        self.add_decoded(conn, topic, payload, qos, retain, props, None);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_decoded(
        &mut self,
        conn: usize,
        topic: &str,
        payload: &[u8],
        qos: u8,
        retain: bool,
        props: Props,
        decoded: Option<crate::pretty::Decoded>,
    ) {
        if self.conns[conn].removed {
            return; // its thread is on its way out
        }
        let keep = payload.len().min(self.max_payload);
        self.seq += 1;
        let now = SystemTime::now();
        let msg = Arc::new(Message {
            seq: self.seq,
            time: now,
            payload: payload[..keep].to_vec(),
            size: payload.len(),
            qos,
            retain,
            props,
            decoded: decoded.map(Box::new),
        });

        let mut n = self.conns[conn].node;
        let mut start = 0;
        for seg in topic.split('/') {
            let end = start + seg.len();
            let next = match self.nodes[n].children.get(seg) {
                Some(&c) => c,
                None => {
                    let id = self.nodes.len();
                    let depth = self.nodes[n].depth + 1;
                    self.nodes.push(Node::new(seg, &topic[..end], Some(n), depth, conn));
                    let p = &mut self.nodes[n];
                    p.children.insert(seg.to_string(), id);
                    p.sorted_dirty = true;
                    self.struct_ver += 1;
                    id
                }
            };
            n = next;
            start = end + 1;
        }

        let new_topic = self.nodes[n].msg_count == 0;
        let history = self.history;
        let node = &mut self.nodes[n];
        node.msg_count += 1;
        if node.msgs.len() == history {
            node.msgs.pop_front();
        }
        node.msgs.push_back(msg);

        let mut a = Some(n);
        while let Some(i) = a {
            let node = &mut self.nodes[i];
            node.sub_msgs += 1;
            node.last_seen = now;
            if new_topic {
                node.topics += 1;
            }
            a = node.parent;
        }
    }

    /// Re-sorts children that changed since the last call. Call before reading
    /// children (the UI does this once per rebuild).
    pub fn sort_children(&mut self) {
        if !self.nodes.iter().any(|n| n.sorted_dirty) {
            return;
        }
        for i in 0..self.nodes.len() {
            if !self.nodes[i].sorted_dirty {
                continue;
            }
            let mut ids: Vec<usize> = self.nodes[i].children.values().copied().collect();
            ids.sort_by(|&a, &b| natural_cmp(&self.nodes[a].name, &self.nodes[b].name));
            let n = &mut self.nodes[i];
            n.sorted = ids;
            n.sorted_dirty = false;
        }
    }
}

/// Compares so that "sensor2" < "sensor10".
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let (si, sj) = (i, j);
            while i < a.len() && a[i].is_ascii_digit() {
                i += 1;
            }
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let trim = |s: &[u8]| -> usize { s.iter().take_while(|&&c| c == b'0').count() };
            let (na, nb) = (&a[si..i], &b[sj..j]);
            let (na, nb) = (&na[trim(na)..], &nb[trim(nb)..]);
            let o = na.len().cmp(&nb.len()).then(na.cmp(nb));
            if o != Equal {
                return o;
            }
            continue;
        }
        if a[i] != b[j] {
            return a[i].cmp(&b[j]);
        }
        i += 1;
        j += 1;
    }
    (a.len() - i).cmp(&(b.len() - j))
}

// ── Filtering ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FilterMode {
    None,
    /// Case-insensitive substring over topic paths.
    Search,
    /// MQTT-style (v1/+/temp, v1/#). The last literal level is a
    /// case-insensitive prefix and everything below a match is included.
    Pattern,
}

#[derive(Clone)]
pub struct Filter {
    pub mode: FilterMode,
    segs: Vec<String>,
    q: String,
}

pub const VISIBLE: u8 = 1;
pub const MATCHED: u8 = 2;
pub const INSIDE: u8 = 4;

impl Filter {
    pub fn compile(raw: &str) -> Filter {
        let mode = if raw.is_empty() {
            FilterMode::None
        } else if raw.contains(['/', '+', '#']) {
            FilterMode::Pattern
        } else {
            FilterMode::Search
        };
        Filter {
            mode,
            segs: if mode == FilterMode::Pattern {
                raw.split('/').map(String::from).collect()
            } else {
                vec![]
            },
            q: raw.to_lowercase(),
        }
    }

    /// Marks per node id (None without a filter) and the number of matched topics.
    pub fn visibility(&self, st: &Store) -> (Option<Vec<u8>>, u32) {
        if self.mode == FilterMode::None {
            return (None, 0);
        }
        let mut v = Visitor {
            f: self,
            st,
            marks: vec![0; st.nodes.len()],
            topics: 0,
        };
        // Connections are the top level; filters match the topics below them.
        for &c in st.nodes[ROOT].children() {
            if self.mode == FilterMode::Search {
                let mut any = false;
                for &t in st.nodes[c].children() {
                    any |= v.search(t);
                }
                if any {
                    v.marks[c] |= VISIBLE;
                }
            } else {
                v.pattern(c, 0);
            }
        }
        (Some(v.marks), v.topics)
    }
}

struct Visitor<'a> {
    f: &'a Filter,
    st: &'a Store,
    marks: Vec<u8>,
    topics: u32,
}

impl Visitor<'_> {
    fn match_subtree(&mut self, n: usize) {
        self.topics += self.st.nodes[n].topics;
        self.marks[n] |= VISIBLE | MATCHED;
        self.inside(n);
    }

    fn inside(&mut self, n: usize) {
        for &c in self.st.nodes[n].children() {
            let mut m = VISIBLE | INSIDE;
            if self.f.mode == FilterMode::Search && self.st.nodes[c].lower_name.contains(&self.f.q) {
                m |= MATCHED;
            }
            self.marks[c] |= m;
            self.inside(c);
        }
    }

    fn search(&mut self, n: usize) -> bool {
        if self.st.nodes[n].lower_path.contains(&self.f.q) {
            self.match_subtree(n);
            return true;
        }
        let mut any = false;
        for &c in self.st.nodes[n].children() {
            any |= self.search(c);
        }
        if any {
            self.marks[n] |= VISIBLE;
        }
        any
    }

    fn pattern(&mut self, n: usize, i: usize) -> bool {
        let segs = &self.f.segs;
        if i == segs.len() {
            self.match_subtree(n);
            return true;
        }
        let seg = segs[i].as_str();
        let last = i == segs.len() - 1;
        let mut any = false;
        match seg {
            "#" => {
                self.match_subtree(n);
                return true;
            }
            "+" => {
                for &c in self.st.nodes[n].children() {
                    any |= self.pattern(c, i + 1);
                }
            }
            _ if !last => {
                if let Some(&c) = self.st.nodes[n].children.get(seg) {
                    any = self.pattern(c, i + 1);
                }
            }
            _ => {
                let lseg = seg.to_lowercase();
                for &c in self.st.nodes[n].children() {
                    if self.st.nodes[c].lower_name.starts_with(&lseg) {
                        any |= self.pattern(c, i + 1);
                    }
                }
            }
        }
        if any && n != ROOT {
            self.marks[n] |= VISIBLE;
        }
        any
    }
}

#[derive(Clone, Copy)]
pub struct Row {
    pub node: usize,
    pub expanded: bool,
    pub mark: u8,
}

/// Depth-first list of the rows to display.
pub fn flatten(st: &Store, marks: Option<&[u8]>, expanded: &dyn Fn(usize, u8) -> bool) -> Vec<Row> {
    fn walk(st: &Store, n: usize, marks: Option<&[u8]>, expanded: &dyn Fn(usize, u8) -> bool, rows: &mut Vec<Row>) {
        for &c in st.nodes[n].children() {
            let m = match marks {
                Some(mk) => {
                    if mk[c] & VISIBLE == 0 {
                        continue;
                    }
                    mk[c]
                }
                None => 0,
            };
            let exp = st.nodes[c].has_children() && expanded(c, m);
            rows.push(Row {
                node: c,
                expanded: exp,
                mark: m,
            });
            if exp {
                walk(st, c, marks, expanded, rows);
            }
        }
    }
    let mut rows = Vec::with_capacity(256);
    walk(st, ROOT, marks, expanded, &mut rows);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(topics: &[&str]) -> Store {
        let mut s = Store::new(3, 8);
        s.add_conn("broker", "mqtt://x:1883", None, vec![]);
        for t in topics {
            s.add(0, t, format!("payload-{t}").as_bytes(), 0, false, Props::default());
        }
        s.sort_children();
        s
    }

    fn visible(s: &Store, f: &str) -> Vec<String> {
        let (marks, _) = Filter::compile(f).visibility(s);
        flatten(s, marks.as_deref(), &|_, _| true)
            .iter()
            .filter(|r| !s.nodes[r.node].is_conn())
            .map(|r| s.nodes[r.node].path.clone())
            .collect()
    }

    #[test]
    fn filter() {
        let s = store(&[
            "v1/a/temp",
            "v1/a/hum",
            "v1/b/temp",
            "v2/a/temp",
            "dev10/x",
            "dev2/x",
            "/lead",
        ]);
        let cases: &[(&str, &[&str])] = &[
            (
                "",
                &[
                    "",
                    "/lead",
                    "dev2",
                    "dev2/x",
                    "dev10",
                    "dev10/x",
                    "v1",
                    "v1/a",
                    "v1/a/hum",
                    "v1/a/temp",
                    "v1/b",
                    "v1/b/temp",
                    "v2",
                    "v2/a",
                    "v2/a/temp",
                ],
            ),
            ("v1/#", &["v1", "v1/a", "v1/a/hum", "v1/a/temp", "v1/b", "v1/b/temp"]),
            ("v1/+/temp", &["v1", "v1/a", "v1/a/temp", "v1/b", "v1/b/temp"]),
            ("+/a/te", &["v1", "v1/a", "v1/a/temp", "v2", "v2/a", "v2/a/temp"]),
            ("V1/A", &[]),
            ("v1/A", &["v1", "v1/a", "v1/a/hum", "v1/a/temp"]),
            ("hum", &["v1", "v1/a", "v1/a/hum"]),
            ("/l", &["", "/lead"]),
        ];
        for (f, want) in cases {
            assert_eq!(visible(&s, f), *want, "filter {f:?}");
        }
    }

    fn find<'a>(s: &'a Store, conn: usize, path: &str) -> &'a Node {
        s.nodes
            .iter()
            .find(|n| n.conn == conn && n.depth > 1 && n.path == path)
            .expect(path)
    }

    #[test]
    fn connections_are_separate_branches() {
        let mut s = Store::new(10, 64);
        s.add_conn("local", "mqtt://a:1883", None, vec![]);
        s.add_conn("staging", "mqtt://b:1883", None, vec![]);
        s.add(0, "devices/d1/temp", b"1", 0, false, Props::default());
        s.add(1, "devices/d1/temp", b"2", 0, false, Props::default());
        s.add(1, "other/x", b"3", 0, false, Props::default());
        s.sort_children();
        assert_eq!(find(&s, 0, "devices/d1/temp").latest().unwrap().payload, b"1");
        assert_eq!(find(&s, 1, "devices/d1/temp").latest().unwrap().payload, b"2");
        assert_eq!(s.nodes[s.conns[1].node].topics, 2);
        assert_eq!((s.topic_count(), s.total()), (3, 3));

        // A pattern matches under every connection, and the connection rows show.
        let (marks, matched) = Filter::compile("devices/#").visibility(&s);
        assert_eq!(matched, 2);
        let rows = flatten(&s, marks.as_deref(), &|_, _| true);
        let conns: Vec<_> = rows
            .iter()
            .filter(|r| s.nodes[r.node].is_conn())
            .map(|r| &s.nodes[r.node].name)
            .collect();
        assert_eq!(conns, ["local", "staging"]);
        assert!(!rows.iter().any(|r| s.nodes[r.node].path == "other/x"));
        // Search doesn't match connection names themselves.
        assert_eq!(Filter::compile("staging").visibility(&s).1, 0);
    }

    #[test]
    fn history_and_truncation() {
        let mut s = Store::new(3, 4);
        s.add_conn("c", "mqtt://x:1883", None, vec![]);
        for _ in 0..5 {
            s.add(0, "a/b", b"123456", 0, false, Props::default());
        }
        let n = find(&s, 0, "a/b");
        let m = n.messages();
        assert_eq!(m.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![5, 4, 3]);
        assert_eq!(m[0].payload, b"1234");
        assert!(m[0].truncated());
        assert_eq!((s.topic_count(), s.total()), (1, 5));
    }

    #[test]
    fn large_payload_keeps_only_prefix() {
        let mut s = Store::new(10, 4 * 1024);
        s.add_conn("c", "mqtt://x:1883", None, vec![]);
        let big = vec![b'x'; 5 * 1024 * 1024];
        s.add(0, "cam/frame", &big, 0, false, Props::default());
        let n = find(&s, 0, "cam/frame");
        let m = n.latest().unwrap();
        assert_eq!(m.payload.len(), 4096);
        assert!(
            m.payload.capacity() <= 4096,
            "allocation must not keep the full payload"
        );
        assert_eq!(m.size, big.len());
        assert!(m.truncated());
    }

    #[test]
    fn natural() {
        let mut v = vec!["s10", "s2", "s1", "a", "s02x"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["a", "s1", "s2", "s02x", "s10"]);
    }

    /// Filter + flatten over 100k topics.
    /// Run with: cargo test --release bench_filter -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_filter() {
        let mut s = Store::new(10, 1024);
        s.add_conn("c", "mqtt://x:1883", None, vec![]);
        let metrics = [
            "temp", "hum", "status", "rssi", "battery", "fw", "uptime", "load", "mem", "disk",
        ];
        for site in 0..10 {
            for dev in 0..1000 {
                for m in metrics {
                    s.add(
                        0,
                        &format!("v1/site{site}/dev{dev}/{m}"),
                        b"42",
                        0,
                        false,
                        Props::default(),
                    );
                }
            }
        }
        s.sort_children();
        for f in ["", "v1/+/dev12/te", "temp"] {
            let iters = 50;
            let t = std::time::Instant::now();
            for _ in 0..iters {
                let (marks, matched) = Filter::compile(f).visibility(&s);
                let rows = flatten(&s, marks.as_deref(), &|_, m| m & INSIDE == 0 || matched <= 300);
                std::hint::black_box(rows);
            }
            println!(
                "filter {f:>14}: {:>9.3} ms/op",
                t.elapsed().as_secs_f64() * 1000.0 / iters as f64
            );
        }
    }
}
