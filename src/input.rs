//! Keys as the app takes them: crossterm's on the desktop, and the same
//! shapes in the browser (web version), where crossterm doesn't build. Only
//! what the app uses is here.

#[cfg(not(target_arch = "wasm32"))]
pub use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[cfg(target_arch = "wasm32")]
pub use web::{KeyCode, KeyEvent, KeyModifiers};

#[cfg(target_arch = "wasm32")]
mod web {
    use std::ops::BitOr;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum KeyCode {
        Backspace,
        Enter,
        Left,
        Right,
        Up,
        Down,
        Home,
        End,
        PageUp,
        PageDown,
        Tab,
        BackTab,
        Delete,
        Esc,
        Char(char),
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct KeyModifiers(u8);

    impl KeyModifiers {
        pub const NONE: KeyModifiers = KeyModifiers(0);
        pub const SHIFT: KeyModifiers = KeyModifiers(1);
        pub const CONTROL: KeyModifiers = KeyModifiers(2);
        pub const ALT: KeyModifiers = KeyModifiers(4);

        pub fn contains(self, other: KeyModifiers) -> bool {
            self.0 & other.0 == other.0
        }
    }

    impl BitOr for KeyModifiers {
        type Output = KeyModifiers;
        fn bitor(self, o: KeyModifiers) -> KeyModifiers {
            KeyModifiers(self.0 | o.0)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct KeyEvent {
        pub code: KeyCode,
        pub modifiers: KeyModifiers,
    }

    impl KeyEvent {
        pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
            KeyEvent { code, modifiers }
        }
    }
}
