//! Touch screens. A phone has no keys but its keyboard's, and shows that
//! only for a text field. So a tap on the grid selects what's under it
//! (`App::tap`), a swipe (or the mouse wheel) moves up and down, the page's
//! key bar (#keys) sends the keys a phone lacks, and a hidden text field
//! (#kbd) takes what its keyboard types.

use std::cell::Cell;
use std::rc::Rc;

use ratzilla::web_sys::{
    AddEventListenerOptions, CompositionEvent, Document, Event, EventTarget, HtmlElement, HtmlInputElement, InputEvent,
    MouseEvent, TouchEvent, WheelEvent, window,
};
use wasm_bindgen::JsCast;
use wasm_bindgen::closure::Closure;

use crate::input::{KeyCode, KeyEvent, KeyModifiers};

/// What the page hands the app.
pub enum Act {
    Key(KeyEvent),
    /// A tap on a cell of the grid.
    Tap(u16, u16),
}

pub type Feed = Rc<dyn Fn(Act)>;

fn press(feed: &Feed, code: KeyCode, m: KeyModifiers) {
    feed(Act::Key(KeyEvent::new(code, m)));
}

/// Sets it all up on the page's #app, #keys and #kbd.
pub fn start(feed: Feed) -> Option<()> {
    let doc = window()?.document()?;
    let grid: EventTarget = doc.get_element_by_id("app")?.into();
    let field: HtmlInputElement = doc.get_element_by_id("kbd")?.dyn_into().ok()?;
    taps(&grid, &field, feed.clone());
    swipes(&grid, feed.clone());
    key_bar(&doc, &field, feed.clone());
    typing(&field, feed);
    Some(())
}

/// Brings up the phone's keyboard (the hidden field takes the focus).
pub fn keyboard() {
    let field = window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id("kbd"))
        .and_then(|e| e.dyn_into::<HtmlInputElement>().ok());
    if let Some(f) = field {
        let _ = f.focus();
    }
}

/// Calls `f` on the target's `kind` events for as long as the page is
/// open; `f` may cancel them (the listener isn't passive).
fn listen(target: &EventTarget, kind: &str, f: impl FnMut(Event) + 'static) {
    let cb = Closure::<dyn FnMut(Event)>::new(f);
    let opts = AddEventListenerOptions::new();
    opts.set_passive(false);
    let _ = target.add_event_listener_with_callback_and_add_event_listener_options(
        kind,
        cb.as_ref().unchecked_ref(),
        &opts,
    );
    cb.forget();
}

/// The grid's cell size in pixels, from one of its cells.
fn cell_size(doc: &Document) -> Option<(f64, f64, f64, f64)> {
    let r = doc.query_selector("#app pre span").ok()??.get_bounding_client_rect();
    (r.width() > 0.0 && r.height() > 0.0).then(|| (r.left(), r.top(), r.width(), r.height()))
}

/// The grid cell at page point `x`, `y`, and how many rows the grid has.
fn cell_at(x: f64, y: f64) -> Option<(u16, u16, u16)> {
    let doc = window()?.document()?;
    let (left, top, w, h) = cell_size(&doc)?;
    let (col, row) = ((x - left) / w, (y - top) / h);
    let rows = doc.query_selector_all("#app pre").ok()?.length() as u16;
    (col >= 0.0 && row >= 0.0).then_some((col as u16, row as u16, rows))
}

/// A tap selects, and one on the filter line at the bottom brings up the
/// keyboard to type into it.
fn taps(grid: &EventTarget, field: &HtmlInputElement, feed: Feed) {
    let field = field.clone();
    listen(grid, "click", move |e| {
        let e: MouseEvent = e.unchecked_into();
        let Some((x, y, rows)) = cell_at(e.client_x() as f64, e.client_y() as f64) else {
            return;
        };
        feed(Act::Tap(x, y));
        if y + 4 >= rows {
            let _ = field.focus();
        }
    });
}

/// A swipe moves a row per row's height it travels: up shows what's below,
/// as a page scrolls. The wheel likewise.
fn swipes(grid: &EventTarget, feed: Feed) {
    let last = Rc::new(Cell::new(0.0));
    let moved = Rc::new(Cell::new(0.0));
    let (at, gone) = (last.clone(), moved.clone());
    listen(grid, "touchstart", move |e| {
        let e: TouchEvent = e.unchecked_into();
        if let Some(t) = e.touches().get(0) {
            at.set(t.client_y() as f64);
            gone.set(0.0);
        }
    });
    let step = |feed: &Feed, moved: &Cell<f64>, by: f64| {
        let Some(h) = window()
            .and_then(|w| w.document())
            .and_then(|d| cell_size(&d))
            .map(|c| c.3)
        else {
            return;
        };
        moved.set(moved.get() + by);
        while moved.get() >= h {
            moved.set(moved.get() - h);
            press(feed, KeyCode::Down, KeyModifiers::NONE);
        }
        while moved.get() <= -h {
            moved.set(moved.get() + h);
            press(feed, KeyCode::Up, KeyModifiers::NONE);
        }
    };
    let touch_feed = feed.clone();
    listen(grid, "touchmove", move |e| {
        let e: TouchEvent = e.unchecked_into();
        let Some(t) = e.touches().get(0) else { return };
        e.prevent_default(); // the app moves, not the page
        let y = t.client_y() as f64;
        step(&touch_feed, &moved, last.get() - y);
        last.set(y);
    });
    let wheeled = Rc::new(Cell::new(0.0));
    listen(grid, "wheel", move |e| {
        let e: WheelEvent = e.unchecked_into();
        e.prevent_default();
        step(&feed, &wheeled, e.delta_y());
    });
}

/// The key bar's buttons: keys by name, and abc to show or hide the phone's
/// keyboard. A button doesn't take the focus, so the keyboard stays up.
fn key_bar(doc: &Document, field: &HtmlInputElement, feed: Feed) {
    let Ok(buttons) = doc.query_selector_all("#keys button") else {
        return;
    };
    for i in 0..buttons.length() {
        let Some(b) = buttons.get(i).and_then(|n| n.dyn_into::<HtmlElement>().ok()) else {
            continue;
        };
        listen(&b, "pointerdown", |e| e.prevent_default());
        let (name, feed, field) = (b.dataset().get("key").unwrap_or_default(), feed.clone(), field.clone());
        listen(&b, "click", move |_| {
            let ctrl = |c| (KeyCode::Char(c), KeyModifiers::CONTROL);
            let (code, m) = match name.as_str() {
                "esc" => (KeyCode::Esc, KeyModifiers::NONE),
                "tab" => (KeyCode::Tab, KeyModifiers::NONE),
                "left" => (KeyCode::Left, KeyModifiers::NONE),
                "up" => (KeyCode::Up, KeyModifiers::NONE),
                "down" => (KeyCode::Down, KeyModifiers::NONE),
                "right" => (KeyCode::Right, KeyModifiers::NONE),
                "enter" => (KeyCode::Enter, KeyModifiers::NONE),
                "bs" => (KeyCode::Backspace, KeyModifiers::NONE),
                "^e" => ctrl('e'),
                "^s" => ctrl('s'),
                "^t" => ctrl('t'),
                "^y" => ctrl('y'),
                "kbd" => {
                    let typing = field.owner_document().and_then(|d| d.active_element());
                    if typing.is_some_and(|a| a.is_same_node(Some(&field))) {
                        let _ = field.blur();
                    } else {
                        let _ = field.focus();
                    }
                    return;
                }
                _ => return,
            };
            press(&feed, code, m);
        });
    }
}

/// What the phone's keyboard types into the hidden field, as keys. Keys it
/// reports as keys (Enter, often Backspace) the page's key handler has
/// taken already, and they never get here; the rest arrive as text.
fn typing(field: &HtmlInputElement, feed: Feed) {
    let chars = |feed: &Feed, s: &str| {
        for c in s.chars() {
            press(feed, KeyCode::Char(c), KeyModifiers::NONE);
        }
    };
    let (f, input_feed) = (field.clone(), feed.clone());
    listen(field, "input", move |e| {
        let e: InputEvent = e.unchecked_into();
        let kind = e.input_type();
        // A word being composed (predictive text) comes when it's done.
        if e.is_composing() || kind == "insertCompositionText" {
            return;
        }
        if kind.starts_with("delete") {
            press(&input_feed, KeyCode::Backspace, KeyModifiers::NONE);
        } else if let Some(d) = e.data() {
            chars(&input_feed, &d);
        }
        f.set_value("");
    });
    let f = field.clone();
    listen(field, "compositionend", move |e| {
        let e: CompositionEvent = e.unchecked_into();
        if let Some(d) = e.data() {
            chars(&feed, &d);
        }
        f.set_value("");
    });
}
