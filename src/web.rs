//! The web version: the app in a web page (ratzilla draws ratatui there),
//! with the demo's data (demo.rs) until it connects to brokers itself.
//! The keys, panes and views are the terminal's.

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
use ratzilla::web_sys::{KeyboardEvent, console, window};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;
use web_time::{Duration, Instant};

use crate::app::{App, ConnCtl, Options};
use crate::config::ConnCfg;
use crate::input::{KeyCode, KeyEvent, KeyModifiers};
use crate::mqtt::Cmd;
use crate::store::Store;
use crate::{demo, theme, ui};

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
    let mut m = KeyModifiers::NONE;
    for (on, flag) in [
        (k.shift, KeyModifiers::SHIFT),
        (k.ctrl, KeyModifiers::CONTROL),
        (k.alt, KeyModifiers::ALT),
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
    let ctls = {
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
            .collect()
    };
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
        vec![],
    )));

    let backend = DomBackend::new().expect("a page to draw in");
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
