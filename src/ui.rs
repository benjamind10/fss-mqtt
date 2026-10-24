//! Rendering. Everything is drawn straight into the frame buffer.

use web_time::Duration;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Modifier;
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Chart, Dataset, GraphType, Widget};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Ask, Focus, PanelRow, msg_index, node_title};
use crate::composer::{self, Editor};
use crate::fmt::{self, ago, commas, human_bytes, human_count, msg_count, plural};
use crate::glyph;
use crate::pretty;
use crate::store::{ConnInfo, ConnState, FilterMode, MATCHED, Message, Row, Store};
use crate::theme::Theme;

const ACTIVE_FOR: Duration = Duration::from_millis(1500);

pub fn draw(f: &mut Frame, app: &App) {
    draw_all(f, app);
    steady_cells(f.buffer_mut());
}

/// Replaces what terminals might draw wider or narrower than the buffer
/// assumes (see [`fmt::steady`]); such a cell shifts the rest of its row,
/// which then wraps onto the next line. Payloads, topics and properties can
/// hold any character, so this runs over the whole frame.
fn steady_cells(buf: &mut Buffer) {
    for cell in buf.content.iter_mut() {
        let sym = cell.symbol();
        if sym.is_ascii() {
            continue;
        }
        let mut chars = sym.chars();
        let first = chars.next().filter(|&c| fmt::steady(c));
        if first.is_some() && chars.all(fmt::is_mark) {
            continue;
        }
        // Keep the base of a cluster (an emoji without its joiners and
        // selectors) if it fits the cells the cluster took.
        let fits = first.filter(|c| c.width().unwrap_or(0) <= sym.width());
        let keep = fits.map_or_else(|| "·".to_string(), String::from);
        cell.set_symbol(&keep);
    }
}

fn draw_all(f: &mut Frame, app: &App) {
    let area = f.area();
    if area.width < 10 || area.height < 8 {
        return;
    }
    let st = app.store.lock().unwrap();
    let buf = f.buffer_mut();

    if app.composer.is_some() {
        composer_view(buf, area, app);
        if app.prompt.is_some() {
            prompt_box(buf, area, app, &st);
        }
        return;
    }
    if app.viewer.is_some() {
        viewer(buf, area, app);
        return;
    }

    // The frame's own size: the window may have changed since the app last
    // heard (laying out for the old size would draw past the edge).
    let l = app.layout_for(area.width, area.height);
    put(buf, 0, 0, header(app, &st), area.width);
    panel_box(buf, Rect::new(0, 1, area.width, l.panel_h), app, &st);
    let top = 1 + l.panel_h;
    let tree_area = Rect::new(0, top, l.tree_w, l.tree_h);
    let det_area = if l.stacked {
        Rect::new(0, top + l.tree_h, l.det_w, l.det_h)
    } else {
        Rect::new(l.tree_w, top, l.det_w, l.det_h)
    };
    tree_box(buf, tree_area, app, &st);
    detail_box(buf, det_area, app, &st);
    let y = det_area.bottom();
    input_box(buf, Rect::new(0, y, area.width, 3), app);
    put(buf, 0, y + 3, hints(app), area.width);
    if app.prompt.is_some() {
        prompt_box(buf, area, app, &st);
    }
}

fn status_dot<'a>(th: &Theme, c: &ConnInfo) -> Span<'a> {
    if let Some(f) = &c.file {
        let style = match (&f.err, f.done) {
            (Some(_), _) => th.red,
            (None, false) => th.yellow,
            (None, true) => th.text,
        };
        return Span::styled(glyph::FILE, style);
    }
    match (c.status.state, c.status.err.is_some()) {
        (ConnState::Connected, _) => Span::styled("●", th.green),
        (ConnState::Connecting, false) => Span::styled(glyph::CONNECTING, th.yellow),
        (ConnState::Connecting, true) => Span::styled(glyph::CONNECTING, th.red),
        (ConnState::Disconnected, _) => Span::styled("○", th.dim),
    }
}

fn status_text(c: &ConnInfo) -> String {
    if let Some(f) = &c.file {
        return match (&f.err, f.done) {
            (Some(e), _) => format!("couldn't read · {e}"),
            (None, false) if f.total > 0 => format!("reading {}%", f.read * 100 / f.total),
            (None, false) => "reading…".into(),
            (None, true) if f.skipped > 0 => format!("file · {} lines skipped", fmt::commas(f.skipped as u64)),
            (None, true) => "file".into(),
        };
    }
    match (&c.status.state, &c.status.err) {
        (ConnState::Connected, _) => "connected".into(),
        (ConnState::Connecting, None) => "connecting…".into(),
        (ConnState::Connecting, Some(e)) => format!("retrying · {e}"),
        (ConnState::Disconnected, None) => "disconnected".into(),
        (ConnState::Disconnected, Some(e)) => format!("stopped · {e}"),
    }
}

fn panel_box(buf: &mut Buffer, r: Rect, app: &App, st: &Store) {
    let th = &app.th;
    let (ih, iw) = (r.height.saturating_sub(2) as usize, r.width.saturating_sub(4) as usize);
    let focused = app.focus == Focus::Conns;
    let mut lines = Vec::new();
    if app.panel.is_empty() {
        lines.push(Line::styled(
            format!(
                "No connections. Run fss-mqtt <broker> or add one to {}",
                app.config_path.display()
            ),
            th.dim,
        ));
    }
    // Where the panel was scrolled to, unless the frame is another size
    // than the app has heard of yet: then wherever shows the cursor.
    let offset = if (app.panel_offset..app.panel_offset + ih).contains(&app.panel_cursor) {
        app.panel_offset
    } else {
        app.panel_cursor.saturating_sub(ih.saturating_sub(1))
    };
    let name_w = st
        .conns
        .iter()
        .map(|c| fmt::str_width(&c.name))
        .max()
        .unwrap_or(0)
        .min(24);
    let mut sel_line = None;
    for (i, row) in app.panel.iter().enumerate().skip(offset).take(ih) {
        let sel = i == app.panel_cursor;
        if sel {
            sel_line = Some(lines.len());
        }
        let mark = match (sel, focused) {
            (true, true) => Span::styled(glyph::CURSOR, th.accent_b),
            (true, false) => Span::styled("› ", th.dim),
            _ => Span::raw("  "),
        };
        let line = match *row {
            PanelRow::Conn(c) => {
                let ci = &st.conns[c];
                let node = &st.nodes[ci.node];
                let mut left = vec![
                    mark,
                    status_dot(th, ci),
                    Span::raw(" "),
                    Span::styled(fmt::pad(&ci.name, name_w), if sel { th.bold } else { th.text }),
                    Span::raw("  "),
                    Span::styled(ci.url.clone(), th.dim),
                ];
                if let Some(u) = &ci.username {
                    left.push(Span::styled(format!("  {u}"), th.dim));
                }
                let failed = ci.status.err.is_some() || ci.file.as_ref().is_some_and(|f| f.err.is_some());
                let state_style = match (ci.status.state, failed) {
                    (_, true) => th.red,
                    (ConnState::Connected, _) => th.green,
                    _ => th.dim,
                };
                left.push(Span::styled(format!("  {}", status_text(ci)), state_style));
                join_lr(left, vec![Span::styled(msg_count(node.sub_msgs), th.faint)], iw)
            }
            PanelRow::Sub(c, j) => {
                let s = &st.conns[c].subs[j];
                let mut spans = vec![
                    mark,
                    Span::raw("    "),
                    if s.enabled {
                        Span::styled(glyph::ON, th.green)
                    } else {
                        Span::styled(glyph::OFF, th.dim)
                    },
                    Span::styled(s.topic.clone(), if s.enabled { th.text } else { th.dim }),
                ];
                if let Some(e) = &s.err {
                    spans.push(Span::styled(format!("  refused: {e}"), th.red));
                }
                Line::from(spans)
            }
            PanelRow::Capture(c, j) => capture_row(th, &st.conns[c].capture, j, vec![mark, Span::raw("    ")]),
        };
        lines.push(line);
    }
    let up = st.live().filter(|c| c.status.state == ConnState::Connected).count();
    let brokers = st.live().filter(|c| c.file.is_none()).count();
    let files = st.live().count() - brokers;
    let title = match files {
        0 => format!("Connections {up}/{brokers}"),
        1 => format!("Connections {up}/{brokers} · 1 file"),
        n => format!("Connections {up}/{brokers} · {n} files"),
    };
    draw_box(buf, r, &title, lines, focused, sel_line, th);
    // Rows scrolled out of view, on the left of the bottom border.
    let (above, below) = (offset, app.panel.len().saturating_sub(offset + ih));
    let more = match (above, below) {
        (0, 0) => String::new(),
        (0, b) => format!(" ↓ {b} more "),
        (a, 0) => format!(" ↑ {a} more "),
        (a, b) => format!(" ↑ {a} · ↓ {b} more "),
    };
    let mut used = fmt::str_width(&more);
    if used > 0 && r.height > 2 && r.intersection(buf.area) == r && r.width as usize > used + 4 {
        buf.set_string(r.x + 2, r.bottom() - 1, &more, th.dim);
    } else {
        used = 0; // not drawn: the path may have the room
    }
    // Where the connections live (edits to the file apply while running),
    // on the right, in what room that leaves.
    border_label(buf, r, &crate::config::tilde(&app.config_path), used, th.faint);
}

/// A capture rule after `lead`: `● capture site/# → file.jsonl · 12 msgs`.
fn capture_row<'a>(th: &Theme, cap: &crate::capture::Capture, j: usize, lead: Vec<Span<'a>>) -> Line<'a> {
    let r = &cap.rules[j];
    let mut spans = lead;
    spans.extend([
        if r.enabled {
            Span::styled("● ", th.red)
        } else {
            Span::styled("○ ", th.dim)
        },
        Span::styled("capture ", if r.enabled { th.text } else { th.dim }),
        Span::styled(r.filter.clone(), if r.enabled { th.text } else { th.dim }),
    ]);
    if !r.enabled {
        spans.push(Span::styled(" · off", th.dim));
    } else if let Some(e) = cap.err() {
        spans.push(Span::styled(format!("  failed: {e}"), th.red));
    } else {
        if let Some(f) = cap.file().as_ref().and_then(|f| f.file_name()) {
            spans.push(Span::styled(format!(" → {}", f.to_string_lossy()), th.dim));
        }
        let count = match r.written() {
            0 => "no messages yet".into(),
            n => msg_count(n),
        };
        spans.push(Span::styled(format!(" · {count}"), th.faint));
    }
    Line::from(spans)
}

/// Writes `label` into the bottom border of `r`, right-aligned, keeping its
/// end (the file name) when it has to be cut; `taken` cells on the left of
/// the border are kept clear.
fn border_label(buf: &mut Buffer, r: Rect, label: &str, taken: usize, style: ratatui::style::Style) {
    let room = (r.width as usize).saturating_sub(8 + taken);
    if room < 12 || r.height < 2 {
        return;
    }
    if r.intersection(buf.area) != r {
        return;
    }
    let text = format!(" {} ", fmt::trunc_left(label, room));
    let x = r.right() - 2 - fmt::str_width(&text) as u16;
    buf.set_string(x, r.bottom() - 1, text, style);
}

fn prompt_box(buf: &mut Buffer, area: Rect, app: &App, st: &Store) {
    let th = &app.th;
    let p = app.prompt.as_ref().unwrap();
    let c = &st.conns[p.conn];
    let w = area.width.clamp(20, 64).min(area.width); // never wider than the screen
    let r = Rect::new((area.width - w) / 2, area.height / 3, w, 7);
    for y in r.y..r.bottom().min(area.height) {
        for x in r.x..r.right() {
            buf[(x, y)].reset();
        }
    }
    let (who, label) = match p.ask {
        Ask::Password => (
            format!("{}@{}", c.username.as_deref().unwrap_or(""), c.url),
            "Password ",
        ),
        Ask::KeyPassphrase => {
            let key = app.ctls[p.conn].cfg.key.clone().unwrap_or_default();
            (format!("client key {key}"), "Key passphrase ")
        }
    };
    let lines = vec![
        Line::styled(fmt::trunc_left(&who, w as usize - 4), th.dim),
        match p.err {
            Some(e) => Line::styled(e, th.red),
            None => Line::default(),
        },
        Line::from(vec![
            Span::styled(label, th.text),
            Span::styled("•".repeat(p.input.chars().count()), th.text),
            Span::styled(" ", th.cursor),
        ]),
        Line::default(),
        Line::styled(
            format!("{} connect  ·  esc cancel  ·  kept in memory only", glyph::ENTER),
            th.faint,
        ),
    ];
    draw_box(buf, r, &format!("Connect {}", c.name), lines, true, None, th);
}

fn put(buf: &mut Buffer, x: u16, y: u16, line: Line, w: u16) {
    if y < buf.area.bottom() {
        let line = fmt::fit(line.spans, w as usize);
        buf.set_line(x, y, &line, w);
    }
}

/// Rounded box with a title in the top edge.
/// `sel` is the index of the selected line, drawn as a full-width bar.
fn draw_box(buf: &mut Buffer, r: Rect, title: &str, lines: Vec<Line>, focused: bool, sel: Option<usize>, th: &Theme) {
    // Never past the edge of the screen, whatever the layout says.
    let r = r.intersection(buf.area);
    if r.width < 4 || r.height < 2 {
        return;
    }
    let (bs, ts) = if focused {
        (th.focus_border, th.accent_b)
    } else {
        (th.border, th.dim)
    };
    let inner = (r.width - 4) as usize;
    let mut top = vec![Span::styled("╭─", bs)];
    let mut used = 2;
    if !title.is_empty() {
        let t = format!(" {} ", fmt::trunc_left(title, inner.saturating_sub(2)));
        used += fmt::str_width(&t);
        top.push(Span::styled(t, ts));
    }
    let fill = (r.width as usize).saturating_sub(used + 1);
    top.push(Span::styled("─".repeat(fill) + "╮", bs));
    buf.set_line(r.x, r.y, &Line::from(top), r.width);

    let mut lines = lines.into_iter();
    for y in r.y + 1..r.bottom() - 1 {
        buf.set_string(r.x, y, "│", bs);
        buf.set_string(r.right() - 1, y, "│", bs);
        if let Some(l) = lines.next() {
            put(buf, r.x + 2, y, l, inner as u16);
        }
    }
    buf.set_string(
        r.x,
        r.bottom() - 1,
        format!("╰{}╯", "─".repeat(r.width as usize - 2)),
        bs,
    );
    if let Some(i) = sel
        && (i as u16) < r.height - 2
    {
        let bar = Rect::new(r.x + 1, r.y + 1 + i as u16, r.width - 2, 1);
        buf.set_style(bar, if focused { th.sel } else { th.sel_dim });
    }
}

fn header<'a>(app: &App, st: &Store) -> Line<'a> {
    let th = &app.th;
    let up = st.live().filter(|c| c.status.state == ConnState::Connected).count();
    let failing = st.live().filter(|c| c.status.err.is_some()).count();
    let brokers = st.live().filter(|c| c.file.is_none()).count();
    let mut left = vec![
        Span::raw(" "),
        Span::styled(
            format!("{} {}", glyph::LOGO, crate::brand::name(app.opts.oma)),
            th.accent_b,
        ),
        Span::raw("  "),
        Span::styled(
            format!("{up}/{brokers} connected"),
            if up > 0 { th.green } else { th.dim },
        ),
    ];
    if failing > 0 {
        left.push(Span::styled(format!("  {failing} failing"), th.red));
    }
    if st.live().any(|c| c.capture.active()) {
        left.push(Span::styled("  ● capturing", th.red));
    }
    if let Some(v) = app.upgrade.as_ref().and_then(|w| w.offering()) {
        let v = if v == env!("CARGO_PKG_VERSION") { "new build" } else { v };
        left.push(Span::styled(format!("  ↑ {v} · ^n restart"), th.accent_b));
    } else if let Some(v) = app.restarting_into() {
        left.push(Span::styled(format!("  ↑ {v} · restarting…"), th.accent_b));
    } else if let Some(j) = &app.updating {
        let what = match j.step() {
            crate::update::Step::Working(s) => s,
            _ => "installing".into(),
        };
        left.push(Span::styled(format!("  ↑ {} · {what}…", j.version), th.accent_b));
    } else if let Some(v) = app.update_offer() {
        left.push(Span::styled(format!("  ↑ {v} available · ^n update"), th.accent_b));
    }
    let right = vec![
        Span::styled(commas(st.topic_count() as u64), th.text),
        Span::styled(" topics  ", th.dim),
        Span::styled(human_count(st.total()), th.text),
        Span::styled(" msgs  ", th.dim),
        Span::styled(format!("{:.0}", app.rate), th.text),
        Span::styled("/s ", th.dim),
    ];
    join_lr(left, right, app.width as usize)
}

/// Left spans, then right spans flush right; right is dropped if it doesn't fit.
fn join_lr<'a>(mut left: Vec<Span<'a>>, right: Vec<Span<'a>>, w: usize) -> Line<'a> {
    let lw: usize = left.iter().map(|s| s.width()).sum();
    let rw: usize = right.iter().map(|s| s.width()).sum();
    if lw + rw + 1 > w {
        return Line::from(left);
    }
    left.push(Span::raw(" ".repeat(w - lw - rw)));
    left.extend(right);
    Line::from(left)
}

fn tree_box(buf: &mut Buffer, r: Rect, app: &App, st: &Store) {
    let th = &app.th;
    let (ih, iw) = (r.height.saturating_sub(2) as usize, r.width.saturating_sub(4) as usize);
    let mut lines = Vec::new();
    if app.rows.is_empty() || (st.topic_count() == 0 && app.filter_text.is_empty()) {
        if st.topic_count() == 0 && app.filter_text.is_empty() {
            let up = st.live().any(|c| c.status.state == ConnState::Connected);
            let refused = st.live().flat_map(|c| &c.subs).any(|s| s.err.is_some());
            lines.push(Line::default());
            if refused {
                lines.push(Line::styled(
                    "The broker refused a subscription (see Connections). Pick topics this user may read.",
                    th.red,
                ));
            } else if up {
                lines.push(Line::styled("Waiting for messages …", th.dim));
            } else {
                lines.push(Line::styled(
                    "Not connected. tab to Connections, then space to connect.",
                    th.dim,
                ));
            }
        } else {
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled("Nothing matches ", th.dim),
                Span::styled(app.filter_text.clone(), th.accent),
            ]));
        }
    }
    let end = (app.offset + ih).min(app.rows.len());
    let mut sel_line = None;
    for i in app.offset..end {
        if i == app.cursor {
            sel_line = Some(lines.len());
        }
        lines.push(tree_line(app, st, &app.rows[i], i == app.cursor, iw));
    }
    let title = if app.rows.len() > ih {
        format!("Topics {}/{}", app.cursor + 1, app.rows.len())
    } else {
        "Topics".into()
    };
    draw_box(buf, r, &title, lines, app.focus == Focus::Tree, sel_line, th);
}

fn tree_line<'a>(app: &App, st: &Store, r: &Row, sel: bool, w: usize) -> Line<'a> {
    let th = &app.th;
    let n = &st.nodes[r.node];
    let mut spans = vec![match (sel, app.focus) {
        (true, Focus::Tree) => Span::styled(glyph::CURSOR, th.accent_b),
        (true, _) => Span::styled("› ", th.dim),
        _ => Span::raw("  "),
    }];
    spans.push(Span::raw("  ".repeat(n.depth - 1)));

    if n.is_conn() {
        let c = &st.conns[n.conn];
        spans.push(Span::styled(if r.expanded { "▾ " } else { "▸ " }, th.faint));
        spans.push(status_dot(th, c));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(n.name.clone(), th.bold));
        if c.capture.active() {
            spans.push(Span::styled(" ●", th.red));
        }
        if !r.expanded {
            spans.push(Span::styled(format!(" ({})", n.topics), th.faint));
        }
        return join_lr(spans, vec![Span::styled(human_count(n.sub_msgs), th.faint)], w);
    }

    let active = app
        .now
        .duration_since(n.last_seen)
        .map(|d| d < ACTIVE_FOR)
        .unwrap_or(true);
    let gs = if active { th.green } else { th.faint };
    spans.push(Span::styled(
        if r.expanded {
            "▾ "
        } else if n.has_children() {
            "▸ "
        } else {
            "• "
        },
        gs,
    ));
    spans.extend(node_name(app, &n.name, r.mark, sel));
    if st.conns[n.conn].capture.captures(&n.path) {
        spans.push(Span::styled(" ●", th.red)); // written to disk
    }

    if let Some(last) = n.latest() {
        if last.size <= app.opts.inline_bytes {
            spans.push(Span::styled(" = ", th.faint));
            spans.push(Span::styled(
                fmt::one_line(&last.payload, app.opts.inline_bytes),
                th.value,
            ));
        } else {
            spans.push(Span::styled(format!(" [{}]", human_bytes(last.size)), th.faint));
        }
    }
    if n.has_children() && !r.expanded {
        spans.push(Span::styled(format!(" ({})", n.topics), th.faint));
    }
    join_lr(spans, vec![Span::styled(human_count(n.sub_msgs), th.faint)], w)
}

fn node_name<'a>(app: &App, name: &str, mark: u8, sel: bool) -> Vec<Span<'a>> {
    let th = &app.th;
    let base = if sel { th.bold } else { th.text };
    if name.is_empty() {
        return vec![Span::styled(glyph::EMPTY, th.dim.add_modifier(Modifier::ITALIC))];
    }
    if mark & MATCHED == 0 {
        return vec![Span::styled(name.to_string(), base)];
    }
    if app.filter.mode == FilterMode::Pattern {
        return vec![Span::styled(name.to_string(), th.matched)];
    }
    // Search: underline each occurrence of the query.
    let q = app.filter_text.to_lowercase();
    let lower = name.to_lowercase();
    if lower.len() != name.len() || q.is_empty() {
        return vec![Span::styled(name.to_string(), th.matched)];
    }
    let hl = th.matched.add_modifier(Modifier::UNDERLINED);
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(j) = lower[i..].find(&q) {
        if j > 0 {
            out.push(Span::styled(name[i..i + j].to_string(), base));
        }
        out.push(Span::styled(name[i + j..i + j + q.len()].to_string(), hl));
        i += j + q.len();
    }
    if i < name.len() {
        out.push(Span::styled(name[i..].to_string(), base));
    }
    out
}

/// Rows a chart needs to be worth drawing: its box, a heading, the plot.
const CHART_MIN_H: u16 = 8;

fn detail_box(buf: &mut Buffer, r: Rect, app: &App, st: &Store) {
    let Some(n) = app.selected else {
        draw_box(buf, r, "", vec![], false, None, &app.th);
        return;
    };
    let node = &st.nodes[n];
    // A topic with numbers gives the lower half to their chart.
    let (r, chart) = if app.chart.topic() == Some(n) && app.chart.current().is_some() && r.height >= 2 * CHART_MIN_H {
        let h = r.height / 2;
        (
            Rect {
                height: r.height - h,
                ..r
            },
            Some(Rect {
                y: r.y + r.height - h,
                height: h,
                ..r
            }),
        )
    } else {
        (r, None)
    };
    if let Some(c) = chart {
        chart_box(buf, c, app);
    }
    let (iw, ih) = (r.width.saturating_sub(4) as usize, r.height.saturating_sub(2) as usize);
    let (mut lines, sel) = if node.is_conn() {
        (conn_detail(app, st, n, iw), None)
    } else if node.has_messages() {
        topic_detail(app, st, n, iw, ih)
    } else {
        (branch_detail(app, st, n, iw), None)
    };
    lines.truncate(ih);
    let focused = app.focus == Focus::Msgs;
    draw_box(buf, r, &node_title(st, n), lines, focused, sel, &app.th);
}

/// The topic's details, and which line is the selected message.
fn topic_detail<'a>(app: &App, st: &Store, n: usize, w: usize, h: usize) -> (Vec<Line<'a>>, Option<usize>) {
    let th = &app.th;
    let node = &st.nodes[n];
    let msgs = node.messages();
    let idx = if matches!(app.focus, Focus::Msgs | Focus::Chart) {
        msg_index(&msgs, app.msg_sel)
    } else {
        0
    };
    let cur = &msgs[idx];
    let mut out: Vec<Line> = Vec::new();

    let mut meta = vec![Span::styled(
        format!(
            "{} · last {} · qos {}",
            msg_count(node.msg_count),
            ago(app.now, cur.time),
            cur.qos
        ),
        th.dim,
    )];
    if cur.retain {
        meta.push(Span::styled(" · ", th.dim));
        meta.push(Span::styled("retained", th.yellow));
    }
    if node.has_children() {
        meta.push(Span::styled(
            format!(" · {}", plural(node.num_children(), "child", "children")),
            th.dim,
        ));
    }
    out.push(Line::from(meta));
    out.push(Line::default());

    // Messages
    let mut hdr = vec![Span::styled("Messages", th.section)];
    if app.focus == Focus::Msgs {
        hdr.push(Span::styled(format!("  {}/{}", idx + 1, msgs.len()), th.dim));
    }
    out.push(Line::from(hdr));
    // As many as fit (under the chart, say), the picked one among them.
    let room = h.saturating_sub(out.len() + 1).max(1);
    let shown = msgs.len().min(room);
    let first = idx.saturating_sub(shown / 2).min(msgs.len() - shown);
    let sel_line = out.len() + idx - first;
    for (i, m) in msgs.iter().enumerate().skip(first).take(shown) {
        let (mark, ts) = match (i == idx, app.focus) {
            (true, Focus::Msgs) => (Span::styled(glyph::CURSOR, th.accent_b), th.text),
            (true, _) => (Span::styled("› ", th.faint), th.dim),
            _ => (Span::raw("  "), th.dim),
        };
        let mut spans = vec![
            mark,
            Span::styled(fmt::clock(m.time, false), ts),
            Span::raw(" "),
            if m.retain {
                Span::styled("R", th.yellow)
            } else {
                Span::raw(" ")
            },
            Span::raw(" "),
            Span::styled(format!("{:>8}", human_bytes(m.size)), th.dim),
            Span::raw("  "),
        ];
        let used: usize = spans.iter().map(|s| s.width()).sum();
        if w > used {
            spans.push(Span::styled(fmt::one_line(&m.payload, (w - used) * 4), th.value));
        }
        out.push(Line::from(spans));
    }
    out.push(Line::default());

    // Properties
    let props = message_props(cur);
    if !props.is_empty() {
        out.push(Line::styled("Properties", th.section));
        let kw = props.iter().map(|p| p.0.len()).max().unwrap_or(0).min(w / 2);
        for (k, v) in props {
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(fmt::pad(&k, kw), th.purple),
                Span::raw("  "),
                Span::styled(fmt::one_line(v.as_bytes(), 512), th.text),
            ]));
        }
        out.push(Line::default());
    }

    // Payload, formatted, in whatever room is left.
    let format = pretty::detect(cur);
    out.push(Line::from(vec![
        Span::styled("Payload ", th.section),
        Span::styled(format!("· {} · {}", format.label(), human_bytes(cur.size)), th.dim),
    ]));
    let indent = |mut l: Line<'a>| {
        l.spans.insert(0, Span::raw("  "));
        l
    };
    let body = pretty::render(cur, th, w.saturating_sub(2));
    let room = h.saturating_sub(out.len());
    if body.len() <= room {
        out.extend(body.into_iter().map(indent));
    } else {
        let show = room.saturating_sub(1);
        let more = body.len() - show;
        out.extend(body.into_iter().take(show).map(indent));
        out.push(Line::styled(
            format!("  … {more} more lines · {} to view all", glyph::ENTER),
            th.faint,
        ));
    }
    (out, Some(sel_line))
}

/// The selected topic's numbers: the one picked (`↑↓` with the chart in
/// focus), where it comes from and its range, the list to pick from, and
/// the plot.
fn chart_box(buf: &mut Buffer, r: Rect, app: &App) {
    let th = &app.th;
    let ch = &app.chart;
    let Some(s) = ch.current() else { return };
    draw_box(buf, r, "Chart", vec![], app.focus == Focus::Chart, None, th);
    let inner =
        Rect::new(r.x + 2, r.y + 1, r.width.saturating_sub(4), r.height.saturating_sub(2)).intersection(buf.area);
    if inner.width < 16 || inner.height < 4 {
        return;
    }
    let body = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
    // With a choice and room for it, the list to pick from on the left.
    let mut plot = body;
    if ch.series.len() > 1 && body.width >= 50 {
        let longest = ch.series.iter().map(|s| fmt::str_width(&s.name)).max().unwrap_or(0);
        let lw = (longest + 2).clamp(8, body.width as usize / 3) as u16;
        let rows = body.height as usize;
        let first = ch.sel.saturating_sub(rows.saturating_sub(1));
        for (i, x) in ch.series.iter().enumerate().skip(first).take(rows) {
            let (mark, style) = match (i == ch.sel, app.focus == Focus::Chart) {
                (true, true) => (Span::styled(glyph::CURSOR, th.accent_b), th.text),
                (true, false) => (Span::styled("› ", th.faint), th.text),
                _ => (Span::raw("  "), th.dim),
            };
            let line = Line::from(vec![mark, Span::styled(x.name.clone(), style)]);
            put(buf, body.x, body.y + (i - first) as u16, line, lw);
        }
        for y in body.y..body.bottom() {
            buf.set_string(body.x + lw, y, "│", th.border);
        }
        plot = Rect {
            x: body.x + lw + 2,
            width: body.width - lw - 2,
            ..body
        };
    }
    // A point to a dot column (two to a cell, less the y labels'); more
    // than that scroll.
    ch.room.set((plot.width as usize).saturating_sub(8).max(1) * 2);
    let Some((start, shown)) = ch.in_view() else { return };

    // The latest value first (when it's a live one), where the numbers
    // come from last: a narrow pane cuts that first.
    let (lo, hi) = range(&s.points);
    let num = |v: f64| Span::styled(short(v), th.number);
    let dim = |t: String| Span::styled(t, th.dim);
    let mut head = vec![
        Span::styled(s.name.clone(), th.accent_b),
        dim(format!("  {}/{}", ch.sel + 1, ch.series.len())),
    ];
    if let (true, Some(last)) = (s.over_time, s.points.last()) {
        head.extend([dim(" · last ".into()), num(last.1)]);
    }
    head.extend([
        dim(" · min ".into()),
        num(lo),
        dim(" · max ".into()),
        num(hi),
        dim(format!(" · {}", s.source())),
    ]);
    put(buf, inner.x, inner.y, Line::from(head), inner.width);
    // In the bottom border: which of them are in view (by message, or by
    // row number), and what would keep all of a table cut on arrival.
    let mut foot = Vec::new();
    if shown.len() < s.points.len() {
        let (a, b) = if s.over_time {
            (start + 1, start + shown.len())
        } else {
            (shown[0].0 as usize + 1, shown[shown.len() - 1].0 as usize + 1)
        };
        foot.push(format!("{} {}–{}", s.unit, commas(a as u64), commas(b as u64)));
    }
    if ch.reading() {
        foot.push("reading all rows…".into());
    }
    if let Some(size) = s.whole {
        foot.push(format!(
            "--max-payload {} keeps all",
            crate::chart::max_payload_for(size)
        ));
    }
    if !foot.is_empty() {
        border_label(buf, r, &foot.join(" · "), 0, th.dim);
    }

    // The y axis fits what's in view.
    let (x0, x1) = (shown[0].0, shown[shown.len() - 1].0);
    let (vlo, vhi) = range(shown);
    let xb = if x1 > x0 { [x0, x1] } else { [x0 - 1.0, x0 + 1.0] };
    let yb = if vhi > vlo { [vlo, vhi] } else { [vlo - 1.0, vhi + 1.0] };
    let ylabels = [yb[0], (yb[0] + yb[1]) / 2.0, yb[1]].map(|v| Span::styled(short(v), th.dim));
    let (e0, e1) = s.axis.ends(x0, x1);
    let xlabels = [e0, e1].map(|e| Span::styled(e, th.dim));
    // Braille dots: 2×4 to a cell. The Windows console fonts don't have
    // them; half blocks are the finest they draw.
    let marker = if cfg!(windows) {
        Marker::HalfBlock
    } else {
        Marker::Braille
    };
    let mut sets = vec![
        Dataset::default()
            .marker(marker)
            .graph_type(GraphType::Line)
            .style(th.accent_b)
            .data(shown),
    ];
    // Few enough to tell apart: each one marked.
    if shown.len() <= plot.width as usize / 3 {
        sets.push(
            Dataset::default()
                .marker(Marker::Dot)
                .graph_type(GraphType::Scatter)
                .style(th.accent_b)
                .data(shown),
        );
    }
    Chart::new(sets)
        // The first label starts at the y axis rather than ending there.
        .x_axis(
            Axis::default()
                .bounds(xb)
                .labels(xlabels)
                .labels_alignment(Alignment::Right)
                .style(th.faint),
        )
        .y_axis(
            Axis::default()
                .bounds(yb)
                .labels(ylabels)
                .labels_alignment(Alignment::Right)
                .style(th.faint),
        )
        .legend_position(None)
        .render(plot, buf);
}

/// The lowest and highest value.
fn range(points: &[(f64, f64)]) -> (f64, f64) {
    points.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
        (lo.min(p.1), hi.max(p.1))
    })
}

/// A number for an axis or a heading: short, but as precise as it is.
fn short(v: f64) -> String {
    let a = v.abs();
    if a == 0.0 {
        return "0".into();
    }
    if !(1e-3..1e7).contains(&a) {
        return format!("{v:.2e}");
    }
    let s = match a {
        a if a >= 100.0 => format!("{v:.1}"),
        a if a >= 1.0 => format!("{v:.2}"),
        _ => format!("{v:.3}"),
    };
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The MQTT v5 properties worth showing, user properties last.
pub fn message_props(m: &Message) -> Vec<(String, String)> {
    let p = &m.props;
    let mut out = Vec::new();
    if let Some(v) = &p.content_type {
        out.push(("content-type".into(), v.clone()));
    }
    if let Some(v) = p.payload_format {
        out.push(("payload-format".into(), if v == 1 { "utf-8" } else { "bytes" }.into()));
    }
    if let Some(v) = &p.response_topic {
        out.push(("response-topic".into(), v.clone()));
    }
    if let Some(v) = &p.correlation_data {
        let s = if fmt::is_text(v) {
            String::from_utf8_lossy(v).into_owned()
        } else {
            v.iter().map(|b| format!("{b:02x}")).collect()
        };
        out.push(("correlation-data".into(), s));
    }
    if let Some(v) = p.message_expiry {
        out.push(("message-expiry".into(), format!("{v}s")));
    }
    if let Some(v) = p.subscription_id {
        out.push(("subscription-id".into(), v.to_string()));
    }
    out.extend(p.user.iter().cloned());
    out
}

fn conn_detail<'a>(app: &App, st: &Store, n: usize, w: usize) -> Vec<Line<'a>> {
    let th = &app.th;
    let c = &st.conns[st.nodes[n].conn];
    let mut out = vec![
        Line::from(vec![
            status_dot(th, c),
            Span::raw(" "),
            Span::styled(status_text(c), if c.status.err.is_some() { th.red } else { th.text }),
        ]),
        Line::styled(c.url.clone(), th.dim),
    ];
    if let Some(u) = &c.username {
        out.push(Line::styled(format!("user {u}"), th.dim));
    }
    if let Some(f) = &c.file {
        out.push(Line::default());
        out.push(Line::styled("Opened file", th.section));
        out.push(Line::styled(format!("  {}", f.path.display()), th.text));
        out.push(Line::styled(
            format!(
                "  {} · read once, nothing to connect to · ^e sends one to a broker",
                msg_count(st.nodes[n].sub_msgs)
            ),
            th.dim,
        ));
        if let Some(e) = &f.first_skip {
            out.push(Line::styled(
                format!("  skipped {} lines; first: {e}", f.skipped),
                th.yellow,
            ));
        }
        return out;
    }
    out.push(Line::default());
    out.push(Line::styled("Subscriptions", th.section));
    for s in &c.subs {
        let mut spans = vec![
            Span::raw("  "),
            if s.enabled {
                Span::styled(glyph::ON, th.green)
            } else {
                Span::styled(glyph::OFF, th.dim)
            },
            Span::styled(s.topic.clone(), th.text),
        ];
        if let Some(e) = &s.err {
            spans.push(Span::styled(format!("  refused: {e}"), th.red));
        }
        out.push(Line::from(spans));
    }
    if !c.capture.rules.is_empty() {
        out.push(Line::default());
        out.push(Line::styled("Capture to disk", th.section));
        for j in 0..c.capture.rules.len() {
            out.push(capture_row(th, &c.capture, j, vec![Span::raw("  ")]));
        }
        match (c.capture.err(), c.capture.file()) {
            (Some(e), _) => out.push(Line::styled(format!("  stopped: {e}"), th.red)),
            (None, Some(f)) => out.push(Line::styled(format!("  {}", f.display()), th.dim)),
            (None, None) => out.push(Line::styled(
                format!("  to {} (from the first message)", st.capture_dir.display()),
                th.dim,
            )),
        }
    }
    out.push(Line::default());
    out.extend(branch_detail(app, st, n, w));
    out
}

fn branch_detail<'a>(app: &App, st: &Store, n: usize, w: usize) -> Vec<Line<'a>> {
    let th = &app.th;
    let node = &st.nodes[n];
    let mut out = vec![
        Line::styled(
            format!(
                "{} · {} · {} · last {}",
                plural(node.topics as usize, "topic", "topics"),
                msg_count(node.sub_msgs),
                plural(node.num_children(), "child", "children"),
                ago(app.now, node.last_seen)
            ),
            th.dim,
        ),
        Line::default(),
        Line::styled("Recently active", th.section),
    ];
    let mut kids: Vec<usize> = node.children().to_vec();
    kids.sort_by(|&a, &b| st.nodes[b].last_seen.cmp(&st.nodes[a].last_seen));
    kids.truncate(12);
    let nw = kids
        .iter()
        .map(|&k| fmt::str_width(&st.nodes[k].name))
        .max()
        .unwrap_or(0)
        .min(w / 2);
    for k in kids {
        let kn = &st.nodes[k];
        let active = app
            .now
            .duration_since(kn.last_seen)
            .map(|d| d < ACTIVE_FOR)
            .unwrap_or(true);
        out.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("• ", if active { th.green } else { th.faint }),
            Span::styled(fmt::pad(&kn.name, nw), th.text),
            Span::raw("  "),
            Span::styled(
                format!("{:<8} {}", ago(app.now, kn.last_seen), msg_count(kn.sub_msgs)),
                th.dim,
            ),
        ]));
    }
    out
}

fn input_box(buf: &mut Buffer, r: Rect, app: &App) {
    if r.intersection(buf.area) != r || r.height < 3 || r.width < 4 {
        return; // doesn't fit (the screen is about to be laid out again)
    }
    let th = &app.th;
    let inner = r.width.saturating_sub(4) as usize;
    let info = match app.filter.mode {
        FilterMode::Pattern => vec![
            Span::styled("pattern · ", th.dim),
            Span::styled(plural(app.matched as usize, "topic", "topics"), th.text),
        ],
        FilterMode::Search => vec![
            Span::styled("search · ", th.dim),
            Span::styled(plural(app.matched as usize, "topic", "topics"), th.text),
        ],
        FilterMode::None => vec![],
    };
    let mut left = vec![Span::styled("> ", th.accent_b)];
    // Vim mode shows an edit cursor only while `/` is filtering.
    let navigating = app.opts.vim && !app.filtering;
    if navigating && app.filter_text.is_empty() {
        left.push(Span::styled(
            "/ to filter  ·  v1/#   sensor/+/temp   or any text",
            th.faint,
        ));
    } else if navigating {
        left.push(Span::styled(app.filter_text.clone(), th.text));
    } else if app.filter_text.is_empty() {
        left.push(Span::styled("T", th.cursor));
        left.push(Span::styled(
            "ype to filter  ·  v1/#   sensor/+/temp   or any text",
            th.faint,
        ));
    } else {
        left.push(Span::styled(app.filter_text.clone(), th.text));
        left.push(Span::styled(" ", th.cursor));
    }
    let line = join_lr(left, info, inner);
    let bs = th.border;
    let bar = "─".repeat(r.width.saturating_sub(2) as usize);
    buf.set_string(r.x, r.y, format!("╭{bar}╮"), bs);
    buf.set_string(r.x, r.y + 1, "│", bs);
    put(buf, r.x + 2, r.y + 1, line, inner as u16);
    buf.set_string(r.right() - 1, r.y + 1, "│", bs);
    buf.set_string(r.x, r.y + 2, format!("╰{bar}╯"), bs);
    // The version sits in the bottom-right of the border.
    let v = format!(" {} ", version(app.opts.oma));
    let vw = v.chars().count() as u16;
    if r.width > vw + 6 {
        buf.set_string(r.right() - 2 - vw, r.y + 2, v, th.faint);
    }
}

/// Key hints in the order given, as many as fit in `width` columns: when
/// they don't all fit, those with the highest rank go first (the rightmost
/// of equal rank first); rank 0 always stays.
fn hint<'a>(th: &Theme, pairs: &[(&str, &str, u8)], width: usize) -> Line<'a> {
    const SEP: &str = " · ";
    let w = |(k, v, _): &(&str, &str, u8)| fmt::str_width(k) + 1 + fmt::str_width(v);
    let mut keep = vec![true; pairs.len()];
    loop {
        let shown: Vec<_> = pairs
            .iter()
            .zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(p, _)| w(p))
            .collect();
        let total = 2 + shown.iter().sum::<usize>() + fmt::str_width(SEP) * shown.len().saturating_sub(1);
        let drop = (0..pairs.len())
            .filter(|&i| keep[i] && pairs[i].2 > 0)
            .max_by_key(|&i| pairs[i].2);
        match drop {
            Some(i) if total > width => keep[i] = false,
            _ => break,
        }
    }
    let mut spans = vec![Span::raw("  ")];
    for (k, v, _) in pairs.iter().zip(&keep).filter(|(_, k)| **k).map(|(p, _)| p) {
        if spans.len() > 1 {
            spans.push(Span::styled(SEP, th.faint));
        }
        spans.push(Span::styled(k.to_string(), th.key));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(v.to_string(), th.dim));
    }
    Line::from(spans)
}

/// Columns left for hints on a bottom line that also shows the version.
fn hint_room(app: &App) -> usize {
    (app.width as usize).saturating_sub(fmt::str_width(&version(app.opts.oma)) + 3)
}

fn version(oma: bool) -> String {
    format!("{} v{}", crate::brand::name(oma), env!("CARGO_PKG_VERSION"))
}

/// The hint line with the version in the bottom-right corner; the version is
/// dropped when the line is too narrow for both.
fn with_version<'a>(app: &App, hints: Line<'a>) -> Line<'a> {
    let v = Span::styled(format!("{} ", version(app.opts.oma)), app.th.faint);
    join_lr(hints.spans, vec![v], app.width as usize)
}

/// The latest notice, in place of the key hints while it shows.
fn notice_line<'a>(app: &App) -> Option<Line<'a>> {
    let (text, error, _) = app.notice.as_ref()?;
    Some(Line::styled(
        format!("  {text}"),
        if *error { app.th.red } else { app.th.yellow },
    ))
}

fn hints<'a>(app: &App) -> Line<'a> {
    if let Some(line) = notice_line(app) {
        return line;
    }
    // Ranks: the new actions stay longest; arrows are the first to go. (The
    // main screen's version sits on the input box, not on this line.)
    let room = (app.width as usize).saturating_sub(1);
    let space = format!("space {}", glyph::ENTER);
    let mut keys: Vec<(&str, &str, u8)> = match (app.opts.vim, app.filtering, app.focus) {
        (true, true, _) => vec![(glyph::ENTER, "keep", 0), ("esc", "clear", 0), ("^w", "level", 2)],
        (true, false, Focus::Conns) => vec![
            ("j k", "move", 3),
            (&space, "connect / on-off", 1),
            ("tab", "pane", 2),
            ("/", "filter", 1),
        ],
        (true, false, Focus::Msgs) => vec![
            ("j k", "messages", 3),
            (glyph::ENTER, "open", 2),
            ("e", "publish", 1),
            ("t y", "copy", 1),
            ("^r", "capture", 1),
            ("h esc", "back", 2),
            ("tab", "pane", 2),
        ],
        (true, false, Focus::Chart) => vec![
            ("j k", "column / field", 3),
            ("h l", "scroll", 2),
            (glyph::ENTER, "open", 2),
            ("e", "publish", 1),
            ("t y", "copy", 1),
            ("esc", "back", 2),
            ("tab", "pane", 2),
        ],
        (true, false, Focus::Tree) => vec![
            ("j k", "move", 3),
            ("h l", "collapse/expand", 3),
            (glyph::ENTER, "open", 2),
            ("/", "filter", 1),
            ("e", "publish", 1),
            ("t y", "copy", 1),
            ("^r", "capture", 1),
            ("esc", "clear filter", 2),
            ("tab", "pane", 2),
        ],
        (false, _, Focus::Conns) => vec![
            ("↑↓", "move", 3),
            (&space, "connect / on-off", 1),
            ("tab", "pane", 2),
            ("type", "filter", 2),
        ],
        (false, _, Focus::Msgs) => vec![
            ("↑↓", "messages", 3),
            (glyph::ENTER, "open", 2),
            ("^e", "publish", 1),
            ("^t ^y", "copy", 1),
            ("^r", "capture", 1),
            ("← esc", "back", 2),
            ("tab", "pane", 2),
        ],
        (false, _, Focus::Chart) => vec![
            ("↑↓", "column / field", 3),
            ("←→", "scroll", 2),
            (glyph::ENTER, "open", 2),
            ("^e", "publish", 1),
            ("^t ^y", "copy", 1),
            ("esc", "back", 2),
            ("tab", "pane", 2),
        ],
        (false, _, Focus::Tree) => vec![
            ("↑↓", "move", 3),
            ("←→", "collapse/expand", 3),
            (glyph::ENTER, "open", 2),
            ("^e", "publish", 1),
            ("^t ^y", "copy", 1),
            ("^r", "capture", 1),
            ("esc", "clear filter", 2),
            ("tab", "pane", 2),
        ],
    };
    // Scrolling the chart, when there's more than fits.
    if app.focus == Focus::Chart && !app.chart.scrollable() {
        keys.retain(|(k, _, _)| !matches!(*k, "←→" | "h l"));
    }
    if app.upgrade.as_ref().is_some_and(|w| w.offering().is_some()) {
        keys.push(("^n", "restart", 0));
    } else if app.updating.is_none() && app.update_offer().is_some() {
        keys.push(("^n", "update", 0));
    }
    if cfg!(target_arch = "wasm32") {
        // A web page isn't quit.
    } else if !app.opts.vim {
        keys.push(("^c", "quit", 0));
    } else if !app.filtering {
        keys.push(("q", "quit", 0));
    }
    hint(&app.th, &keys, room)
}

fn viewer(buf: &mut Buffer, area: Rect, app: &App) {
    let v = app.viewer.as_ref().unwrap();
    let th = &app.th;
    let body_h = app.viewer_body_h();
    let mut lines = v.head.clone();
    let end = (v.scroll + body_h).min(v.lines.len());
    lines.extend(v.lines[v.scroll.min(end)..end].iter().map(|l| {
        if v.hscroll > 0 {
            crate::pretty::skip_cells(l, v.hscroll)
        } else {
            l.clone()
        }
    }));
    let r = Rect::new(0, 0, area.width, area.height - 1);
    draw_box(buf, r, &v.path, lines, true, None, th);

    let mode = format!("view: {}", v.mode_name());
    let mut keys = vec![("↑↓ pgup pgdn", "scroll", 3), ("←→", "older/newer", 2)];
    if v.max_width > v.width_now() {
        keys.push(("shift ←→", "pan", 1));
    }
    keys.extend([
        ("tab", mode.as_str(), 1),
        ("e", "publish", 1),
        ("t y", "copy", 1),
        ("esc", "back", 0),
    ]);
    let lines = (v.lines.len() > body_h && app.notice.is_none())
        .then(|| format!("  {}–{} of {} lines", v.scroll + 1, end, v.lines.len()));
    let room = hint_room(app).saturating_sub(lines.as_deref().map_or(0, fmt::str_width));
    let mut h = notice_line(app).unwrap_or_else(|| hint(th, &keys, room));
    if let Some(lines) = lines {
        h.spans.push(Span::styled(lines, th.faint));
    }
    put(buf, 0, area.height - 1, with_version(app, h), area.width);
}

/// The publish composer: the message as a document over the whole screen.
fn composer_view(buf: &mut Buffer, area: Rect, app: &App) {
    let c = app.composer.as_ref().unwrap();
    let th = &app.th;
    let v = c.view(area.width, area.height);
    let status = c.status(area.width);
    let mut lines: Vec<Line> = status
        .iter()
        .map(|(t, err)| Line::styled(t.clone(), if *err { th.red } else { th.yellow }))
        .collect();
    let ed = &c.ed;
    // Lines before the blank one are headers.
    let headers = c.payload_row().map_or(ed.lines.len(), |r| r - 1);
    for row in ed.top..(ed.top + v.rows).min(ed.lines.len()) {
        let mut spans = vec![Span::styled(format!("{:>w$} ", row + 1, w = v.gutter - 1), th.faint)];
        let (text, more) = editor_row(ed, row, v.cols, row < headers, th);
        let used: usize = text.iter().map(|s| s.width()).sum();
        spans.extend(text);
        if more {
            spans.push(Span::raw(" ".repeat(v.cols.saturating_sub(used))));
            spans.push(Span::styled("›", th.faint));
        }
        lines.push(Line::from(spans));
    }
    let r = Rect::new(0, 0, area.width, area.height - 1);
    draw_box(buf, r, &c.title, lines, true, None, th);

    // The cursor, a block like the filter's.
    let x = ed.cells_to(ed.row, ed.col).checked_sub(ed.left);
    let y = ed.row.checked_sub(ed.top).filter(|&y| y < v.rows);
    if let (Some(x), Some(y)) = (x.filter(|&x| x < v.cols), y) {
        let (cx, cy) = (r.x as usize + 2 + v.gutter + x, r.y as usize + 1 + status.len() + y);
        if cx < area.width as usize && cy < area.height as usize {
            buf[(cx as u16, cy as u16)].set_style(th.cursor);
        }
    }

    border_label(buf, r, &format!("ln {}, col {}", ed.row + 1, ed.col + 1), 0, th.faint);
    let keys = [
        ("^s", "send", 0),
        ("esc", "cancel", 0),
        ("^f", "JSON pretty/compact", 1),
    ];
    let h = hint(th, &keys, hint_room(app));
    put(buf, 0, area.height - 1, with_version(app, h), area.width);
}

/// What shows of line `row` of the editor: `cols` cells from its sideways
/// scroll position, a header's key in purple, tabs as a visible mark. Also
/// whether the line goes on past the right edge.
fn editor_row<'a>(ed: &Editor, row: usize, cols: usize, header: bool, th: &Theme) -> (Vec<Span<'a>>, bool) {
    let line = &ed.lines[row];
    let colon = if header {
        line.iter().position(|&c| c == ':')
    } else {
        None
    };
    let (mut key, mut text) = (String::new(), String::new());
    let (mut x, mut used) = (0, 0);
    let mut more = false;
    for (i, &c) in line.iter().enumerate() {
        let w = composer::cell_width(c);
        let part = if colon.is_some_and(|k| i <= k) {
            &mut key
        } else {
            &mut text
        };
        if x < ed.left {
            // A wide character cut by the left edge: its visible half.
            let shown = (x + w).saturating_sub(ed.left);
            part.extend(std::iter::repeat_n(' ', shown));
            used += shown;
            x += w;
            continue;
        }
        if used + w > cols {
            more = true;
            break;
        }
        part.push(if c == '\t' { composer::TAB } else { c });
        used += w;
        x += w;
    }
    let mut spans = Vec::new();
    if !key.is_empty() {
        spans.push(Span::styled(key, th.purple));
    }
    spans.push(Span::styled(text, th.text));
    (spans, more)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, Mutex};

    use crate::input::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::app::{App, Ask, ConnCtl, Focus, Options, PanelRow};
    use crate::config::{self, ConnCfg};
    use crate::glyph;
    use crate::mqtt::Cmd;
    use crate::store::{ConnState, Props, Store, Sub};
    use crate::theme::Theme;

    fn sub(t: &str) -> Sub {
        Sub {
            topic: t.into(),
            enabled: true,
            err: None,
        }
    }

    /// Two connections: "local" (connected, with data) and "staging"
    /// (disconnected, needs a password). Returns the receivers of their commands.
    fn sample(config_path: PathBuf) -> (App, Vec<Receiver<Cmd>>) {
        app_with(sample_store(), config_path)
    }

    /// local (connected, with messages) and staging (needs a password and
    /// a key passphrase).
    fn sample_store() -> Store {
        let mut st = Store::new(10, 64 * 1024);
        let local = st.add_conn("local", "mqtts://127.0.0.1:8883", None, vec![sub("v1/#"), sub("v2/#")]);
        let staging = st.add_conn(
            "staging",
            "mqtt://staging:1883",
            Some("backend".into()),
            vec![sub("devices/+/up/#")],
        );
        st.conns[local].status.state = ConnState::Connected;
        for i in 0..3 {
            for line in ["line1", "line2", "line10"] {
                let props = Props {
                    content_type: Some("application/json".into()),
                    user: vec![("site".into(), "oslo".into()), ("unit".into(), "C".into())],
                    ..Default::default()
                };
                let p = format!(r#"{{"t":{}.5,"ok":true,"tags":["a","b"],"note":null}}"#, 20 + i);
                st.add(local, &format!("v1/plant/{line}/temp"), p.as_bytes(), 1, false, props);
                st.add(
                    local,
                    &format!("v1/plant/{line}/status"),
                    b"RUNNING",
                    0,
                    false,
                    Props::default(),
                );
            }
        }
        st.add(
            local,
            "v1/camera/frame",
            &vec![0u8; 200_000],
            0,
            false,
            Props::default(),
        );
        st.add(local, "v2/fleet/truck7/gps", b"59.91,10.75", 0, true, Props::default());
        st.add(
            local,
            "v2/fleet/truck7/bin",
            &[0, 1, 2, 0xff, b'h', b'i'],
            0,
            false,
            Props::default(),
        );
        st.add(staging, "devices/d9/up/temp", b"19.0", 0, false, Props::default());
        let ct = |c: &str| Props {
            content_type: Some(c.into()),
            ..Default::default()
        };
        st.add(
            local,
            "v2/scada/online/data",
            br#"<?xml version="1.0"?><DataStatus><StationStatus><StationData><Station id="91" kind="grid"><CommunicationId>91</CommunicationId><Name>gridStation</Name><Power unit="kW">1234.5</Power></Station></StationData></StationStatus></DataStatus>"#,
            0,
            false,
            ct("application/xml"),
        );
        st.add(
            local,
            "v2/report/daily",
            b"turbine;energy_kwh;availability;status\nT01;28450.5;99.2;RUNNING\nT02;27110;98.7;RUNNING\nT03;0;0;IDLE\n",
            0,
            false,
            Props::default(),
        );
        let pq = std::fs::read(format!(
            "{}/tests/fixtures/turbine-snappy.parquet",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let decoded = crate::pretty::decode_on_arrival(Some("application/parquet"), &pq);
        st.add_decoded(
            local,
            "v2/turbine/T01/fastlog",
            &pq,
            0,
            false,
            ct("application/parquet"),
            decoded,
        );

        st
    }

    fn app_with(st: Store, config_path: PathBuf) -> (App, Vec<Receiver<Cmd>>) {
        let mut rxs = Vec::new();
        let mut ctls = Vec::new();
        for (name, user, topics) in [
            ("local", None, vec!["v1/#", "v2/#"]),
            ("staging", Some("backend"), vec!["devices/+/up/#"]),
        ] {
            let (tx, rx) = mpsc::channel();
            rxs.push(rx);
            ctls.push(ConnCtl {
                cfg: ConnCfg {
                    name: name.into(),
                    url: "mqtt://x:1883".into(),
                    username: user.map(String::from),
                    // staging's client key is passphrase-protected.
                    key: user.map(|_| {
                        format!(
                            "{}/tests/fixtures/keys/rsa-legacy-aes256.key",
                            env!("CARGO_MANIFEST_DIR")
                        )
                    }),
                    topics: topics.into_iter().map(String::from).collect(),
                    ..Default::default()
                },
                saved: true,
                tx,
                password: None,
                key_password: None,
            });
        }
        let opts = Options {
            inline_bytes: 64,
            vim: false,
            oma: false,
        };
        let mut app = App::new(
            Arc::new(Mutex::new(st)),
            opts,
            Theme::new(false),
            ctls,
            config_path,
            vec![],
        );
        app.resize(110, 36);
        app.tick();
        (app, rxs)
    }

    #[test]
    fn a_tap_moves_the_cursor_and_a_second_one_enters() {
        let (mut app, _rx) = sample(PathBuf::new());
        let l = app.layout();
        let top = 1 + l.panel_h;
        // The tree's second row: the cursor goes there.
        app.tap(3, top + 2);
        assert_eq!((app.focus, app.cursor), (Focus::Tree, 1));
        let row = &app.rows[1];
        let (expanded, kids) = (row.expanded, app.store.lock().unwrap().nodes[row.node].has_children());
        assert!(kids, "{}", frame(&app, "tapped"));
        // Again: Enter, which opens or closes it.
        app.tap(3, top + 2);
        assert!(app.viewer.is_some() || app.rows[1].expanded != expanded);
        // The panel's second row (local's first subscription).
        app.viewer = None;
        app.tap(5, 3);
        assert_eq!((app.focus, app.panel_cursor), (Focus::Conns, 1));
        // A border does nothing.
        app.tap(0, top);
        assert_eq!(app.focus, Focus::Conns);
    }

    fn typ(app: &mut App, s: &str) {
        for c in s.chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }
    fn press(app: &mut App, codes: &[KeyCode]) {
        for &c in codes {
            app.key(KeyEvent::new(c, KeyModifiers::NONE));
        }
    }

    fn frame(app: &App, label: &str) -> String {
        let mut t = Terminal::new(TestBackend::new(app.width, app.height)).unwrap();
        t.draw(|f| super::draw(f, app)).unwrap();
        let buf = t.backend().buffer();
        let mut s = String::new();
        for y in 0..buf.area.height {
            let mut line = String::new();
            for x in 0..buf.area.width {
                line.push_str(buf[(x, y)].symbol());
            }
            s.push_str(line.trim_end());
            s.push('\n');
        }
        println!("── {label} ──\n{s}");
        s
    }

    /// Renders a frame as coloured HTML, to check the theme by eye.
    fn html(app: &App, light: bool) -> String {
        use ratatui::style::{Color, Modifier};
        let mut t = Terminal::new(TestBackend::new(app.width, app.height)).unwrap();
        t.draw(|f| super::draw(f, app)).unwrap();
        let buf = t.backend().buffer();
        let (bg0, fg0) = if light {
            ("#ffffff", "#1f1f1f")
        } else {
            ("#1a1a1a", "#e8e6e3")
        };
        let css = |c: Color, def: &str| match c {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            Color::Indexed(n @ 16..=231) => {
                let lv = |i: u8| [0u8, 95, 135, 175, 215, 255][i as usize];
                let i = n - 16;
                format!("#{:02x}{:02x}{:02x}", lv(i / 36), lv(i / 6 % 6), lv(i % 6))
            }
            Color::Indexed(n @ 232..) => {
                let v = 8 + 10 * (n - 232);
                format!("#{v:02x}{v:02x}{v:02x}")
            }
            _ => def.to_string(),
        };
        let mut out = format!(
            "<pre style=\"background:{bg0};color:{fg0};font:14px/1.25 Menlo,monospace;padding:12px;margin:0;display:inline-block\">"
        );
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let c = &buf[(x, y)];
                let sym = match c.symbol() {
                    "<" => "&lt;",
                    ">" => "&gt;",
                    "&" => "&amp;",
                    s => s,
                };
                let mut st = format!("color:{};background:{}", css(c.fg, fg0), css(c.bg, "transparent"));
                if c.modifier.contains(Modifier::BOLD) {
                    st += ";font-weight:bold";
                }
                if c.modifier.contains(Modifier::UNDERLINED) {
                    st += ";text-decoration:underline";
                }
                if c.modifier.contains(Modifier::REVERSED) {
                    st = format!("color:{bg0};background:{fg0}");
                }
                out += &format!("<span style=\"{st}\">{sym}</span>");
            }
            out.push('\n');
        }
        out + "</pre>"
    }

    /// Writes a few screens as HTML to $FSS_SNAPSHOT_DIR.
    /// Run with: FSS_SNAPSHOT_DIR=/tmp/snap cargo test snapshots -- --ignored
    #[test]
    #[ignore]
    fn snapshots() {
        let dir = PathBuf::from(std::env::var("FSS_SNAPSHOT_DIR").expect("set FSS_SNAPSHOT_DIR"));
        std::fs::create_dir_all(&dir).unwrap();
        for (light, ansi256) in [(false, false), (true, false), (false, true)] {
            let (mut app, _rx) = sample(tmp_config("snap"));
            app.th = Theme::new(light);
            if ansi256 {
                app.th.use_ansi256();
            }
            let mode = match (light, ansi256) {
                (_, true) => "dark256",
                (true, _) => "light",
                _ => "dark",
            };
            typ(&mut app, "v1/+/line1");
            press(&mut app, &[KeyCode::Down, KeyCode::Down]);
            std::fs::write(dir.join(format!("{mode}-1-tree.html")), html(&app, light)).unwrap();
            press(&mut app, &[KeyCode::Right, KeyCode::Down]);
            std::fs::write(dir.join(format!("{mode}-2-messages.html")), html(&app, light)).unwrap();
            for (i, filter) in ["v2/scada/online/data", "daily", "fastlog"].iter().enumerate() {
                press(&mut app, &[KeyCode::Esc, KeyCode::Esc]);
                typ(&mut app, filter);
                let name = filter.replace('/', "-");
                std::fs::write(dir.join(format!("{mode}-4{i}-{name}.html")), html(&app, light)).unwrap();
                press(&mut app, &[KeyCode::Enter]);
                std::fs::write(dir.join(format!("{mode}-5{i}-{name}-full.html")), html(&app, light)).unwrap();
            }
            press(&mut app, &[KeyCode::Esc, KeyCode::Esc, KeyCode::BackTab, KeyCode::Down]);
            assert!(app.focus == crate::app::Focus::Conns);
            std::fs::write(dir.join(format!("{mode}-3-connections.html")), html(&app, light)).unwrap();
        }
    }

    fn tmp_config(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("fss-mqtt-test-{}-{name}.toml", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn omamqtt_goes_by_its_own_name() {
        let (mut app, _rx) = sample(tmp_config("oma"));
        app.opts.oma = true;
        let v = frame(&app, "OmaMQTT");
        let top = v.lines().next().unwrap();
        assert!(top.contains("OmaMQTT") && !top.contains("fss-mqtt"), "{top}");
        assert!(v.contains(&format!(" OmaMQTT v{} ─╯", env!("CARGO_PKG_VERSION"))));
        assert_eq!(app.name(), "OmaMQTT");
    }

    #[test]
    fn filter_and_navigate() {
        let (mut app, _rx) = sample(tmp_config("nav"));
        let v = frame(&app, "start");
        assert!(v.contains("Connections 1/2") && v.contains("local") && v.contains("staging"));
        assert!(v.contains("v1") && v.contains("v2") && v.contains("devices"));
        assert!(
            v.contains(&format!(" {} ─╯", super::version(false))),
            "version in the filter box border"
        );

        typ(&mut app, "v1/+/line1");
        let v = frame(&app, "filter v1/+/line1");
        assert!(v.contains("line1") && !v.contains("line2") && !v.contains("truck7"));

        press(&mut app, &[KeyCode::Down, KeyCode::Down]);
        let path = |a: &App| a.store.lock().unwrap().nodes[a.selected.unwrap()].path.clone();
        assert_eq!(path(&app), "v1/plant/line1/temp");
        let v = frame(&app, "on leaf");
        for want in [
            "local · v1/plant/line1/temp",
            "site",
            "oslo",
            "content-type",
            r#"{"t":22.5"#,
        ] {
            assert!(v.contains(want), "detail pane missing {want}");
        }

        press(&mut app, &[KeyCode::Right, KeyCode::Down, KeyCode::Down]);
        assert!(app.focus == Focus::Msgs);
        frame(&app, "message list");

        press(&mut app, &[KeyCode::Enter]);
        assert!(app.viewer.is_some());
        let v = frame(&app, "viewer");
        assert!(
            v.contains(r#""t": 20.5"#),
            "viewer should pretty-print the oldest message"
        );
        press(&mut app, &[KeyCode::Tab, KeyCode::Tab]);
        frame(&app, "viewer hex");
        press(&mut app, &[KeyCode::Esc, KeyCode::Left]);
        assert!(app.focus == Focus::Tree && app.viewer.is_none());

        press(&mut app, &[KeyCode::Esc]);
        frame(&app, "filter cleared, path kept open");
        assert_eq!(path(&app), "v1/plant/line1/temp");

        // A pattern matches under every connection.
        typ(&mut app, "+/+/up");
        let v = frame(&app, "pattern across brokers");
        assert!(v.contains("staging") && v.contains("d9") && !v.contains("plant"));
        press(&mut app, &[KeyCode::Esc]);

        // Parquet in the viewer: the row table stays on one line per row and pans sideways.
        typ(&mut app, "fastlog");
        press(&mut app, &[KeyCode::Enter]);
        let v = frame(&app, "parquet viewer");
        assert!(v.contains("view: Parquet") && v.contains("shift ←→ pan"));
        assert!(v.contains("TIMESTAMP(ms, UTC)") && v.contains("Rows · first"));
        assert!(
            !v.lines().any(|l| l.trim_start_matches(['│', ' ']).starts_with("10-08")),
            "row table wrapped"
        );
        press(&mut app, &[KeyCode::Char('>')]);
        assert_eq!(app.viewer.as_ref().unwrap().hscroll, 8);
        press(&mut app, &[KeyCode::Esc, KeyCode::Esc]);

        typ(&mut app, "truck");
        press(&mut app, &[KeyCode::Down, KeyCode::Down]);
        frame(&app, "search truck");
        app.resize(60, 30);
        frame(&app, "narrow");
    }

    #[test]
    fn sparkplug_metrics_in_the_details_pane() {
        let (mut app, _rx) = sample(tmp_config("sparkplug"));
        let fixture =
            |n: &str| std::fs::read(format!("{}/tests/fixtures/sparkplug/{n}", env!("CARGO_MANIFEST_DIR"))).unwrap();
        {
            // As a connection's thread decodes them: the birth names the aliases.
            let mut aliases = crate::sparkplug::Aliases::default();
            let mut st = app.store.lock().unwrap();
            for (topic, file) in [
                ("spBv1.0/Plant1/NBIRTH/Gateway1", "nbirth.bin"),
                ("spBv1.0/Plant1/NDATA/Gateway1", "ndata.bin"),
            ] {
                let p = fixture(file);
                let decoded = aliases.decode(topic, &p);
                st.add_decoded(0, topic, &p, 0, false, Props::default(), decoded);
            }
        }
        app.tick();
        typ(&mut app, "NDATA");
        press(&mut app, &[KeyCode::Down]);
        let v = frame(&app, "sparkplug data");
        for want in [
            "Sparkplug B",
            "NDATA node data · 6 metrics",
            "Inputs/Int8",
            "-24",
            "Inputs/Float",
        ] {
            assert!(v.contains(want), "{want:?} missing:\n{v}");
        }
        press(&mut app, &[KeyCode::Enter]);
        let v = frame(&app, "sparkplug data, full screen");
        for want in ["Metrics", "Inputs/String", "world", "historical", "alias 99", "NBIRTH"] {
            assert!(v.contains(want), "{want:?} missing:\n{v}");
        }
    }

    #[test]
    fn odd_characters_stay_in_their_cells() {
        let cfg = tmp_config("odd");
        let (mut app, _rx) = sample(cfg.clone());
        let odd =
            "rtl\u{202E}x\u{200B}y\u{AD}z\u{E000}\u{2028}❤\u{FE0F}👨\u{200D}👩\u{1F3FB}\u{E0041}\u{40000}e\u{301}";
        let props = Props {
            user: vec![(odd.into(), odd.into())],
            ..Props::default()
        };
        app.store
            .lock()
            .unwrap()
            .add(0, &format!("v1/odd{odd}"), odd.as_bytes(), 0, false, props);
        app.tick();
        typ(&mut app, "v1/odd");
        press(&mut app, &[KeyCode::Down]);
        let mut views = vec![frame(&app, "odd characters")];
        press(&mut app, &[KeyCode::Enter]);
        views.push(frame(&app, "odd characters, viewer"));
        for v in views {
            assert!(v.contains("v1/odd") && v.contains("rtl"), "{v}");
            for c in v.chars() {
                assert!(
                    c == '\n' || crate::fmt::steady(c) || crate::fmt::is_mark(c),
                    "{c:?} reached the terminal"
                );
            }
        }
        let _ = std::fs::remove_file(&cfg);
    }

    #[test]
    fn connections_file_reloads_live() {
        let cfg = tmp_config("reload");
        let (mut app, rx) = sample(cfg.clone());
        let (total, staging_msgs) = {
            let st = app.store.lock().unwrap();
            (st.total(), st.nodes[st.conns[1].node].sub_msgs)
        };
        let local = app.ctls[0].cfg.clone();

        // local (connected): v2/# dropped, v3/# added, applied live. staging
        // removed. lab added (no autoconnect, so it stays disconnected).
        let edited = ConnCfg {
            topics: vec!["v1/#".into(), "v3/#".into()],
            ..local.clone()
        };
        let lab = ConnCfg {
            name: "lab".into(),
            url: "mqtt://lab:1883".into(),
            topics: vec!["#".into()],
            ..Default::default()
        };
        // The rate's last sample saw every message, so the removal makes
        // the total smaller than it; the next rate must not underflow.
        app.rate_total = total;
        app.rate_at = web_time::Instant::now() - web_time::Duration::from_secs(2);
        app.apply_config(vec![edited.clone(), lab]);
        app.tick();
        assert_eq!(app.rate, 0.0);
        assert!(matches!(rx[0].try_recv(), Ok(Cmd::Subscribe(t)) if t == "v3/#"));
        assert!(matches!(rx[0].try_recv(), Ok(Cmd::Unsubscribe(t)) if t == "v2/#"));
        assert!(rx[0].try_recv().is_err(), "subscription edits must not reconnect");
        assert!(matches!(rx[1].try_recv(), Ok(Cmd::Disconnect)));
        assert!(rx[1].recv().is_err(), "a removed connection's thread is let go");
        {
            let st = app.store.lock().unwrap();
            assert!(st.conns[1].removed);
            assert_eq!(st.total(), total - staging_msgs, "removed messages leave the totals");
            assert_eq!(st.live().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["local", "lab"]);
            let subs: Vec<_> = st.conns[0].subs.iter().map(|s| s.topic.as_str()).collect();
            assert_eq!(subs, ["v1/#", "v3/#"]);
        }
        let v = frame(&app, "reloaded");
        assert!(v.contains("lab") && v.contains("v3/#") && !v.contains("staging") && !v.contains("v2/#"));
        assert!(v.contains("connections reloaded: 1 added, 1 changed, 1 removed"), "{v}");
        assert!(
            v.contains(&format!("fss-mqtt-test-{}-reload.toml", std::process::id())),
            "path shown"
        );

        // A new address reconnects with the new settings.
        let moved = ConnCfg {
            url: "mqtt://elsewhere:1884".into(),
            ..edited.clone()
        };
        app.apply_config(vec![moved.clone()]);
        assert!(matches!(
            rx[0].try_recv(),
            Ok(Cmd::Reconfigure { cfg, connect: true }) if cfg.host == "elsewhere" && cfg.port == 1884
        ));
        assert!(app.store.lock().unwrap().conns[2].removed, "lab is gone from this file");

        // Through the file: a broken file is reported and changes nothing;
        // fixing it applies it. The same content again is not a change.
        std::fs::write(&cfg, "[[connection]]\nname = \"local\"\nurl = 42\n").unwrap();
        app.check_config();
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|(t, err, _)| *err && t.starts_with("not reloaded"))
        );
        assert_eq!(app.ctls[0].cfg, moved);
        std::fs::remove_file(&cfg).unwrap();
        config::save(
            &cfg,
            &ConnCfg {
                qos: 1,
                ..moved.clone()
            },
        )
        .unwrap();
        app.check_config();
        assert_eq!(app.ctls[0].cfg.qos, 1, "{:?}", app.notice);
        assert!(matches!(rx[0].try_recv(), Ok(Cmd::Reconfigure { cfg, .. }) if cfg.qos == 1));
        app.notice = None;
        app.check_config();
        assert!(app.notice.is_none() && rx[0].try_recv().is_err());

        // An edit landing just before a panel toggle is applied first, not
        // overwritten by the toggle's save.
        let local_now = app.ctls[0].cfg.clone();
        std::fs::remove_file(&cfg).unwrap();
        let mut external = ConnCfg {
            topics: vec!["v1/#".into(), "v3/#".into(), "v4/#".into()],
            ..local_now
        };
        config::save(&cfg, &external).unwrap();
        app.toggle_sub(0, 0); // v1/#, pause
        assert!(
            matches!(rx[0].try_recv(), Ok(Cmd::Subscribe(t)) if t == "v4/#"),
            "edit applied first"
        );
        assert!(
            matches!(rx[0].try_recv(), Ok(Cmd::Unsubscribe(t)) if t == "v1/#"),
            "then the toggle"
        );
        external.paused = vec!["v1/#".into()];
        assert_eq!(
            config::load(&cfg).unwrap(),
            vec![external.clone()],
            "the edit survived the save"
        );
        assert_eq!(app.ctls[0].cfg, external);
        let _ = std::fs::remove_file(&cfg);
    }

    #[test]
    fn reload_leaves_unsaved_alone_and_keeps_empty_password() {
        let cfg = tmp_config("unsaved");
        let (mut app, rx) = sample(cfg.clone());
        // local came from the command line with --no-save: the file's
        // entry of the same name doesn't touch it.
        app.ctls[0].saved = false;
        let file_local = ConnCfg {
            url: "mqtt://from-file:1883".into(),
            ..app.ctls[0].cfg.clone()
        };
        // staging (saved) was connected with an empty password: username only.
        app.ctls[1].password = Some(String::new());
        app.ctls[1].key_password = Some("s3cret".into());
        let staging = ConnCfg {
            url: "mqtt://staging2:1883".into(),
            ..app.ctls[1].cfg.clone()
        };
        app.apply_config(vec![file_local, staging]);
        assert!(rx[0].try_recv().is_err(), "unsaved connection was changed");
        assert_eq!(app.ctls[0].cfg.url, "mqtt://x:1883");
        assert!(!app.ctls[0].saved);
        assert!(matches!(
            rx[1].try_recv(),
            Ok(Cmd::Reconfigure { cfg, .. }) if cfg.host == "staging2" && cfg.password.is_none()
        ));
        let _ = std::fs::remove_file(&cfg);
    }

    #[test]
    fn focus_helpers_and_thread_notices() {
        let (mut app, _rx) = sample(tmp_config("focus"));
        typ(&mut app, "v1/plant/line1");
        press(&mut app, &[KeyCode::Down]);
        let (n, latest) = app.focused_message().expect("latest message of the selected topic");
        assert_eq!(app.focused_node(), Some(n));
        assert_eq!(app.store.lock().unwrap().nodes[n].latest().unwrap().seq, latest.seq);
        // In the message list: the picked one; full screen: the shown one.
        press(&mut app, &[KeyCode::Right, KeyCode::Down]);
        let (_, picked) = app.focused_message().unwrap();
        assert!(picked.seq < latest.seq);
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(app.focused_message().map(|(_, m)| m.seq), Some(picked.seq));
        assert_eq!(app.focused_node(), Some(n));

        // Notices from connection threads show on the next tick; an error
        // isn't hidden by a later success.
        app.store.lock().unwrap().notify("capture failed", true);
        app.store.lock().unwrap().notify("published", false);
        app.tick();
        assert!(matches!(&app.notice, Some((t, true, _)) if t == "capture failed"));
    }

    fn ctrl(app: &mut App, c: char) {
        app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    #[test]
    fn compose_and_publish() {
        let cfg = tmp_config("publish");
        let (mut app, rx) = sample(cfg.clone());
        typ(&mut app, "v1/plant/line1/temp");
        press(&mut app, &[KeyCode::Down]);
        assert!(frame(&app, "tree hints").contains("^e publish"));

        // ctrl+e: a copy of the latest message, every property included.
        ctrl(&mut app, 'e');
        let v = frame(&app, "composer");
        for want in [
            "Publish · copy of the message from",
            "1 topic: v1/plant/line1/temp",
            "connection: local",
            "qos: 1",
            "retain: no",
            "content-type: application/json",
            "user: site = oslo",
            "user: unit = C",
            r#"{"t":22.5,"ok":true,"tags":["a","b"],"note":null}"#,
            "^s send",
        ] {
            assert!(v.contains(want), "{want:?} missing:\n{v}");
        }
        // ctrl+f indents the JSON; ctrl+s sends it on local's channel.
        ctrl(&mut app, 'f');
        let v = frame(&app, "composer, JSON indented");
        assert!(v.contains(r#"  "t": 22.5,"#), "{v}");
        ctrl(&mut app, 's');
        assert!(app.composer.is_none());
        let Ok(Cmd::Publish(o)) = rx[0].try_recv() else {
            panic!("no publish")
        };
        assert_eq!((o.topic.as_str(), o.qos, o.retain), ("v1/plant/line1/temp", 1, false));
        assert_eq!(o.props.content_type.as_deref(), Some("application/json"));
        assert_eq!(
            o.props.user,
            vec![("site".into(), "oslo".into()), ("unit".into(), "C".into())]
        );
        let text = String::from_utf8(o.payload).unwrap();
        assert!(text.starts_with("{\n  \"t\": 22.5,\n  \"ok\": true,"), "{text}");
        assert!(frame(&app, "sent").contains("publishing to local · v1/plant/line1/temp…"));

        // Full screen, e edits the message shown; esc on an untouched copy
        // goes back to it. An edited one needs esc twice, and sends nothing.
        // (A notice takes the full screen's hint line while it shows.)
        app.notice = None;
        press(&mut app, &[KeyCode::Enter]);
        assert!(frame(&app, "viewer").contains("e publish"));
        press(&mut app, &[KeyCode::Char('e')]);
        assert!(app.composer.is_some());
        press(&mut app, &[KeyCode::Esc]);
        assert!(app.composer.is_none() && app.viewer.is_some());
        press(&mut app, &[KeyCode::Char('e')]);
        typ(&mut app, "draft");
        press(&mut app, &[KeyCode::Esc]);
        assert!(frame(&app, "discard?").contains("esc again discards your changes"));
        press(&mut app, &[KeyCode::Esc]);
        assert!(app.composer.is_none() && app.viewer.is_some());
        assert!(rx[0].try_recv().is_err());
        press(&mut app, &[KeyCode::Esc]);

        // staging isn't connected: the composer stays open and says so.
        app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        typ(&mut app, "devices/d9/up/temp");
        press(&mut app, &[KeyCode::Down]);
        ctrl(&mut app, 'e');
        ctrl(&mut app, 's');
        let v = frame(&app, "not connected");
        assert!(
            v.contains("line 2: staging isn't connected; connect it in the panel first"),
            "{v}"
        );
        assert!(rx[1].try_recv().is_err());
        press(&mut app, &[KeyCode::Esc]);

        // On a topic without messages: a new, empty message for it.
        app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        typ(&mut app, "v1/plant");
        ctrl(&mut app, 'e');
        let v = frame(&app, "new message");
        assert!(
            v.contains("Publish · new message") && v.contains("topic: v1/plant"),
            "{v}"
        );
        let _ = std::fs::remove_file(&cfg);
    }

    #[test]
    fn composer_refuses_cut_off_payloads() {
        let (mut app, rx) = sample(tmp_config("compose-cut"));
        typ(&mut app, "v1/camera/frame");
        press(&mut app, &[KeyCode::Down]);
        ctrl(&mut app, 'e');
        let v = frame(&app, "cut off");
        assert!(
            v.contains("only the first 64.0 KB of this 195.3 KB payload were kept"),
            "{v}"
        );
        assert!(v.contains("encoding: base64"), "{v}");
        ctrl(&mut app, 's');
        let v = frame(&app, "cut off, refused");
        assert!(v.contains("restart with --max-payload 256KiB"), "{v}");
        assert!(app.composer.is_some() && rx[0].try_recv().is_err());
    }

    #[test]
    fn composer_says_when_a_connection_has_stopped() {
        let (mut app, rx) = sample(tmp_config("compose-stopped"));
        // local's thread is gone: nothing takes its commands any more.
        drop(rx);
        typ(&mut app, "v1/plant/line1/temp");
        press(&mut app, &[KeyCode::Down]);
        ctrl(&mut app, 'e');
        ctrl(&mut app, 's');
        let v = frame(&app, "connection stopped");
        assert!(
            v.contains("line 2: not sent: local's connection has stopped; restart fss-mqtt to use it again"),
            "{v}"
        );
        assert!(app.composer.is_some(), "the document stays open");
        assert!(!v.contains("publishing to"), "{v}");
    }

    #[test]
    fn composer_scrolls_long_lines() {
        let (mut app, _rx) = sample(tmp_config("compose-scroll"));
        app.resize(40, 20);
        typ(&mut app, "v1/plant/line1/temp");
        press(&mut app, &[KeyCode::Down]);
        ctrl(&mut app, 'e');
        // The payload line is longer than the box: it's marked, and End
        // scrolls to show its end with the cursor in view.
        let v = frame(&app, "long line");
        assert!(v.lines().any(|l| l.contains(r#"{"t":22.5"#) && l.contains('›')), "{v}");
        press(&mut app, &[KeyCode::End]);
        let v = frame(&app, "long line, end");
        assert!(v.contains(r#""note":null}"#), "{v}");
        assert!(v.contains("ln 9, col 50"), "{v}");
    }

    #[test]
    fn copy_topic_and_message() {
        let (mut app, _rx) = sample(tmp_config("copy"));
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let copied = |a: &App| a.clipboard.recorded.clone().unwrap();
        let last = |a: &App| copied(a).last().cloned().unwrap_or_default();
        let notice = |a: &App| a.notice.clone().map(|(t, err, _)| (t, err)).unwrap();
        let select = |a: &mut App, filter: &str| {
            press(a, &[KeyCode::Esc]);
            typ(a, filter);
        };

        // The first row is a connection: there's no topic to copy.
        app.key(ctrl('t'));
        let (t, err) = notice(&app);
        assert!(err && t.contains("connection has no topic"), "{t}");
        assert!(copied(&app).is_empty());

        // A topic: its path without the connection, and the payload as it is.
        select(&mut app, "v1/+/line1/temp");
        app.key(ctrl('t'));
        assert_eq!(last(&app), "v1/plant/line1/temp");
        assert!(notice(&app).0.starts_with("copied topic v1/plant/line1/temp · "));
        app.key(ctrl('y'));
        let payload = r#"{"t":22.5,"ok":true,"tags":["a","b"],"note":null}"#;
        assert_eq!(last(&app), payload);
        let (t, err) = notice(&app);
        assert!(
            !err && t.starts_with(&format!("copied payload, {} B · ", payload.len())),
            "{t}"
        );

        // In the message list: the picked message.
        press(&mut app, &[KeyCode::Right, KeyCode::Down, KeyCode::Down]);
        app.key(ctrl('y'));
        assert_eq!(last(&app), payload.replace("22.5", "20.5"));

        // Full screen: what the view shows, in each mode; the notice shows there.
        press(&mut app, &[KeyCode::Enter]);
        typ(&mut app, "y");
        let pretty = last(&app);
        assert!(
            pretty.starts_with("{\n  \"t\": 20.5,\n  \"ok\": true,") && pretty.ends_with("\n}"),
            "{pretty}"
        );
        let v = frame(&app, "copied full screen");
        assert!(v.contains("copied formatted JSON, "), "notice full screen");
        press(&mut app, &[KeyCode::Tab]);
        typ(&mut app, "y");
        assert_eq!(last(&app), payload.replace("22.5", "20.5"), "raw");
        press(&mut app, &[KeyCode::Tab]);
        typ(&mut app, "y");
        assert!(
            last(&app).starts_with("00000000  7b 22 74 22 3a 32 30 2e"),
            "hex: {}",
            last(&app)
        );
        assert!(notice(&app).0.starts_with("copied hex dump, "));
        typ(&mut app, "t");
        assert_eq!(last(&app), "v1/plant/line1/temp");
        press(&mut app, &[KeyCode::Esc, KeyCode::Esc]);

        // A table full screen gives whole rows, even narrow and panned.
        select(&mut app, "daily");
        app.resize(40, 30);
        press(&mut app, &[KeyCode::Enter, KeyCode::Char('>')]);
        assert!(app.viewer.as_ref().unwrap().hscroll > 0, "panned");
        typ(&mut app, "y");
        let table = last(&app);
        let row = table.lines().find(|l| l.contains("T01")).unwrap_or_default();
        assert!(
            row.contains("28450.5") && row.contains("99.2") && row.contains("RUNNING"),
            "{table}"
        );
        assert!(notice(&app).0.starts_with("copied formatted CSV, "));
        press(&mut app, &[KeyCode::Esc]);
        app.resize(110, 36);

        // Binary elsewhere: decoded formats as their view, the rest as hex.
        select(&mut app, "fastlog");
        app.key(ctrl('y'));
        assert!(notice(&app).0.starts_with("copied formatted Parquet, "));
        assert!(last(&app).contains("TIMESTAMP(ms, UTC)"));
        select(&mut app, "v2/fleet/truck7/bin");
        app.key(ctrl('y'));
        let hex = last(&app);
        assert!(
            hex.starts_with("00000000  00 01 02 ff 68 69 ") && hex.ends_with(" ....hi"),
            "{hex}"
        );
        assert!(notice(&app).0.starts_with("copied hex dump, "));
        // Cut on arrival: what was kept, and the notice says so.
        select(&mut app, "v1/camera/frame");
        app.key(ctrl('y'));
        let (t, _) = notice(&app);
        assert!(
            t.contains("only the first 64.0 KB of 195.3 KB was kept, see --max-payload"),
            "{t}"
        );
        assert_eq!(last(&app).lines().count(), 64 * 1024 / 16);

        // An empty payload: nothing to copy.
        app.store
            .lock()
            .unwrap()
            .add(0, "v1/empty", b"", 0, false, Props::default());
        app.tick();
        let before = copied(&app).len();
        select(&mut app, "v1/empty");
        app.key(ctrl('y'));
        assert_eq!(
            notice(&app),
            ("nothing to copy: the payload is empty".to_string(), false)
        );
        // In the connections panel the keys do nothing.
        app.focus = Focus::Conns;
        app.key(ctrl('t'));
        app.key(ctrl('y'));
        assert_eq!(copied(&app).len(), before);
    }

    #[test]
    fn capture_from_the_tree_and_the_panel() {
        let cfg = tmp_config("capture");
        let (mut app, rx) = sample(cfg.clone());
        let dir = std::env::temp_dir().join(format!("fss-mqtt-test-{}-capture-dir", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        app.store.lock().unwrap().capture_dir = dir.clone();
        let ctrl_r = |app: &mut App| app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        let selected_path = |app: &App| {
            let st = app.store.lock().unwrap();
            st.nodes[app.selected.unwrap()].path.clone()
        };

        // ctrl+r on v1/plant captures its subtree: saved, listed, marked.
        typ(&mut app, "v1/plant");
        assert_eq!(selected_path(&app), "v1/plant");
        ctrl_r(&mut app);
        assert_eq!(config::load(&cfg).unwrap()[0].capture, vec!["v1/plant/#"]);
        assert!(app.panel.contains(&PanelRow::Capture(0, 0)));
        let v = frame(&app, "capture on");
        assert!(v.contains("capture v1/plant/# · no messages yet"), "{v}");
        assert!(v.contains("● capturing"), "header");
        assert!(v.lines().any(|l| l.contains("plant ●")), "tree mark");
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|(t, _, _)| t.contains(&dir.display().to_string()))
        );
        // Below it, no narrower rule.
        press(&mut app, &[KeyCode::Down]);
        assert_eq!(selected_path(&app), "v1/plant/line1");
        ctrl_r(&mut app);
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|(t, _, _)| t == "v1/plant/line1/# is already captured by v1/plant/#")
        );
        assert_eq!(config::load(&cfg).unwrap()[0].capture, vec!["v1/plant/#"]);
        // Nothing on disk until a message arrives, and the connection is untouched.
        assert!(!dir.exists());
        assert!(rx[0].try_recv().is_err());

        // space on the rule in the panel turns it off (saved), and on again.
        app.focus = Focus::Conns;
        app.panel_cursor = app.panel.iter().position(|r| *r == PanelRow::Capture(0, 0)).unwrap();
        press(&mut app, &[KeyCode::Char(' ')]);
        assert_eq!(config::load(&cfg).unwrap()[0].capture_paused, vec!["v1/plant/#"]);
        let v = frame(&app, "capture off");
        assert!(
            v.contains("capture v1/plant/# · off") && !v.contains("● capturing"),
            "{v}"
        );
        press(&mut app, &[KeyCode::Char(' ')]);
        assert!(config::load(&cfg).unwrap()[0].capture_paused.is_empty());
        assert!(app.store.lock().unwrap().conns[0].capture.active());

        // ctrl+r on v1/plant again stops it altogether.
        app.focus = Focus::Tree;
        press(&mut app, &[KeyCode::Up]);
        assert_eq!(selected_path(&app), "v1/plant");
        ctrl_r(&mut app);
        let saved = config::load(&cfg).unwrap();
        assert!(saved[0].capture.is_empty() && saved[0].capture_paused.is_empty());
        assert!(!app.panel.iter().any(|r| matches!(r, PanelRow::Capture(..))));
        assert!(!frame(&app, "capture removed").contains("● capturing"));

        // The connections file can add and remove rules too, live and
        // without touching the connection.
        let (local, staging) = (app.ctls[0].cfg.clone(), app.ctls[1].cfg.clone());
        let only_v2 = ConnCfg {
            capture: vec!["v2/#".into()],
            ..local.clone()
        };
        app.apply_config(vec![only_v2, staging.clone()]);
        assert!(app.store.lock().unwrap().conns[0].capture.captures("v2/fleet"));
        assert!(rx[0].try_recv().is_err(), "capture changes don't reconnect");

        // A write that fails (the capture dir can't be made) stops capture
        // and says so, in the panel and the status line.
        let wait = web_time::Duration::from_secs(5);
        let blocker = dir.with_extension("blocker");
        std::fs::write(&blocker, "x").unwrap();
        app.store.lock().unwrap().capture_dir = blocker.join("captures");
        let target = app
            .store
            .lock()
            .unwrap()
            .capture_target(0, "v2/fleet")
            .expect("captured");
        target.record("v2/fleet", b"1", 0, false, &Props::default());
        let writer = app.store.lock().unwrap().conns[0].capture.writer().unwrap();
        assert!(writer.flush_wait(wait));
        app.tick();
        let v = frame(&app, "capture failed");
        assert!(v.contains("capture v2/#  failed: "), "{v}");
        assert!(
            app.notice
                .as_ref()
                .is_some_and(|(t, err, _)| *err && t.starts_with("capture stopped for local: "))
        );

        let with = ConnCfg {
            capture: vec!["v2/#".into(), "#".into()],
            capture_paused: vec!["#".into()],
            ..local.clone()
        };
        app.apply_config(vec![with, staging.clone()]);
        {
            let st = app.store.lock().unwrap();
            assert!(st.conns[0].capture.err().is_none(), "an edited rule tries again");
            let rules: Vec<_> = st.conns[0]
                .capture
                .rules
                .iter()
                .map(|r| (r.filter.as_str(), r.enabled))
                .collect();
            assert_eq!(rules, [("v2/#", true), ("#", false)]);
        }
        assert!(rx[0].try_recv().is_err(), "capture changes don't reconnect");

        // Quitting waits for what's queued to be written, but not forever.
        app.store.lock().unwrap().capture_dir = dir.clone();
        let target = app
            .store
            .lock()
            .unwrap()
            .capture_target(0, "v2/fleet")
            .expect("captured");
        let go_on = app.store.lock().unwrap().conns[0].capture.writer().unwrap().stall();
        target.record("v2/fleet", b"2", 0, false, &Props::default());
        let start = web_time::Instant::now();
        let late = app.flush_captures(web_time::Duration::from_millis(100));
        assert!(start.elapsed() < web_time::Duration::from_secs(1), "bounded");
        assert!(
            late.len() == 1 && late[0].starts_with("local: ") && late[0].ends_with("the disk is too slow"),
            "{late:?}"
        );
        drop(go_on);
        assert!(app.flush_captures(wait).is_empty());
        let file = app.store.lock().unwrap().conns[0].capture.file().expect("a new file");
        assert!(std::fs::read_to_string(file).unwrap().ends_with("\"payload\":\"2\"}\n"));

        app.apply_config(vec![local, staging]);
        assert!(app.store.lock().unwrap().conns[0].capture.rules.is_empty());
        assert!(rx[0].try_recv().is_err());
        let _ = std::fs::remove_file(&cfg);
        let _ = std::fs::remove_file(&blocker);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_opened_file_is_browsed_and_sent_on_to_a_broker() {
        let (mut app, rx) = sample(tmp_config("open"));
        // A capture file, as the capture writer makes it.
        let path = std::env::temp_dir().join(format!("fss-mqtt-test-{}-open.jsonl", std::process::id()));
        let t = web_time::SystemTime::now() - web_time::Duration::from_secs(3600);
        let props = Props {
            content_type: Some("application/json".into()),
            ..Props::default()
        };
        let text: String = (0..3)
            .map(|i| {
                crate::capture::encode(
                    t,
                    "plant/line9/temp",
                    format!("{{\"t\":{i}}}").as_bytes(),
                    1,
                    false,
                    &props,
                )
            })
            .collect();
        std::fs::write(&path, text).unwrap();
        let idx = {
            let mut st = app.store.lock().unwrap();
            let i = st.add_conn("open.jsonl", "file ~/open.jsonl", None, vec![]);
            st.conns[i].file = Some(crate::store::FileSource {
                path: path.clone(),
                ..Default::default()
            });
            i
        };
        app.ctls.push(ConnCtl {
            cfg: ConnCfg {
                name: "open.jsonl".into(),
                ..Default::default()
            },
            saved: false,
            tx: mpsc::channel().0,
            password: None,
            key_password: None,
        });
        app.refresh_panel();
        crate::session::open(path.clone(), app.store.clone(), idx)
            .join()
            .unwrap();
        app.tick();
        let v = frame(&app, "opened file");
        assert!(
            v.contains(&format!("{} open.jsonl", glyph::FILE)) && v.contains("file ~/open.jsonl"),
            "{v}"
        );
        assert!(
            v.contains("Connections 1/2 · 1 file") && v.contains("1/2 connected"),
            "{v}"
        );
        assert!(v.contains("opened open.jsonl: 3 messages"), "{v}");

        // Its topic: browsed like any other, an hour old.
        typ(&mut app, "plant/line9/temp");
        press(&mut app, &[KeyCode::Down]);
        let v = frame(&app, "file topic");
        assert!(v.contains("3 msgs · last 1h ago · qos 1"), "{v}");
        // Capture doesn't apply; space on it in the panel connects nothing.
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .0
                .contains("is a file: capture works on messages as they arrive")
        );
        app.toggle_conn(idx);
        assert!(app.notice.as_ref().unwrap().0.contains("is a file: it was read once"));

        // ctrl+e sends it on to a broker: the connected one.
        app.key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        let text = app.composer.as_ref().unwrap().ed.text();
        assert!(
            text.contains("connection: local\n") && text.contains("topic: plant/line9/temp\n"),
            "{text}"
        );
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        assert!(matches!(rx[0].try_recv(), Ok(Cmd::Publish(o)) if o.payload == br#"{"t":2}"#));
        // Naming the file as the connection is refused, with the brokers to pick from.
        app.key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        let c = app.composer.as_mut().unwrap();
        let doc = c.ed.text().replace("connection: local", "connection: open.jsonl");
        *c = crate::composer::Composer::restored(&doc, None);
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let err = app.composer.as_ref().unwrap().error.clone().unwrap_or_default();
        assert!(
            err.contains("open.jsonl is a file; send to a broker: local, staging"),
            "{err}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_session_comes_back_as_it_was() {
        let cfg = tmp_config("resume");
        let (mut app, _rx) = sample(cfg.clone());
        // Filtered, in the message list on an older message, a draft open.
        typ(&mut app, "v1/+/line1/temp");
        press(&mut app, &[KeyCode::Right, KeyCode::Down]);
        app.key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        typ(&mut app, "draft!");
        let view = app.session_view();
        assert_eq!(
            (view.filter.as_str(), view.focus.as_str()),
            ("v1/+/line1/temp", "messages")
        );
        assert_eq!(view.selected, Some(("local".into(), "v1/plant/line1/temp".into())));
        assert!(view.draft.as_ref().unwrap().0.contains("draft!"));
        let conns = app.session_conns();
        let up: Vec<_> = conns
            .iter()
            .map(|c| (c.name.as_str(), c.connected, c.config.is_some()))
            .collect();
        assert_eq!(
            up,
            [("local", true, false), ("staging", false, false)],
            "saved entries aren't copied"
        );
        let path = std::env::temp_dir().join(format!("fss-mqtt-test-{}-resume.jsonl.gz", std::process::id()));
        let saved = crate::session::save(&path, &view, &conns, &app.store.lock().unwrap()).unwrap();
        assert_eq!(saved as u64, app.store.lock().unwrap().total());

        // A new run with the same connections replays it.
        let mut st = Store::new(10, 64 * 1024);
        st.add_conn("local", "mqtts://127.0.0.1:8883", None, vec![sub("v1/#"), sub("v2/#")]);
        st.add_conn(
            "staging",
            "mqtt://staging:1883",
            Some("backend".into()),
            vec![sub("devices/+/up/#")],
        );
        let (mut back, _rx2) = app_with(st, cfg.clone());
        let l = crate::session::load(&path).unwrap();
        for r in l.records {
            let i = ["local", "staging"]
                .iter()
                .position(|n| Some(*n) == r.conn.as_deref())
                .unwrap();
            crate::session::restore(&back.store, i, r);
        }
        back.restore_view(&l.header.view);
        back.tick();
        assert_eq!(back.session_view(), view);
        // Times are kept to the millisecond.
        let msgs = |a: &App| -> Vec<(usize, String, Vec<u8>, usize, u8, u128)> {
            let st = a.store.lock().unwrap();
            st.all_messages()
                .into_iter()
                .map(|(c, t, m)| {
                    let ms = m.time.duration_since(web_time::UNIX_EPOCH).unwrap().as_millis();
                    (c, t.to_string(), m.payload.clone(), m.size, m.qos, ms)
                })
                .collect()
        };
        assert!(msgs(&back) == msgs(&app), "same messages, same arrival times");
        // The decoded Parquet view (kept whole on arrival) is there again.
        let pq = |a: &App| {
            let st = a.store.lock().unwrap();
            let n = st.find(0, "v2/turbine/T01/fastlog").unwrap();
            st.nodes[n]
                .latest()
                .unwrap()
                .decoded
                .as_ref()
                .map(|d| (d.format, d.sections.len()))
        };
        assert_eq!(pq(&back), pq(&app));
        assert_eq!(
            back.composer.as_ref().unwrap().ed.text(),
            app.composer.as_ref().unwrap().ed.text()
        );
        // No message rate from the restored messages.
        back.rate_at = web_time::Instant::now() - web_time::Duration::from_secs(2);
        back.tick();
        assert_eq!(back.rate, 0.0);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&cfg);
    }

    #[test]
    fn ctrl_n_restarts_into_a_newer_version() {
        let (mut app, _rx) = sample(tmp_config("restart"));
        let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        app.key(ctrl_n);
        assert!(!app.quit && app.notice.as_ref().unwrap().0 == "no newer fss-mqtt is installed");
        assert!(!frame(&app, "nothing new").contains("^n"));

        app.notice = None;
        let bin = std::env::temp_dir().join(format!("fss-mqtt-test-{}-ctrl-n", std::process::id()));
        std::fs::write(&bin, "v9").unwrap();
        app.upgrade = Some(crate::upgrade::Watch::found(bin.clone(), "9.9.9"));
        assert_eq!(app.upgrade_note().as_deref(), Some("fss-mqtt 9.9.9 is installed"));
        let v = frame(&app, "newer installed");
        assert!(
            v.contains("↑ 9.9.9 · ^n restart") && v.contains("^n restart · ^c quit"),
            "{v}"
        );
        // Changing again since it said its version: not now.
        std::thread::sleep(web_time::Duration::from_millis(20));
        std::fs::write(&bin, "v10, being written").unwrap();
        app.key(ctrl_n);
        assert!(!app.quit && app.notice.as_ref().unwrap().0.contains("changing on disk"));
        app.upgrade = Some(crate::upgrade::Watch::found(bin.clone(), "9.9.9"));
        app.key(ctrl_n);
        assert!(app.restart && app.quit);

        // The same version rebuilt says so.
        app.upgrade = Some(crate::upgrade::Watch::found(bin.clone(), env!("CARGO_PKG_VERSION")));
        assert_eq!(app.upgrade_note().as_deref(), Some("fss-mqtt was rebuilt"));
        assert!(frame(&app, "rebuilt").contains("↑ new build · ^n restart"));
        let _ = std::fs::remove_file(&bin);
    }

    #[test]
    fn ctrl_n_updates_to_a_newer_release() {
        use crate::update::{Checker, Job, Step};
        let (mut app, _rx) = sample(tmp_config("update"));
        let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        let notice = |app: &App| app.notice.as_ref().map(|n| n.0.clone()).unwrap_or_default();

        // Looked, and this is the newest: nothing offered.
        app.release = Some(Checker::found(env!("CARGO_PKG_VERSION")));
        app.key(ctrl_n);
        assert!(notice(&app).ends_with("is the newest"), "{}", notice(&app));
        app.notice = None;
        assert!(!frame(&app, "the newest").contains("^n"));

        // A newer release: offered; ctrl+n installs it the way this copy
        // was installed (a test binary is built from source: it says so).
        app.release = Some(Checker::found("99.0.0"));
        let v = frame(&app, "a release out");
        assert!(
            v.contains("↑ 99.0.0 available · ^n update") && v.contains("^n update · ^c quit"),
            "{v}"
        );
        app.key(ctrl_n);
        assert!(app.updating.is_none() && notice(&app).contains("built from source"));

        // While it installs, the header says so; a failure says why.
        app.updating = Some(Job::ended("99.0.0", Step::Working("downloading 40%".into())));
        assert!(frame(&app, "installing").contains("↑ 99.0.0 · downloading 40%…"));
        app.updating = Some(Job::ended("99.0.0", Step::Failed("no network".into())));
        app.tick();
        assert!(app.updating.is_none() && notice(&app) == "couldn't update: no network");

        // Installed, but nothing watches where this one was started from;
        // not offered again.
        app.updating = Some(Job::ended("99.0.0", Step::Done));
        app.tick();
        assert!(!app.quit && notice(&app).contains("start it again"), "{}", notice(&app));
        app.notice = None;
        assert!(!frame(&app, "installed").contains("^n update"));
        app.key(ctrl_n);
        assert!(app.updating.is_none() && notice(&app).contains("99.0.0 is installed: start it again"));

        // Installed in our place: the restart, continuing the session.
        let bin = std::env::temp_dir().join(format!("fss-mqtt-test-{}-update", std::process::id()));
        std::fs::write(&bin, "v99").unwrap();
        app.upgrade = Some(crate::upgrade::Watch::found(bin.clone(), "99.0.0"));
        app.updating = Some(Job::ended("99.0.0", Step::Done));
        app.tick();
        assert!(app.restart && app.quit);
        let _ = std::fs::remove_file(&bin);
    }

    #[test]
    fn a_frame_of_another_size_than_the_last_resize_still_fits() {
        // The window can change size between the app hearing of it and the
        // next frame (Terminal.app's tab bar appearing takes two rows):
        // every screen must draw inside whatever frame it gets.
        let cfg = tmp_config("sizes");
        let (mut app, _rx) = sample(cfg.clone());
        let draw_at = |app: &App, w: u16, h: u16| {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| super::draw(f, app)).unwrap();
        };
        let sizes = [(184, 55), (184, 57), (110, 36), (79, 40), (60, 20), (20, 9), (10, 8)];
        let check = |app: &mut App, what: &str| {
            for &(aw, ah) in &sizes {
                app.resize(aw, ah);
                for &(w, h) in &sizes {
                    // A panic here names the screen and both sizes.
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| draw_at(app, w, h)));
                    assert!(r.is_ok(), "{what}: laid out for {aw}x{ah}, drawn at {w}x{h}");
                }
            }
            app.resize(110, 36);
        };
        check(&mut app, "tree");
        press(&mut app, &[KeyCode::Tab]);
        check(&mut app, "connections");
        // A password prompt over it (staging needs one).
        press(&mut app, &[KeyCode::End, KeyCode::Up, KeyCode::Char(' ')]);
        assert!(app.prompt.is_some());
        check(&mut app, "prompt");
        press(&mut app, &[KeyCode::Esc, KeyCode::Esc, KeyCode::Esc, KeyCode::Tab]);
        app.prompt = None;
        // Full screen, and the publish editor.
        typ(&mut app, "v1/plant/line1/temp");
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.viewer.is_some());
        check(&mut app, "viewer");
        app.key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(app.composer.is_some());
        check(&mut app, "composer");
        let _ = std::fs::remove_file(&cfg);
    }

    #[test]
    fn connections_panel() {
        let cfg = tmp_config("panel");
        let (mut app, rx) = sample(cfg.clone());

        // tab from the tree (a connection row, no messages) goes to the panel.
        press(&mut app, &[KeyCode::Tab]);
        assert!(app.focus == Focus::Conns);
        frame(&app, "panel focused");

        // Pause v1/# on local: unsubscribe, and saved as paused.
        press(&mut app, &[KeyCode::Down, KeyCode::Char(' ')]);
        assert!(matches!(rx[0].try_recv(), Ok(Cmd::Unsubscribe(t)) if t == "v1/#"));
        assert!(!app.store.lock().unwrap().conns[0].subs[0].enabled);
        let saved = config::load(&cfg).unwrap();
        assert_eq!(saved[0].paused, vec!["v1/#"]);
        let v = frame(&app, "v1 paused");
        assert!(v.contains(&format!("{}v1/#", glyph::OFF)));
        // And back on.
        press(&mut app, &[KeyCode::Enter]);
        assert!(matches!(rx[0].try_recv(), Ok(Cmd::Subscribe(t)) if t == "v1/#"));
        assert!(config::load(&cfg).unwrap()[0].paused.is_empty());

        // local is connected: space on it disconnects.
        press(&mut app, &[KeyCode::Up, KeyCode::Char(' ')]);
        assert!(matches!(rx[0].try_recv(), Ok(Cmd::Disconnect)));

        // staging has an encrypted key and a username: space asks for the
        // key's passphrase (again if it's wrong), then the password.
        press(&mut app, &[KeyCode::End]);
        press(&mut app, &[KeyCode::Up, KeyCode::Char(' ')]);
        assert_eq!(app.prompt.as_ref().map(|p| p.ask), Some(Ask::KeyPassphrase));
        typ(&mut app, "nope");
        press(&mut app, &[KeyCode::Enter]);
        let v = frame(&app, "wrong passphrase");
        assert!(
            v.contains("wrong passphrase") && v.contains("rsa-legacy-aes256.key"),
            "{v}"
        );
        assert!(rx[1].try_recv().is_err(), "connected with a wrong passphrase");
        typ(&mut app, "s3cret");
        press(&mut app, &[KeyCode::Enter]);
        assert_eq!(app.prompt.as_ref().map(|p| p.ask), Some(Ask::Password));
        typ(&mut app, "pw");
        let v = frame(&app, "password prompt");
        assert!(v.contains("Connect staging") && v.contains("••") && !v.contains("pw "));
        press(&mut app, &[KeyCode::Enter]);
        assert!(app.prompt.is_none());
        assert!(matches!(
            rx[1].try_recv(),
            Ok(Cmd::Connect { password: Some(p), key_password: Some(k) }) if p == "pw" && k == "s3cret"
        ));
        let file = std::fs::read_to_string(&cfg).unwrap();
        assert!(!file.contains("s3cret"), "passwords must not be written");
        // Once known, reconnecting doesn't ask again.
        app.store.lock().unwrap().conns[1].status.state = ConnState::Disconnected;
        press(&mut app, &[KeyCode::Char(' ')]);
        assert!(app.prompt.is_none());
        assert!(matches!(rx[1].try_recv(), Ok(Cmd::Connect { .. })));
        // An empty password connects with the username only (no password sent).
        app.ctls[1].password = Some(String::new());
        press(&mut app, &[KeyCode::Char(' ')]);
        assert!(matches!(rx[1].try_recv(), Ok(Cmd::Connect { password: None, .. })));

        // Typing in the panel goes to the filter.
        typ(&mut app, "tr");
        assert!(app.focus == Focus::Tree && app.filter_text == "tr");
        let _ = std::fs::remove_file(&cfg);
    }

    /// `sample` with --vim.
    fn sample_vim(config_path: PathBuf) -> (App, Vec<Receiver<Cmd>>) {
        let (mut app, rx) = sample(config_path);
        app.opts.vim = true;
        (app, rx)
    }

    fn sel_path(a: &App) -> String {
        a.store.lock().unwrap().nodes[a.selected.unwrap()].path.clone()
    }

    #[test]
    fn letters_filter_without_vim() {
        let (mut app, _rx) = sample(tmp_config("novim"));
        typ(&mut app, "j");
        assert_eq!(app.filter_text, "j");
    }

    #[test]
    fn vim_letters_navigate_the_tree() {
        let (mut app, _rx) = sample_vim(tmp_config("vim-tree"));
        let start = sel_path(&app);
        typ(&mut app, "j");
        assert_eq!(app.filter_text, "");
        assert_ne!(sel_path(&app), start);

        typ(&mut app, "G");
        assert_eq!(app.cursor, app.rows.len() - 1);
        typ(&mut app, "g");
        assert_eq!(app.cursor, 0);

        // The first topic under local: l opens it, h closes it.
        typ(&mut app, "j");
        assert!(!app.rows[app.cursor].expanded);
        typ(&mut app, "l");
        assert!(app.rows[app.cursor].expanded);
        typ(&mut app, "h");
        assert!(!app.rows[app.cursor].expanded);
        assert_eq!(app.filter_text, "");
    }

    #[test]
    fn vim_letters_move_the_connections_panel() {
        let (mut app, _rx) = sample_vim(tmp_config("vim-panel"));
        press(&mut app, &[KeyCode::Tab]);
        assert!(app.focus == Focus::Conns);
        let sel = app.selected;
        typ(&mut app, "j");
        assert_eq!(app.panel_cursor, 1);
        assert_eq!(app.selected, sel);
        assert!(app.focus == Focus::Conns);
    }

    #[test]
    fn vim_slash_filters() {
        let (mut app, _rx) = sample_vim(tmp_config("vim-filter"));
        typ(&mut app, "/");
        assert!(app.filtering);
        typ(&mut app, "v1/+/line1");
        assert_eq!(app.filter_text, "v1/+/line1");
        let v = frame(&app, "vim filter v1/+/line1");
        assert!(v.contains("line1") && !v.contains("line2"));

        // ⏎ keeps the filter and goes back to moving.
        press(&mut app, &[KeyCode::Enter]);
        assert!(!app.filtering);
        assert_eq!(app.filter_text, "v1/+/line1");
        let before = sel_path(&app);
        typ(&mut app, "j");
        assert_eq!(app.filter_text, "v1/+/line1");
        assert_ne!(sel_path(&app), before);

        // esc while filtering clears it.
        typ(&mut app, "/");
        press(&mut app, &[KeyCode::Esc]);
        assert!(!app.filtering);
        assert_eq!(app.filter_text, "");

        // A paste goes to the filter, and letters after it too.
        app.paste("v2");
        assert!(app.filtering);
        typ(&mut app, "/fleet");
        assert_eq!(app.filter_text, "v2/fleet");
    }

    #[test]
    fn vim_ctrl_u_pages_unless_filtering() {
        let (mut app, _rx) = sample_vim(tmp_config("vim-ctrl-u"));
        typ(&mut app, "/line1");
        press(&mut app, &[KeyCode::Enter]);
        ctrl(&mut app, 'u');
        assert_eq!(app.filter_text, "line1", "ctrl+u pages up while moving");
        ctrl(&mut app, 'd');
        assert_eq!(app.filter_text, "line1");
        typ(&mut app, "/");
        ctrl(&mut app, 'u');
        assert_eq!(app.filter_text, "", "ctrl+u clears while filtering");
    }

    #[test]
    fn vim_backspace_and_ctrl_w_edit_only_while_filtering() {
        let (mut app, _rx) = sample_vim(tmp_config("vim-backspace"));
        typ(&mut app, "/v1/plant");
        press(&mut app, &[KeyCode::Enter]);
        press(&mut app, &[KeyCode::Backspace]);
        ctrl(&mut app, 'w');
        assert_eq!(app.filter_text, "v1/plant", "kept while moving");
        typ(&mut app, "/");
        press(&mut app, &[KeyCode::Backspace]);
        assert_eq!(app.filter_text, "v1/plan");
        ctrl(&mut app, 'w');
        assert_eq!(app.filter_text, "v1/");
    }

    #[test]
    fn vim_tab_ends_filtering() {
        let (mut app, _rx) = sample_vim(tmp_config("vim-tab"));
        typ(&mut app, "/line1");
        press(&mut app, &[KeyCode::Tab]);
        assert!(app.focus == Focus::Conns && !app.filtering);
        assert_eq!(app.filter_text, "line1", "tab keeps the filter");
        // esc in the panel goes back to the tree rather than clearing.
        press(&mut app, &[KeyCode::Esc]);
        assert!(app.focus == Focus::Tree);
        assert_eq!(app.filter_text, "line1");
        typ(&mut app, "/");
        press(&mut app, &[KeyCode::BackTab]);
        assert!(!app.filtering);
    }

    #[test]
    fn vim_ctrl_d_u_page_the_viewer() {
        let (mut app, _rx) = sample_vim(tmp_config("vim-viewer"));
        typ(&mut app, "/v1/+/line1/temp");
        press(&mut app, &[KeyCode::Enter, KeyCode::Enter]);
        app.resize(app.width, 10); // short enough for the message to need paging
        assert!(app.viewer.is_some());
        ctrl(&mut app, 'd');
        assert!(app.viewer.as_ref().unwrap().scroll > 0, "ctrl+d pages down");
        ctrl(&mut app, 'u');
        assert_eq!(app.viewer.as_ref().unwrap().scroll, 0, "ctrl+u pages up");
    }

    #[test]
    fn vim_q_e_t_y() {
        let (mut app, _rx) = sample_vim(tmp_config("vim-q"));
        typ(&mut app, "q");
        assert!(app.quit);

        let (mut app, _rx) = sample_vim(tmp_config("vim-e"));
        typ(&mut app, "e");
        assert!(app.composer.is_some());

        let (mut app, _rx) = sample_vim(tmp_config("vim-copy"));
        let last = |a: &App| {
            a.clipboard
                .recorded
                .clone()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
        };
        let notice = |a: &App| a.notice.clone().map(|(t, err, _)| (t, err)).unwrap();
        typ(&mut app, "/v1/+/line1/temp");
        press(&mut app, &[KeyCode::Enter]);
        typ(&mut app, "t");
        assert_eq!(last(&app), "v1/plant/line1/temp");
        assert!(notice(&app).0.starts_with("copied topic v1/plant/line1/temp · "));
        typ(&mut app, "y");
        let payload = r#"{"t":22.5,"ok":true,"tags":["a","b"],"note":null}"#;
        assert_eq!(last(&app), payload);
        let (t, err) = notice(&app);
        assert!(
            !err && t.starts_with(&format!("copied payload, {} B · ", payload.len())),
            "{t}"
        );
        assert_eq!(app.filter_text, "v1/+/line1/temp");
    }

    #[test]
    fn vim_hints_and_placeholder() {
        let (app, _rx) = sample(tmp_config("hints-default"));
        let v = frame(&app, "default hints");
        assert!(v.contains("ype to filter") && !v.contains("/ to filter"), "{v}");
        assert!(v.contains("^c quit") && !v.contains("q quit · "), "{v}");

        let (mut app, _rx) = sample_vim(tmp_config("hints-vim"));
        let v = frame(&app, "vim tree hints");
        for want in ["j k move", "/ filter", "q quit", "/ to filter"] {
            assert!(v.contains(want), "{want}: {v}");
        }
        assert!(!v.contains("^c quit") && !v.contains("ype to filter"), "{v}");

        typ(&mut app, "/");
        let v = frame(&app, "vim filtering hints");
        assert!(v.contains(&format!("{} keep · esc clear", glyph::ENTER)), "{v}");
        assert!(!v.contains("j k move") && !v.contains("q quit"), "{v}");
        assert!(v.contains("ype to filter"), "{v}");

        typ(&mut app, "line1");
        press(&mut app, &[KeyCode::Enter, KeyCode::Down]);
        assert!(frame(&app, "vim tree after filter").contains("j k move"));
        press(&mut app, &[KeyCode::Right]);
        let v = frame(&app, "vim msgs hints");
        assert!(v.contains("j k messages") && v.contains("h esc back"), "{v}");
    }

    #[test]
    fn chart_of_the_selected_topic() {
        let (mut app, _rx) = sample(tmp_config("chart"));
        let selected = |app: &App| {
            let st = app.store.lock().unwrap();
            st.nodes[app.selected.unwrap()].path.clone()
        };

        // A JSON field, over the messages kept.
        typ(&mut app, "v1/plant/line1/temp");
        assert_eq!(selected(&app), "v1/plant/line1/temp");
        let v = frame(&app, "chart of t");
        assert!(v.contains("╭─ Chart ─"), "{v}");
        assert!(
            v.contains("t  1/1 · last 22.5 · min 20.5 · max 22.5 · 3 messages"),
            "{v}"
        );
        assert!(
            v.contains("Messages") && v.contains("Properties"),
            "the details keep the upper half:\n{v}"
        );

        // tab: the messages, then the chart; its keys in the hints.
        press(&mut app, &[KeyCode::Tab, KeyCode::Tab]);
        assert!(app.focus == Focus::Chart);
        assert!(frame(&app, "chart in focus").contains("↑↓ column / field"));
        assert_eq!(app.session_view().focus, "chart");
        // Back: shift+tab to the messages, esc from the chart to the tree.
        press(&mut app, &[KeyCode::BackTab]);
        assert!(app.focus == Focus::Msgs);
        press(&mut app, &[KeyCode::Tab, KeyCode::Esc]);
        assert!(app.focus == Focus::Tree);

        // Nothing to chart: no chart, and tab goes past where it would be.
        press(&mut app, &[KeyCode::Esc]);
        typ(&mut app, "v1/plant/line1/status");
        assert!(!frame(&app, "no numbers").contains("Chart"));
        press(&mut app, &[KeyCode::Tab, KeyCode::Tab]);
        assert!(app.focus == Focus::Conns);

        // A Parquet table: a series per number column, picked with ↑↓.
        press(&mut app, &[KeyCode::Tab, KeyCode::Esc]);
        typ(&mut app, "fastlog");
        press(&mut app, &[KeyCode::Tab, KeyCode::Tab]);
        assert!(app.focus == Focus::Chart);
        let first = app.chart.current().unwrap().name.clone();
        press(&mut app, &[KeyCode::Down]);
        let second = app.chart.current().unwrap().name.clone();
        assert_ne!(first, second);
        let v = frame(&app, "parquet columns");
        assert!(v.contains(&format!("{second}  2/{}", app.chart.series.len())), "{v}");
        assert!(
            v.contains(&format!("{}{second}", glyph::CURSOR)),
            "picked in the list:\n{v}"
        );
        press(&mut app, &[KeyCode::End]);
        assert_eq!(app.chart.sel, app.chart.series.len() - 1);
        press(&mut app, &[KeyCode::Home]);
        assert_eq!(app.chart.sel, 0);

        // CSV: the number columns, with the first column along the x axis.
        press(&mut app, &[KeyCode::Esc, KeyCode::Esc]);
        typ(&mut app, "v2/report/daily");
        let v = frame(&app, "csv columns");
        assert!(v.contains("energy_kwh  1/2 · min 0 · max 28450.5 · 3 rows"), "{v}");
        assert!(v.contains("T01") && v.contains("T03"), "{v}");

        // Too little room for both halves: the details only.
        app.resize(110, 24);
        assert!(!frame(&app, "short window").contains("Chart"));
    }

    #[test]
    fn the_picked_message_stays_in_view_above_a_chart() {
        let mut st = Store::new(10, 64 * 1024);
        let c = st.add_conn("local", "mqtt://x", None, vec![sub("#")]);
        for i in 0..10 {
            st.add(
                c,
                "plant/temp",
                format!("{}", 20 + i).as_bytes(),
                0,
                false,
                Props::default(),
            );
        }
        let (mut app, _rx) = app_with(st, tmp_config("chart-list"));
        typ(&mut app, "temp");
        assert!(frame(&app, "chart").contains("╭─ Chart ─"));
        // The oldest message: last in the list, still shown and marked.
        press(&mut app, &[KeyCode::Right, KeyCode::End]);
        let v = frame(&app, "oldest picked");
        let picked = v
            .lines()
            .find(|l| l.contains(glyph::CURSOR) && l.contains(" B "))
            .unwrap_or_default();
        assert!(picked.ends_with("20 │") || picked.contains(" 20 "), "{v}");
        assert!(v.contains("Messages  10/10"), "{v}");
    }

    #[test]
    fn chart_rows_scroll_with_left_and_right() {
        // The sample keeps payloads whole (64 KiB): the Parquet table, cut
        // to its first rows on arrival, is read again with all 500.
        let (mut app, _rx) = sample(tmp_config("chart-scroll"));
        typ(&mut app, "fastlog");
        press(&mut app, &[KeyCode::Tab, KeyCode::Tab]);
        assert!(app.focus == Focus::Chart);
        // Read on a thread of its own: the first rows meanwhile.
        if app.chart.reading() {
            assert!(frame(&app, "reading").contains("reading all rows…"));
        }
        let start = web_time::Instant::now();
        while app.chart.reading() {
            assert!(start.elapsed() < web_time::Duration::from_secs(10), "never read");
            std::thread::sleep(web_time::Duration::from_millis(2));
            app.tick();
        }
        let v = frame(&app, "500 rows, the first in view");
        assert!(v.contains("· 500 rows") && v.contains(" rows 1–"), "{v}");
        assert!(v.contains("←→ scroll") && v.contains("esc back"), "{v}");
        assert_eq!(app.chart.in_view().unwrap().0, 0);

        // → a quarter of the view on, shift+→ a whole one, ← back.
        press(&mut app, &[KeyCode::Right]);
        let quarter = app.chart.in_view().unwrap().0;
        assert!(quarter > 0);
        assert!(frame(&app, "scrolled").contains(&format!(" rows {}–", quarter + 1)));
        app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT));
        assert_eq!(app.chart.in_view().unwrap().0, quarter + app.chart.room.get());
        app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
        press(&mut app, &[KeyCode::Left, KeyCode::Left]);
        assert_eq!(app.chart.in_view().unwrap().0, 0, "no further back than the first");
        // Another column of the same table keeps the place.
        press(&mut app, &[KeyCode::Right, KeyCode::Down]);
        assert_eq!(app.chart.in_view().unwrap().0, quarter);
        press(&mut app, &[KeyCode::Esc]);
        assert!(app.focus == Focus::Tree);

        // Messages kept to 4 KiB: the newest table is kept whole all the
        // same, so its chart has every row.
        let pq = std::fs::read(format!(
            "{}/tests/fixtures/turbine-snappy.parquet",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let table_topic = |budget: usize| {
            let mut st = Store::new(10, 4096);
            st.set_whole_budget(budget);
            let c = st.add_conn("local", "mqtt://x", None, vec![sub("#")]);
            let decoded = crate::pretty::decode_on_arrival(Some("application/parquet"), &pq);
            let props = Props {
                content_type: Some("application/parquet".into()),
                ..Default::default()
            };
            st.add_decoded(c, "turbine/fastlog", &pq, 0, false, props, decoded);
            st
        };
        let (mut app, _rx) = app_with(table_topic(crate::store::WHOLE_BUDGET), tmp_config("chart-kept"));
        typ(&mut app, "fastlog");
        press(&mut app, &[KeyCode::Tab, KeyCode::Tab]);
        let start = web_time::Instant::now();
        while app.chart.reading() {
            assert!(start.elapsed() < web_time::Duration::from_secs(10), "never read");
            std::thread::sleep(web_time::Duration::from_millis(2));
            app.tick();
        }
        let v = frame(&app, "the newest kept whole");
        assert!(v.contains("· 500 rows") && !v.contains("--max-payload"), "{v}");
        press(&mut app, &[KeyCode::Right]);
        assert!(app.chart.in_view().unwrap().0 > 0, "scrolls without --max-payload");

        // Past the budget for whole payloads: the first rows, what would
        // keep them all, and ← → say so rather than nothing.
        let (mut app, _rx) = app_with(table_topic(1024), tmp_config("chart-cut"));
        typ(&mut app, "fastlog");
        press(&mut app, &[KeyCode::Tab, KeyCode::Tab]);
        let v = frame(&app, "cut on arrival");
        assert!(
            v.contains("· first ") && v.contains(" --max-payload 32KiB keeps all "),
            "{v}"
        );
        assert!(!v.contains("←→ scroll"), "nothing to scroll: no such hint\n{v}");
        app.notice = None;
        press(&mut app, &[KeyCode::Right]);
        let n = app.notice.as_ref().map(|n| n.0.clone()).unwrap_or_default();
        assert!(
            n.starts_with("only the first ") && n.ends_with("--max-payload 32KiB keeps them all"),
            "{n}"
        );

        // A chart that's all in view says so.
        let (mut app, _rx) = sample(tmp_config("chart-all-in-view"));
        typ(&mut app, "v1/plant/line1/temp");
        press(&mut app, &[KeyCode::Tab, KeyCode::Tab]);
        frame(&app, "all in view");
        press(&mut app, &[KeyCode::Left]);
        let n = app.notice.as_ref().map(|n| n.0.clone()).unwrap_or_default();
        assert_eq!(n, "all 3 messages are in view: nothing to scroll to");
    }

    #[test]
    fn chart_numbers_read_short() {
        assert_eq!(super::short(0.0), "0");
        assert_eq!(super::short(-0.0), "0");
        assert_eq!(super::short(2130.25), "2130.2");
        assert_eq!(super::short(20.5), "20.5");
        assert_eq!(super::short(12.0), "12");
        assert_eq!(super::short(0.125), "0.125");
        assert_eq!(super::short(-3.5), "-3.5");
        assert_eq!(super::short(28450.5), "28450.5");
        assert_eq!(super::short(1.5e9), "1.50e9");
        assert_eq!(super::short(0.0004), "4.00e-4");
    }

    #[test]
    fn many_connections_scroll_in_a_panel_of_five() {
        let mut st = Store::new(10, 64 * 1024);
        let mut ctls = Vec::new();
        for i in 0..8 {
            let name = format!("site{i}");
            st.add_conn(&name, "mqtt://x:1883", None, vec![sub("#")]);
            ctls.push(ConnCtl {
                cfg: ConnCfg {
                    name,
                    url: "mqtt://x:1883".into(),
                    topics: vec!["#".into()],
                    ..Default::default()
                },
                saved: true,
                tx: mpsc::channel().0,
                password: None,
                key_password: None,
            });
        }
        let opts = Options {
            inline_bytes: 64,
            vim: false,
            oma: false,
        };
        let mut app = App::new(
            Arc::new(Mutex::new(st)),
            opts,
            Theme::new(false),
            ctls,
            tmp_config("many"),
            vec![],
        );
        // However tall the window, five connections' rows; the rest scroll.
        app.resize(110, 60);
        app.tick();
        assert_eq!(app.layout().panel_h, crate::app::PANEL_ROWS + 2);
        let panel = |app: &App, label: &str| {
            let v = frame(app, label);
            let rows = crate::app::PANEL_ROWS as usize + 2;
            v.lines().skip(1).take(rows).collect::<Vec<_>>().join("\n")
        };
        let p = panel(&app, "eight connections");
        assert!(
            p.contains("site0") && p.contains("site4") && !p.contains("site5"),
            "{p}"
        );
        assert!(p.contains("↓ 6 more"), "{p}");

        // In the panel, the end scrolls into view; moving up within the
        // view doesn't scroll it.
        press(&mut app, &[KeyCode::BackTab, KeyCode::End]);
        assert!(app.focus == Focus::Conns);
        let p = panel(&app, "scrolled to the end");
        assert!(
            p.contains("site7") && p.contains("site3") && !p.contains("site2"),
            "{p}"
        );
        assert!(p.contains("↑ 6 more"), "{p}");
        press(&mut app, &[KeyCode::Up, KeyCode::Up, KeyCode::Up]);
        assert_eq!(app.panel_offset, 6, "still in view");
        press(&mut app, &[KeyCode::PageUp]);
        assert!(panel(&app, "a page up").contains("↑ 3 · ↓ 3 more"));
        press(&mut app, &[KeyCode::Home]);
        assert_eq!((app.panel_cursor, app.panel_offset), (0, 0));

        // A short window has fewer rows, and still scrolls.
        app.resize(110, 24);
        press(&mut app, &[KeyCode::End]);
        let p = frame(&app, "short window");
        assert!(p.contains("site7") && p.contains("more"), "{p}");
    }
}
