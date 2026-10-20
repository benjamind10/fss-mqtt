//! Keys whose escape sequence arrived in two reads.
//!
//! A terminal sends an arrow key as `ESC [ B`. When a read from the terminal
//! ends right after the `ESC` (it happens with key repeat, or a trackpad
//! scrolling through Terminal.app's arrow keys), crossterm reports Esc on its
//! own and the rest as the characters `[` and `B`, which would clear the
//! filter and type `[B` into it. Like editors do, a lone Esc waits a moment:
//! if the rest of a key's sequence follows within it, that key is meant.
//! Nobody types Esc, `[`, `B` that fast.

use std::io;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// How long after a lone Esc the rest of a sequence may arrive.
pub const ESC_WAIT: Duration = Duration::from_millis(25);

/// The key an escape sequence stands for, given what followed the `ESC`:
/// `[B` is down, `[1;2C` shift+right, `[5~` page up, `OA` up.
pub fn escape_key(seq: &str) -> Option<KeyEvent> {
    use KeyCode::*;
    let (kind, body) = (seq.get(..1)?, seq.get(1..)?);
    match kind {
        // SS3: one final character.
        "O" => {
            let code = match body {
                "A" => Up,
                "B" => Down,
                "C" => Right,
                "D" => Left,
                "H" => Home,
                "F" => End,
                "P" => F(1),
                "Q" => F(2),
                "R" => F(3),
                "S" => F(4),
                _ => return None,
            };
            Some(KeyEvent::new(code, KeyModifiers::NONE))
        }
        // CSI: numbers separated by ';', then a final character.
        "[" => {
            let last = body.chars().last()?;
            let params = &body[..body.len() - last.len_utf8()];
            if !params.chars().all(|c| c.is_ascii_digit() || c == ';') {
                return None;
            }
            let mut nums = params.split(';');
            let first: Option<u16> = nums.next().filter(|s| !s.is_empty()).and_then(|s| s.parse().ok());
            let modifier: Option<u8> = match nums.next() {
                Some(m) => Some(m.parse().ok()?),
                None => None,
            };
            if nums.next().is_some() {
                return None;
            }
            let code = match (last, first) {
                ('A', None | Some(1)) => Up,
                ('B', None | Some(1)) => Down,
                ('C', None | Some(1)) => Right,
                ('D', None | Some(1)) => Left,
                ('H', None | Some(1)) => Home,
                ('F', None | Some(1)) => End,
                ('Z', None) => BackTab,
                ('~', Some(1 | 7)) => Home,
                ('~', Some(4 | 8)) => End,
                ('~', Some(2)) => Insert,
                ('~', Some(3)) => Delete,
                ('~', Some(5)) => PageUp,
                ('~', Some(6)) => PageDown,
                ('~', Some(n @ 11..=15)) => F((n - 10) as u8),
                ('~', Some(n @ 17..=21)) => F((n - 11) as u8),
                ('~', Some(n @ 23..=24)) => F((n - 12) as u8),
                _ => return None,
            };
            // The modifier parameter, read as crossterm reads it when the
            // sequence arrives whole (so a split key comes out the same):
            // 1 + shift 1, alt 2, ctrl 4, super 8, hyper 16, meta 32; the
            // lock bits (64, 128) aren't modifiers.
            let mut mods = KeyModifiers::NONE;
            if let Some(m) = modifier {
                let bits = m.checked_sub(1)?;
                mods.set(KeyModifiers::SHIFT, bits & 1 != 0);
                mods.set(KeyModifiers::ALT, bits & 2 != 0);
                mods.set(KeyModifiers::CONTROL, bits & 4 != 0);
                mods.set(KeyModifiers::SUPER, bits & 8 != 0);
                mods.set(KeyModifiers::HYPER, bits & 16 != 0);
                mods.set(KeyModifiers::META, bits & 32 != 0);
            }
            if code == BackTab {
                mods |= KeyModifiers::SHIFT;
            }
            Some(KeyEvent::new(code, mods))
        }
        _ => None,
    }
}

/// Whether `seq` (what followed the `ESC` so far) could still grow into a
/// sequence `escape_key` knows.
fn could_grow(seq: &str) -> bool {
    match seq.as_bytes() {
        [] | [b'O'] => true,
        [b'[', rest @ ..] => rest.iter().all(|c| c.is_ascii_digit() || *c == b';'),
        _ => false,
    }
}

/// After a lone Esc: the events it and what follows stand for. `next`
/// returns the next event if one arrives within the given time. Returns
/// the key of a sequence that arrived split, or else the Esc and whatever
/// followed it, as they came.
pub fn after_esc(esc: KeyEvent, mut next: impl FnMut(Duration) -> io::Result<Option<Event>>) -> io::Result<Vec<Event>> {
    let deadline = Instant::now() + ESC_WAIT;
    let mut seq = String::new();
    let mut came = vec![Event::Key(esc)];
    while could_grow(&seq) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let Some(e) = next(left)? else { break };
        let ch = match &e {
            Event::Key(k)
                if k.kind != KeyEventKind::Release
                    && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                match k.code {
                    KeyCode::Char(c) => Some(c),
                    _ => None,
                }
            }
            _ => None,
        };
        came.push(e);
        let Some(c) = ch else { break };
        seq.push(c);
        if let Some(key) = escape_key(&seq) {
            return Ok(vec![Event::Key(key)]);
        }
    }
    Ok(came)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn sequences() {
        use KeyCode::*;
        let none = KeyModifiers::NONE;
        for (seq, want) in [
            ("[A", key(Up, none)),
            ("[B", key(Down, none)),
            ("[C", key(Right, none)),
            ("[D", key(Left, none)),
            ("OA", key(Up, none)),
            ("OB", key(Down, none)),
            ("[H", key(Home, none)),
            ("[F", key(End, none)),
            ("[1~", key(Home, none)),
            ("[4~", key(End, none)),
            ("[5~", key(PageUp, none)),
            ("[6~", key(PageDown, none)),
            ("[3~", key(Delete, none)),
            ("[Z", key(BackTab, KeyModifiers::SHIFT)),
            ("[1;2C", key(Right, KeyModifiers::SHIFT)),
            ("[1;2D", key(Left, KeyModifiers::SHIFT)),
            ("[1;5A", key(Up, KeyModifiers::CONTROL)),
            ("[1;3B", key(Down, KeyModifiers::ALT)),
            ("[5;5~", key(PageUp, KeyModifiers::CONTROL)),
            ("[1;9A", key(Up, KeyModifiers::SUPER)),
            ("[1;17A", key(Up, KeyModifiers::HYPER)),
            ("[1;33A", key(Up, KeyModifiers::META)),
            ("[1;65A", key(Up, KeyModifiers::NONE)),
            (
                "[1;16B",
                key(
                    Down,
                    KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER,
                ),
            ),
            ("[15~", key(F(5), none)),
            ("[24~", key(F(12), none)),
            ("OP", key(F(1), none)),
        ] {
            assert_eq!(escape_key(seq), Some(want), "{seq}");
        }
        for seq in [
            "", "[", "O", "[1;", "[1;2", "x", "[x", "[9~", "[1;2;3A", "[;A", "[1;0A", "OZ", "[2A",
        ] {
            assert_eq!(escape_key(seq), None, "{seq:?}");
        }
        assert_eq!(escape_key("[1;256A"), None, "not a modifier value");
        assert!(could_grow("") && could_grow("[") && could_grow("[1;2") && could_grow("O"));
        assert!(!could_grow("x") && !could_grow("OA") && !could_grow("[A"));
    }

    /// Feeds `events` to `after_esc` as if they arrived one after another.
    fn run(events: Vec<Event>) -> Vec<Event> {
        let mut q: VecDeque<Event> = events.into();
        after_esc(key(KeyCode::Esc, KeyModifiers::NONE), |_| Ok(q.pop_front())).unwrap()
    }

    fn ch(c: char) -> Event {
        // As crossterm reports them: capitals with shift.
        let mods = if c.is_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        Event::Key(key(KeyCode::Char(c), mods))
    }

    #[test]
    fn a_split_sequence_is_one_key() {
        let down = Event::Key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(run(vec![ch('['), ch('B')]), vec![down.clone()]);
        assert_eq!(
            run(vec![ch('['), ch('1'), ch(';'), ch('2'), ch('C')]),
            [Event::Key(key(KeyCode::Right, KeyModifiers::SHIFT))]
        );
        assert_eq!(
            run(vec![ch('O'), ch('A')]),
            [Event::Key(key(KeyCode::Up, KeyModifiers::NONE))]
        );
        // What comes after the key is left for the next read.
        assert_eq!(run(vec![ch('['), ch('B'), ch('x')]), [down]);
    }

    #[test]
    fn a_real_esc_stays_esc() {
        let esc = Event::Key(key(KeyCode::Esc, KeyModifiers::NONE));
        // Nothing follows.
        assert_eq!(run(vec![]), vec![esc.clone()]);
        // Esc, then typing: both, as they came.
        assert_eq!(run(vec![ch('v')]), [esc.clone(), ch('v')]);
        // Not a sequence after all: everything that came, in order.
        assert_eq!(run(vec![ch('['), ch('x')]), [esc.clone(), ch('['), ch('x')]);
        let up = Event::Key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(run(vec![up.clone()]), [esc.clone(), up]);
        assert_eq!(run(vec![Event::Resize(80, 24)]), [esc.clone(), Event::Resize(80, 24)]);
        let paste = Event::Paste("[B".into());
        assert_eq!(run(vec![paste.clone()]), [esc.clone(), paste]);
        // Ctrl or Alt keys aren't part of a sequence.
        let ctrl = Event::Key(key(KeyCode::Char('['), KeyModifiers::CONTROL));
        assert_eq!(run(vec![ctrl.clone()]), [esc, ctrl]);
    }

    #[test]
    fn one_deadline_for_the_whole_sequence() {
        // Characters a few ms apart, then nothing: each wait asked for is
        // what's left of the same deadline, so it shrinks, and none goes
        // past ESC_WAIT from the Esc.
        let mut q: VecDeque<Event> = vec![ch('['), ch('1')].into();
        let mut asked = Vec::new();
        let out = after_esc(key(KeyCode::Esc, KeyModifiers::NONE), |d| {
            asked.push(d);
            std::thread::sleep(Duration::from_millis(3).min(d));
            Ok(q.pop_front())
        })
        .unwrap();
        assert_eq!(out.len(), 3, "Esc, '[', '1': not a key after all");
        assert_eq!(asked.len(), 3);
        assert!(asked[0] <= ESC_WAIT);
        assert!(asked.windows(2).all(|w| w[1] < w[0]), "{asked:?}");
    }
}
