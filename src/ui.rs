//! Rendering. Everything is drawn straight into the frame buffer.

use std::time::Duration;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
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

    let l = app.layout();
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
    match (c.status.state, c.status.err.is_some()) {
        (ConnState::Connected, _) => Span::styled("●", th.green),
        (ConnState::Connecting, false) => Span::styled(glyph::CONNECTING, th.yellow),
        (ConnState::Connecting, true) => Span::styled(glyph::CONNECTING, th.red),
        (ConnState::Disconnected, _) => Span::styled("○", th.dim),
    }
}

fn status_text(c: &ConnInfo) -> String {
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
    // Keep the cursor in view.
    let offset = app.panel_cursor.saturating_sub(ih.saturating_sub(1));
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
                let state_style = match (ci.status.state, ci.status.err.is_some()) {
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
        };
        lines.push(line);
    }
    let up = st.live().filter(|c| c.status.state == ConnState::Connected).count();
    let title = format!("Connections {up}/{}", st.live().count());
    draw_box(buf, r, &title, lines, focused, sel_line, th);
    // Where the connections live; edits to the file apply while running.
    border_label(buf, r, &crate::config::tilde(&app.config_path), th.faint);
}

/// Writes `label` into the bottom border of `r`, right-aligned, keeping its
/// end (the file name) when it has to be cut.
fn border_label(buf: &mut Buffer, r: Rect, label: &str, style: ratatui::style::Style) {
    let room = (r.width as usize).saturating_sub(8);
    if room < 12 || r.height < 2 {
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
    let w = area.width.clamp(20, 64);
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
    let mut left = vec![
        Span::raw(" "),
        Span::styled(format!("{} fss-mqtt", glyph::LOGO), th.accent_b),
        Span::raw("  "),
        Span::styled(
            format!("{up}/{} connected", st.live().count()),
            if up > 0 { th.green } else { th.dim },
        ),
    ];
    if failing > 0 {
        left.push(Span::styled(format!("  {failing} failing"), th.red));
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

fn detail_box(buf: &mut Buffer, r: Rect, app: &App, st: &Store) {
    let Some(n) = app.selected else {
        draw_box(buf, r, "", vec![], false, None, &app.th);
        return;
    };
    let node = &st.nodes[n];
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
    let idx = if app.focus == Focus::Msgs {
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
    let sel_line = out.len() + idx;
    for (i, m) in msgs.iter().enumerate() {
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
    if app.filter_text.is_empty() {
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
    let v = format!(" {} ", version());
    let vw = v.chars().count() as u16;
    if r.width > vw + 6 {
        buf.set_string(r.right() - 2 - vw, r.y + 2, v, th.faint);
    }
}

fn hint<'a>(th: &Theme, pairs: &[(&str, &str)]) -> Line<'a> {
    let mut spans = vec![Span::raw("  ")];
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", th.faint));
        }
        spans.push(Span::styled(k.to_string(), th.key));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(v.to_string(), th.dim));
    }
    Line::from(spans)
}

fn version() -> String {
    format!("fss-mqtt v{}", env!("CARGO_PKG_VERSION"))
}

/// The hint line with the version in the bottom-right corner; the version is
/// dropped when the line is too narrow for both.
fn with_version<'a>(app: &App, hints: Line<'a>) -> Line<'a> {
    let v = Span::styled(format!("{} ", version()), app.th.faint);
    join_lr(hints.spans, vec![v], app.width as usize)
}

fn hints<'a>(app: &App) -> Line<'a> {
    if let Some((text, error, _)) = &app.notice {
        return Line::styled(format!("  {text}"), if *error { app.th.red } else { app.th.yellow });
    }
    if app.focus == Focus::Conns {
        hint(
            &app.th,
            &[
                ("↑↓", "move"),
                (&format!("space {}", glyph::ENTER), "connect / subscribe on-off"),
                ("tab", "next pane"),
                ("type", "filter"),
                ("^c", "quit"),
            ],
        )
    } else if app.focus == Focus::Msgs {
        hint(
            &app.th,
            &[
                ("↑↓", "messages"),
                (glyph::ENTER, "open"),
                ("^e", "publish"),
                ("← esc", "back to tree"),
                ("tab", "next pane"),
                ("^c", "quit"),
            ],
        )
    } else {
        hint(
            &app.th,
            &[
                ("↑↓", "move"),
                ("←→", "collapse/expand"),
                (glyph::ENTER, "open"),
                ("^e", "publish"),
                ("esc", "clear filter"),
                ("tab", "next pane"),
                ("^c", "quit"),
            ],
        )
    }
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
    let mut keys = vec![("↑↓ pgup pgdn", "scroll"), ("←→", "older/newer")];
    if v.max_width > v.width_now() {
        keys.push(("shift ←→", "pan"));
    }
    keys.extend([("tab", mode.as_str()), ("e", "publish"), ("esc", "back")]);
    let mut h = hint(th, &keys);
    if v.lines.len() > body_h {
        h.spans.push(Span::styled(
            format!("  {}–{} of {} lines", v.scroll + 1, end, v.lines.len()),
            th.faint,
        ));
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

    border_label(buf, r, &format!("ln {}, col {}", ed.row + 1, ed.col + 1), th.faint);
    let h = hint(th, &[("^s", "send"), ("esc", "cancel"), ("^f", "JSON pretty/compact")]);
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

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::app::{App, Ask, ConnCtl, Focus, Options};
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
        let opts = Options { inline_bytes: 64 };
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
    fn filter_and_navigate() {
        let (mut app, _rx) = sample(tmp_config("nav"));
        let v = frame(&app, "start");
        assert!(v.contains("Connections 1/2") && v.contains("local") && v.contains("staging"));
        assert!(v.contains("v1") && v.contains("v2") && v.contains("devices"));
        assert!(
            v.contains(&format!(" {} ─╯", super::version())),
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
        app.rate_at = std::time::Instant::now() - std::time::Duration::from_secs(2);
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
}
