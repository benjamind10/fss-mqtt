//! The client in a web page: the page's WebSocket carries what the desktop's
//! connection threads send over TCP. A page has no threads, so `poll`, once
//! a frame, does what a thread does: commands, packets, pings and retries,
//! with the same protocol code.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{BinaryType, CloseEvent, Event, MessageEvent, WebSocket};
use web_time::Instant;

use super::{
    CONNECT_TIMEOUT, Cmd, Config, DISCONNECT, End, Ids, Inbox, Limits, Outbox, PINGREQ, RETRY, Reader, command,
    connack, connect_packet, not_connected, packet_in, ping_every, set_status, silence_limit, subscribe_all,
};
use crate::store::{ConnState, Store};

thread_local! {
    static CONNS: RefCell<Vec<Conn>> = const { RefCell::new(Vec::new()) };
}

/// Starts connection `cfg.conn`, connecting now if `start`; `poll` runs it.
pub fn spawn(cfg: Config, store: Arc<Mutex<Store>>, start: bool) -> Sender<Cmd> {
    let (tx, rx) = mpsc::channel();
    let sparkplug = store.lock().unwrap().conns[cfg.conn].sparkplug.clone();
    let mut c = Conn {
        cfg,
        store,
        rx,
        inbox: Inbox { sparkplug },
        outbox: Outbox::default(),
        state: State::Off,
    };
    c.state = if start { c.open() } else { c.end(End::Stopped) };
    CONNS.with(|cs| cs.borrow_mut().push(c));
    tx
}

/// Moves every connection along; the page calls it each frame, without
/// holding the store's lock.
pub fn poll() {
    CONNS.with(|cs| {
        for c in cs.borrow_mut().iter_mut() {
            while let Ok(cmd) = c.rx.try_recv() {
                c.command(cmd);
            }
            c.step();
        }
    });
}

struct Conn {
    cfg: Config,
    store: Arc<Mutex<Store>>,
    rx: Receiver<Cmd>,
    inbox: Inbox,
    outbox: Outbox,
    state: State,
}

enum State {
    /// Disconnected, until the UI says connect.
    Off,
    /// Failed; tries again then.
    Retry(Instant),
    /// The WebSocket is opening, until the deadline.
    Opening(Socket, Instant),
    /// CONNECT went; the CONNACK is due by the deadline.
    Connecting(Socket, Reader, Instant),
    Up(Box<Session>),
}

struct Session {
    sock: Socket,
    rd: Reader,
    ids: Ids,
    limits: Limits,
    keep_alive: u16,
    last_send: Instant,
    last_recv: Instant,
}

impl Conn {
    fn url(&self) -> String {
        let c = &self.cfg;
        let host = if c.host.contains(':') {
            format!("[{}]", c.host)
        } else {
            c.host.clone()
        };
        let path = c.ws.as_ref().map_or("/mqtt", |u| u.path.as_str());
        format!("{}://{host}:{}{path}", if c.tls { "wss" } else { "ws" }, c.port)
    }

    /// Why a web page can't make this connection, if it can't.
    fn unsupported(&self) -> Option<&'static str> {
        let c = &self.cfg;
        let https = web_sys::window()
            .and_then(|w| w.location().protocol().ok())
            .is_some_and(|p| p == "https:");
        Some(match &c.ws {
            None => "a web page reaches brokers over WebSockets: use a ws:// or wss:// URL",
            Some(u) if !u.headers.is_empty() || !u.headers_env.is_empty() => {
                "a web page can't send WebSocket headers: leave them out"
            }
            _ if c.proxy.is_some() => "a web page uses the browser's proxy settings: leave the proxy out",
            _ if c.ca_file.is_some() || c.cert_file.is_some() || c.key_file.is_some() => {
                "a web page uses the browser's certificates: leave the certificate files out"
            }
            _ if !c.tls && https => "a page served over https can only reach wss:// brokers",
            _ => return None,
        })
    }

    fn open(&mut self) -> State {
        if let Some(why) = self.unsupported() {
            return self.end(End::Halted(why.into()));
        }
        set_status(&self.store, self.cfg.conn, ConnState::Connecting, None);
        match Socket::open(&self.url()) {
            Ok(s) => State::Opening(s, Instant::now() + CONNECT_TIMEOUT),
            Err(e) => self.end(End::Failed(e)),
        }
    }

    /// After a session, or instead of one: what comes next.
    fn end(&mut self, end: End) -> State {
        self.outbox.abandon(&self.store);
        let (conn, store) = (self.cfg.conn, &self.store);
        match end {
            End::Failed(err) => {
                set_status(store, conn, ConnState::Connecting, Some(err));
                State::Retry(Instant::now() + RETRY)
            }
            End::Halted(err) => {
                set_status(store, conn, ConnState::Disconnected, Some(err));
                State::Off
            }
            End::Stopped | End::Closed => {
                set_status(store, conn, ConnState::Disconnected, None);
                State::Off
            }
            End::Reconfigured(cfg, connect) => {
                self.cfg = *cfg;
                if connect { self.open() } else { self.end(End::Stopped) }
            }
        }
    }

    fn command(&mut self, cmd: Cmd) {
        self.state = match std::mem::replace(&mut self.state, State::Off) {
            State::Up(mut s) => {
                let mut sent = None;
                let (cfg, store) = (&self.cfg, &self.store);
                match command(cmd, cfg, store, &mut s.ids, &mut self.outbox, &s.limits, &mut sent) {
                    Ok(None) => State::Up(s),
                    Ok(Some(pkt)) => match s.sock.send(&pkt) {
                        Ok(()) => {
                            s.last_send = Instant::now();
                            if let Some(m) = sent {
                                store.lock().unwrap().notify(m, false); // QoS 0: written is all we learn
                            }
                            State::Up(s)
                        }
                        Err(e) => self.end(End::Failed(e)),
                    },
                    Err(end) => {
                        let _ = s.sock.send(&DISCONNECT);
                        drop(s);
                        self.end(end)
                    }
                }
            }
            other => match cmd {
                Cmd::Connect { password, key_password } => {
                    self.cfg.update(password, key_password);
                    match other {
                        State::Off | State::Retry(_) => self.open(),
                        busy => busy,
                    }
                }
                Cmd::Disconnect => {
                    drop(other);
                    self.end(End::Stopped)
                }
                Cmd::Reconfigure { cfg, connect } => {
                    drop(other);
                    self.end(End::Reconfigured(cfg, connect))
                }
                Cmd::Publish(out) => {
                    not_connected(&self.store, self.cfg.conn, &out);
                    other
                }
                Cmd::Subscribe(_) | Cmd::Unsubscribe(_) => other, // applied on the next connect
            },
        };
    }

    fn step(&mut self) {
        let now = Instant::now();
        self.state = match std::mem::replace(&mut self.state, State::Off) {
            State::Off => State::Off,
            State::Retry(at) if now >= at => self.open(),
            State::Retry(at) => State::Retry(at),
            State::Opening(sock, deadline) => match sock.take() {
                Some(Note::Open) => match sock.send(&connect_packet(&self.cfg)) {
                    Ok(()) => State::Connecting(sock, Reader::default(), now + CONNECT_TIMEOUT),
                    Err(e) => self.end(End::Failed(e)),
                },
                Some(Note::Closed(why)) => {
                    let mut err = format!("couldn't open {}", self.url());
                    if let Some(why) = why {
                        err += &format!(": {why}");
                    }
                    self.end(End::Failed(err))
                }
                Some(Note::Data(_)) | None if now > deadline => {
                    self.end(End::Failed(format!("timed out opening {}", self.url())))
                }
                _ => State::Opening(sock, deadline),
            },
            State::Connecting(sock, mut rd, deadline) => {
                let closed = sock.drain(&mut rd);
                match rd.try_take() {
                    Some((kind, body)) => match connack(kind, &body) {
                        Ok((keep_alive, limits)) => self.up(sock, rd, keep_alive, limits),
                        Err(e) => self.end(End::Failed(e)),
                    },
                    None if closed.is_some() => self.end(End::Failed(lost(closed))),
                    None if now > deadline => self.end(End::Failed("timed out waiting for CONNACK".into())),
                    None => State::Connecting(sock, rd, deadline),
                }
            }
            State::Up(s) => self.run(s),
        };
    }

    /// The broker took the connection: subscribe.
    fn up(&mut self, sock: Socket, rd: Reader, keep_alive: u16, limits: Limits) -> State {
        set_status(&self.store, self.cfg.conn, ConnState::Connected, None);
        let now = Instant::now();
        let mut s = Box::new(Session {
            sock,
            rd,
            ids: Ids::default(),
            limits,
            keep_alive,
            last_send: now,
            last_recv: now,
        });
        for pkt in subscribe_all(&self.cfg, &self.store, &mut s.ids, &self.outbox) {
            if let Err(e) = s.sock.send(&pkt) {
                return self.end(End::Failed(e));
            }
        }
        State::Up(s)
    }

    /// A connected session's frame: what arrived, then keeping it alive.
    fn run(&mut self, mut s: Box<Session>) -> State {
        let closed = s.sock.drain(&mut s.rd);
        // What arrived before a close counts too.
        while let Some((kind, body)) = s.rd.try_take() {
            s.last_recv = Instant::now();
            let (cfg, store) = (&self.cfg, &self.store);
            match packet_in(kind, &body, cfg, store, &mut self.inbox, &mut self.outbox, &mut s.ids) {
                Ok(None) => {}
                Ok(Some(reply)) => {
                    if let Err(e) = s.sock.send(&reply) {
                        return self.end(End::Failed(e));
                    }
                    s.last_send = Instant::now();
                }
                Err(end) => return self.end(end),
            }
        }
        if closed.is_some() {
            return self.end(End::Failed(lost(closed)));
        }
        self.outbox.expire(&self.store);
        if s.last_send.elapsed() >= ping_every(s.keep_alive) {
            if let Err(e) = s.sock.send(&PINGREQ) {
                return self.end(End::Failed(e));
            }
            s.last_send = Instant::now();
        }
        if s.last_recv.elapsed() > silence_limit(s.keep_alive) {
            return self.end(End::Failed("keepalive timeout".into()));
        }
        State::Up(s)
    }
}

/// Why an open connection ended, from what its close said.
fn lost(closed: Option<Option<String>>) -> String {
    match closed.flatten() {
        Some(why) => format!("connection closed: {why}"),
        None => "connection closed by broker".into(),
    }
}

/// What the socket's callbacks leave for the next poll.
enum Note {
    Open,
    Data(Vec<u8>),
    /// Closed or failed, with the reason the close gave, if any.
    Closed(Option<String>),
}

type Notes = Rc<RefCell<VecDeque<Note>>>;

/// The page's WebSocket to a broker, speaking "mqtt".
struct Socket {
    ws: WebSocket,
    notes: Notes,
    _on_open: Closure<dyn FnMut(Event)>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
    _on_error: Closure<dyn FnMut(Event)>,
}

impl Socket {
    fn open(url: &str) -> Result<Socket, String> {
        let ws = WebSocket::new_with_str(url, "mqtt").map_err(|e| js_error(&e))?;
        ws.set_binary_type(BinaryType::Arraybuffer);
        let notes: Notes = Rc::default();
        let note = |n: &Notes, f: fn(&JsValue) -> Note| {
            let n = n.clone();
            move |e: JsValue| n.borrow_mut().push_back(f(&e))
        };
        let on_open = note(&notes, |_| Note::Open);
        let on_message = note(&notes, |e| {
            let data = e.unchecked_ref::<MessageEvent>().data();
            Note::Data(js_sys::Uint8Array::new(&data).to_vec())
        });
        let on_close = note(&notes, |e| {
            let e = e.unchecked_ref::<CloseEvent>();
            let why = e.reason();
            Note::Closed((!why.is_empty()).then(|| format!("{why} ({})", e.code())))
        });
        let on_error = note(&notes, |_| Note::Closed(None)); // the browser doesn't say why
        let s = Socket {
            _on_open: Closure::new(move |e: Event| on_open(e.into())),
            _on_message: Closure::new(move |e: MessageEvent| on_message(e.into())),
            _on_close: Closure::new(move |e: CloseEvent| on_close(e.into())),
            _on_error: Closure::new(move |e: Event| on_error(e.into())),
            ws,
            notes,
        };
        s.ws.set_onopen(Some(s._on_open.as_ref().unchecked_ref()));
        s.ws.set_onmessage(Some(s._on_message.as_ref().unchecked_ref()));
        s.ws.set_onclose(Some(s._on_close.as_ref().unchecked_ref()));
        s.ws.set_onerror(Some(s._on_error.as_ref().unchecked_ref()));
        Ok(s)
    }

    fn send(&self, b: &[u8]) -> Result<(), String> {
        self.ws.send_with_u8_array(b).map_err(|e| js_error(&e))
    }

    fn take(&self) -> Option<Note> {
        self.notes.borrow_mut().pop_front()
    }

    /// Moves what arrived into `rd`; `Some` if the socket closed (with the
    /// reason, if it gave one).
    fn drain(&self, rd: &mut Reader) -> Option<Option<String>> {
        while let Some(n) = self.take() {
            match n {
                Note::Data(b) => rd.push(&b),
                Note::Closed(why) => return Some(why),
                Note::Open => {}
            }
        }
        None
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // The callbacks go with this: unhook them first. What was sent
        // still goes before the close.
        self.ws.set_onopen(None);
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
        self.ws.set_onerror(None);
        let _ = self.ws.close();
    }
}

fn js_error(e: &JsValue) -> String {
    match e.dyn_ref::<js_sys::Error>() {
        Some(err) => String::from(err.message()),
        None => e.as_string().unwrap_or_else(|| format!("{e:?}")),
    }
}
