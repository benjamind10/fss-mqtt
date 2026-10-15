//! Text helpers: sizes, times, payload sanitising, JSON and hex rendering.

use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::theme::Theme;

pub fn human_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

pub fn human_count(n: u64) -> String {
    if n < 10_000 {
        commas(n)
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        format!("{:.1}M", n as f64 / 1e6)
    }
}

pub fn msg_count(n: u64) -> String {
    if n == 1 {
        "1 msg".into()
    } else {
        format!("{} msgs", human_count(n))
    }
}

pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

pub fn ago(now: SystemTime, t: SystemTime) -> String {
    let d = now.duration_since(t).unwrap_or_default().as_secs();
    match d {
        0 => "now".into(),
        1..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

/// Local wall-clock time; `date` adds the calendar day.
pub fn clock(t: SystemTime, date: bool) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (y, mo, day, h, mi, s) = local_parts(d.as_secs());
    let hms = format!("{h:02}:{mi:02}:{s:02}.{:03}", d.subsec_millis());
    if date {
        format!("{y:04}-{mo:02}-{day:02} {hms}")
    } else {
        hms
    }
}

#[cfg(unix)]
fn local_parts(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let secs = secs as _;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    (
        tm.tm_year + 1900,
        tm.tm_mon as u32 + 1,
        tm.tm_mday as u32,
        tm.tm_hour as u32,
        tm.tm_min as u32,
        tm.tm_sec as u32,
    )
}

#[cfg(windows)]
fn local_parts(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    // FILETIME counts 100 ns ticks since 1601-01-01.
    let ticks = (secs + 11_644_473_600) * 10_000_000;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        FileTimeToSystemTime(&ft, &mut utc);
        SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local);
    }
    let l = local;
    (
        l.wYear as i32,
        l.wMonth as u32,
        l.wDay as u32,
        l.wHour as u32,
        l.wMinute as u32,
        l.wSecond as u32,
    )
}

/// Whether terminals reliably draw `c` as wide as `unicode-width` says. When
/// they don't, the rest of the row shifts and spills onto the next line, so
/// such characters are shown as `·`: zero-width and control characters, soft
/// hyphens, line separators, private-use and special code points, and
/// anything past the CJK planes (tags, unassigned, private use).
pub fn steady(c: char) -> bool {
    matches!(c.width(), Some(1 | 2))
        && !matches!(
            c as u32,
            0xAD | 0x2028 | 0x2029 | 0xE000..=0xF8FF | 0xFFF0..=0xFFFF | 0x40000..
        )
}

/// Combining marks in any script (é written as e + ◌́, Devanagari vowel signs,
/// Thai tone marks, Arabic harakat …): zero-width characters that terminals
/// draw on the character before them. Not the invisible format characters
/// (soft hyphen, bidi controls, zero-width space and joiner, variation
/// selectors, tags) or Hangul's conjoining jamo, which terminals handle
/// inconsistently.
pub fn is_mark(c: char) -> bool {
    c.width() == Some(0)
        && !matches!(
            c as u32,
            0x00 | 0xAD
                | 0x061C
                | 0x1160..=0x11FF
                | 0x180B..=0x180F
                | 0x200B..=0x200F
                | 0x2028..=0x202E
                | 0x2060..=0x206F
                | 0xD7B0..=0xD7FF
                | 0xFE00..=0xFE0F
                | 0xFEFF
                | 0xE0000..=0xE0FFF
        )
}

/// At most `max` bytes of `b` as one printable line. Binary payloads show only
/// their printable ASCII; stray Unicode decoded from binary bytes is noise.
pub fn one_line(b: &[u8], max: usize) -> String {
    let b = &b[..b.len().min(max)];
    if !is_text(b) {
        return b
            .iter()
            .map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '·' })
            .collect();
    }
    let s = match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(e) if e.error_len().is_none() => String::from_utf8_lossy(&b[..e.valid_up_to()]).into_owned(),
        Err(_) => String::from_utf8_lossy(b).into_owned(),
    };
    s.chars()
        .filter_map(|c| match c {
            '\n' => Some(crate::glyph::NEWLINE),
            '\r' => None,
            '\t' => Some(' '),
            // Emoji joiners and presentation selectors only change how a
            // character looks; dropped, ❤️ shows as ❤ and 👨‍👩‍👧 as 👨👩👧.
            '\u{200D}' | '\u{FE0E}' | '\u{FE0F}' => None,
            c if steady(c) || is_mark(c) => Some(c),
            _ => Some('·'),
        })
        .collect()
}

/// Does this look like human-readable text?
pub fn is_text(b: &[u8]) -> bool {
    let ok = match std::str::from_utf8(b) {
        Ok(_) => true,
        // tolerate a rune cut off by truncation
        Err(e) => e.error_len().is_none() && b.len() - e.valid_up_to() < 4,
    };
    if !ok {
        return false;
    }
    let ctrl = b
        .iter()
        .filter(|&&c| c < 0x20 && c != b'\n' && c != b'\r' && c != b'\t')
        .count();
    ctrl * 100 <= b.len()
}

pub fn str_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Hard-wraps plain text to `w` cells.
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return vec![];
    }
    let mut out = vec![String::new()];
    let mut cur = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if cur + cw > w {
            out.push(String::new());
            cur = 0;
        }
        out.last_mut().unwrap().push(c);
        cur += cw;
    }
    out
}

/// Truncates (with …) or pads plain text to exactly `w` cells.
pub fn pad(s: &str, w: usize) -> String {
    let line = fit(vec![Span::raw(s.to_string())], w);
    let mut out: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let cur = str_width(&out);
    out.extend(std::iter::repeat_n(' ', w.saturating_sub(cur)));
    out
}

/// Keeps the end of `s` so long topic paths keep their leaf.
pub fn trunc_left(s: &str, w: usize) -> String {
    if str_width(s) <= w {
        return s.to_string();
    }
    if w <= 1 {
        return "…".into();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut cur = 0;
    for c in s.chars().rev() {
        let cw = c.width().unwrap_or(0);
        if cur + cw > w - 1 {
            break;
        }
        kept.push(c);
        cur += cw;
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

/// Truncates styled spans to `w` cells, ending in … when cut.
pub fn fit<'a>(spans: Vec<Span<'a>>, w: usize) -> Line<'a> {
    let total: usize = spans.iter().map(|s| s.width()).sum();
    if total <= w {
        return Line::from(spans);
    }
    let mut out = Vec::new();
    let mut used = 0;
    for s in spans {
        let sw = s.width();
        if used + sw < w {
            used += sw;
            out.push(s);
            continue;
        }
        let mut text = String::new();
        for c in s.content.chars() {
            let cw = c.width().unwrap_or(0);
            if used + cw > w - 1 {
                break;
            }
            text.push(c);
            used += cw;
        }
        out.push(Span::styled(text + "…", s.style));
        break;
    }
    Line::from(out)
}

/// Styled lines as plain text, one per line, without trailing blanks.
pub fn plain(lines: &[Line]) -> String {
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        out.push_str(text.trim_end());
    }
    out
}

pub fn hex_lines(b: &[u8], w: usize, th: &Theme) -> Vec<Line<'static>> {
    let mut per = 16;
    while per > 4 && 10 + per * 3 + 2 + per > w {
        per /= 2;
    }
    b.chunks(per)
        .enumerate()
        .map(|(i, chunk)| {
            let mut hx = String::with_capacity(per * 3 + 1);
            let mut asc = String::with_capacity(per);
            for j in 0..per {
                match chunk.get(j) {
                    Some(c) => {
                        hx.push_str(&format!("{c:02x} "));
                        asc.push(if (0x20..0x7f).contains(c) { *c as char } else { '.' });
                    }
                    None => hx.push_str("   "),
                }
                if j == 7 && per == 16 {
                    hx.push(' ');
                }
            }
            Line::from(vec![
                Span::styled(format!("{:08x}  ", i * per), th.faint),
                Span::styled(hx, th.number),
                Span::raw(" "),
                Span::styled(asc, th.value),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn misc() {
        assert_eq!(commas(1234567), "1,234,567");
        assert_eq!(one_line(b"a\nb\tc", 10), format!("a{}b c", crate::glyph::NEWLINE));
        assert!(is_text(b"hello"));
        assert!(!is_text(&[0, 1, 2, 0xff]));
        assert_eq!(trunc_left("v1/plant/temp", 6), "…/temp");
    }

    #[test]
    fn plain_text_of_styled_lines() {
        let th = Theme::new(false);
        let lines = vec![
            Line::from(vec![
                Span::styled("key", th.purple),
                Span::raw("  "),
                Span::styled("value", th.text),
            ]),
            Line::raw("padded   "),
            Line::default(),
            Line::raw("end"),
        ];
        assert_eq!(plain(&lines), "key  value\npadded\n\nend");
        assert_eq!(plain(&[]), "");
    }

    #[test]
    fn one_line_keeps_cells_steady() {
        // Binary: printable ASCII only, even where the bytes happen to be valid UTF-8 (ћ).
        let pb = b"\x0a\x03T04\x11\xd1\x9b\x00\x00\x00@%\x1a\x01\x00";
        assert!(!is_text(pb));
        assert_eq!(one_line(pb, 64), "··T04······@%···");
        // Text keeps its letters, CJK, emoji and accents; the rest becomes ·.
        let s = "Ørsted 風力 ❤ e\u{301} \u{202E}rtl\u{200B}\u{AD}\u{E000}\u{2028}❤\u{FE0F}\u{E0041}\u{40000}!";
        assert_eq!(one_line(s.as_bytes(), 256), "Ørsted 風力 ❤ e\u{301} ·rtl····❤··!");
        // Marks in other scripts stay; emoji joiners and selectors go.
        assert_eq!(one_line("हिन्दी ไม้ שָׁלוֹם".as_bytes(), 256), "हिन्दी ไม้ שָׁלוֹם");
        assert_eq!(one_line("✔\u{FE0F} 👨\u{200D}👩\u{200D}👧".as_bytes(), 256), "✔ 👨👩👧");
        for c in one_line(s.as_bytes(), 256).chars() {
            assert!(steady(c) || is_mark(c), "{c:?}");
        }
        assert!(steady('a') && steady('風') && steady('🌬'));
        assert!(!steady('\u{200D}') && !steady('\u{FE0F}') && !steady('\u{1b}') && !steady('\u{9b}'));
    }
}
