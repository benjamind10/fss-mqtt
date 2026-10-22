//! OmaMQTT: fss-mqtt started as `omamqtt`, the way Omarchy's package installs
//! it. It goes by that name and wears the current Omarchy theme's colours
//! (omarchy.rs); otherwise it's the same program, with the same connections
//! file and session.

use std::path::Path;
use std::sync::OnceLock;

/// Whether this is OmaMQTT: started as `omamqtt`.
pub fn oma() -> bool {
    static OMA: OnceLock<bool> = OnceLock::new();
    *OMA.get_or_init(|| {
        std::env::args_os()
            .next()
            .is_some_and(|arg0| started_as_oma(Path::new(&arg0)))
    })
}

fn started_as_oma(arg0: &Path) -> bool {
    arg0.file_stem().is_some_and(|s| s.eq_ignore_ascii_case("omamqtt"))
}

/// The name it shows: "OmaMQTT" or "fss-mqtt".
pub fn name(oma: bool) -> &'static str {
    if oma { "OmaMQTT" } else { "fss-mqtt" }
}

/// The command it goes by: "omamqtt" or "fss-mqtt".
pub fn command() -> &'static str {
    if oma() { "omamqtt" } else { "fss-mqtt" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_by_how_it_was_started() {
        for arg0 in ["omamqtt", "/usr/bin/omamqtt", "OmaMQTT", r"C:\tools\omamqtt.exe"] {
            let arg0 = arg0.replace('\\', std::path::MAIN_SEPARATOR_STR);
            assert!(started_as_oma(Path::new(&arg0)), "{arg0}");
        }
        for arg0 in ["fss-mqtt", "/opt/homebrew/bin/fss-mqtt", "omamqtt-test", ""] {
            assert!(!started_as_oma(Path::new(arg0)), "{arg0}");
        }
        assert_eq!((name(true), name(false)), ("OmaMQTT", "fss-mqtt"));
        assert_eq!(command(), "fss-mqtt", "a test binary isn't omamqtt");
    }
}
