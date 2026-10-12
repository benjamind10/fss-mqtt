use ratatui::style::{Color, Modifier, Style};

pub struct Theme {
    pub accent: Style,
    pub accent_b: Style,
    pub text: Style,
    pub bold: Style,
    pub dim: Style,
    pub faint: Style,
    pub border: Style,       // panes without focus
    pub focus_border: Style, // the pane your keys go to
    pub green: Style,
    pub red: Style,
    pub yellow: Style,
    pub blue: Style,
    pub purple: Style,
    pub value: Style,
    pub number: Style,
    pub keyword: Style,
    pub section: Style,
    pub cursor: Style,
    pub matched: Style,
    pub key: Style,     // key names in the hint line
    pub sel: Style,     // selected row in the focused pane (background bar)
    pub sel_dim: Style, // selected row in other panes
}

fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// How many colours the terminal can show.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Depth {
    TrueColor,
    Ansi256,
}

impl Depth {
    /// 24-bit colour where the terminal says so (or is known to support it),
    /// otherwise the xterm 256-colour palette. Apple Terminal, for one, has no
    /// 24-bit colour and shows its profile colours instead.
    /// `FSS_MQTT_COLOR=truecolor|256` overrides the guess.
    pub fn detect() -> Depth {
        let var = |k: &str| std::env::var(k).unwrap_or_default().to_lowercase();
        match var("FSS_MQTT_COLOR").as_str() {
            "truecolor" | "24bit" => return Depth::TrueColor,
            "256" => return Depth::Ansi256,
            _ => {}
        }
        let truecolor = cfg!(windows)
            || matches!(var("COLORTERM").as_str(), "truecolor" | "24bit")
            || matches!(
                var("TERM_PROGRAM").as_str(),
                "iterm.app" | "wezterm" | "vscode" | "ghostty" | "hyper" | "tabby"
            )
            || var("TERM").contains("direct");
        if truecolor { Depth::TrueColor } else { Depth::Ansi256 }
    }
}

/// The nearest colour in the xterm 256-colour palette (6×6×6 cube or grey ramp).
pub fn to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |v: u8| -> usize {
        match v {
            0..48 => 0,
            48..115 => 1,
            _ => (v as usize - 35) / 40,
        }
    };
    let dist = |(r2, g2, b2): (u8, u8, u8)| {
        let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
        d(r, r2) + d(g, g2) + d(b, b2)
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let cube_idx = 16 + 36 * ri + 6 * gi + bi;
    let avg = (r as usize + g as usize + b as usize) / 3;
    let gray_i = if avg < 8 { 0 } else { ((avg - 8) / 10).min(23) };
    let gray = (8 + 10 * gray_i) as u8;
    if dist((gray, gray, gray)) < dist(cube) {
        (232 + gray_i) as u8
    } else {
        cube_idx as u8
    }
}

fn downgrade(c: Option<Color>) -> Option<Color> {
    match c {
        Some(Color::Rgb(r, g, b)) => Some(Color::Indexed(to_ansi256(r, g, b))),
        other => other,
    }
}

impl Theme {
    /// Picks light or dark from $COLORFGBG ("fg;bg"), defaulting to dark, and
    /// fits the colours to what the terminal can show.
    pub fn detect() -> Theme {
        let light = std::env::var("COLORFGBG")
            .ok()
            .and_then(|v| v.rsplit(';').next().and_then(|b| b.parse::<u8>().ok()))
            .is_some_and(|bg| bg == 7 || bg == 15);
        let mut th = Theme::new(light);
        if Depth::detect() == Depth::Ansi256 {
            th.use_ansi256();
        }
        th
    }

    /// Replaces every 24-bit colour with its nearest 256-palette colour.
    pub fn use_ansi256(&mut self) {
        self.for_each_style(|s| {
            s.fg = downgrade(s.fg);
            s.bg = downgrade(s.bg);
        });
        // The nearest match for the activity green is a dull blue-green; use a
        // clear palette green instead (dark: 78 = #5fd787, light: 28 = #008700).
        let green = if self.text.fg == Some(Color::Indexed(to_ansi256(0x1F, 0x1F, 0x1F))) {
            28
        } else {
            78
        };
        self.green = self.green.fg(Color::Indexed(green));
    }

    fn for_each_style(&mut self, mut f: impl FnMut(&mut Style)) {
        for s in [
            &mut self.accent,
            &mut self.accent_b,
            &mut self.text,
            &mut self.bold,
            &mut self.dim,
            &mut self.faint,
            &mut self.border,
            &mut self.focus_border,
            &mut self.green,
            &mut self.red,
            &mut self.yellow,
            &mut self.blue,
            &mut self.purple,
            &mut self.value,
            &mut self.number,
            &mut self.keyword,
            &mut self.section,
            &mut self.cursor,
            &mut self.matched,
            &mut self.key,
            &mut self.sel,
            &mut self.sel_dim,
        ] {
            f(s);
        }
    }

    pub fn new(light: bool) -> Theme {
        let c = |l: u32, d: u32| Style::new().fg(rgb(if light { l } else { d }));
        let accent = c(0xC15F3C, 0xD77757); // warm orange
        let dim = c(0x8A8A8A, 0x7A7A7A);
        Theme {
            accent,
            accent_b: accent.add_modifier(Modifier::BOLD),
            text: c(0x1F1F1F, 0xE8E6E3),
            bold: c(0x1F1F1F, 0xE8E6E3).add_modifier(Modifier::BOLD),
            dim,
            faint: c(0xB8B8B8, 0x4A4A4A),
            border: c(0xC8C8C8, 0x4E4E4E),
            focus_border: dim,
            green: c(0x2E8B47, 0x5FBF77),
            red: c(0xC0392B, 0xFF6B6B),
            yellow: c(0xA07800, 0xE5C07B),
            blue: c(0x2F6FB0, 0x7AB4E8),
            purple: c(0x6F5FC4, 0xB1A8F5),
            value: c(0x2B7A78, 0x7FD1C7),
            number: c(0xB35A1F, 0xE8A36B),
            keyword: c(0x8A3FA0, 0xD49BE8),
            section: dim.add_modifier(Modifier::BOLD),
            cursor: Style::new().add_modifier(Modifier::REVERSED),
            matched: accent.add_modifier(Modifier::BOLD),
            key: c(0x1F1F1F, 0xE8E6E3),
            // No background bars: the selected row is marked by the cursor glyph.
            sel: Style::new(),
            sel_dim: Style::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ansi256_nearest() {
        assert_eq!(to_ansi256(0, 0, 0), 16);
        assert_eq!(to_ansi256(255, 255, 255), 231);
        assert_eq!(to_ansi256(0xD7, 0x77, 0x57), 173); // accent → (215,135,95)
        assert_eq!(to_ansi256(0x7A, 0x7A, 0x7A), 243); // dim grey → grey ramp (118)
        assert_eq!(to_ansi256(0x5F, 0xBF, 0x77), 72); // green → (95,175,135)
    }

    #[test]
    fn downgraded_theme_has_no_rgb() {
        let mut th = Theme::new(false);
        th.use_ansi256();
        th.for_each_style(|s| {
            assert!(!matches!(s.fg, Some(Color::Rgb(..))) && !matches!(s.bg, Some(Color::Rgb(..))));
        });
    }
}
