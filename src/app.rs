//! UI state and key handling.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::broker;
use crate::config::{self, ConnCfg};
use crate::mqtt::{self, Cmd};
use crate::store::{ConnState, Filter, FilterMode, INSIDE, MATCHED, Message, ROOT, Row, Store, Sub};
use crate::theme::Theme;
use crate::viewer::Viewer;

/// Up to this many matching topics, a filter expands everything it matched.
const AUTO_EXPAND_LIMIT: u32 = 300;
const NOTICE_FOR: Duration = Duration::from_secs(6);
/// How often the connections file is checked for changes.
const CONFIG_EVERY: Duration = Duration::from_secs(1);

/// The connections file's modified time and size, to notice edits.
type Stamp = Option<(SystemTime, u64)>;

fn stamp(path: &Path) -> Stamp {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

/// The value of the environment variable `name` (a `password_env`), if set.
pub fn env_secret(name: Option<&str>) -> Option<String> {
    name.and_then(|v| std::env::var(v).ok())
}

/// A connection's subscriptions as the store tracks them.
pub fn subs_of(c: &ConnCfg) -> Vec<Sub> {
    c.topics
        .iter()
        .map(|t| Sub {
            topic: t.clone(),
            enabled: !c.paused.contains(t),
            err: None,
        })
        .collect()
}

/// Whether two entries differ only in what applies without reconnecting
/// (subscriptions, capture and autoconnect).
fn same_link(a: &ConnCfg, b: &ConnCfg) -> bool {
    let strip = |c: &ConnCfg| ConnCfg {
        topics: vec![],
        paused: vec![],
        capture: vec![],
        capture_paused: vec![],
        autoconnect: false,
        ..c.clone()
    };
    strip(a) == strip(b)
}

pub struct Options {
    pub inline_bytes: usize, // payloads up to this size are shown inline in the tree
}

/// The UI's handle on one connection (same index as `Store::conns`).
pub struct ConnCtl {
    pub cfg: ConnCfg,
    pub saved: bool, // lives in the connections file; subscription toggles are written back
    pub tx: Sender<Cmd>,
    pub password: Option<String>,     // in memory only
    pub key_password: Option<String>, // in memory only
}

impl ConnCtl {
    /// What to ask before connecting, if anything: the key's passphrase first
    /// (also when the one we have doesn't open it), then the password.
    fn missing(&self) -> Option<Ask> {
        let key = self.cfg.key.as_deref().map(config::expand);
        if key.is_some_and(|k| mqtt::key_needs_passphrase(&k, self.key_password.as_deref())) {
            Some(Ask::KeyPassphrase)
        } else if self.cfg.username.is_some() && self.password.is_none() {
            Some(Ask::Password)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Conns,
    Tree,
    Msgs,
}

/// A row in the connections panel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PanelRow {
    Conn(usize),
    Sub(usize, usize),
    Capture(usize, usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ask {
    Password,
    KeyPassphrase,
}

pub struct Prompt {
    pub conn: usize,
    pub ask: Ask,
    pub input: String,
    pub err: Option<&'static str>,
}

pub struct Layout {
    pub panel_h: u16,
    pub tree_w: u16,
    pub tree_h: u16,
    pub det_w: u16,
    pub det_h: u16,
    pub stacked: bool,
}

pub struct App {
    pub store: Arc<Mutex<Store>>,
    pub opts: Options,
    pub th: Theme,
    pub width: u16,
    pub height: u16,
    pub now: SystemTime,

    pub ctls: Vec<ConnCtl>,
    pub config_path: PathBuf,
    config_stamp: Stamp,
    config_checked: Instant,
    pub panel: Vec<PanelRow>,
    pub panel_cursor: usize,
    pub prompt: Option<Prompt>,
    prompt_queue: Vec<usize>,
    pub notice: Option<(String, bool, Instant)>, // text, is_error, when

    pub filter_text: String,
    pub filter: Filter,
    pub marks: Option<Vec<u8>>,
    pub matched: u32,

    pub rows: Vec<Row>,
    pub cursor: usize,
    pub offset: usize,
    pub selected: Option<usize>,

    expanded: HashSet<usize>,   // used when no filter is active
    fexp: HashMap<usize, bool>, // overrides while a filter is active
    struct_ver: u64,
    need_rebuild: bool,

    pub focus: Focus,
    pub msg_sel: Option<u64>, // seq of the selected message in the list

    pub viewer: Option<Viewer>,

    pub rate: f64,
    pub(crate) rate_total: u64,
    pub(crate) rate_at: Instant,
    pub quit: bool,
}

impl App {
    /// `ask` lists connections to connect now, asking for passwords they need first.
    pub fn new(
        store: Arc<Mutex<Store>>,
        opts: Options,
        th: Theme,
        ctls: Vec<ConnCtl>,
        config_path: PathBuf,
        ask: Vec<usize>,
    ) -> App {
        // Connections start expanded so their top-level topics show.
        let expanded: HashSet<usize> = store.lock().unwrap().conns.iter().map(|c| c.node).collect();
        let mut app = App {
            store,
            opts,
            th,
            width: 0,
            height: 0,
            now: SystemTime::now(),
            ctls,
            config_stamp: stamp(&config_path),
            config_checked: Instant::now(),
            config_path,
            panel: Vec::new(),
            panel_cursor: 0,
            prompt: None,
            prompt_queue: ask,
            notice: None,
            filter_text: String::new(),
            filter: Filter::compile(""),
            marks: None,
            matched: 0,
            rows: Vec::new(),
            cursor: 0,
            offset: 0,
            selected: None,
            expanded,
            fexp: HashMap::new(),
            struct_ver: u64::MAX,
            need_rebuild: true,
            focus: Focus::Tree,
            msg_sel: None,
            viewer: None,
            rate: 0.0,
            rate_total: 0,
            rate_at: Instant::now(),
            quit: false,
        };
        app.refresh_panel();
        app.next_prompt();
        app
    }

    pub fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.notice = Some((text.into(), error, Instant::now()));
    }

    pub fn layout(&self) -> Layout {
        // The connections panel takes what it needs, up to a third of the screen.
        let max_panel = (self.height / 3).max(3);
        let panel_h = (self.panel.len() as u16 + 2).clamp(3, max_panel);
        let body_h = self.height.saturating_sub(5 + panel_h).max(4); // header + input box (3) + hints
        if self.width < 80 {
            let th = body_h * 55 / 100;
            return Layout {
                panel_h,
                tree_w: self.width,
                tree_h: th,
                det_w: self.width,
                det_h: body_h - th,
                stacked: true,
            };
        }
        let tw = (self.width as u32 * 45 / 100) as u16;
        Layout {
            panel_h,
            tree_w: tw,
            tree_h: body_h,
            det_w: self.width - tw,
            det_h: body_h,
            stacked: false,
        }
    }

    pub fn tree_height(&self) -> usize {
        self.layout().tree_h.saturating_sub(2) as usize
    }

    pub fn resize(&mut self, w: u16, h: u16) {
        self.width = w;
        self.height = h;
        if let Some(v) = &mut self.viewer {
            v.relayout(w.saturating_sub(4) as usize, &self.th);
        }
        self.clamp_cursor();
    }

    pub fn tick(&mut self) {
        self.now = SystemTime::now();
        let (total, notices) = {
            let mut st = self.store.lock().unwrap();
            if st.struct_ver != self.struct_ver {
                self.need_rebuild = true;
            }
            (st.total(), st.take_notices())
        };
        // The last one shows; an error isn't hidden by a later success.
        let last_err = notices.iter().rposition(|(_, err)| *err);
        if let Some((text, err)) = last_err.or(notices.len().checked_sub(1)).map(|i| notices[i].clone()) {
            self.notify(text, err);
        }
        let d = self.rate_at.elapsed();
        if d >= Duration::from_secs(1) {
            self.rate = total.saturating_sub(self.rate_total) as f64 / d.as_secs_f64();
            self.rate_total = total;
            self.rate_at = Instant::now();
        }
        if let Some((_, _, at)) = self.notice
            && at.elapsed() > NOTICE_FOR
        {
            self.notice = None;
        }
        if self.config_checked.elapsed() >= CONFIG_EVERY {
            self.config_checked = Instant::now();
            self.check_config();
        }
        self.rebuild(false);
    }

    fn refresh_panel(&mut self) {
        let st = self.store.lock().unwrap();
        self.panel.clear();
        for (i, c) in st.conns.iter().enumerate().filter(|(_, c)| !c.removed) {
            self.panel.push(PanelRow::Conn(i));
            self.panel.extend((0..c.subs.len()).map(|j| PanelRow::Sub(i, j)));
            self.panel
                .extend((0..c.capture.rules.len()).map(|j| PanelRow::Capture(i, j)));
        }
        drop(st);
        self.panel_cursor = self.panel_cursor.min(self.panel.len().saturating_sub(1));
    }

    /// Recomputes visible rows if the tree, filter or expansion changed.
    fn rebuild(&mut self, filter_changed: bool) {
        if !self.need_rebuild && !filter_changed {
            return;
        }
        self.need_rebuild = false;
        let store = self.store.clone();
        let mut st = store.lock().unwrap();
        st.sort_children();
        self.struct_ver = st.struct_ver;
        let (marks, matched) = self.filter.visibility(&st);
        self.marks = marks;
        self.matched = matched;
        self.rows = crate::store::flatten(&st, self.marks.as_deref(), &|n, m| self.is_expanded(n, m));
        drop(st);

        if filter_changed {
            // Like fzf: jump to the first thing that matched.
            self.cursor = self.rows.iter().position(|r| r.mark & MATCHED != 0).unwrap_or(0);
            self.offset = 0;
        } else if let Some(sel) = self.selected
            && let Some(i) = self.rows.iter().position(|r| r.node == sel)
        {
            self.cursor = i;
        }
        self.clamp_cursor();
    }

    fn is_expanded(&self, n: usize, mark: u8) -> bool {
        if self.filter.mode == FilterMode::None {
            return self.expanded.contains(&n);
        }
        if let Some(&v) = self.fexp.get(&n) {
            return v;
        }
        mark & INSIDE == 0 || self.matched <= AUTO_EXPAND_LIMIT
    }

    fn set_expanded(&mut self, n: usize, v: bool) {
        if self.filter.mode == FilterMode::None {
            if v {
                self.expanded.insert(n);
            } else {
                self.expanded.remove(&n);
            }
        } else {
            self.fexp.insert(n, v);
        }
        self.need_rebuild = true;
    }

    fn move_cursor(&mut self, delta: isize) {
        self.cursor = (self.cursor as isize + delta).max(0) as usize;
    }

    fn clamp_cursor(&mut self) {
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
        let prev = self.selected;
        self.selected = self.rows.get(self.cursor).map(|r| r.node);
        if self.selected != prev {
            self.msg_sel = None;
            if self.focus == Focus::Msgs {
                self.focus = Focus::Tree;
            }
        }
        let h = self.tree_height();
        if self.cursor < self.offset {
            self.offset = self.cursor;
        }
        if h > 0 && self.cursor >= self.offset + h {
            self.offset = self.cursor + 1 - h;
        }
        self.offset = self.offset.min(self.rows.len().saturating_sub(h));
    }

    fn set_filter(&mut self, s: String) {
        if s == self.filter_text {
            return;
        }
        let was_filtered = self.filter.mode != FilterMode::None;
        self.filter = Filter::compile(&s);
        self.filter_text = s;
        self.fexp.clear();
        self.focus = Focus::Tree;
        if self.filter.mode == FilterMode::None {
            // Clearing the filter: open the path to where you were.
            if let (true, Some(sel)) = (was_filtered, self.selected) {
                let st = self.store.lock().unwrap();
                let mut p = st.nodes[sel].parent;
                while let Some(i) = p {
                    if i == ROOT {
                        break;
                    }
                    self.expanded.insert(i);
                    p = st.nodes[i].parent;
                }
            }
            self.need_rebuild = true;
            self.rebuild(false);
            return;
        }
        self.rebuild(true);
    }

    pub fn paste(&mut self, s: &str) {
        if let Some(p) = &mut self.prompt {
            p.input.push_str(s.trim_end_matches(['\r', '\n']));
        } else if self.viewer.is_none() {
            let t = format!("{}{}", self.filter_text, s.trim());
            self.set_filter(t);
        }
    }

    /// `tab` cycles connections → tree → messages (when the topic has any).
    fn next_focus(&mut self, back: bool) {
        let has_msgs = self
            .selected
            .is_some_and(|n| self.store.lock().unwrap().nodes[n].has_messages());
        let order: &[Focus] = if has_msgs {
            &[Focus::Conns, Focus::Tree, Focus::Msgs]
        } else {
            &[Focus::Conns, Focus::Tree]
        };
        let i = order.iter().position(|&f| f == self.focus).unwrap_or(1);
        let n = order.len();
        let next = order[if back { (i + n - 1) % n } else { (i + 1) % n }];
        match next {
            Focus::Msgs => self.enter_msgs(),
            f => {
                self.focus = f;
                self.msg_sel = None;
            }
        }
    }

    pub fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.prompt.is_some() {
            self.prompt_key(k);
            return;
        }
        if self.viewer.is_some() {
            self.viewer_key(k);
            return;
        }
        match k.code {
            KeyCode::Tab => return self.next_focus(false),
            KeyCode::BackTab => return self.next_focus(true),
            _ => {}
        }
        if self.focus == Focus::Conns && self.panel_key(k) {
            return;
        }
        match k.code {
            KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => {
                let t = format!("{}{c}", self.filter_text);
                self.set_filter(t);
                return;
            }
            KeyCode::Backspace => {
                let mut t = self.filter_text.clone();
                t.pop();
                self.set_filter(t);
                return;
            }
            KeyCode::Char('w') if ctrl => {
                // delete back to the previous level
                let s = self.filter_text.trim_end_matches('/');
                let t = match s.rfind('/') {
                    Some(i) => s[..=i].to_string(),
                    None => String::new(),
                };
                self.set_filter(t);
                return;
            }
            KeyCode::Char('u') if ctrl => {
                self.set_filter(String::new());
                return;
            }
            KeyCode::Char('r') if ctrl && self.focus != Focus::Conns => {
                self.capture_selected();
                return;
            }
            _ => {}
        }

        if self.focus == Focus::Msgs {
            self.msg_key(k);
            return;
        }

        let Some(n) = self.selected else {
            if k.code == KeyCode::Esc {
                self.set_filter(String::new());
            }
            return;
        };
        let (has_kids, has_msgs, parent) = {
            let st = self.store.lock().unwrap();
            let node = &st.nodes[n];
            (node.has_children(), node.has_messages(), node.parent)
        };
        let expanded = self.rows[self.cursor].expanded;
        let page = self.tree_height() as isize;
        match k.code {
            KeyCode::Esc => self.set_filter(String::new()),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Down => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-page),
            KeyCode::PageDown => self.move_cursor(page),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.rows.len().saturating_sub(1),
            KeyCode::Right => {
                if has_kids && !expanded {
                    self.set_expanded(n, true);
                } else if has_msgs {
                    self.enter_msgs();
                } else if expanded {
                    self.move_cursor(1);
                }
            }
            KeyCode::Left => {
                if expanded {
                    self.set_expanded(n, false);
                } else if let Some(p) = parent.filter(|&p| p != ROOT)
                    && let Some(i) = self.rows[..self.cursor].iter().rposition(|r| r.node == p)
                {
                    self.cursor = i;
                }
            }
            KeyCode::Enter => {
                if has_msgs {
                    self.open_viewer(n, None);
                } else if has_kids {
                    self.set_expanded(n, !expanded);
                }
            }
            _ => {}
        }
        self.clamp_cursor();
        self.rebuild(false);
    }

    // ── Connections panel ──────────────────────────────────────────────────

    /// Handles a key in the panel; false lets it fall through (e.g. typing).
    fn panel_key(&mut self, k: KeyEvent) -> bool {
        let last = self.panel.len().saturating_sub(1);
        match k.code {
            KeyCode::Up => self.panel_cursor = self.panel_cursor.saturating_sub(1),
            KeyCode::Down => self.panel_cursor = (self.panel_cursor + 1).min(last),
            KeyCode::Home => self.panel_cursor = 0,
            KeyCode::End => self.panel_cursor = last,
            KeyCode::Char(' ') | KeyCode::Enter => match self.panel.get(self.panel_cursor).copied() {
                Some(PanelRow::Conn(i)) => self.toggle_conn(i),
                Some(PanelRow::Sub(i, j)) => self.toggle_sub(i, j),
                Some(PanelRow::Capture(i, j)) => self.toggle_capture(i, j),
                None => {}
            },
            KeyCode::Esc => self.focus = Focus::Tree,
            _ => return false,
        }
        true
    }

    pub fn toggle_conn(&mut self, i: usize) {
        let state = self.store.lock().unwrap().conns[i].status.state;
        if state == ConnState::Disconnected {
            self.connect_or_ask(i);
        } else {
            let _ = self.ctls[i].tx.send(Cmd::Disconnect);
        }
    }

    pub fn toggle_sub(&mut self, i: usize, j: usize) {
        // Apply pending edits to the file first: the save below keeps the
        // rest of the file and marks it as read, and must not write over an
        // edit to this connection with what we had before it.
        let Some(topic) = self.store.lock().unwrap().conns[i].subs.get(j).map(|s| s.topic.clone()) else {
            return;
        };
        self.check_config();
        let (topic, enabled) = {
            let mut st = self.store.lock().unwrap();
            let Some(s) = st.conns[i].subs.iter_mut().find(|s| s.topic == topic) else {
                return; // removed by the edit (or the connection is gone)
            };
            s.enabled = !s.enabled;
            s.err = None;
            (s.topic.clone(), s.enabled)
        };
        let ctl = &mut self.ctls[i];
        let _ = ctl.tx.send(if enabled {
            Cmd::Subscribe(topic.clone())
        } else {
            Cmd::Unsubscribe(topic.clone())
        });
        ctl.cfg.paused.retain(|t| t != &topic);
        if !enabled {
            ctl.cfg.paused.push(topic);
        }
        self.save_conn(i);
    }

    /// Writes connection `i`'s entry back to the connections file, if it
    /// came from there.
    fn save_conn(&mut self, i: usize) {
        if self.ctls[i].saved {
            match config::save(&self.config_path, &self.ctls[i].cfg) {
                // Our own write: nothing to reload.
                Ok(()) => self.config_stamp = stamp(&self.config_path),
                Err(e) => self.notify(format!("couldn't save: {e}"), true),
            }
        }
    }

    // ── Capture to disk ────────────────────────────────────────────────────

    /// `ctrl+r`: starts or stops capturing the selected topic's subtree (a
    /// whole connection on its row). A topic an enabled rule already
    /// captures gets no narrower rule.
    fn capture_selected(&mut self) {
        let Some(n) = self.selected else { return };
        let (i, path) = {
            let st = self.store.lock().unwrap();
            (st.nodes[n].conn, st.nodes[n].path.clone())
        };
        self.check_config(); // apply pending edits before saving over them
        if self.is_removed(i) {
            return;
        }
        let rule = crate::capture::subtree(&path);
        let cfg = &mut self.ctls[i].cfg;
        let added = if cfg.capture.contains(&rule) {
            cfg.capture.retain(|r| r != &rule);
            cfg.capture_paused.retain(|r| r != &rule);
            false
        } else {
            let covering = {
                let st = self.store.lock().unwrap();
                st.conns[i].capture.covering(&rule).map(String::from)
            };
            if let Some(by) = covering {
                self.notify(format!("{rule} is already captured by {by}"), false);
                return;
            }
            cfg.capture.push(rule.clone());
            true
        };
        self.sync_capture(i);
        let text = if added {
            let st = self.store.lock().unwrap();
            match st.conns[i].capture.file() {
                Some(f) => format!("capturing {rule} to {} (appending)", f.display()),
                None => format!(
                    "capturing {rule} to {} (from the next message)",
                    st.capture_dir.display()
                ),
            }
        } else {
            format!("stopped capturing {rule}")
        };
        self.notify(text, false);
        self.save_conn(i);
    }

    /// `space` on a capture rule in the panel: turns it off or on.
    pub fn toggle_capture(&mut self, i: usize, j: usize) {
        let Some(rule) = self.store.lock().unwrap().conns[i]
            .capture
            .rules
            .get(j)
            .map(|r| r.filter.clone())
        else {
            return;
        };
        self.check_config();
        let cfg = &mut self.ctls[i].cfg;
        if !cfg.capture.contains(&rule) {
            return; // removed by the edit (or the connection is gone)
        }
        if cfg.capture_paused.contains(&rule) {
            cfg.capture_paused.retain(|r| r != &rule);
        } else {
            cfg.capture_paused.push(rule);
        }
        self.sync_capture(i);
        self.save_conn(i);
    }

    /// Hands connection `i`'s capture rules to the store, where its thread
    /// reads them. Toggling also retries after capture stopped, and what was
    /// captured so far is written out.
    fn sync_capture(&mut self, i: usize) {
        {
            let mut st = self.store.lock().unwrap();
            let cfg = &self.ctls[i].cfg;
            let c = &mut st.conns[i].capture;
            c.set_rules(&cfg.capture, &cfg.capture_paused);
            c.retry();
        }
        self.refresh_panel();
        crate::capture::flush(&self.store, i);
    }

    /// Writes out what every connection captured, before quitting: waits up
    /// to `wait` in all (a stuck disk mustn't hang the exit), never with the
    /// store lock held. Returns what couldn't be written in time.
    pub fn flush_captures(&self, wait: Duration) -> Vec<String> {
        let writers: Vec<_> = {
            let st = self.store.lock().unwrap();
            st.conns
                .iter()
                .filter_map(|c| Some((c.name.clone(), c.capture.writer()?)))
                .collect()
        };
        let deadline = Instant::now() + wait;
        let acks: Vec<_> = writers.into_iter().map(|(name, w)| (name, w.flush_ack(), w)).collect();
        acks.into_iter()
            .filter(|(_, ack, w)| {
                let left = deadline.saturating_duration_since(Instant::now());
                ack.recv_timeout(left).is_err() && w.pending() > 0
            })
            .map(|(name, _, w)| {
                let left = crate::fmt::human_bytes(w.pending());
                format!("{name}: {left} of captured messages not written, the disk is too slow")
            })
            .collect()
    }

    // ── Live reload of the connections file ────────────────────────────────

    /// Re-reads the connections file if it changed since we last read it.
    pub(crate) fn check_config(&mut self) {
        let now = stamp(&self.config_path);
        if now == self.config_stamp {
            return;
        }
        self.config_stamp = now;
        if now.is_none() {
            return; // deleted, or mid-replace by an editor: keep what we have
        }
        match config::load(&self.config_path) {
            Ok(entries) => self.apply_config(entries),
            Err(e) => self.notify(format!("not reloaded: {e}"), true),
        }
    }

    /// Brings the connections in line with the file's `entries`: adds new
    /// ones, updates changed ones, removes saved ones that are gone.
    /// Connections that aren't from the file (`--no-save`, the first-run
    /// localhost) are left alone, and so is a file entry of the same name.
    pub fn apply_config(&mut self, entries: Vec<ConnCfg>) {
        let (mut added, mut changed, mut removed) = (0, 0, 0);
        let mut errors = Vec::new();
        for e in &entries {
            let res = match self.conn_named(&e.name) {
                None => self.add_conn(e.clone()).map(|()| added += 1),
                Some(i) if !self.ctls[i].saved || self.ctls[i].cfg == *e => Ok(()),
                Some(i) => self.update_conn(i, e.clone()).map(|()| changed += 1),
            };
            if let Err(err) = res {
                errors.push(format!("{:?}: {err}", e.name));
            }
        }
        let gone: Vec<usize> = (0..self.ctls.len())
            .filter(|&i| self.ctls[i].saved && !self.is_removed(i))
            .filter(|&i| !entries.iter().any(|e| e.name == self.ctls[i].cfg.name))
            .collect();
        for i in gone {
            self.remove_conn(i);
            removed += 1;
        }
        if added + changed + removed > 0 {
            self.refresh_panel();
            self.need_rebuild = true;
            self.rebuild(false);
        }
        if let Some(err) = errors.first() {
            self.notify(format!("connection {err}"), true);
        } else if added + changed + removed > 0 {
            let parts: Vec<String> = [(added, "added"), (changed, "changed"), (removed, "removed")]
                .iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, what)| format!("{n} {what}"))
                .collect();
            self.notify(format!("connections reloaded: {}", parts.join(", ")), false);
        }
        self.next_prompt();
    }

    fn conn_named(&self, name: &str) -> Option<usize> {
        (0..self.ctls.len()).find(|&i| self.ctls[i].cfg.name == name && !self.is_removed(i))
    }

    fn is_removed(&self, i: usize) -> bool {
        self.store.lock().unwrap().conns[i].removed
    }

    fn add_conn(&mut self, c: ConnCfg) -> Result<(), String> {
        let i = self.ctls.len();
        let password = env_secret(c.password_env.as_deref());
        let key_password = env_secret(c.key_password_env.as_deref());
        let cfg = broker::mqtt_config(i, &c, password.clone(), key_password.clone())?;
        let display = broker::display_url(&c)?;
        let node = {
            let mut st = self.store.lock().unwrap();
            let idx = st.add_conn(&c.name, &display, c.username.clone(), subs_of(&c));
            debug_assert_eq!(idx, i);
            st.conns[idx].capture.set_rules(&c.capture, &c.capture_paused);
            st.conns[idx].node
        };
        self.expanded.insert(node);
        if c.autoconnect {
            self.prompt_queue.push(i);
        }
        self.ctls.push(ConnCtl {
            tx: mqtt::spawn(cfg, self.store.clone(), false),
            cfg: c,
            saved: true,
            password,
            key_password,
        });
        Ok(())
    }

    /// Applies an edited entry. Subscription changes go to the live
    /// connection; anything else reconnects it with the new settings.
    fn update_conn(&mut self, i: usize, new: ConnCfg) -> Result<(), String> {
        let display = broker::display_url(&new)?;
        let ctl = &mut self.ctls[i];
        let old = std::mem::replace(&mut ctl.cfg, new.clone());
        if old.username != new.username || old.password_env != new.password_env {
            ctl.password = env_secret(new.password_env.as_deref());
        }
        if old.key != new.key || old.key_password_env != new.key_password_env {
            ctl.key_password = env_secret(new.key_password_env.as_deref());
        }
        let (state, before) = {
            let mut st = self.store.lock().unwrap();
            let c = &mut st.conns[i];
            let mut subs = subs_of(&new);
            for s in &mut subs {
                // Keep a refusal shown until the broker says otherwise.
                s.err = c
                    .subs
                    .iter()
                    .find(|o| o.topic == s.topic && o.enabled)
                    .and_then(|o| o.err.clone());
            }
            let before = std::mem::replace(&mut c.subs, subs);
            c.url = display;
            c.username = new.username.clone();
            (c.status.state, before)
        };
        if old.capture != new.capture || old.capture_paused != new.capture_paused {
            {
                let mut st = self.store.lock().unwrap();
                let c = &mut st.conns[i].capture;
                c.set_rules(&new.capture, &new.capture_paused);
                c.retry();
            }
            crate::capture::flush(&self.store, i);
        }
        let ctl = &mut self.ctls[i];
        let active = state != ConnState::Disconnected;
        if !same_link(&old, &new) {
            // An empty password means username only, as in connect_or_ask.
            let password = ctl.password.clone().filter(|p| !p.is_empty());
            let cfg = broker::mqtt_config(i, &new, password, ctl.key_password.clone())?;
            let connect = active && ctl.missing().is_none();
            let _ = ctl.tx.send(Cmd::Reconfigure {
                cfg: Box::new(cfg),
                connect,
            });
            if active && !connect {
                self.prompt_queue.push(i); // a new password or passphrase is needed
            }
        } else if state == ConnState::Connected {
            let on =
                |subs: &[Sub]| -> Vec<String> { subs.iter().filter(|s| s.enabled).map(|s| s.topic.clone()).collect() };
            let (was, now) = (on(&before), on(&subs_of(&new)));
            for t in now.iter().filter(|t| !was.contains(t)) {
                let _ = ctl.tx.send(Cmd::Subscribe(t.clone()));
            }
            for t in was.iter().filter(|t| !now.contains(t)) {
                let _ = ctl.tx.send(Cmd::Unsubscribe(t.clone()));
            }
        }
        Ok(())
    }

    /// Disconnects connection `i` and takes it out of the panel and tree.
    fn remove_conn(&mut self, i: usize) {
        let ctl = &mut self.ctls[i];
        let _ = ctl.tx.send(Cmd::Disconnect);
        ctl.tx = mpsc::channel().0; // dropping the old sender ends the thread
        ctl.saved = false;
        let mut st = self.store.lock().unwrap();
        // Its messages leave the total; keep the rate's baseline in step.
        let msgs = st.remove_conn(i);
        self.rate_total = self.rate_total.saturating_sub(msgs);
        self.expanded.remove(&st.conns[i].node);
        if self.selected.is_some_and(|n| st.nodes[n].conn == i) {
            self.selected = None;
            self.msg_sel = None;
            if self.focus == Focus::Msgs {
                self.focus = Focus::Tree;
            }
        }
        drop(st);
        if self.prompt.as_ref().is_some_and(|p| p.conn == i) {
            self.prompt = None;
        }
        self.prompt_queue.retain(|&c| c != i);
    }

    /// Connects `i`, or opens the prompt for what it still needs.
    fn connect_or_ask(&mut self, i: usize) {
        let ctl = &self.ctls[i];
        match ctl.missing() {
            Some(ask) => {
                self.prompt = Some(Prompt {
                    conn: i,
                    ask,
                    input: String::new(),
                    err: None,
                })
            }
            None => {
                // An empty password connects with the username only.
                let _ = ctl.tx.send(Cmd::Connect {
                    password: ctl.password.clone().filter(|p| !p.is_empty()),
                    key_password: ctl.key_password.clone(),
                });
            }
        }
    }

    fn next_prompt(&mut self) {
        while self.prompt.is_none() && !self.prompt_queue.is_empty() {
            let conn = self.prompt_queue.remove(0);
            self.connect_or_ask(conn);
        }
    }

    fn prompt_key(&mut self, k: KeyEvent) {
        let Some(p) = &mut self.prompt else { return };
        match k.code {
            KeyCode::Char(c) => p.input.push(c),
            KeyCode::Backspace => {
                p.input.pop();
            }
            KeyCode::Enter => {
                let ctl = &mut self.ctls[p.conn];
                match p.ask {
                    // Kept even when empty, so it isn't asked for again.
                    Ask::Password => ctl.password = Some(std::mem::take(&mut p.input)),
                    Ask::KeyPassphrase => {
                        ctl.key_password = Some(std::mem::take(&mut p.input));
                        if ctl.missing() == Some(Ask::KeyPassphrase) {
                            p.err = Some("wrong passphrase, try again");
                            return;
                        }
                    }
                }
                let conn = p.conn;
                self.prompt = None;
                self.connect_or_ask(conn);
                self.next_prompt();
            }
            KeyCode::Esc => {
                self.prompt = None;
                self.next_prompt();
            }
            _ => {}
        }
    }

    // ── Messages and viewer ────────────────────────────────────────────────

    fn enter_msgs(&mut self) {
        if let Some(n) = self.selected {
            let st = self.store.lock().unwrap();
            let latest = st.nodes[n].latest().map(|m| m.seq);
            drop(st);
            if latest.is_some() {
                self.msg_sel = latest;
                self.focus = Focus::Msgs;
            }
        }
    }

    fn msg_key(&mut self, k: KeyEvent) {
        let Some(n) = self.selected else { return };
        let msgs = self.store.lock().unwrap().nodes[n].messages();
        if msgs.is_empty() {
            return;
        }
        let mut i = msg_index(&msgs, self.msg_sel) as isize;
        match k.code {
            KeyCode::Up => i -= 1,
            KeyCode::Down => i += 1,
            KeyCode::Home | KeyCode::PageUp => i = 0,
            KeyCode::End | KeyCode::PageDown => i = msgs.len() as isize - 1,
            KeyCode::Left | KeyCode::Esc => {
                self.focus = Focus::Tree;
                self.msg_sel = None;
                return;
            }
            KeyCode::Enter => {
                self.open_viewer(n, self.msg_sel);
                return;
            }
            _ => {}
        }
        let i = i.clamp(0, msgs.len() as isize - 1) as usize;
        self.msg_sel = Some(msgs[i].seq);
    }

    fn open_viewer(&mut self, n: usize, sel: Option<u64>) {
        let (msgs, title) = {
            let st = self.store.lock().unwrap();
            let node = &st.nodes[n];
            (node.messages(), node_title(&st, n))
        };
        if msgs.is_empty() {
            return;
        }
        let idx = msg_index(&msgs, sel);
        let mut v = Viewer::new(n, title, msgs, idx);
        v.relayout(self.width.saturating_sub(4) as usize, &self.th);
        self.viewer = Some(v);
    }

    /// The topic node in focus: the one shown full screen, else the
    /// selected tree node (which may be a connection).
    #[allow(dead_code)] // until an action uses it
    pub fn focused_node(&self) -> Option<usize> {
        match &self.viewer {
            Some(v) => Some(v.node),
            None => self.selected,
        }
    }

    /// The message in focus, with its topic node: the one shown full screen,
    /// the one picked in the message list, or the selected topic's latest.
    #[allow(dead_code)] // until an action uses it
    pub fn focused_message(&self) -> Option<(usize, Arc<Message>)> {
        if let Some(v) = &self.viewer {
            return v.msgs.get(v.idx).map(|m| (v.node, m.clone()));
        }
        let n = self.selected?;
        let st = self.store.lock().unwrap();
        let m = if self.focus == Focus::Msgs {
            let msgs = st.nodes[n].messages(); // newest first
            msgs.get(msg_index(&msgs, self.msg_sel)).cloned()
        } else {
            st.nodes[n].latest().cloned()
        };
        m.map(|m| (n, m))
    }

    fn viewer_key(&mut self, k: KeyEvent) {
        let body_h = self.viewer_body_h();
        let v = self.viewer.as_mut().unwrap();
        if v.key(k, body_h, &self.th) {
            self.viewer = None;
        }
    }

    pub fn viewer_body_h(&self) -> usize {
        match &self.viewer {
            Some(v) => (self.height as usize).saturating_sub(3 + v.head.len()).max(1),
            None => 0,
        }
    }
}

/// "connection · topic/path" for a topic, or the connection name.
pub fn node_title(st: &Store, n: usize) -> String {
    let node = &st.nodes[n];
    let conn = &st.conns[node.conn].name;
    if node.is_conn() {
        conn.clone()
    } else {
        format!("{conn} · {}", node.path)
    }
}

/// Finds `sel` in `msgs`, falling back to the oldest if it rotated out.
pub fn msg_index(msgs: &[Arc<Message>], sel: Option<u64>) -> usize {
    let Some(sel) = sel else { return 0 };
    msgs.iter()
        .position(|m| m.seq == sel)
        .or_else(|| msgs.iter().position(|m| m.seq < sel))
        .unwrap_or(msgs.len().saturating_sub(1))
}
