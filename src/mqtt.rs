//! A minimal MQTT v5 client: connect (TCP or TLS), subscribe, receive,
//! publish, keep alive, reconnect. One blocking thread per connection,
//! steered by [`Cmd`]s from the UI.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};

use crate::pemkey::{self, KeyError};
use crate::store::{ConnState, Props, Status, Store};

pub struct Config {
    pub conn: usize, // index into Store::conns
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub ca_file: Option<String>,
    pub cert_file: Option<String>,
    pub key_file: Option<String>,
    pub key_password: Option<String>, // for an encrypted key_file
    pub insecure: bool,
    pub client_id: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub qos: u8,
}

impl Config {
    fn update(&mut self, password: Option<String>, key_password: Option<String>) {
        if password.is_some() {
            self.password = password;
        }
        if key_password.is_some() {
            self.key_password = key_password;
        }
    }
}

/// What the UI can ask a connection to do. Which topics to subscribe on
/// connect is read from the connection's `subs` in the store.
pub enum Cmd {
    /// Passwords given here replace the ones the connection has.
    Connect {
        password: Option<String>,
        key_password: Option<String>,
    },
    Disconnect,
    Subscribe(String),
    Unsubscribe(String),
    /// New settings (the connections file changed): drop the current
    /// connection if any, then connect with them if `connect`.
    Reconfigure {
        cfg: Box<Config>,
        connect: bool,
    },
    /// Sends a message; the outcome is reported with `Store::notify`.
    Publish(Box<Outgoing>),
}

/// A message to publish.
#[derive(Clone, Debug, PartialEq)]
pub struct Outgoing {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: u8,
    pub retain: bool,
    pub props: Props, // subscription_id is never sent
}

/// Why `topic` can't be published to, if it can't: topic names (unlike
/// filters) have no wildcards.
pub fn topic_problem(topic: &str) -> Option<&'static str> {
    if topic.is_empty() {
        Some("the topic is empty")
    } else if topic.contains(['+', '#']) {
        Some("a topic to publish to can't contain + or #")
    } else if topic.contains('\0') {
        Some("the topic contains a NUL character")
    } else if topic.len() > u16::MAX as usize {
        Some("the topic is longer than 65535 bytes")
    } else {
        None
    }
}

const KEEP_ALIVE: u16 = 30;
const RETRY: Duration = Duration::from_secs(2);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a QoS 1 or 2 publish may wait for the broker's acknowledgement.
const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// Why TLS couldn't be set up. Key problems are told apart because retrying
/// won't help them: the connection stops (and the UI asks for the passphrase
/// when that's what's missing).
#[derive(Debug)]
pub enum TlsError {
    Key(String, KeyError), // the key file's path
    Other(String),
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsError::Key(path, e) => write!(f, "key {path}: {e}"),
            TlsError::Other(e) => f.write_str(e),
        }
    }
}

impl From<String> for TlsError {
    fn from(e: String) -> TlsError {
        TlsError::Other(e)
    }
}

/// Whether the key at `path` is encrypted and `passphrase` doesn't open it.
/// False for every other problem, where no passphrase helps: an unreadable
/// file (connecting retries) or an unsupported or damaged key (connecting
/// stops with `stopped · …`).
pub fn key_needs_passphrase(path: &str, passphrase: Option<&str>) -> bool {
    matches!(
        load_key(path, passphrase),
        Err(TlsError::Key(_, KeyError::NeedsPassphrase | KeyError::WrongPassphrase))
    )
}

/// Reads the private key at `path`, decrypting it with `passphrase` if needed.
pub fn load_key(path: &str, passphrase: Option<&str>) -> Result<PrivateKeyDer<'static>, TlsError> {
    let pem = std::fs::read(path).map_err(|e| TlsError::Other(format!("key {path}: {e}")))?;
    pemkey::load(&pem, passphrase).map_err(|e| TlsError::Key(path.into(), e))
}

pub fn tls_config(cfg: &Config) -> Result<Option<Arc<rustls::ClientConfig>>, TlsError> {
    if !cfg.tls {
        return Ok(None);
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;

    let builder = if cfg.insecure {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerify(provider)))
    } else {
        let mut roots = rustls::RootCertStore::empty();
        match &cfg.ca_file {
            Some(path) => {
                let certs: Vec<_> = CertificateDer::pem_file_iter(path)
                    .map_err(|e| format!("cafile {path}: {e}"))?
                    .collect::<Result<_, _>>()
                    .map_err(|e| format!("cafile {path}: {e}"))?;
                if certs.is_empty() {
                    return Err(format!("cafile {path}: no PEM certificates found").into());
                }
                for c in certs {
                    roots.add(c).map_err(|e| format!("cafile {path}: {e}"))?;
                }
            }
            None => {
                for c in rustls_native_certs::load_native_certs().certs {
                    let _ = roots.add(c);
                }
            }
        }
        builder.with_root_certificates(roots)
    };

    let config = match (&cfg.cert_file, &cfg.key_file) {
        (Some(cert), Some(key)) => {
            let chain: Vec<_> = CertificateDer::pem_file_iter(cert)
                .map_err(|e| format!("cert {cert}: {e}"))?
                .collect::<Result<_, _>>()
                .map_err(|e| format!("cert {cert}: {e}"))?;
            let key = load_key(key, cfg.key_password.as_deref())?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|e| format!("client certificate: {e}"))?
        }
        (None, None) => builder.with_no_client_auth(),
        _ => return Err(TlsError::Other("cert and key must be given together".into())),
    };
    Ok(Some(Arc::new(config)))
}

#[derive(Debug)]
struct NoVerify(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(msg, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        msg: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(msg, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Conn {
    fn tcp(&self) -> &TcpStream {
        match self {
            Conn::Plain(s) => s,
            Conn::Tls(s) => &s.sock,
        }
    }
}

impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(b),
            Conn::Tls(s) => s.read(b),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(b),
            Conn::Tls(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

/// How a session ended.
enum End {
    Failed(String),                  // retry after a pause
    Halted(String),                  // retrying won't help (the key's passphrase): wait for the UI
    Stopped,                         // the UI asked to disconnect
    Closed,                          // the UI is gone
    Reconfigured(Box<Config>, bool), // new settings; connect with them?
}

/// Starts the connection's thread. It connects now if `start`, then follows
/// commands; after a failure it retries every few seconds until told to stop.
pub fn spawn(mut cfg: Config, store: Arc<Mutex<Store>>, start: bool) -> Sender<Cmd> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut inbox = Inbox::default();
        let mut outbox = Outbox::default();
        let mut want = start;
        let mut halted = None;
        loop {
            if !want {
                set_status(&store, cfg.conn, ConnState::Disconnected, halted.take());
                match rx.recv() {
                    Ok(Cmd::Connect { password, key_password }) => {
                        cfg.update(password, key_password);
                        want = true;
                    }
                    Ok(Cmd::Reconfigure { cfg: c, connect }) => {
                        cfg = *c;
                        want = connect;
                        continue;
                    }
                    Ok(Cmd::Publish(out)) => {
                        not_connected(&store, cfg.conn, &out);
                        continue;
                    }
                    Ok(_) => continue, // subscription changes apply on the next connect
                    Err(_) => return,
                }
            }
            set_status(&store, cfg.conn, ConnState::Connecting, None);
            let end = session(&cfg, &store, &rx, &mut inbox, &mut outbox);
            outbox.abandon(&store);
            match end {
                End::Stopped => want = false,
                End::Halted(err) => {
                    halted = Some(err);
                    want = false;
                }
                End::Closed => return,
                End::Reconfigured(c, connect) => {
                    cfg = *c;
                    want = connect;
                }
                End::Failed(err) => {
                    set_status(&store, cfg.conn, ConnState::Connecting, Some(err));
                    // Pause before retrying, but stay responsive.
                    let until = Instant::now() + RETRY;
                    loop {
                        match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                            Ok(Cmd::Disconnect) => {
                                want = false;
                                break;
                            }
                            Ok(Cmd::Connect { password, key_password }) => {
                                cfg.update(password, key_password);
                                break;
                            }
                            Ok(Cmd::Reconfigure { cfg: c, connect }) => {
                                cfg = *c;
                                want = connect;
                                break;
                            }
                            Ok(Cmd::Publish(out)) => not_connected(&store, cfg.conn, &out),
                            Ok(_) => {}
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                }
            }
        }
    });
    tx
}

fn set_status(store: &Mutex<Store>, conn: usize, state: ConnState, err: Option<String>) {
    store.lock().unwrap().conns[conn].status = Status { state, err };
}

/// "oslo · site/x": where a publish went, for notices.
fn target(store: &Mutex<Store>, conn: usize, topic: &str) -> String {
    format!("{} · {topic}", store.lock().unwrap().conns[conn].name)
}

fn not_connected(store: &Mutex<Store>, conn: usize, out: &Outgoing) {
    let mut st = store.lock().unwrap();
    let msg = format!(
        "not published to {}: {} isn't connected",
        out.topic, st.conns[conn].name
    );
    st.notify(msg, true);
}

fn set_sub_err(store: &Mutex<Store>, conn: usize, topic: &str, err: Option<String>) {
    let mut st = store.lock().unwrap();
    if let Some(s) = st.conns[conn].subs.iter_mut().find(|s| s.topic == topic) {
        s.err = err;
    }
}

fn dial(host: &str, port: u16) -> Result<TcpStream, String> {
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("resolve {host}: {e}"))?;
    let mut last = format!("resolve {host}: no addresses");
    for a in addrs {
        match TcpStream::connect_timeout(&a, CONNECT_TIMEOUT) {
            Ok(s) => return Ok(s),
            Err(e) => last = format!("connect {host}:{port}: {e}"),
        }
    }
    Err(last)
}

/// What happens to each message that arrives, with whatever state that needs
/// across messages and reconnects (one per connection thread).
#[derive(Default)]
struct Inbox {}

impl Inbox {
    #[allow(clippy::too_many_arguments)]
    fn receive(
        &mut self,
        store: &Mutex<Store>,
        conn: usize,
        topic: &str,
        payload: &[u8],
        qos: u8,
        retain: bool,
        props: Props,
    ) {
        // Decode binary formats now, while the whole payload is here
        // (and outside the lock).
        let decoded = crate::pretty::decode_on_arrival(props.content_type.as_deref(), payload);
        store
            .lock()
            .unwrap()
            .add_decoded(conn, topic, payload, qos, retain, props, decoded);
    }
}

/// What the broker accepts, from its CONNACK.
struct Limits {
    max_packet: Option<u32>,
    max_qos: u8,
    retain: bool,
    /// How many QoS 1 and 2 publishes may wait for its acknowledgement at
    /// once (Receive Maximum).
    receive_max: u16,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_packet: None,
            max_qos: 2,
            retain: true,
            receive_max: u16::MAX,
        }
    }
}

/// Publishes waiting for the broker's acknowledgement (QoS 1 and 2).
#[derive(Default)]
struct Outbox {
    waiting: Vec<Waiting>,
}

struct Waiting {
    pid: u16,
    what: String, // "oslo · site/x", for the notice
    qos: u8,
    released: bool,       // QoS 2: PUBREC came and PUBREL went; PUBCOMP is next
    no_subscribers: bool, // the broker said nobody subscribes to it
    since: Instant,
}

impl Outbox {
    /// Handles a PUBACK (4), PUBREC (5) or PUBCOMP (7); a PUBREC returns the
    /// PUBREL to send.
    fn ack(&mut self, store: &Mutex<Store>, kind: u8, body: &[u8]) -> Option<[u8; 4]> {
        let pid = u16::from_be_bytes([body[0], body[1]]);
        let code = body.get(2).copied().unwrap_or(0);
        let i = self.waiting.iter().position(|w| {
            w.pid == pid
                && match kind {
                    4 => w.qos == 1,
                    5 => w.qos == 2 && !w.released,
                    _ => w.qos == 2 && w.released,
                }
        })?;
        let w = &mut self.waiting[i];
        w.no_subscribers |= code == 0x10;
        if kind == 5 && code < 0x80 {
            w.released = true;
            w.since = Instant::now();
            return Some([0x62, 2, body[0], body[1]]);
        }
        let w = self.waiting.remove(i);
        let msg = if code >= 0x80 {
            Err(format!("broker refused {}: {}", w.what, pub_reason(code)))
        } else if w.qos == 1 {
            Ok(format!("published to {} (QoS 1, acknowledged)", w.what))
        } else {
            Ok(format!("published to {} (QoS 2, completed)", w.what))
        };
        let mut st = store.lock().unwrap();
        match msg {
            Ok(m) if w.no_subscribers => st.notify(m + "; no one subscribes to it", false),
            Ok(m) => st.notify(m, false),
            Err(m) => st.notify(m, true),
        }
        None
    }

    /// Gives up on publishes the broker hasn't acknowledged in time.
    fn expire(&mut self, store: &Mutex<Store>) {
        while let Some(i) = self.waiting.iter().position(|w| w.since.elapsed() > ACK_TIMEOUT) {
            let w = self.waiting.remove(i);
            let secs = ACK_TIMEOUT.as_secs();
            let msg = format!("{} not confirmed: no acknowledgement within {secs} s", w.what);
            store.lock().unwrap().notify(msg, true);
        }
    }

    /// After a session: what wasn't acknowledged may not have arrived.
    fn abandon(&mut self, store: &Mutex<Store>) {
        for w in self.waiting.drain(..) {
            let msg = format!(
                "{} not confirmed: the connection closed before the broker acknowledged it",
                w.what
            );
            store.lock().unwrap().notify(msg, true);
        }
    }
}

/// Packet ids. A SUBSCRIBE, UNSUBSCRIBE or QoS 1/2 publish keeps its id
/// until the broker answers it, and no other packet may use that id in the
/// meantime, also once the counter has gone round.
#[derive(Default)]
struct Ids {
    last: u16,
    subscribes: Vec<(u16, String)>, // waiting for SUBACK, with their filter
    unsubscribes: Vec<u16>,         // waiting for UNSUBACK
}

impl Ids {
    /// An id nothing is waiting on; None if all 65535 are taken.
    fn next(&mut self, outbox: &Outbox) -> Option<u16> {
        let (subs, unsubs) = (&self.subscribes, &self.unsubscribes);
        free_pid(&mut self.last, |p| {
            subs.iter().any(|(q, _)| *q == p) || unsubs.contains(&p) || outbox.waiting.iter().any(|w| w.pid == p)
        })
    }
}

/// Moves `last` on to the next id (1 to 65535, then round again) that isn't
/// `in_use`.
fn free_pid(last: &mut u16, in_use: impl Fn(u16) -> bool) -> Option<u16> {
    for _ in 0..u16::MAX {
        *last = last.wrapping_add(1).max(1);
        if !in_use(*last) {
            return Some(*last);
        }
    }
    None
}

const NO_FREE_ID: &str = "all 65535 packet ids are waiting for the broker's answer";

/// The packet id to publish `o` with (0 for QoS 0), or why it can't go now:
/// a broker takes at most its Receive Maximum of QoS 1 and 2 publishes it
/// hasn't acknowledged yet.
fn publish_id(o: &Outgoing, ids: &mut Ids, outbox: &Outbox, lim: &Limits) -> Result<u16, String> {
    if o.qos == 0 {
        return Ok(0);
    }
    let n = outbox.waiting.len();
    if n >= lim.receive_max as usize {
        let earlier = if n == 1 { "message" } else { "messages" };
        return Err(format!(
            "the broker has yet to acknowledge {n} earlier {earlier}, as many as it accepts at once \
             (its Receive Maximum); try again once it does"
        ));
    }
    ids.next(outbox).ok_or_else(|| NO_FREE_ID.to_string())
}

/// One connection's lifetime.
fn session(cfg: &Config, store: &Mutex<Store>, rx: &Receiver<Cmd>, inbox: &mut Inbox, outbox: &mut Outbox) -> End {
    let tls = match tls_config(cfg) {
        Ok(t) => t,
        Err(e @ TlsError::Key(..)) => return End::Halted(e.to_string()),
        Err(e) => return End::Failed(e.to_string()),
    };
    let tcp = match dial(&cfg.host, cfg.port) {
        Ok(s) => s,
        Err(e) => return End::Failed(e),
    };
    let _ = tcp.set_nodelay(true);
    let mut conn = match tls {
        None => Conn::Plain(tcp),
        Some(tc) => {
            let name = match ServerName::try_from(cfg.host.clone()) {
                Ok(n) => n,
                Err(e) => return End::Failed(format!("tls: {e}")),
            };
            match rustls::ClientConnection::new(tc, name) {
                Ok(c) => Conn::Tls(Box::new(rustls::StreamOwned::new(c, tcp))),
                Err(e) => return End::Failed(format!("tls: {e}")),
            }
        }
    };
    let _ = conn.tcp().set_read_timeout(Some(CONNECT_TIMEOUT));

    if let Err(e) = conn.write_all(&connect_packet(cfg)).and_then(|_| conn.flush()) {
        return End::Failed(tidy_io(e));
    }

    let mut rd = Reader::default();
    let (kind, body) = match rd.next(&mut conn) {
        Ok(Some(p)) => p,
        Ok(None) => return End::Failed("timed out waiting for CONNACK".into()),
        Err(e) => return End::Failed(tidy_io(e)),
    };
    if kind >> 4 != 2 || body.len() < 2 {
        return End::Failed("unexpected reply to CONNECT".into());
    }
    let mut keep_alive = KEEP_ALIVE;
    let mut reason_str = None;
    let mut limits = Limits::default();
    let mut b = Buf(&body[2..]);
    if let Some(props) = b.props() {
        for (id, v) in props {
            match (id, v) {
                (0x13, Prop::Int(k)) => keep_alive = k as u16,
                (0x1F, Prop::Str(s)) => reason_str = Some(s),
                (0x21, Prop::Int(n)) if n > 0 => limits.receive_max = n as u16, // 0 isn't allowed
                (0x24, Prop::Int(q)) => limits.max_qos = q.min(2) as u8,
                (0x25, Prop::Int(r)) => limits.retain = r != 0,
                (0x27, Prop::Int(n)) => limits.max_packet = Some(n),
                _ => {}
            }
        }
    }
    if body[1] != 0 {
        let mut msg = format!("broker refused connection: {}", connack_reason(body[1]));
        if let Some(r) = reason_str {
            msg += &format!(": {r}");
        }
        return End::Failed(msg);
    }
    set_status(store, cfg.conn, ConnState::Connected, None);

    let mut ids = Ids::default();
    // One SUBSCRIBE per filter, so a refused one is named and the rest work.
    let topics: Vec<String> = {
        let mut st = store.lock().unwrap();
        let c = &mut st.conns[cfg.conn];
        c.subs.iter_mut().for_each(|s| s.err = None);
        c.subs.iter().filter(|s| s.enabled).map(|s| s.topic.clone()).collect()
    };
    for t in topics {
        let Some(p) = ids.next(outbox) else {
            set_sub_err(store, cfg.conn, &t, Some(NO_FREE_ID.into()));
            continue;
        };
        if let Err(e) = conn.write_all(&subscribe_packet(p, &t, cfg.qos)) {
            return End::Failed(tidy_io(e));
        }
        ids.subscribes.push((p, t));
    }
    let _ = conn.flush();

    let _ = conn.tcp().set_read_timeout(Some(Duration::from_millis(100)));
    let ping_every = Duration::from_secs((keep_alive.max(2) as u64) * 3 / 4);
    let mut last_send = Instant::now();
    let mut last_recv = Instant::now();

    let mut sent: Option<String> = None; // a QoS 0 publish, reported once written
    loop {
        // Commands from the UI.
        loop {
            let cmd = match rx.try_recv() {
                Ok(c) => c,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return End::Closed,
            };
            let out = match cmd {
                Cmd::Disconnect => {
                    let _ = conn.write_all(&[0xE0, 0]).and_then(|_| conn.flush());
                    return End::Stopped;
                }
                Cmd::Reconfigure { cfg, connect } => {
                    let _ = conn.write_all(&[0xE0, 0]).and_then(|_| conn.flush());
                    return End::Reconfigured(cfg, connect);
                }
                Cmd::Connect { .. } => continue,
                Cmd::Publish(o) => {
                    let what = target(store, cfg.conn, &o.topic);
                    let ready = publish_id(&o, &mut ids, outbox, &limits)
                        .and_then(|p| publish_packet(&o, p, &limits).map(|pkt| (p, pkt)));
                    let (p, pkt) = match ready {
                        Ok(r) => r,
                        Err(e) => {
                            store
                                .lock()
                                .unwrap()
                                .notify(format!("not published to {what}: {e}"), true);
                            continue;
                        }
                    };
                    if o.qos == 0 {
                        sent = Some(format!("published to {what} (QoS 0)"));
                    } else {
                        outbox.waiting.push(Waiting {
                            pid: p,
                            what,
                            qos: o.qos,
                            released: false,
                            no_subscribers: false,
                            since: Instant::now(),
                        });
                    }
                    pkt
                }
                Cmd::Subscribe(t) => {
                    let Some(p) = ids.next(outbox) else {
                        set_sub_err(store, cfg.conn, &t, Some(NO_FREE_ID.into()));
                        continue;
                    };
                    let pkt = subscribe_packet(p, &t, cfg.qos);
                    ids.subscribes.push((p, t));
                    pkt
                }
                Cmd::Unsubscribe(t) => {
                    let Some(p) = ids.next(outbox) else {
                        store
                            .lock()
                            .unwrap()
                            .notify(format!("{t} not unsubscribed: {NO_FREE_ID}"), true);
                        continue;
                    };
                    ids.unsubscribes.push(p);
                    unsubscribe_packet(p, &t)
                }
            };
            if let Err(e) = conn.write_all(&out).and_then(|_| conn.flush()) {
                return End::Failed(tidy_io(e));
            }
            last_send = Instant::now();
            if let Some(m) = sent.take() {
                store.lock().unwrap().notify(m, false); // QoS 0: written is all we learn
            }
        }
        outbox.expire(store);

        let pkt = match rd.next(&mut conn) {
            Ok(p) => p,
            Err(e) => return End::Failed(tidy_io(e)),
        };
        if let Some((kind, body)) = pkt {
            last_recv = Instant::now();
            let mut reply: Option<[u8; 4]> = None;
            match kind >> 4 {
                3 => {
                    let qos = (kind >> 1) & 3;
                    let retain = kind & 1 == 1;
                    let Some((topic, pid, props, payload)) = parse_publish(&body, qos) else {
                        return End::Failed("malformed PUBLISH".into());
                    };
                    inbox.receive(store, cfg.conn, &topic, payload, qos, retain, props);
                    match qos {
                        1 => reply = Some([0x40, 2, (pid >> 8) as u8, pid as u8]),
                        2 => reply = Some([0x50, 2, (pid >> 8) as u8, pid as u8]),
                        _ => {}
                    }
                }
                4 | 5 | 7 if body.len() >= 2 => reply = outbox.ack(store, kind >> 4, &body),
                6 if body.len() >= 2 => reply = Some([0x70, 2, body[0], body[1]]), // PUBREL → PUBCOMP
                9 if body.len() >= 2 => {
                    // SUBACK
                    let p = u16::from_be_bytes([body[0], body[1]]);
                    let mut b = Buf(&body[2..]);
                    let _ = b.props();
                    let code = b.0.first().copied().unwrap_or(0x80);
                    if let Some(i) = ids.subscribes.iter().position(|(q, _)| *q == p) {
                        let (_, t) = ids.subscribes.remove(i);
                        let err = (code >= 0x80).then(|| suback_reason(code));
                        set_sub_err(store, cfg.conn, &t, err);
                    }
                }
                11 if body.len() >= 2 => {
                    // UNSUBACK: its id is free again.
                    let p = u16::from_be_bytes([body[0], body[1]]);
                    ids.unsubscribes.retain(|&q| q != p);
                }
                14 => {
                    let code = body.first().copied().unwrap_or(0);
                    return End::Failed(format!("broker disconnected: {}", connack_reason(code)));
                }
                _ => {}
            }
            if let Some(r) = reply {
                if let Err(e) = conn.write_all(&r) {
                    return End::Failed(tidy_io(e));
                }
                last_send = Instant::now();
            }
            if rd.buffered() {
                continue; // drain what we already have before flushing
            }
            let _ = conn.flush();
        }
        if last_send.elapsed() >= ping_every {
            if let Err(e) = conn.write_all(&[0xC0, 0]).and_then(|_| conn.flush()) {
                return End::Failed(tidy_io(e));
            }
            last_send = Instant::now();
        }
        if last_recv.elapsed() > Duration::from_secs(keep_alive.max(2) as u64 * 2) {
            return End::Failed("keepalive timeout".into());
        }
    }
}

fn tidy_io(e: io::Error) -> String {
    match e.kind() {
        io::ErrorKind::UnexpectedEof => "connection closed by broker".into(),
        _ => e.to_string(),
    }
}

/// Accumulates bytes and yields complete packets; a read timeout yields None.
#[derive(Default)]
struct Reader {
    buf: Vec<u8>,
    pos: usize,
}

impl Reader {
    fn buffered(&self) -> bool {
        self.pos < self.buf.len()
    }

    fn try_take(&mut self) -> Option<(u8, Vec<u8>)> {
        let b = &self.buf[self.pos..];
        if b.len() < 2 {
            return None;
        }
        let (mut len, mut mult, mut i) = (0usize, 1usize, 1);
        loop {
            let c = *b.get(i)?;
            len += (c & 0x7f) as usize * mult;
            i += 1;
            if c & 0x80 == 0 {
                break;
            }
            mult *= 128;
            if i > 4 {
                return None;
            }
        }
        if b.len() < i + len {
            return None;
        }
        let pkt = (b[0], b[i..i + len].to_vec());
        self.pos += i + len;
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
            // A large packet grew the buffer; give that memory back.
            if self.buf.capacity() > 1 << 20 {
                self.buf = Vec::new();
            }
        }
        Some(pkt)
    }

    fn next(&mut self, r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
        loop {
            if let Some(p) = self.try_take() {
                return Ok(Some(p));
            }
            if self.pos > 0 {
                self.buf.drain(..self.pos);
                self.pos = 0;
            }
            let mut tmp = [0u8; 16 * 1024];
            match r.read(&mut tmp) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => self.buf.extend_from_slice(&tmp[..n]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

// ── Encoding ───────────────────────────────────────────────────────────────

fn put_varint(out: &mut Vec<u8>, mut n: usize) {
    loop {
        let mut b = (n % 128) as u8;
        n /= 128;
        if n > 0 {
            b |= 0x80;
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
}

fn put_str(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s);
}

fn packet(header: u8, body: Vec<u8>) -> Vec<u8> {
    let mut out = vec![header];
    put_varint(&mut out, body.len());
    out.extend(body);
    out
}

fn connect_packet(cfg: &Config) -> Vec<u8> {
    let mut b = Vec::new();
    put_str(&mut b, b"MQTT");
    b.push(5);
    let mut flags = 0x02; // clean start
    if cfg.username.is_some() {
        flags |= 0x80;
    }
    if cfg.password.is_some() {
        flags |= 0x40;
    }
    b.push(flags);
    b.extend_from_slice(&KEEP_ALIVE.to_be_bytes());
    b.push(0); // no properties
    put_str(&mut b, cfg.client_id.as_bytes());
    if let Some(u) = &cfg.username {
        put_str(&mut b, u.as_bytes());
    }
    if let Some(p) = &cfg.password {
        put_str(&mut b, p.as_bytes());
    }
    packet(0x10, b)
}

fn unsubscribe_packet(pid: u16, topic: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&pid.to_be_bytes());
    b.push(0); // no properties
    put_str(&mut b, topic.as_bytes());
    packet(0xA2, b)
}

/// The PUBLISH packet for `o` (with packet id `pid` for QoS 1 and 2), or why
/// it can't be sent to a broker with these limits.
fn publish_packet(o: &Outgoing, pid: u16, lim: &Limits) -> Result<Vec<u8>, String> {
    if let Some(p) = topic_problem(&o.topic) {
        return Err(p.into());
    }
    if o.qos > 2 {
        return Err("QoS must be 0, 1 or 2".into());
    }
    if o.qos > lim.max_qos {
        return Err(format!("the broker accepts QoS {} at most", lim.max_qos));
    }
    if o.retain && !lim.retain {
        return Err("the broker doesn't keep retained messages".into());
    }
    let p = &o.props;
    let mut lengths = [p.content_type.as_deref(), p.response_topic.as_deref()]
        .into_iter()
        .flatten()
        .map(str::len)
        .chain(p.correlation_data.as_ref().map(Vec::len))
        .chain(p.user.iter().flat_map(|(k, v)| [k.len(), v.len()]));
    if lengths.any(|n| n > u16::MAX as usize) {
        return Err("a property is longer than 65535 bytes".into());
    }
    let mut b = Vec::with_capacity(o.topic.len() + o.payload.len() + 64);
    put_str(&mut b, o.topic.as_bytes());
    if o.qos > 0 {
        b.extend_from_slice(&pid.to_be_bytes());
    }
    let mut props = Vec::new();
    if let Some(v) = p.payload_format {
        props.extend([0x01, v]);
    }
    if let Some(v) = p.message_expiry {
        props.push(0x02);
        props.extend_from_slice(&v.to_be_bytes());
    }
    if let Some(v) = &p.content_type {
        props.push(0x03);
        put_str(&mut props, v.as_bytes());
    }
    if let Some(v) = &p.response_topic {
        props.push(0x08);
        put_str(&mut props, v.as_bytes());
    }
    if let Some(v) = &p.correlation_data {
        props.push(0x09);
        put_str(&mut props, v);
    }
    for (k, v) in &p.user {
        props.push(0x26);
        put_str(&mut props, k.as_bytes());
        put_str(&mut props, v.as_bytes());
    }
    put_varint(&mut b, props.len());
    b.extend(props);
    b.extend_from_slice(&o.payload);
    // The remaining length is a varint of at most four bytes.
    if b.len() > 268_435_455 {
        return Err("the message is larger than MQTT allows (256 MB)".into());
    }
    let pkt = packet(0x30 | o.qos << 1 | o.retain as u8, b);
    match lim.max_packet {
        Some(max) if pkt.len() > max as usize => Err(format!(
            "it takes {} on the wire and the broker accepts at most {}",
            crate::fmt::human_bytes(pkt.len()),
            crate::fmt::human_bytes(max as usize)
        )),
        _ => Ok(pkt),
    }
}

fn subscribe_packet(pid: u16, topic: &str, qos: u8) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&pid.to_be_bytes());
    b.push(0); // no properties
    put_str(&mut b, topic.as_bytes());
    b.push(qos);
    packet(0x82, b)
}

// ── Decoding ───────────────────────────────────────────────────────────────

struct Buf<'a>(&'a [u8]);

enum Prop {
    Int(u32),
    Str(String),
    Bin(Vec<u8>),
    Pair(String, String),
}

impl<'a> Buf<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn varint(&mut self) -> Option<usize> {
        let (mut n, mut mult) = (0usize, 1usize);
        for _ in 0..4 {
            let c = self.u8()?;
            n += (c & 0x7f) as usize * mult;
            if c & 0x80 == 0 {
                return Some(n);
            }
            mult *= 128;
        }
        None
    }
    fn bin(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()? as usize;
        self.take(n)
    }
    fn str(&mut self) -> Option<String> {
        Some(String::from_utf8_lossy(self.bin()?).into_owned())
    }

    fn props(&mut self) -> Option<Vec<(u8, Prop)>> {
        let n = self.varint()?;
        let mut b = Buf(self.take(n)?);
        let mut out = Vec::new();
        while !b.0.is_empty() {
            let id = b.u8()?;
            let v = match id {
                0x01 | 0x17 | 0x19 | 0x24 | 0x25 | 0x28 | 0x29 | 0x2A => Prop::Int(b.u8()? as u32),
                0x13 | 0x21 | 0x22 | 0x23 => Prop::Int(b.u16()? as u32),
                0x02 | 0x11 | 0x18 | 0x27 => Prop::Int(b.u32()?),
                0x0B => Prop::Int(b.varint()? as u32),
                0x03 | 0x08 | 0x12 | 0x15 | 0x1A | 0x1C | 0x1F => Prop::Str(b.str()?),
                0x09 | 0x16 => Prop::Bin(b.bin()?.to_vec()),
                0x26 => Prop::Pair(b.str()?, b.str()?),
                _ => return None,
            };
            out.push((id, v));
        }
        Some(out)
    }
}

fn parse_publish(body: &[u8], qos: u8) -> Option<(String, u16, Props, &[u8])> {
    let mut b = Buf(body);
    let topic = b.str()?;
    let pid = if qos > 0 { b.u16()? } else { 0 };
    let mut p = Props::default();
    for (id, v) in b.props()? {
        match (id, v) {
            (0x01, Prop::Int(v)) => p.payload_format = Some(v as u8),
            (0x02, Prop::Int(v)) => p.message_expiry = Some(v),
            (0x03, Prop::Str(v)) => p.content_type = Some(v),
            (0x08, Prop::Str(v)) => p.response_topic = Some(v),
            (0x09, Prop::Bin(v)) => p.correlation_data = Some(v),
            (0x0B, Prop::Int(v)) => p.subscription_id = Some(v),
            (0x26, Prop::Pair(k, v)) => p.user.push((k, v)),
            _ => {}
        }
    }
    Some((topic, pid, p, b.0))
}

fn connack_reason(code: u8) -> String {
    match code {
        0x80 => "unspecified error",
        0x81 => "malformed packet",
        0x82 => "protocol error",
        0x84 => "unsupported protocol version",
        0x85 => "client id not valid",
        0x86 => "bad username or password",
        0x87 => "not authorized",
        0x88 => "server unavailable",
        0x89 => "server busy",
        0x8A => "banned",
        0x8B => "server shutting down",
        0x8C => "bad authentication method",
        0x8D => "keepalive timeout",
        0x8E => "session taken over",
        0x90 => "topic name invalid",
        0x93 => "receive maximum exceeded",
        0x95 => "packet too large",
        0x97 => "quota exceeded",
        0x99 => "payload format invalid",
        0x9A => "retain not supported",
        0x9B => "QoS not supported",
        0x9F => "connection rate exceeded",
        _ => return format!("reason code {code}"),
    }
    .into()
}

/// Why a broker refused a PUBLISH (PUBACK, PUBREC or PUBCOMP reason codes).
fn pub_reason(code: u8) -> String {
    match code {
        0x80 => "unspecified error",
        0x83 => "implementation specific error",
        0x87 => "not authorized",
        0x90 => "topic name invalid",
        0x91 => "packet id in use",
        0x92 => "packet id not found",
        0x97 => "quota exceeded",
        0x99 => "payload format invalid",
        _ => return format!("reason code {code}"),
    }
    .into()
}

fn suback_reason(code: u8) -> String {
    match code {
        0x80 => "unspecified error",
        0x83 => "implementation specific error",
        0x87 => "not authorized",
        0x8F => "topic filter invalid",
        0x91 => "packet id in use",
        0x97 => "quota exceeded",
        0x9E => "shared subscriptions not supported",
        0xA1 => "subscription ids not supported",
        0xA2 => "wildcards not supported",
        _ => return format!("reason code {code}"),
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_roundtrip() {
        // topic "a/b", qos1 pid 7, props: content-type "t", user ("k","v"), payload "hi"
        let mut body = Vec::new();
        put_str(&mut body, b"a/b");
        body.extend_from_slice(&7u16.to_be_bytes());
        let mut props = vec![0x03];
        put_str(&mut props, b"t");
        props.push(0x26);
        put_str(&mut props, b"k");
        put_str(&mut props, b"v");
        put_varint(&mut body, props.len());
        body.extend(props);
        body.extend_from_slice(b"hi");

        let (topic, pid, p, payload) = parse_publish(&body, 1).unwrap();
        assert_eq!((topic.as_str(), pid, payload), ("a/b", 7, &b"hi"[..]));
        assert_eq!(p.content_type.as_deref(), Some("t"));
        assert_eq!(p.user, vec![("k".to_string(), "v".to_string())]);
    }

    fn outgoing() -> Outgoing {
        Outgoing {
            topic: "site/oslo/t".into(),
            payload: br#"{"t":1}"#.to_vec(),
            qos: 2,
            retain: true,
            props: Props {
                content_type: Some("application/json".into()),
                response_topic: Some("site/reply".into()),
                correlation_data: Some(vec![0, 1, 0xff]),
                payload_format: Some(1),
                message_expiry: Some(60),
                subscription_id: None,
                user: vec![("k".into(), "v".into()), ("k".into(), "w".into())],
            },
        }
    }

    #[test]
    fn publish_packet_roundtrip() {
        let o = outgoing();
        let pkt = publish_packet(&o, 513, &Limits::default()).unwrap();
        assert_eq!(pkt[0], 0x30 | 2 << 1 | 1);
        let (kind, body) = Reader::default().next(&mut &pkt[..]).unwrap().unwrap();
        let (topic, pid, props, payload) = parse_publish(&body, (kind >> 1) & 3).unwrap();
        assert_eq!((topic.as_str(), pid, payload), ("site/oslo/t", 513, &o.payload[..]));
        assert_eq!(props, o.props);
        // QoS 0 has no packet id.
        let q0 = Outgoing {
            qos: 0,
            retain: false,
            props: Props::default(),
            ..outgoing()
        };
        let pkt = publish_packet(&q0, 9, &Limits::default()).unwrap();
        let (kind, body) = Reader::default().next(&mut &pkt[..]).unwrap().unwrap();
        assert_eq!(kind, 0x30);
        let (_, pid, props, payload) = parse_publish(&body, 0).unwrap();
        assert_eq!((pid, payload), (0, &o.payload[..]));
        assert_eq!(props, Props::default());
    }

    #[test]
    fn publish_respects_limits() {
        let o = Outgoing {
            payload: vec![0; 100],
            props: Props::default(),
            ..outgoing()
        };
        let tight = Limits {
            max_packet: Some(64),
            max_qos: 1,
            retain: false,
            ..Limits::default()
        };
        assert!(publish_packet(&o, 1, &tight).unwrap_err().contains("QoS 1 at most"));
        let o = Outgoing { qos: 1, ..o };
        assert!(publish_packet(&o, 1, &tight).unwrap_err().contains("retained"));
        let o = Outgoing { retain: false, ..o };
        let e = publish_packet(&o, 1, &tight).unwrap_err();
        assert!(e.contains("at most 64 B"), "{e}");
        let small = Outgoing {
            payload: vec![0; 10],
            ..o.clone()
        };
        assert!(publish_packet(&small, 1, &tight).is_ok());
        let long = Outgoing {
            props: Props {
                content_type: Some("x".repeat(70_000)),
                ..Props::default()
            },
            ..o
        };
        assert!(
            publish_packet(&long, 1, &Limits::default())
                .unwrap_err()
                .contains("65535")
        );
        for (t, bad) in [
            ("", true),
            ("a/+/b", true),
            ("a/#", true),
            ("a\0b", true),
            ("a/b c/ü", false),
        ] {
            assert_eq!(topic_problem(t).is_some(), bad, "{t:?}");
        }
    }

    #[test]
    fn acks_complete_publishes() {
        let store = Mutex::new(Store::new(10, 1024));
        let mut ob = Outbox::default();
        let wait = |pid: u16, qos| Waiting {
            pid,
            what: format!("c · t{pid}"),
            qos,
            released: false,
            no_subscribers: false,
            since: Instant::now(),
        };
        ob.waiting.extend([wait(1, 1), wait(2, 2), wait(3, 1), wait(4, 2)]);
        // QoS 1: PUBACK (here: nobody subscribes).
        assert_eq!(ob.ack(&store, 4, &[0, 1, 0x10]), None);
        // QoS 2: PUBREC is answered with PUBREL; PUBCOMP completes it.
        assert_eq!(ob.ack(&store, 5, &[0, 2]), Some([0x62, 2, 0, 2]));
        assert_eq!(ob.ack(&store, 4, &[0, 2]), None, "a PUBACK doesn't finish QoS 2");
        assert_eq!(ob.waiting.len(), 3);
        assert_eq!(ob.ack(&store, 7, &[0, 2, 0]), None);
        // Refusals; no PUBREL after a refused PUBREC.
        assert_eq!(ob.ack(&store, 4, &[0, 3, 0x87]), None);
        assert_eq!(ob.ack(&store, 5, &[0, 4, 0x97]), None);
        assert!(ob.waiting.is_empty());
        let notices = store.lock().unwrap().take_notices();
        let got: Vec<(&str, bool)> = notices.iter().map(|(t, e)| (t.as_str(), *e)).collect();
        assert_eq!(
            got,
            [
                (
                    "published to c · t1 (QoS 1, acknowledged); no one subscribes to it",
                    false
                ),
                ("published to c · t2 (QoS 2, completed)", false),
                ("broker refused c · t3: not authorized", true),
                ("broker refused c · t4: quota exceeded", true),
            ]
        );
        // Unconfirmed when the connection goes, or when the broker is silent.
        ob.waiting.push(wait(5, 1));
        ob.abandon(&store);
        let mut old = wait(6, 2);
        old.since = Instant::now() - ACK_TIMEOUT - Duration::from_secs(1);
        ob.waiting.push(old);
        ob.expire(&store);
        let n = store.lock().unwrap().take_notices();
        assert!(
            n[0].1 && n[0].0.contains("t5 not confirmed: the connection closed"),
            "{n:?}"
        );
        assert!(
            n[1].1 && n[1].0.contains("t6 not confirmed: no acknowledgement within 10 s"),
            "{n:?}"
        );
    }

    /// A store with one connection, `name`, and its settings for a broker on
    /// this machine's `port`.
    fn local(name: &str, port: u16) -> (Arc<Mutex<Store>>, Config) {
        let mut st = Store::new(10, 1024);
        st.add_conn(name, "test", None, vec![]);
        let cfg = Config {
            conn: 0,
            host: "127.0.0.1".into(),
            port,
            tls: false,
            ca_file: None,
            cert_file: None,
            key_file: None,
            key_password: None,
            insecure: false,
            client_id: "unit".into(),
            username: None,
            password: None,
            qos: 0,
        };
        (Arc::new(Mutex::new(st)), cfg)
    }

    /// The next status-line notice from the connection's thread.
    fn next_notice(store: &Mutex<Store>) -> (String, bool) {
        for _ in 0..500 {
            if let Some(n) = store.lock().unwrap().take_notices().into_iter().next() {
                return n;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("no notice");
    }

    fn waiting(pid: u16) -> Waiting {
        Waiting {
            pid,
            what: String::new(),
            qos: 1,
            released: false,
            no_subscribers: false,
            since: Instant::now(),
        }
    }

    #[test]
    fn publish_while_disconnected_says_so() {
        let (store, cfg) = local("lab", 1);
        let tx = spawn(cfg, store.clone(), false);
        tx.send(Cmd::Publish(Box::new(outgoing()))).unwrap();
        let (t, err) = next_notice(&store);
        assert!(err && t == "not published to site/oslo/t: lab isn't connected", "{t}");
    }

    #[test]
    fn packet_ids_skip_those_in_use() {
        // After 65535 comes 1 (0 isn't an id), and taken ids are skipped.
        let mut last = 65534;
        assert_eq!(free_pid(&mut last, |_| false), Some(65535));
        assert_eq!(free_pid(&mut last, |p| p == 1 || p == 2), Some(3));
        assert_eq!(free_pid(&mut last, |_| true), None);
        // Taken: SUBSCRIBEs and UNSUBSCRIBEs waiting for their answer, and
        // publishes waiting for their acknowledgement.
        let mut outbox = Outbox::default();
        outbox.waiting.push(waiting(2));
        let mut ids = Ids {
            last: u16::MAX,
            subscribes: vec![(1, "a/#".into())],
            unsubscribes: vec![3],
        };
        assert_eq!(ids.next(&outbox), Some(4));
        ids.subscribes.clear();
        ids.last = u16::MAX;
        assert_eq!(ids.next(&outbox), Some(1), "free again once answered");
    }

    #[test]
    fn receive_maximum_window() {
        let lim = Limits {
            receive_max: 2,
            ..Limits::default()
        };
        let (mut ids, mut outbox) = (Ids::default(), Outbox::default());
        let q1 = Outgoing { qos: 1, ..outgoing() };
        for _ in 0..2 {
            let p = publish_id(&q1, &mut ids, &outbox, &lim).unwrap();
            outbox.waiting.push(waiting(p));
        }
        assert_ne!(outbox.waiting[0].pid, outbox.waiting[1].pid);
        let e = publish_id(&q1, &mut ids, &outbox, &lim).unwrap_err();
        assert!(
            e.contains("yet to acknowledge 2 earlier messages") && e.contains("Receive Maximum"),
            "{e}"
        );
        // QoS 0 isn't acknowledged, so it doesn't count.
        let q0 = Outgoing { qos: 0, ..outgoing() };
        assert_eq!(publish_id(&q0, &mut ids, &outbox, &lim), Ok(0));
        // An acknowledgement makes room.
        outbox.waiting.pop();
        assert!(publish_id(&q1, &mut ids, &outbox, &lim).is_ok());
    }

    /// The next PUBLISH a broker reads from the client: its topic and id.
    fn read_publish(rd: &mut Reader, s: &mut TcpStream) -> (String, u16) {
        let (kind, body) = rd.next(s).unwrap().expect("a packet within the read timeout");
        assert_eq!(kind, 0x32, "QoS 1 PUBLISH");
        let (topic, pid, _, _) = parse_publish(&body, 1).unwrap();
        (topic, pid)
    }

    #[test]
    fn receive_maximum_from_connack_holds_publishes() {
        // A broker on a local port, scripted by hand: it takes one
        // unacknowledged publish at a time.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (store, cfg) = local("t", listener.local_addr().unwrap().port());
        let tx = spawn(cfg, store.clone(), true);
        let (mut broker, _) = listener.accept().unwrap();
        broker.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut rd = Reader::default();
        assert_eq!(rd.next(&mut broker).unwrap().unwrap().0, 0x10, "CONNECT");
        // CONNACK: accepted, with Receive Maximum 1.
        broker.write_all(&packet(0x20, vec![0, 0, 3, 0x21, 0, 1])).unwrap();

        let publish = |topic: &str| {
            Cmd::Publish(Box::new(Outgoing {
                topic: topic.into(),
                payload: b"x".to_vec(),
                qos: 1,
                retain: false,
                props: Props::default(),
            }))
        };
        tx.send(publish("a")).unwrap();
        tx.send(publish("b")).unwrap();
        let (topic, a) = read_publish(&mut rd, &mut broker);
        assert_eq!(topic, "a");
        let (n, err) = next_notice(&store);
        assert!(
            err && n.starts_with("not published to t · b: the broker has yet to acknowledge 1 earlier message"),
            "{n}"
        );
        // Its PUBACK for "a" makes room for the next one.
        broker.write_all(&[0x40, 2, (a >> 8) as u8, a as u8]).unwrap();
        assert_eq!(
            next_notice(&store),
            ("published to t · a (QoS 1, acknowledged)".into(), false)
        );
        tx.send(publish("c")).unwrap();
        let (topic, c) = read_publish(&mut rd, &mut broker);
        assert_eq!(topic, "c");
        assert_ne!(c, 0);
    }

    #[test]
    fn reader_releases_large_buffer() {
        let data = packet(0x30, vec![0u8; 3 << 20]);
        let mut rd = Reader::default();
        let mut src = &data[..];
        assert_eq!(rd.next(&mut src).unwrap().unwrap().1.len(), 3 << 20);
        assert!(rd.buf.capacity() <= 1 << 20, "buffer kept {} bytes", rd.buf.capacity());
    }

    #[test]
    fn reader_splits_packets() {
        let mut data = packet(0x30, vec![0, 1, b'x', 0, b'p']);
        data.extend(packet(0xD0, vec![]));
        let mut rd = Reader::default();
        let mut src = &data[..];
        assert_eq!(rd.next(&mut src).unwrap().unwrap().0, 0x30);
        assert_eq!(rd.next(&mut src).unwrap().unwrap().0, 0xD0);
    }
}

/// Needs mosquitto on 127.0.0.1:18830 (plain) and localhost:18883 (TLS, with
/// the CA in $FSS_TEST_CA). CI runs it; locally:
/// scripts/e2e-brokers.sh /tmp/b && FSS_TEST_CA=/tmp/b/ca.pem cargo test --release e2e -- --ignored
#[cfg(test)]
mod e2e {
    use super::*;
    use crate::store::{Message, Sub};
    use std::path::Path;
    use std::process::Command;

    fn sub(t: &str) -> Sub {
        Sub {
            topic: t.into(),
            enabled: true,
            err: None,
        }
    }

    /// A store with one connection subscribed to `topics`, and its config.
    fn setup(port: u16, tls: bool, ca: Option<&str>, topics: &[&str]) -> (Arc<Mutex<Store>>, Config) {
        let mut st = Store::new(10, 64 * 1024);
        let conn = st.add_conn("t", "test", None, topics.iter().map(|t| sub(t)).collect());
        let cfg = Config {
            conn,
            host: if tls { "localhost".into() } else { "127.0.0.1".into() },
            port,
            tls,
            ca_file: ca.map(String::from),
            cert_file: None,
            key_file: None,
            key_password: None,
            insecure: false,
            client_id: format!("e2e-{port}-{}", ca.is_some()),
            username: None,
            password: None,
            qos: 2,
        };
        (Arc::new(Mutex::new(st)), cfg)
    }

    fn status(st: &Arc<Mutex<Store>>) -> Status {
        st.lock().unwrap().conns[0].status.clone()
    }

    /// Waits for `state` (or an error), then a moment for SUBACKs to land.
    fn wait_state(st: &Arc<Mutex<Store>>, state: ConnState) -> Status {
        for _ in 0..50 {
            let s = status(st);
            if s.state == state || s.err.is_some() {
                std::thread::sleep(Duration::from_millis(300));
                return status(st);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        status(st)
    }

    fn wait_for(st: &Arc<Mutex<Store>>, total: u64) -> u64 {
        let start = Instant::now();
        loop {
            let n = st.lock().unwrap().total();
            if n >= total || start.elapsed() > Duration::from_secs(10) {
                return n;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn publish(args: &[&str]) {
        let ok = Command::new("mosquitto_pub")
            .args(args)
            .status()
            .expect("mosquitto_pub")
            .success();
        assert!(ok, "mosquitto_pub {args:?} failed");
    }

    fn latest(st: &Arc<Mutex<Store>>, path: &str) -> Option<Arc<Message>> {
        let st = st.lock().unwrap();
        st.nodes
            .iter()
            .find(|n| n.depth > 1 && n.path == path)
            .and_then(|n| n.latest().cloned())
    }

    #[test]
    #[ignore]
    fn e2e() {
        // Plain TCP: QoS 1 and 2 with v5 properties.
        let (st, cfg) = setup(18830, false, None, &["e2e/#"]);
        let tx = spawn(cfg, st.clone(), true);
        let s = wait_state(&st, ConnState::Connected);
        assert!(
            s.state == ConnState::Connected && s.err.is_none(),
            "plain connect: {:?}",
            s.err
        );

        for q in ["1", "2"] {
            let topic = format!("e2e/plant/qos{q}");
            #[rustfmt::skip]
            publish(&[
                "-p", "18830", "-V", "mqttv5", "-q", q, "-t", &topic, "-m", r#"{"t":21.5}"#,
                "-D", "publish", "user-property", "site", "oslo",
                "-D", "publish", "content-type", "application/json",
            ]);
        }
        assert_eq!(wait_for(&st, 2), 2);
        for q in [1u8, 2] {
            let m = latest(&st, &format!("e2e/plant/qos{q}")).expect("message stored");
            assert_eq!(m.qos, q);
            assert_eq!(m.payload, br#"{"t":21.5}"#);
            assert_eq!(m.props.user, vec![("site".to_string(), "oslo".to_string())]);
            assert_eq!(m.props.content_type.as_deref(), Some("application/json"));
        }

        // A QoS 1 burst must arrive complete (QoS 0 bursts may be dropped by the broker).
        let n = 5_000;
        let ok = Command::new("sh")
            .arg("-c")
            .arg(format!(
                r#"seq 1 {n} | sed 's/.*/{{"v":&}}/' | mosquitto_pub -p 18830 -q 1 -t e2e/load -l"#
            ))
            .status()
            .unwrap()
            .success();
        assert!(ok);
        assert_eq!(wait_for(&st, n + 2), n + 2, "burst incomplete");

        // Subscribe to more at runtime, then unsubscribe: delivery starts and stops.
        let before = st.lock().unwrap().total();
        tx.send(Cmd::Subscribe("live/#".into())).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        publish(&["-p", "18830", "-q", "1", "-t", "live/a", "-m", "1"]);
        assert_eq!(wait_for(&st, before + 1), before + 1, "runtime subscribe");
        tx.send(Cmd::Unsubscribe("live/#".into())).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        publish(&["-p", "18830", "-q", "1", "-t", "live/a", "-m", "2"]);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(st.lock().unwrap().total(), before + 1, "message after unsubscribe");

        // Disconnect keeps the data; reconnect resubscribes.
        tx.send(Cmd::Disconnect).unwrap();
        assert_eq!(wait_state(&st, ConnState::Disconnected).state, ConnState::Disconnected);
        assert_eq!(st.lock().unwrap().total(), before + 1, "data kept after disconnect");
        tx.send(Cmd::Connect {
            password: None,
            key_password: None,
        })
        .unwrap();
        assert_eq!(wait_state(&st, ConnState::Connected).state, ConnState::Connected);
        publish(&["-p", "18830", "-q", "1", "-t", "e2e/again", "-m", "x"]);
        assert_eq!(wait_for(&st, before + 2), before + 2, "after reconnect");

        // New settings (the connections file changed): it drops the session
        // and connects with them, keeping the store's subscriptions.
        let (_, mut moved) = setup(18830, false, None, &[]);
        moved.client_id = "e2e-reconfigured".into();
        tx.send(Cmd::Reconfigure {
            cfg: Box::new(moved),
            connect: true,
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(wait_state(&st, ConnState::Connected).state, ConnState::Connected);
        publish(&["-p", "18830", "-q", "1", "-t", "e2e/reconfigured", "-m", "x"]);
        assert_eq!(wait_for(&st, before + 3), before + 3, "after reconfigure");

        // Publishing: QoS 0, 1 and 2 come back through the subscription with
        // their properties, and each is acknowledged as far as its QoS goes.
        st.lock().unwrap().take_notices();
        let props = |q: u8| Props {
            content_type: Some("text/plain".into()),
            response_topic: Some("e2e/reply".into()),
            correlation_data: Some(vec![q, 0xff]),
            payload_format: Some(1),
            message_expiry: Some(600),
            subscription_id: None,
            user: vec![("k".into(), format!("v{q}")), ("k".into(), "again".into())],
        };
        for q in 0..=2u8 {
            tx.send(Cmd::Publish(Box::new(Outgoing {
                topic: format!("e2e/pub/qos{q}"),
                payload: format!("payload {q}").into_bytes(),
                qos: q,
                retain: false,
                props: props(q),
            })))
            .unwrap();
        }
        assert_eq!(wait_for(&st, before + 6), before + 6, "own publishes received");
        for q in 0..=2u8 {
            let m = latest(&st, &format!("e2e/pub/qos{q}")).expect("published message received");
            assert_eq!(m.qos, q);
            assert_eq!(m.payload, format!("payload {q}").as_bytes());
            // The broker counts the expiry down on the way.
            let mut got = m.props.clone();
            assert!(got.message_expiry.is_some_and(|s| s <= 600 && s > 590), "{got:?}");
            got.message_expiry = Some(600);
            got.subscription_id = None;
            assert_eq!(got, props(q));
        }
        let mut notices = Vec::new();
        for _ in 0..50 {
            notices.extend(st.lock().unwrap().take_notices());
            if notices.len() >= 3 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let texts: Vec<&str> = notices.iter().map(|(t, _)| t.as_str()).collect();
        assert!(notices.iter().all(|(_, err)| !err), "{texts:?}");
        for want in [
            "t · e2e/pub/qos0 (QoS 0)",
            "t · e2e/pub/qos1 (QoS 1, acknowledged)",
            "t · e2e/pub/qos2 (QoS 2, completed)",
        ] {
            assert!(texts.iter().any(|t| t.contains(want)), "{want} missing: {texts:?}");
        }

        // TLS with the test CA connects and receives; without it the cert is refused.
        let ca = std::env::var("FSS_TEST_CA").expect("set FSS_TEST_CA");
        let (st, cfg) = setup(18883, true, Some(&ca), &["#"]);
        let _tx = spawn(cfg, st.clone(), true);
        let s = wait_state(&st, ConnState::Connected);
        assert!(
            s.state == ConnState::Connected && s.err.is_none(),
            "tls connect: {:?}",
            s.err
        );
        publish(&[
            "-p",
            "18883",
            "-h",
            "localhost",
            "--cafile",
            &ca,
            "-t",
            "e2e/tls",
            "-m",
            "hi",
        ]);
        assert_eq!(wait_for(&st, 1), 1);

        let (st, cfg) = setup(18883, true, None, &["#"]);
        let _tx = spawn(cfg, st.clone(), true);
        let s = wait_state(&st, ConnState::Connected);
        assert!(s.state != ConnState::Connected, "connected without trusting the CA");
        let err = s.err.unwrap_or_default();
        assert!(
            err.contains("certificate") || err.contains("UnknownIssuer"),
            "unexpected error: {err}"
        );

        // Mutual TLS with an encrypted client key: a wrong passphrase stops the
        // connection (no retrying) until the right one comes.
        let dir = Path::new(&ca).parent().unwrap();
        let (st, mut cfg) = setup(18884, true, Some(&ca), &["#"]);
        cfg.cert_file = Some(dir.join("client.pem").to_string_lossy().into());
        cfg.key_file = Some(dir.join("client.key").to_string_lossy().into());
        cfg.key_password = Some("wrong".into());
        let tx = spawn(cfg, st.clone(), true);
        for _ in 0..50 {
            if status(&st).err.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(300));
        let s = status(&st);
        assert!(
            s.state == ConnState::Disconnected,
            "kept trying with a wrong passphrase"
        );
        assert!(
            s.err.as_deref().unwrap_or_default().contains("wrong passphrase"),
            "{:?}",
            s.err
        );
        tx.send(Cmd::Connect {
            password: None,
            key_password: Some("s3cret".into()),
        })
        .unwrap();
        let s = wait_state(&st, ConnState::Connected);
        assert!(
            s.state == ConnState::Connected && s.err.is_none(),
            "mtls connect: {:?}",
            s.err
        );
        #[rustfmt::skip]
        publish(&[
            "-p", "18884", "-h", "localhost", "--cafile", &ca,
            "--cert", &dir.join("client.pem").to_string_lossy(),
            "--key", &dir.join("client-plain.key").to_string_lossy(),
            "-t", "e2e/mtls", "-m", "hi",
        ]);
        assert_eq!(wait_for(&st, 1), 1);
    }
}
