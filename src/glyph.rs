//! Symbols that aren't in the Windows console fonts (Consolas, Lucida Console)
//! get plain stand-ins there; elsewhere they're drawn as designed.

#[cfg(not(windows))]
mod set {
    pub const CURSOR: &str = "❯ ";
    pub const LOGO: &str = "✻";
    pub const ENTER: &str = "⏎";
    pub const ON: &str = "☑ ";
    pub const OFF: &str = "☐ ";
    pub const CONNECTING: &str = "◌";
    pub const NEWLINE: char = '↵';
    pub const EMPTY: &str = "∅";
    pub const FILE: &str = "≡";
}

#[cfg(windows)]
mod set {
    pub const CURSOR: &str = "> ";
    pub const LOGO: &str = "*";
    pub const ENTER: &str = "enter";
    pub const ON: &str = "[x] ";
    pub const OFF: &str = "[ ] ";
    pub const CONNECTING: &str = "○";
    pub const NEWLINE: char = '¶';
    pub const EMPTY: &str = "\"\"";
    pub const FILE: &str = "≡";
}

pub use set::*;
