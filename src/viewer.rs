//! Full-screen message view. It snapshots the topic's history when opened
//! so the message under your eyes doesn't change.

use std::sync::Arc;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

use crate::fmt::{self, human_bytes};
use crate::pretty::{self, Format};
use crate::store::Message;
use crate::theme::Theme;
use crate::ui::message_props;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Pretty, // JSON pretty-printed, otherwise text
    Raw,
    Hex,
}

pub struct Viewer {
    pub node: usize, // the topic's node in the store
    pub path: String,
    pub msgs: Vec<Arc<Message>>,
    pub idx: usize,
    pub mode: Mode,
    pub format: Format,
    pub head: Vec<Line<'static>>,
    pub lines: Vec<Line<'static>>,
    pub scroll: usize,
    /// Sideways offset for tables, which don't wrap.
    pub hscroll: usize,
    pub max_width: usize,
    width: usize,
}

impl Viewer {
    pub fn new(node: usize, path: String, msgs: Vec<Arc<Message>>, idx: usize) -> Viewer {
        Viewer {
            node,
            path,
            msgs,
            idx,
            mode: Mode::Pretty,
            format: Format::Empty,
            head: vec![],
            lines: vec![],
            scroll: 0,
            hscroll: 0,
            max_width: 0,
            width: 0,
        }
    }

    pub fn width_now(&self) -> usize {
        self.width
    }

    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            Mode::Pretty => self.format.label(),
            Mode::Raw => "raw",
            Mode::Hex => "hex",
        }
    }

    pub fn relayout(&mut self, w: usize, th: &Theme) {
        self.width = w;
        let msg = self.msgs[self.idx].clone();
        self.format = pretty::detect(&msg);

        self.head.clear();
        let mut meta = vec![
            Span::styled(format!("message {} of {} · ", self.idx + 1, self.msgs.len()), th.dim),
            Span::styled(fmt::clock(msg.time, true), th.text),
            Span::styled(format!(" · qos {} · {}", msg.qos, human_bytes(msg.size)), th.dim),
        ];
        if msg.retain {
            meta.push(Span::styled(" · ", th.dim));
            meta.push(Span::styled("retained", th.yellow));
        }
        if msg.truncated() {
            meta.push(Span::styled(" · ", th.dim));
            meta.push(Span::styled(
                format!("truncated to {} (--max-payload)", human_bytes(msg.payload.len())),
                th.red,
            ));
        }
        self.head.push(Line::from(meta));
        let props = message_props(&msg);
        if !props.is_empty() {
            let kw = props.iter().map(|p| p.0.len()).max().unwrap_or(0).min(w / 3);
            self.head.push(Line::default());
            for (k, v) in &props {
                for (i, l) in fmt::wrap(&fmt::one_line(v.as_bytes(), 4096), w.saturating_sub(kw + 2))
                    .into_iter()
                    .enumerate()
                {
                    let key = if i == 0 { fmt::pad(k, kw) } else { " ".repeat(kw) };
                    self.head.push(Line::from(vec![
                        Span::styled(key, th.purple),
                        Span::raw("  "),
                        Span::styled(l, th.text),
                    ]));
                }
            }
        }
        self.head.push(Line::styled("─".repeat(w), th.border));

        let p = &msg.payload;
        let lines = match self.mode {
            Mode::Pretty => pretty::render(&msg, th, w),
            Mode::Hex => fmt::hex_lines(p, w, th),
            Mode::Raw if p.is_empty() => vec![Line::styled("(empty payload)", th.dim.add_modifier(Modifier::ITALIC))],
            Mode::Raw if !fmt::is_text(p) => fmt::hex_lines(p, w, th),
            Mode::Raw => String::from_utf8_lossy(p)
                .replace("\r\n", "\n")
                .split('\n')
                .map(|l| Line::styled(fmt::one_line(l.as_bytes(), l.len()), th.text))
                .collect(),
        };
        self.hscroll = 0;
        if self.mode == Mode::Pretty && !pretty::wraps(self.format) {
            self.max_width = lines.iter().map(|l| l.width()).max().unwrap_or(0);
            self.lines = lines;
        } else {
            self.max_width = w;
            self.lines = lines.into_iter().flat_map(|l| pretty::wrap_line(l, w)).collect();
        }
    }

    /// Handles a key; returns true when the viewer should close.
    pub fn key(&mut self, k: KeyEvent, body_h: usize, th: &Theme) -> bool {
        let page = body_h.saturating_sub(1).max(1);
        let mut relayout = false;
        match k.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => return true,
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll += 1,
            KeyCode::PageUp | KeyCode::Char('b') => self.scroll = self.scroll.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') | KeyCode::Char('f') => self.scroll += page,
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll = self.lines.len(),
            // Pan wide tables.
            KeyCode::Left if k.modifiers.contains(KeyModifiers::SHIFT) => self.hscroll = self.hscroll.saturating_sub(8),
            KeyCode::Char('<') => self.hscroll = self.hscroll.saturating_sub(8),
            KeyCode::Right if k.modifiers.contains(KeyModifiers::SHIFT) => self.hscroll += 8,
            KeyCode::Char('>') => self.hscroll += 8,
            KeyCode::Left | KeyCode::Char('h') if self.idx + 1 < self.msgs.len() => {
                self.idx += 1;
                relayout = true;
            }
            KeyCode::Right | KeyCode::Char('l') if self.idx > 0 => {
                self.idx -= 1;
                relayout = true;
            }
            KeyCode::Tab | KeyCode::Char('m') => {
                self.mode = match self.mode {
                    Mode::Pretty => Mode::Raw,
                    Mode::Raw => Mode::Hex,
                    Mode::Hex => Mode::Pretty,
                };
                relayout = true;
            }
            _ => {}
        }
        if relayout {
            self.scroll = 0;
            self.relayout(self.width, th);
        }
        self.scroll = self.scroll.min(self.lines.len().saturating_sub(body_h));
        self.hscroll = self.hscroll.min(self.max_width.saturating_sub(self.width));
        false
    }
}
