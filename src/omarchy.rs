//! The current Omarchy theme's colours, for OmaMQTT (brand.rs). Omarchy keeps
//! them in a flat `colors.toml` (`accent = "#7aa2f7"`, `mode = "dark"`, …)
//! that it renders into each app's config; switching themes points
//! `~/.config/omarchy/current/theme` at another one, which `Watch` notices.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use web_time::{Duration, Instant};

use ratatui::style::{Color, Style};

use crate::theme::Theme;

const CHECK_EVERY: Duration = Duration::from_secs(1);

/// Where Omarchy may keep the current theme's colours.
fn places() -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        ".config/omarchy/current/theme/colors.toml",
        ".local/state/omarchy/current/theme/colors.toml",
    ]
    .iter()
    .map(|p| home.join(p))
    .collect()
}

/// The first of `places` that's there.
fn find(places: &[PathBuf]) -> Option<&Path> {
    places.iter().find(|p| p.is_file()).map(PathBuf::as_path)
}

/// `key = "value"` lines, a comment after the value or not; comments,
/// sections and anything else skipped.
fn read(text: &str) -> HashMap<&str, &str> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .filter_map(|(k, v)| {
            let v = v.trim();
            let v = match v.chars().next() {
                Some(q @ ('"' | '\'')) => v[1..].split(q).next().filter(|_| v[1..].contains(q))?,
                _ => v.split(['#', ' ', '\t']).next().unwrap_or(""),
            };
            Some((k.trim(), v))
        })
        .filter(|(k, _)| !k.is_empty() && !k.starts_with('#'))
        .collect()
}

fn hex(v: &str) -> Option<Color> {
    let h = v.strip_prefix('#')?;
    if h.len() != 6 && h.len() != 8 {
        return None; // (#rrggbbaa: the alpha goes)
    }
    let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    Some(Color::Rgb(byte(0)?, byte(2)?, byte(4)?))
}

/// A theme in Omarchy's colours; what they don't name stays fss-mqtt's.
pub fn theme(text: &str) -> Theme {
    let c = read(text);
    let mut th = Theme::new(c.get("mode") == Some(&"light"));
    let color = |k: &str| c.get(k).and_then(|v| hex(v));
    let set = |s: &mut Style, k: &str| {
        if let Some(col) = color(k) {
            *s = s.fg(col);
        }
    };
    set(&mut th.accent, "accent");
    set(&mut th.accent_b, "accent");
    set(&mut th.matched, "accent");
    for s in [&mut th.text, &mut th.bold, &mut th.key] {
        set(s, "foreground");
    }
    for s in [&mut th.dim, &mut th.focus_border, &mut th.section] {
        set(s, "dark_foreground");
    }
    set(&mut th.faint, "muted");
    set(&mut th.border, "muted");
    set(&mut th.green, "green");
    set(&mut th.red, "red");
    set(&mut th.yellow, "yellow");
    set(&mut th.blue, "blue");
    set(&mut th.purple, "magenta");
    set(&mut th.value, "cyan");
    // Not every theme has these two.
    let or = |k, other| if c.contains_key(k) { k } else { other };
    set(&mut th.number, or("orange", "yellow"));
    set(&mut th.keyword, or("bright_magenta", "magenta"));
    th
}

/// The theme in place now, if Omarchy is here.
pub fn current() -> Option<Theme> {
    Some(theme(&std::fs::read_to_string(find(&places())?).ok()?))
}

/// Notices when Omarchy switches to another theme (or the colours change).
pub struct Watch {
    places: Vec<PathBuf>,
    seen: Option<(PathBuf, std::time::SystemTime)>, // the file system's times
    checked: Instant,
}

impl Watch {
    pub fn new() -> Watch {
        Watch::of(places())
    }

    fn of(places: Vec<PathBuf>) -> Watch {
        Watch {
            seen: find(&places).and_then(stamp),
            places,
            checked: Instant::now(),
        }
    }

    /// Looks now and then; the new theme when it changed. Where it's kept is
    /// looked for each time: Omarchy may arrive, or move it, meanwhile.
    pub fn check(&mut self) -> Option<Theme> {
        if self.checked.elapsed() < CHECK_EVERY {
            return None;
        }
        self.checked = Instant::now();
        let now = stamp(find(&self.places)?)?;
        if self.seen.as_ref() == Some(&now) {
            return None;
        }
        let th = theme(&std::fs::read_to_string(&now.0).ok()?);
        self.seen = Some(now);
        Some(th)
    }
}

/// Which colours file is in place (through the link to the current theme),
/// and when it was written.
fn stamp(file: &Path) -> Option<(PathBuf, std::time::SystemTime)> {
    let real = std::fs::canonicalize(file).ok()?;
    let modified = std::fs::metadata(&real).ok()?.modified().ok()?;
    Some((real, modified))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    const TOKYO_NIGHT: &str = r##"mode = "dark"

accent = "#7aa2f7"
selection = "#292e42"
muted = "#414868"

background = "#1a1b26"
foreground = "#a9b1d6"
dark_foreground = "#565f89"

red = "#f7768e"
yellow = "#e0af68"
orange = "#eb927b"
green = "#9ece6a"
cyan = "#449dab"
blue = "#7aa2f7"
magenta = "#ad8ee6"
bright_magenta = "#bb9af7"
"##;

    #[test]
    fn omarchy_colours_take_the_roles() {
        let th = theme(TOKYO_NIGHT);
        let fg = |s: Style| s.fg.unwrap();
        assert_eq!(fg(th.accent), Color::Rgb(0x7a, 0xa2, 0xf7));
        assert!(
            th.accent_b.add_modifier.contains(Modifier::BOLD),
            "the bold accent stays bold"
        );
        assert_eq!(fg(th.text), Color::Rgb(0xa9, 0xb1, 0xd6));
        assert_eq!(fg(th.dim), Color::Rgb(0x56, 0x5f, 0x89));
        assert_eq!(fg(th.border), Color::Rgb(0x41, 0x48, 0x68));
        assert_eq!(fg(th.green), Color::Rgb(0x9e, 0xce, 0x6a));
        assert_eq!(fg(th.purple), Color::Rgb(0xad, 0x8e, 0xe6));
        assert_eq!(fg(th.number), Color::Rgb(0xeb, 0x92, 0x7b), "orange");
        assert_eq!(fg(th.keyword), Color::Rgb(0xbb, 0x9a, 0xf7), "bright magenta");
        assert!(th.section.add_modifier.contains(Modifier::BOLD));
    }

    #[cfg(unix)]
    #[test]
    fn a_theme_switch_is_noticed() {
        // Two themes and a link to the current one, as Omarchy has them;
        // the link is in the second of two places, and not there at first.
        let dir = std::env::temp_dir().join(format!("fss-mqtt-test-{}-omarchy", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, accent) in [("a", "#111111"), ("b", "#222222")] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
            std::fs::write(dir.join(name).join("colors.toml"), format!("accent = \"{accent}\"\n")).unwrap();
        }
        let current = dir.join("current");
        let mut w = Watch::of(vec![dir.join("elsewhere/colors.toml"), current.join("colors.toml")]);
        let a_second_on = |w: &mut Watch| w.checked = Instant::now() - CHECK_EVERY;
        a_second_on(&mut w);
        assert!(w.check().is_none(), "no Omarchy yet");

        std::os::unix::fs::symlink(dir.join("a"), &current).unwrap();
        assert!(w.check().is_none(), "too soon to look");
        a_second_on(&mut w);
        let th = w.check().expect("Omarchy arrived");
        assert_eq!(th.accent.fg, Some(Color::Rgb(0x11, 0x11, 0x11)));
        a_second_on(&mut w);
        assert!(w.check().is_none(), "nothing changed");

        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink(dir.join("b"), &current).unwrap();
        a_second_on(&mut w);
        let th = w.check().expect("switched to b");
        assert_eq!(th.accent.fg, Some(Color::Rgb(0x22, 0x22, 0x22)));
        a_second_on(&mut w);
        assert!(w.check().is_none(), "seen already");

        // Without the file (no Omarchy), nothing to notice.
        std::fs::remove_dir_all(&dir).unwrap();
        a_second_on(&mut w);
        assert!(w.check().is_none());
        assert!(Watch::of(Vec::new()).check().is_none());
    }

    #[test]
    fn missing_or_odd_colours_stay_as_they_were() {
        let ours = Theme::new(false);
        let th = theme("accent = \"#12345\"\n# red = \"#ff0000\"\n[section]\ngreen = 'nope'\n");
        assert_eq!(th.accent.fg, ours.accent.fg, "not a colour");
        assert_eq!(th.red.fg, ours.red.fg, "a comment");
        assert_eq!(th.green.fg, ours.green.fg);
        // A comment after a value, single quotes, a quote left open.
        let th = theme("accent = \"#7aa2f7\" # blue\ngreen = '#00ff00'\nred = \"#ff0000\nblue = #0000ff # bare\n");
        assert_eq!(th.accent.fg, Some(Color::Rgb(0x7a, 0xa2, 0xf7)));
        assert_eq!(th.green.fg, Some(Color::Rgb(0, 0xff, 0)));
        assert_eq!(th.red.fg, ours.red.fg, "never closed");
        assert_eq!(th.blue.fg, ours.blue.fg, "a bare # starts a comment");
        // A light theme starts from the light colours; alpha is dropped.
        let th = theme("mode = \"light\"\nblue = \"#11223344\"\n");
        assert_eq!(th.text.fg, Theme::new(true).text.fg);
        assert_eq!(th.blue.fg, Some(Color::Rgb(0x11, 0x22, 0x33)));
    }
}
