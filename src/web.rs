//! The web version: the app in a web page (ratzilla draws ratatui there),
//! with the demo's data (demo.rs) and a live broker over the page's
//! WebSocket (mqtt/browser.rs). The keys, panes and views are the terminal's.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use ratzilla::DomBackend;
use ratzilla::event::{KeyCode as WebKey, KeyEvent as WebKeyEvent};
use ratzilla::ratatui::backend::Backend;
use ratzilla::ratatui::layout::Rect;
use ratzilla::ratatui::{Terminal, TerminalOptions, Viewport};
use ratzilla::web_sys::{KeyboardEvent, UrlSearchParams, console, window};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_time::{Duration, Instant};

use crate::app::{App, ConnCtl, Options};
use crate::config::ConnCfg;
use crate::input::{KeyCode, KeyEvent, KeyModifiers};
use crate::mqtt::Cmd;
use crate::store::Store;
use crate::{broker, demo, mqtt, theme, ui};

/// How often the app ticks, as in the terminal.
const TICK: Duration = Duration::from_millis(66);

/// A browser key as the app takes it; `None` for keys it has no use for.
fn key(k: &WebKeyEvent) -> Option<KeyEvent> {
    let code = match k.code {
        WebKey::Char(c) => KeyCode::Char(c),
        WebKey::Backspace => KeyCode::Backspace,
        WebKey::Enter => KeyCode::Enter,
        WebKey::Left => KeyCode::Left,
        WebKey::Right => KeyCode::Right,
        WebKey::Up => KeyCode::Up,
        WebKey::Down => KeyCode::Down,
        WebKey::Tab if k.shift => KeyCode::BackTab,
        WebKey::Tab => KeyCode::Tab,
        WebKey::Delete => KeyCode::Delete,
        WebKey::Home => KeyCode::Home,
        WebKey::End => KeyCode::End,
        WebKey::PageUp => KeyCode::PageUp,
        WebKey::PageDown => KeyCode::PageDown,
        WebKey::Esc => KeyCode::Esc,
        _ => return None,
    };
    // Chrome on Windows and Linux keeps Ctrl+T and Ctrl+W (a new tab, close
    // it) from the page: Alt+T and Alt+W stand in for them.
    let stand_in = k.alt && !k.ctrl && matches!(code, KeyCode::Char('t' | 'w'));
    let mut m = KeyModifiers::NONE;
    for (on, flag) in [
        (k.shift, KeyModifiers::SHIFT),
        (k.ctrl || stand_in, KeyModifiers::CONTROL),
        (k.alt && !stand_in, KeyModifiers::ALT),
    ] {
        if on {
            m = m | flag;
        }
    }
    Some(KeyEvent::new(code, m))
}

pub fn main() {
    on_panic();
    let store = Arc::new(Mutex::new(Store::new(10, 4 * 1024)));
    let mut demo = demo::start(&mut store.lock().unwrap());
    // The demo's connections take their commands here, each frame.
    let mut inboxes = Vec::new();
    let mut ctls = {
        let st = store.lock().unwrap();
        st.conns
            .iter()
            .map(|c| {
                let (tx, rx) = mpsc::channel();
                inboxes.push(rx);
                ConnCtl {
                    cfg: ConnCfg {
                        name: c.name.clone(),
                        url: c.url.clone(),
                        topics: vec!["site/#".into()],
                        ..Default::default()
                    },
                    saved: false,
                    tx,
                    password: None,
                    key_password: None,
                }
            })
            .collect::<Vec<_>>()
    };
    // Then a live broker, connected as the app connects any.
    let live = live_broker();
    let mut connect = vec![];
    match broker::mqtt_config(ctls.len(), &live, None, None).and_then(|cfg| Ok((cfg, broker::display_url(&live)?))) {
        Ok((cfg, display)) => {
            store
                .lock()
                .unwrap()
                .add_conn(&live.name, &display, None, crate::app::subs_of(&live));
            connect.push(ctls.len());
            ctls.push(ConnCtl {
                tx: mqtt::spawn(cfg, store.clone(), false),
                cfg: live,
                saved: false,
                password: None,
                key_password: None,
            });
        }
        Err(e) => store.lock().unwrap().notify(format!("broker: {e}"), true),
    }
    let opts = Options {
        inline_bytes: 64,
        vim: false,
        oma: false,
    };
    let app = Rc::new(RefCell::new(App::new(
        store.clone(),
        opts,
        theme::Theme::new(false),
        ctls,
        PathBuf::new(),
        connect,
    )));

    // The page's #app, which says it's loading until now.
    if let Some(loading) = window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("loading"))
    {
        loading.remove();
    }
    let backend = DomBackend::new_by_id("app").expect("a page to draw in");
    let fixed = TerminalOptions {
        viewport: Viewport::Fixed(Rect::default()),
    };
    let mut terminal = Terminal::with_options(backend, fixed).expect("a terminal in the page");
    // Drawn when something may have changed: a key, a tick, a resize.
    let dirty = Rc::new(Cell::new(true));
    let (keys, pressed) = (app.clone(), dirty.clone());
    on_keys(move |k| {
        let Some(k) = key(&k) else { return false };
        let mut app = keys.borrow_mut();
        app.key(k);
        app.quit = false; // a page isn't quit
        pressed.set(true);
        true
    });
    let (mut ticked, mut second) = (Instant::now(), Instant::now());
    let mut area = Rect::default();
    // A hidden tab gets no frames: the connections are also polled on a
    // timer, which browsers slow down there but keep running, so pings go
    // out and what arrives is taken in.
    every(250, mqtt::poll);
    every_frame(move || {
        // The grid is as many cells as the page measured; ratzilla's own size
        // assumes 10×19 pixel cells, and the frame would miss the page's edges.
        if let Ok(w) = terminal.backend_mut().window_size() {
            let now = Rect::new(0, 0, w.columns_rows.width, w.columns_rows.height);
            if now != area {
                area = now;
                let _ = terminal.resize(area);
                dirty.set(true);
            }
        }
        let mut app = app.borrow_mut();
        {
            let mut st = store.lock().unwrap();
            if second.elapsed() >= Duration::from_secs(1) {
                second = Instant::now();
                demo.second(&mut st);
            }
            for (conn, rx) in inboxes.iter().enumerate() {
                while let Ok(cmd) = rx.try_recv() {
                    if let Cmd::Publish(out) = cmd {
                        demo::publish(&mut st, conn, *out);
                    }
                }
            }
        }
        mqtt::poll(); // not under the store's lock: it takes it
        app.resize(area.width, area.height);
        if ticked.elapsed() >= TICK {
            ticked = Instant::now();
            app.tick();
            dirty.set(true);
        }
        if dirty.replace(false) {
            let _ = terminal.draw(|f| ui::draw(f, &app));
        }
    });
}

/// A panic can't be caught in a page, and ends the app: it says so on the
/// page, and in the console.
fn on_panic() {
    std::panic::set_hook(Box::new(|info| {
        let msg = info.to_string();
        console::error_1(&msg.as_str().into());
        let doc = window().and_then(|w| w.document());
        if let Some(doc) = doc
            && let Some(body) = doc.body()
            && let Ok(note) = doc.create_element("div")
        {
            let style = "position:fixed;left:0;right:0;bottom:0;padding:12px 16px;\
                         background:#b3261e;color:#fff;white-space:pre-wrap";
            let _ = note.set_attribute("style", style);
            note.set_text_content(Some(&format!(
                "fss-mqtt stopped: {msg}\nReload the page to start again."
            )));
            let _ = body.append_child(&note);
        }
    }));
}

/// The page's live connection: the broker and topics its address asks for
/// (`?broker=wss://…&topic=a/#&topic=b`), or else the public test broker's
/// own statistics.
fn live_broker() -> ConnCfg {
    let query = window()
        .and_then(|w| w.location().search().ok())
        .and_then(|q| UrlSearchParams::new_with_str(&q).ok());
    let asked = query.as_ref().and_then(|q| q.get("broker")).filter(|b| !b.is_empty());
    let Some(url) = asked else {
        return ConnCfg {
            name: "mosquitto".into(),
            url: "wss://test.mosquitto.org:8081/mqtt".into(),
            topics: vec!["$SYS/#".into()],
            ..Default::default()
        };
    };
    let topics: Vec<String> = query
        .iter()
        .flat_map(|q| {
            q.get_all("topic")
                .iter()
                .filter_map(|t| t.as_string())
                .collect::<Vec<_>>()
        })
        .filter(|t| !t.is_empty())
        .collect();
    let name = broker::parse_broker(&url, false).map_or("broker".into(), |b| b.host);
    ConnCfg {
        name,
        url,
        topics: if topics.is_empty() { vec!["#".into()] } else { topics },
        ..Default::default()
    }
}

/// Hands the page's keys to `f`, wherever the focus is: ratzilla listens on
/// its grid alone, which takes a click first and is made anew on a resize.
/// A key `f` takes (it returns true) is the app's, not the browser's; the
/// browser keeps ⌘ for itself.
fn on_keys(mut f: impl FnMut(WebKeyEvent) -> bool + 'static) {
    let cb = Closure::<dyn FnMut(KeyboardEvent)>::new(move |e: KeyboardEvent| {
        if !e.meta_key() && f(e.clone().into()) {
            e.prevent_default();
        }
    });
    let window = window().expect("a window");
    let _ = window.add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref());
    cb.forget(); // for as long as the page is open
}

/// Runs `f` every `ms` milliseconds, for as long as the page is open.
fn every(ms: i32, f: fn()) {
    let cb = Closure::<dyn FnMut()>::new(f);
    let window = window().expect("a window");
    let _ = window.set_interval_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ms);
    cb.forget();
}

/// Runs `f` before every frame the browser paints.
fn every_frame(mut f: impl FnMut() + 'static) {
    type Frame = Rc<RefCell<Option<Closure<dyn FnMut()>>>>;
    fn request(next: &Frame) {
        let next = next.borrow();
        let cb = next.as_ref().expect("the frame callback");
        let window = window().expect("a window");
        let _ = window.request_animation_frame(cb.as_ref().unchecked_ref());
    }
    let next: Frame = Rc::new(RefCell::new(None));
    let again = next.clone();
    *next.borrow_mut() = Some(Closure::new(move || {
        f();
        request(&again);
    }));
    request(&next);
}
