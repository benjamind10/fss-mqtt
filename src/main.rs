//! fss-mqtt — interactive terminal MQTT explorer.

mod app;
mod arrow;
mod avro;
mod base64;
mod broker;
mod capture;
mod changeset;
mod compressed;
mod config;
mod crc32;
mod fmt;
mod glyph;
mod mqtt;
mod parquet;
mod pemkey;
mod pretty;
mod protobuf;
mod sqlite;
mod store;
mod theme;
mod ui;
mod values;
mod view;
mod viewer;

use std::io::stdout;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind};
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
  -b, --broker string        broker URL (mqtt://, mqtts://) or host[:port]
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
      --insecure             skip TLS certificate verification
      --key string           client private key PEM for mutual TLS (may be encrypted)
      --max-payload string   payload bytes kept per message; the rest is discarded on arrival (default \"4KiB\")
  -P, --password string      password
  -q, --qos int              subscription QoS (0, 1, 2)
  -t, --topic string         topic filter to subscribe to (repeatable) (default \"#\")
  -u, --username string      username
  -v, --version              print version and exit

Nothing is written to disk unless you ask: --capture or ctrl+r appends the
messages on those topics, whole, to fss-mqtt-<connection>-<time>.jsonl.

Environment: FSS_MQTT_BROKER, FSS_MQTT_USERNAME, FSS_MQTT_PASSWORD, FSS_MQTT_CAFILE,
FSS_MQTT_KEY_PASSWORD (the --key passphrase), FSS_MQTT_CAPTURE_DIR";

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
    client_id: Option<String>,
    qos: u8,
    history: usize,
    max_payload: usize,
    inline: usize,
    insecure: bool,
    ca_file: Option<String>,
    cert: Option<String>,
    key: Option<String>,
    capture: Vec<String>,
    capture_dir: Option<String>,
}

fn main() {
    if let Err(e) = run() {
        eprintln!("fss-mqtt: {e}");
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
        client_id: None,
        qos: 0,
        history: 10,
        max_payload: 4 * 1024,
        inline: 64,
        insecure: false,
        ca_file: env("FSS_MQTT_CAFILE"),
        cert: None,
        key: None,
        capture: vec![],
        capture_dir: env("FSS_MQTT_CAPTURE_DIR"),
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
                println!("{USAGE}");
                return Ok(None);
            }
            "-v" | "--version" => {
                println!("fss-mqtt {VERSION}");
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
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown flag {s}\n\n{USAGE}")),
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
    c.url = b.display.clone();
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

    // Decide which connections to show and which to start.
    let mut cli_idx = None;
    let mut saved: Vec<bool> = vec![true; entries.len()];
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

    let opts = app::Options { inline_bytes: a.inline };
    let mut app = app::App::new(store, opts, theme::Theme::detect(), ctls, path, ask);
    if let Some((text, err)) = notice {
        app.notify(text, err);
    }

    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableBracketedPaste);
    let res = event_loop(&mut terminal, &mut app);
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    // What was captured reaches the disk before we exit, within a bound.
    for late in app.flush_captures(capture::QUIT_WAIT) {
        eprintln!("fss-mqtt: capture: {late}");
    }
    res.map_err(|e| e.to_string())
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut app::App) -> std::io::Result<()> {
    let size = terminal.size()?;
    app.resize(size.width, size.height);
    app.tick();
    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(FRAME)? {
            // Handle everything already queued, then redraw once.
            loop {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => app.key(k),
                    Event::Paste(s) => app.paste(&s),
                    Event::Resize(w, h) => app.resize(w, h),
                    _ => {}
                }
                if app.quit {
                    return Ok(());
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        app.tick();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
