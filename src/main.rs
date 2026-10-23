//! fss-mqtt — interactive terminal MQTT explorer.

mod app;
mod arrow;
mod avro;
mod base64;
mod brand;
mod broker;
mod capture;
mod changeset;
mod chart;
mod clipboard;
mod composer;
mod compressed;
mod config;
mod crc32;
mod fmt;
mod glyph;
mod gzip;
mod http;
mod json;
mod keys;
mod mqtt;
mod omarchy;
mod parquet;
mod pemkey;
mod pretty;
mod protobuf;
mod proxy;
mod record;
mod session;
mod sparkplug;
mod sqlite;
mod store;
mod theme;
mod ui;
mod update;
mod upgrade;
mod values;
mod view;
mod viewer;
mod ws;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::execute;

const FRAME: Duration = Duration::from_millis(66);
const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "fss-mqtt — interactive MQTT explorer

Usage:
  fss-mqtt                     connect the saved connections marked autoconnect
  fss-mqtt -c NAME [-c NAME]   connect these saved connections
  fss-mqtt [flags] BROKER      connect to BROKER and save it as a connection

Connections live in a small file you can edit (see --config). Connecting to a
broker on the command line adds or updates its entry; passwords are never
written, use password_env in the file or type it when asked. The same goes for
the passphrase of an encrypted --key (key_password_env).

Connection flags:
  -b, --broker string        broker URL (mqtt://, mqtts://, ws://, wss://) or host[:port]
  -c, --connection string    saved connection to connect (repeatable)
      --name string          name to save the broker under (default user@host:port)
      --no-save              don't add the broker to the connections file
      --config path          connections file (default: see below)
      --cafile string        PEM file of CA certificates to trust (implies TLS)
      --capture string       topic filter to capture to disk (repeatable); also ctrl+r on a topic
      --capture-dir path     where capture files go (default: the current directory)
      --cert string          client certificate PEM for mutual TLS
  -i, --client-id string     client id (default fss-mqtt-<random>)
      --history int          messages kept per topic (default 10)
      --inline int           payloads up to this many bytes are shown inline in the tree (default 64)
      --vim                  vim keys: j k h l g G ctrl+d ctrl+u move, / filters, q quits
      --insecure             skip TLS certificate verification
      --key string           client private key PEM for mutual TLS (may be encrypted)
      --max-payload string   payload bytes kept per message; the rest is discarded on arrival (default \"4KiB\")
      --proxy string         reach the broker through a proxy: http://[user@]host:port (CONNECT)
                             or socks5://[user@]host:port; the user's password from
                             FSS_MQTT_PROXY_PASSWORD (saved as proxy_password_env)
  -P, --password string      password
  -q, --qos int              subscription QoS (0, 1, 2)
  -t, --topic string         topic filter to subscribe to (repeatable) (default \"#\")
  -u, --username string      username
  -v, --version              print version and exit

Sessions and files:
      --resume               continue the last session: its messages, connections and view
                             (ctrl+n restarts into a newer fss-mqtt installed meanwhile, continuing it)
      --no-session           don't save the session when quitting
      --open path            browse a capture file offline (repeatable; .jsonl or .jsonl.gz);
                             add -c NAME or a broker to send its messages on with ctrl+e

Quitting saves the session (what's in memory, the connections, the view) for
--resume; --no-session or FSS_MQTT_SESSION=off turns that off. Messages are
written anywhere else only when you ask: --capture or ctrl+r appends the
messages on those topics, whole, to fss-mqtt-<connection>-<time>.jsonl.

Environment: FSS_MQTT_BROKER, FSS_MQTT_USERNAME, FSS_MQTT_PASSWORD, FSS_MQTT_CAFILE,
FSS_MQTT_KEY_PASSWORD (the --key passphrase), FSS_MQTT_PROXY, FSS_MQTT_PROXY_PASSWORD,
FSS_MQTT_CAPTURE_DIR, FSS_MQTT_SESSION,
FSS_MQTT_KEYS (vim: as --vim), FSS_MQTT_UPDATE (off: no looking for new releases)";

/// The usage, under the name it was started as (OmaMQTT: see brand.rs).
fn usage() -> String {
    if !brand::oma() {
        return USAGE.into();
    }
    USAGE
        .replacen(
            "fss-mqtt — interactive MQTT explorer",
            "OmaMQTT — interactive MQTT explorer (fss-mqtt)",
            1,
        )
        .replace("\n  fss-mqtt ", "\n  omamqtt ")
}

struct Args {
    broker: Option<String>,
    connections: Vec<String>,
    name: Option<String>,
    no_save: bool,
    config: Option<PathBuf>,
    topics: Vec<String>,
    username: Option<String>,
    password: Option<String>,
    password_from_env: bool,
    key_password: Option<String>, // from the environment only
    proxy: Option<String>,
    proxy_password_from_env: bool,
    client_id: Option<String>,
    qos: u8,
    history: usize,
    max_payload: usize,
    inline: usize,
    /// Vim keys (--vim, FSS_MQTT_KEYS=vim).
    vim: bool,
    insecure: bool,
    ca_file: Option<String>,
    cert: Option<String>,
    key: Option<String>,
    capture: Vec<String>,
    capture_dir: Option<String>,
    open: Vec<PathBuf>,
    resume: bool,
    /// Don't save the session at quit (--no-session, FSS_MQTT_SESSION=off).
    no_session: bool,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{}: {e}", brand::command());
        std::process::exit(1);
    }
}

fn parse_args() -> Result<Option<Args>, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let mut a = Args {
        broker: env("FSS_MQTT_BROKER"),
        connections: vec![],
        name: None,
        no_save: false,
        config: None,
        topics: vec![],
        username: env("FSS_MQTT_USERNAME"),
        password: env("FSS_MQTT_PASSWORD"),
        password_from_env: env("FSS_MQTT_PASSWORD").is_some(),
        key_password: env("FSS_MQTT_KEY_PASSWORD"),
        proxy: env("FSS_MQTT_PROXY"),
        proxy_password_from_env: env("FSS_MQTT_PROXY_PASSWORD").is_some(),
        client_id: None,
        qos: 0,
        history: 10,
        max_payload: 4 * 1024,
        inline: 64,
        vim: env("FSS_MQTT_KEYS").is_some_and(|v| v.eq_ignore_ascii_case("vim")),
        insecure: false,
        ca_file: env("FSS_MQTT_CAFILE"),
        cert: None,
        key: None,
        capture: vec![],
        capture_dir: env("FSS_MQTT_CAPTURE_DIR"),
        open: vec![],
        resume: false,
        no_session: env("FSS_MQTT_SESSION").is_some_and(|v| v.eq_ignore_ascii_case("off")),
    };
    let num = |name: &str, v: String| v.parse::<usize>().map_err(|_| format!("{name}: invalid number {v:?}"));

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let (name, mut inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut val = || {
            inline
                .take()
                .or_else(|| it.next())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match name.as_str() {
            "-h" | "--help" => {
                println!("{}", usage());
                return Ok(None);
            }
            "-v" | "--version" => {
                println!("{} {VERSION}", brand::command());
                return Ok(None);
            }
            "-b" | "--broker" => a.broker = Some(val()?),
            "-c" | "--connection" => a.connections.push(val()?),
            "--name" => a.name = Some(val()?),
            "--no-save" => a.no_save = true,
            "--config" => a.config = Some(PathBuf::from(val()?)),
            "-t" | "--topic" => a.topics.push(val()?),
            "-u" | "--username" => a.username = Some(val()?),
            "-P" | "--password" => {
                a.password = Some(val()?);
                a.password_from_env = false;
            }
            "-i" | "--client-id" => a.client_id = Some(val()?),
            "-q" | "--qos" => {
                a.qos = match val()?.as_str() {
                    "0" => 0,
                    "1" => 1,
                    "2" => 2,
                    _ => return Err("--qos must be 0, 1 or 2".into()),
                }
            }
            "--history" => a.history = num(&name, val()?)?,
            "--max-payload" => a.max_payload = parse_size(&val()?).map_err(|e| format!("--max-payload: {e}"))?,
            "--inline" => a.inline = num(&name, val()?)?,
            "--insecure" => a.insecure = true,
            "--proxy" => a.proxy = Some(val()?),
            "--vim" => a.vim = true,
            "--cafile" => a.ca_file = Some(val()?),
            "--cert" => a.cert = Some(val()?),
            "--key" => a.key = Some(val()?),
            "--capture" => {
                let f = val()?;
                if !capture::valid_filter(&f) {
                    return Err(format!("--capture: {f:?} is not a topic filter"));
                }
                a.capture.push(f);
            }
            "--capture-dir" => a.capture_dir = Some(val()?),
            "--resume" => a.resume = true,
            "--no-session" => a.no_session = true,
            "--open" => {
                let p = PathBuf::from(config::expand(&val()?));
                if !p.is_file() {
                    return Err(format!("--open: {}: no such file", p.display()));
                }
                a.open.push(std::path::absolute(&p).unwrap_or(p));
            }
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown flag {s}\n\n{}", usage())),
            _ => a.broker = Some(arg),
        }
    }
    Ok(Some(a))
}

fn parse_size(s: &str) -> Result<usize, String> {
    let t = s.trim().to_uppercase();
    let (num, mult) = [
        ("KIB", 1 << 10),
        ("MIB", 1 << 20),
        ("KB", 1 << 10),
        ("MB", 1 << 20),
        ("K", 1 << 10),
        ("M", 1 << 20),
        ("B", 1),
    ]
    .iter()
    .find_map(|(suf, m)| t.strip_suffix(suf).map(|n| (n.trim().to_string(), *m)))
    .unwrap_or((t.clone(), 1));
    num.parse::<usize>()
        .map(|n| n * mult)
        .map_err(|_| format!("invalid size {s:?}"))
}

/// The connection described by command-line flags, merged over a saved entry
/// of the same name (flags win; topics are kept unless -t is given).
fn cli_connection(a: &Args, b: &broker::Broker, saved: Option<&config::ConnCfg>) -> config::ConnCfg {
    let mut c = saved.cloned().unwrap_or_default();
    c.name = a.name.clone().unwrap_or_else(|| {
        let hp = b.display.split_once("://").map_or(b.display.as_str(), |(_, hp)| hp);
        match &a.username {
            Some(u) => format!("{u}@{hp}"),
            None => hp.to_string(),
        }
    });
    c.url = b.url.clone();
    // File paths are saved absolute, so the entry works from any directory.
    for (field, v) in [
        (&mut c.cafile, &a.ca_file),
        (&mut c.cert, &a.cert),
        (&mut c.key, &a.key),
    ] {
        if let Some(p) = v {
            *field = Some(absolute_path(p));
        }
    }
    for (field, v) in [(&mut c.username, &a.username), (&mut c.client_id, &a.client_id)] {
        if v.is_some() {
            *field = v.clone();
        }
    }
    if a.password_from_env {
        c.password_env = Some("FSS_MQTT_PASSWORD".into());
    }
    if a.key_password.is_some() {
        c.key_password_env = Some("FSS_MQTT_KEY_PASSWORD".into());
    }
    if a.proxy.is_some() {
        c.proxy = a.proxy.clone();
    }
    // Only for a proxy with a user to log in as (the file refuses it otherwise).
    let proxy_user = c.proxy.as_deref().and_then(|u| proxy::Proxy::parse(u, None).ok()?.user);
    if a.proxy_password_from_env && proxy_user.is_some() {
        c.proxy_password_env = Some("FSS_MQTT_PROXY_PASSWORD".into());
    }
    c.insecure |= a.insecure;
    if a.qos != 0 {
        c.qos = a.qos;
    }
    if !a.topics.is_empty() {
        c.paused.retain(|t| a.topics.contains(t));
        c.topics = a.topics.clone();
    }
    if !a.capture.is_empty() {
        c.capture = a.capture.clone();
        c.capture_paused.clear();
    }
    if c.topics.is_empty() {
        c.topics = vec!["#".into()];
    }
    c.autoconnect = true;
    c
}

/// `~/x` and relative paths made absolute (the file need not exist).
fn absolute_path(p: &str) -> String {
    let p = config::expand(p);
    std::path::absolute(&p)
        .map(|a| a.to_string_lossy().into_owned())
        .unwrap_or(p)
}

fn run() -> Result<(), String> {
    let Some(a) = parse_args()? else { return Ok(()) };
    let path = a.config.clone().unwrap_or_else(config::default_path);
    let mut entries = config::load(&path)?;
    let mut notice: Option<(String, bool)> = None;

    // --resume: the last session, whose connections join the file's.
    let session_path = session::default_path();
    let resumed = match (a.resume, &session_path) {
        (false, _) => None,
        (true, Some(p)) if p.is_file() => {
            Some(session::load(p).map_err(|e| format!("can't continue the last session: {e}"))?)
        }
        (true, _) => {
            notice = Some(("no saved session to continue".into(), false));
            None
        }
    };
    let mut saved: Vec<bool> = vec![true; entries.len()];
    if let Some(l) = &resumed {
        for c in l.header.conns.iter().filter(|c| c.file.is_none()) {
            if entries.iter().any(|e| e.name == c.name) {
                continue; // saved: the file's entry is the one to use
            }
            // A --no-save broker, or one since removed from the file.
            let entry = c
                .config
                .as_deref()
                .and_then(|t| config::parse(t).ok())
                .and_then(|mut v| v.pop())
                .unwrap_or_else(|| config::ConnCfg {
                    name: c.name.clone(),
                    url: c.url.clone(),
                    ..Default::default()
                });
            entries.push(entry);
            saved.push(false);
        }
    }

    // Decide which connections to show and which to start.
    let mut cli_idx = None;
    let mut start: Vec<bool>;
    if let Some(broker) = &a.broker {
        let b = broker::parse_broker(broker, a.ca_file.is_some() || a.cert.is_some())?;
        let name = a.name.clone();
        let existing = entries.iter().position(|e| {
            Some(&e.name) == name.as_ref() || (name.is_none() && cli_connection(&a, &b, None).name == e.name)
        });
        let c = cli_connection(&a, &b, existing.map(|i| &entries[i]));
        // Fail fast on a bad cafile/cert/key for the broker you just typed. An
        // encrypted key without a passphrase is fine: the UI asks for it.
        match mqtt::tls_config(&broker::mqtt_config(0, &c, None, a.key_password.clone())?) {
            Ok(_) | Err(mqtt::TlsError::Key(_, pemkey::KeyError::NeedsPassphrase)) => {}
            Err(e @ mqtt::TlsError::Key(_, pemkey::KeyError::WrongPassphrase)) => {
                return Err(format!("{}: {e} (from FSS_MQTT_KEY_PASSWORD)", c.name));
            }
            Err(e) => return Err(format!("{}: {e}", c.name)),
        }
        let i = match existing {
            Some(i) => {
                // With --no-save the command line's version isn't the file's;
                // live reload must leave it alone.
                entries[i] = c;
                saved[i] = !a.no_save;
                i
            }
            None => {
                entries.push(c);
                saved.push(!a.no_save);
                entries.len() - 1
            }
        };
        if !a.no_save {
            saved[i] = true;
            match config::save(&path, &entries[i]) {
                Ok(()) => {
                    notice = Some((
                        format!("saved connection {:?} to {}", entries[i].name, path.display()),
                        false,
                    ))
                }
                Err(e) => notice = Some((format!("couldn't save connection: {e}"), true)),
            }
        }
        start = (0..entries.len()).map(|j| j == i).collect();
        start
            .iter_mut()
            .zip(&entries)
            .for_each(|(s, e)| *s |= a.connections.contains(&e.name));
        cli_idx = Some(i);
    } else if !a.capture.is_empty() && !entries.is_empty() {
        return Err(
            "--capture goes with a broker on the command line; for a saved connection, \
             add capture = [...] to its entry or press ctrl+r on a topic"
                .into(),
        );
    } else if !a.connections.is_empty() {
        for n in &a.connections {
            if !entries.iter().any(|e| &e.name == n) {
                let known: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
                return Err(format!(
                    "no connection named {n:?} in {} (have: {})",
                    path.display(),
                    known.join(", ")
                ));
            }
        }
        start = entries.iter().map(|e| a.connections.contains(&e.name)).collect();
    } else if !a.open.is_empty() {
        // Browsing files: brokers connect only when asked for.
        start = vec![false; entries.len()];
    } else if entries.is_empty() {
        // First run: a local broker, not saved.
        entries.push(config::ConnCfg {
            name: "localhost".into(),
            url: "mqtt://localhost:1883".into(),
            topics: if a.topics.is_empty() {
                vec!["#".into()]
            } else {
                a.topics.clone()
            },
            capture: a.capture.clone(),
            autoconnect: true,
            ..Default::default()
        });
        saved.push(false);
        start = vec![true];
    } else {
        start = entries.iter().map(|e| e.autoconnect).collect();
    }
    if let Some(l) = &resumed {
        // What was connected when it was saved connects again (with
        // anything named on the command line), instead of autoconnect.
        let named = a.broker.is_some() || !a.connections.is_empty();
        for (s, e) in start.iter_mut().zip(&entries) {
            let was = l.header.conns.iter().any(|c| c.name == e.name && c.connected);
            *s = was || (named && *s);
        }
    }

    let mut st = store::Store::new(a.history, a.max_payload);
    let dir = a.capture_dir.as_deref().map_or_else(|| ".".into(), config::expand);
    st.capture_dir = std::path::absolute(&dir).unwrap_or_else(|_| dir.into());
    let store = Arc::new(Mutex::new(st));
    let mut ctls = Vec::new();
    let mut ask = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let password = if Some(i) == cli_idx && a.password.is_some() {
            a.password.clone()
        } else {
            app::env_secret(e.password_env.as_deref())
        };
        let key_password = if Some(i) == cli_idx && a.key_password.is_some() {
            a.key_password.clone()
        } else {
            app::env_secret(e.key_password_env.as_deref())
        };
        let cfg = broker::mqtt_config(i, e, password.clone(), key_password.clone())
            .map_err(|err| format!("connection {:?}: {err}", e.name))?;
        let display = broker::display_url(e)?;
        let idx = {
            let mut st = store.lock().unwrap();
            let idx = st.add_conn(&e.name, &display, e.username.clone(), app::subs_of(e));
            st.conns[idx].capture.set_rules(&e.capture, &e.capture_paused);
            idx
        };
        debug_assert_eq!(idx, i);
        let ctl = app::ConnCtl {
            cfg: e.clone(),
            saved: saved[i],
            tx: mqtt::spawn(cfg, store.clone(), false),
            password,
            key_password,
        };
        // A connection that needs a password or passphrase we don't have waits
        // for the prompt; the app connects the rest.
        if start[i] {
            ask.push(i);
        }
        ctls.push(ctl);
    }

    // Files to browse: a "connection" each, read on a thread of its own.
    for path in &a.open {
        let base = path
            .file_name()
            .map_or("file".into(), |n| n.to_string_lossy().into_owned());
        let mut name = base.clone();
        let mut k = 1;
        while ctls.iter().any(|c: &app::ConnCtl| c.cfg.name == name) {
            k += 1;
            name = format!("{base} ({k})");
        }
        let idx = {
            let mut st = store.lock().unwrap();
            let idx = st.add_conn(&name, &format!("file {}", config::tilde(path)), None, vec![]);
            st.conns[idx].file = Some(store::FileSource {
                path: path.clone(),
                ..Default::default()
            });
            idx
        };
        ctls.push(app::ConnCtl {
            cfg: config::ConnCfg {
                name,
                ..Default::default()
            },
            saved: false,
            tx: std::sync::mpsc::channel().0,
            password: None,
            key_password: None,
        });
        session::open(path.clone(), store.clone(), idx);
    }

    // A resumed session: its files come back as they were (not read
    // again), then its messages, as if arriving, and the Sparkplug names.
    let mut restored = None;
    if let Some(l) = resumed {
        for c in &l.header.conns {
            let Some(file) = &c.file else { continue };
            let mut st = store.lock().unwrap();
            if st.conns.iter().any(|x| x.name == c.name) {
                continue;
            }
            let idx = st.add_conn(&c.name, &c.url, None, vec![]);
            st.conns[idx].file = Some(store::FileSource {
                path: file.clone(),
                done: true,
                ..Default::default()
            });
            drop(st);
            ctls.push(app::ConnCtl {
                cfg: config::ConnCfg {
                    name: c.name.clone(),
                    ..Default::default()
                },
                saved: false,
                tx: std::sync::mpsc::channel().0,
                password: None,
                key_password: None,
            });
        }
        let index = |name: &str| {
            store
                .lock()
                .unwrap()
                .conns
                .iter()
                .position(|c| c.name == name && !c.removed)
        };
        let count = l.records.len();
        for r in l.records {
            if let Some(i) = r.conn.as_deref().and_then(index) {
                session::restore(&store, i, r);
            }
        }
        for (name, names) in &l.header.sparkplug {
            if let Some(i) = index(name) {
                let table = store.lock().unwrap().conns[i].sparkplug.clone();
                *table.lock().unwrap() = sparkplug::Aliases::from_json(names);
            }
        }
        let ago = fmt::ago(std::time::SystemTime::now(), l.header.saved);
        let by = if l.header.version == VERSION {
            String::new()
        } else {
            format!(" (saved by fss-mqtt {})", l.header.version)
        };
        let mut text = format!(
            "continued the session saved {ago}{by}: {} messages",
            fmt::commas(count as u64)
        );
        let mut err = false;
        if l.skipped > 0 {
            text += &format!("; {} lines skipped ({})", l.skipped, l.first_skip.unwrap_or_default());
            err = true;
        }
        if let Some(e) = l.damaged {
            text += &format!("; the rest of the file is damaged ({e})");
            err = true;
        }
        notice = Some((text, err));
        restored = Some(l.header.view);
    }

    let opts = app::Options {
        inline_bytes: a.inline,
        vim: a.vim,
        oma: brand::oma(),
    };
    // OmaMQTT wears the Omarchy theme's colours, when Omarchy is here.
    let th = match brand::oma().then(omarchy::current).flatten() {
        Some(th) => th.for_terminal(),
        None => theme::Theme::detect(),
    };
    let mut app = app::App::new(store, opts, th, ctls, path, ask);
    if brand::oma() {
        app.omarchy = Some(omarchy::Watch::new());
    }
    if let Some(view) = &restored {
        app.restore_view(view);
    }
    app.upgrade = upgrade::Watch::new();
    if !update::off() {
        let kept = session::default_path().and_then(|p| Some(p.parent()?.join("update-check")));
        app.release = Some(update::Checker::new(kept));
    }
    update::clean_up();
    if let Some((text, err)) = notice {
        app.notify(text, err);
    }

    let mut terminal = ratatui::init();
    // A panic on a thread that catches it (a table read in the background,
    // chart.rs) leaves the terminal as it is.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !chart::quiet_panics() {
            hook(info);
        }
    }));
    let _ = execute!(stdout(), EnableBracketedPaste);
    let res = event_loop(&mut terminal, &mut app);
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    // What was captured reaches the disk before we exit, within a bound.
    for late in app.flush_captures(capture::QUIT_WAIT) {
        eprintln!("{}: capture: {late}", brand::command());
    }
    if app.restart {
        return restart(&a, &app, session_path.as_deref());
    }
    if !a.no_session
        && let Some(p) = &session_path
    {
        // Only browsing files (nothing from a broker, nothing resumed)
        // leaves the last session as it was.
        match save_session(&app, p, restored.is_some()) {
            Ok(0) => {}
            Ok(_) => println!("Continue session: {}", resume_command(&a)),
            Err(e) => eprintln!("{}: couldn't save the session: {e}", brand::command()),
        }
    }
    res.map_err(|e| e.to_string())
}

/// ctrl+n: saves the session, then becomes the fss-mqtt installed in our
/// place, continuing it (on Windows, which can't, says how to).
fn restart(a: &Args, app: &app::App, session: Option<&std::path::Path>) -> Result<(), String> {
    let w = app.upgrade.as_ref().ok_or("nothing to restart into")?;
    let resume = match session {
        // Everything goes, even what --open read: the restart doesn't open
        // files again, it continues the session.
        Some(p) if !a.no_session => match save_session(app, p, true) {
            Ok(n) => n > 0,
            Err(e) => return Err(format!("couldn't save the session, so not restarting: {e}")),
        },
        _ => false,
    };
    let args = upgrade::restart_args(&std::env::args_os().skip(1).collect::<Vec<_>>(), resume);
    let note = app
        .upgrade_note()
        .unwrap_or_else(|| format!("{} changed on disk", app.name()));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        println!(
            "{note}: restarting{}…",
            if resume { ", continuing the session" } else { "" }
        );
        let err = std::process::Command::new(&w.path).args(&args).exec();
        Err(format!("couldn't start {}: {err}", w.path.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = (w, args);
        println!("{note}.");
        if resume {
            println!("Continue session: {}", resume_command(a));
        }
        Ok(())
    }
}

/// Saves what's in memory for `--resume`; 0 when there was nothing to save
/// (the last session is then left as it was). Unless `always`, that
/// includes when nothing came from a broker: browsing files with --open
/// doesn't replace the session of a real run.
fn save_session(app: &app::App, path: &std::path::Path, always: bool) -> Result<usize, String> {
    let view = app.session_view();
    let conns = app.session_conns();
    let st = app.store.lock().unwrap();
    if !session::worth_saving(st.total(), st.received, always) {
        return Ok(0);
    }
    if st.total() > 200_000 {
        eprintln!("Saving session…");
    }
    session::save(path, &view, &conns, &st)
}

/// How to continue this session: as fss-mqtt was started, with --resume.
fn resume_command(a: &Args) -> String {
    let arg0 = std::env::args().next().unwrap_or_default();
    // Run from a path (./target/release/fss-mqtt): say so; from $PATH: its name.
    let prog = if arg0.contains('/') || arg0.contains(std::path::MAIN_SEPARATOR) {
        shell_quote(&arg0)
    } else {
        brand::command().into()
    };
    let mut cmd = format!("{prog} --resume");
    if let Some(c) = &a.config {
        cmd += &format!(" --config {}", shell_quote(&c.to_string_lossy()));
    }
    if a.vim {
        cmd += " --vim";
    }
    cmd
}

/// `s` as one shell word: as it is when that's safe, else single-quoted.
fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%~,=".contains(c) || !c.is_ascii());
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut app::App) -> std::io::Result<()> {
    let size = terminal.size()?;
    app.resize(size.width, size.height);
    app.tick();
    loop {
        // In step with the terminal's size before each frame: a resize event
        // can still be on its way when the window has already changed.
        let size = terminal.size()?;
        if (size.width, size.height) != (app.width, app.height) {
            app.resize(size.width, size.height);
        }
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(FRAME)? {
            // Handle everything already queued, then redraw once.
            loop {
                let events = match event::read()? {
                    // Maybe the start of a key whose sequence arrived in two
                    // reads (an arrow: Esc, then "[B"); see keys.rs.
                    Event::Key(k)
                        if k.kind != KeyEventKind::Release && k.code == KeyCode::Esc && k.modifiers.is_empty() =>
                    {
                        keys::after_esc(k, |wait| {
                            if event::poll(wait)? {
                                event::read().map(Some)
                            } else {
                                Ok(None)
                            }
                        })?
                    }
                    e => vec![e],
                };
                for e in events {
                    match e {
                        Event::Key(k) if k.kind != KeyEventKind::Release => app.key(k),
                        Event::Paste(s) => app.paste(&s),
                        Event::Resize(w, h) => app.resize(w, h),
                        _ => {}
                    }
                    if app.quit {
                        return Ok(());
                    }
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        app.tick();
        // A restart into an update that just got installed.
        if app.quit {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_hint_quotes_what_the_shell_would_split() {
        assert_eq!(shell_quote("fss-mqtt"), "fss-mqtt");
        assert_eq!(shell_quote("./target/release/fss-mqtt"), "./target/release/fss-mqtt");
        assert_eq!(shell_quote("~/x/conn,1=2@h:9.toml"), "~/x/conn,1=2@h:9.toml");
        assert_eq!(shell_quote("/Users/me/My Files/c.toml"), "'/Users/me/My Files/c.toml'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote("a$b"), "'a$b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("64KiB"), Ok(65536));
        assert_eq!(parse_size("2m"), Ok(2 << 20));
    }

    #[test]
    fn paths_saved_absolute() {
        let cwd = std::env::current_dir().unwrap();
        // Compare as paths: Windows writes `\` where the input had `/`.
        assert_eq!(
            std::path::Path::new(&absolute_path("secrets/ca.pem")),
            cwd.join("secrets").join("ca.pem")
        );
        let home = std::env::var(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap();
        assert!(absolute_path("~/ca.pem").starts_with(&home));
        let abs = cwd.join("x.pem").to_string_lossy().into_owned();
        assert_eq!(absolute_path(&abs), abs);
    }
}
