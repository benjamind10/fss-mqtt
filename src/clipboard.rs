//! Copying text to the clipboard: with the system's own tool (pbcopy, the
//! Windows clipboard, wl-copy, xclip, xsel), or through the terminal with an
//! OSC 52 escape sequence, which also reaches your own machine over SSH.
//! `FSS_MQTT_CLIPBOARD=osc52|native|off` overrides the choice.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::fmt::human_bytes;

/// The longest escape sequence sent to the terminal. Terminals cap it, and
/// some drop a longer one without a word.
const OSC52_MAX: usize = 1 << 20;
/// `ESC ] 52 ; c ;` and the closing BEL around the base64.
const OSC52_FRAME: usize = 8;

/// How long the clipboard tools may take, all together: the copy runs on the
/// UI's thread, and one that hangs mustn't freeze it for long.
const TOOL_WAIT: Duration = Duration::from_secs(1);

/// Where copies go: the system clipboard, or (in tests) a list.
pub struct Sink {
    pub recorded: Option<Vec<String>>,
}

impl Default for Sink {
    fn default() -> Sink {
        // Tests never touch the real clipboard.
        Sink {
            recorded: cfg!(test).then(Vec::new),
        }
    }
}

impl Sink {
    /// Copies `text`; returns how, for the status line.
    pub fn put(&mut self, text: &str) -> Result<&'static str, String> {
        match &mut self.recorded {
            Some(list) => {
                list.push(text.to_string());
                Ok("recorded")
            }
            None => copy(text),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Method {
    Pbcopy,
    Windows,
    WlCopy,
    Xclip,
    Xsel,
    Osc52,
}

impl Method {
    fn name(self) -> &'static str {
        match self {
            Method::Pbcopy => "pbcopy",
            Method::Windows => "Windows clipboard",
            Method::WlCopy => "wl-copy",
            Method::Xclip => "xclip",
            Method::Xsel => "xsel",
            Method::Osc52 => "terminal clipboard (OSC 52)",
        }
    }
}

/// The ways to try, in order, on `os` (as in `std::env::consts::OS`) with
/// the environment `var` (empty values count as unset).
fn methods(os: &str, var: &dyn Fn(&str) -> Option<String>) -> Result<Vec<Method>, String> {
    let native = || match os {
        "macos" => vec![Method::Pbcopy],
        "windows" => vec![Method::Windows],
        _ => {
            let mut m = Vec::new();
            if var("WAYLAND_DISPLAY").is_some() {
                m.push(Method::WlCopy);
            }
            if var("DISPLAY").is_some() {
                m.extend([Method::Xclip, Method::Xsel]);
            }
            m
        }
    };
    match var("FSS_MQTT_CLIPBOARD").as_deref() {
        None => {
            // Over SSH the tools would fill the remote machine's clipboard.
            if var("SSH_TTY").is_some() || var("SSH_CONNECTION").is_some() {
                Ok(vec![Method::Osc52])
            } else {
                Ok([native(), vec![Method::Osc52]].concat())
            }
        }
        Some("osc52") => Ok(vec![Method::Osc52]),
        Some("native") => match native() {
            m if m.is_empty() => Err("no clipboard tool here (FSS_MQTT_CLIPBOARD=native)".into()),
            m => Ok(m),
        },
        Some("off") => Err("the clipboard is off (FSS_MQTT_CLIPBOARD=off)".into()),
        Some(other) => Err(format!("FSS_MQTT_CLIPBOARD={other:?}: use osc52, native or off")),
    }
}

/// Copies `text` to the system clipboard; returns how.
pub fn copy(text: &str) -> Result<&'static str, String> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let deadline = Instant::now() + TOOL_WAIT;
    let mut err = String::from("no way to reach the clipboard");
    for m in methods(std::env::consts::OS, &var)? {
        match run(m, text, deadline) {
            Ok(()) => return Ok(m.name()),
            Err(e) => err = format!("{}: {e}", m.name()),
        }
    }
    Err(err)
}

fn run(m: Method, text: &str, deadline: Instant) -> Result<(), String> {
    match m {
        Method::Pbcopy => pipe("pbcopy", &[], text, deadline),
        Method::WlCopy => pipe("wl-copy", &[], text, deadline),
        Method::Xclip => pipe("xclip", &["-selection", "clipboard"], text, deadline),
        Method::Xsel => pipe("xsel", &["--clipboard", "--input"], text, deadline),
        Method::Windows => windows(text),
        Method::Osc52 => {
            let seq = osc52(text)?;
            // Between frames, so it can't land in the middle of the UI's
            // output; it moves nothing on screen.
            let mut out = std::io::stdout().lock();
            out.write_all(seq.as_bytes())
                .and_then(|()| out.flush())
                .map_err(|e| e.to_string())
        }
    }
}

/// `ESC ] 52 ; c ; <base64> BEL`: "set the clipboard to this".
fn osc52(text: &str) -> Result<String, String> {
    // Base64 takes 4 bytes for every 3.
    if text.len().div_ceil(3) * 4 + OSC52_FRAME > OSC52_MAX {
        return Err(format!(
            "{} is more than the terminal takes ({} at most)",
            human_bytes(text.len()),
            human_bytes((OSC52_MAX - OSC52_FRAME) / 4 * 3)
        ));
    }
    Ok(format!("\x1b]52;c;{}\x07", crate::base64::encode(text.as_bytes())))
}

/// Runs a clipboard tool with `text` on its stdin, until `deadline`. wl-copy,
/// xclip and xsel fork: the process started here exits at once, and a copy of
/// it (its output detached from ours) serves the clipboard after that.
fn pipe(tool: &str, args: &[&str], text: &str, deadline: Instant) -> Result<(), String> {
    let mut child = Command::new(tool)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "not installed".to_string(),
            _ => e.to_string(),
        })?;
    // The text is written from another thread: once the pipe is full, the
    // write waits for the tool to read, and one that doesn't read must not
    // hold up the deadline. Killing the tool ends a write stuck that way.
    let wrote = child.stdin.take().map(|mut stdin| {
        let (tx, rx) = mpsc::channel();
        let text = text.to_owned();
        std::thread::spawn(move || {
            // Sent before stdin closes as this returns, which is when the tool
            // sees the end of the text: by the time it exits, this is here.
            let _ = tx.send(stdin.write_all(text.as_bytes()));
        });
        rx
    });
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("didn't finish".into());
            }
        }
    };
    if !status.success() {
        return Err(format!("failed ({status})"));
    }
    // It ended well; the write is done unless it stopped reading early (or a
    // process it started still holds the pipe without reading).
    let Some(wrote) = wrote else { return Ok(()) };
    match wrote.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) if e.kind() != std::io::ErrorKind::BrokenPipe => Err(e.to_string()),
        _ => Err("didn't take all the text".into()),
    }
}

#[cfg(windows)]
fn windows(text: &str) -> Result<(), String> {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, OpenClipboard};

    let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
    // Another program may have the clipboard open for a moment.
    let mut tries = 0;
    // SAFETY: opening the clipboard has no preconditions.
    while unsafe { OpenClipboard(std::ptr::null_mut()) } == 0 {
        tries += 1;
        if tries == 10 {
            return Err(format!("the clipboard is busy ({})", std::io::Error::last_os_error()));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let result = windows_set(&wide);
    // SAFETY: it's open, by us.
    unsafe { CloseClipboard() };
    result
}

/// Puts NUL-terminated UTF-16 text on the clipboard, which must be open.
#[cfg(windows)]
fn windows_set(wide: &[u16]) -> Result<(), String> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{EmptyClipboard, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;

    let last = || std::io::Error::last_os_error().to_string();
    // SAFETY: the text goes into a movable global block of its size, filled
    // while locked. The system owns the block once SetClipboardData takes it;
    // when that fails it's freed here.
    unsafe {
        if EmptyClipboard() == 0 {
            return Err(last());
        }
        let mem = GlobalAlloc(GMEM_MOVEABLE, std::mem::size_of_val(wide));
        if mem.is_null() {
            return Err(last());
        }
        let p = GlobalLock(mem).cast::<u16>();
        if p.is_null() {
            let e = last();
            GlobalFree(mem);
            return Err(e);
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
        GlobalUnlock(mem);
        if SetClipboardData(CF_UNICODETEXT, mem).is_null() {
            let e = last();
            GlobalFree(mem);
            return Err(e);
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn windows(_: &str) -> Result<(), String> {
    Err("only on Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use Method::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    #[test]
    fn picks_the_systems_tool_then_the_terminal() {
        let none = env(&[]);
        assert_eq!(methods("macos", &none), Ok(vec![Pbcopy, Osc52]));
        assert_eq!(methods("windows", &none), Ok(vec![Windows, Osc52]));
        assert_eq!(
            methods("linux", &none),
            Ok(vec![Osc52]),
            "no display: only the terminal"
        );
        let x11 = env(&[("DISPLAY", ":0")]);
        assert_eq!(methods("linux", &x11), Ok(vec![Xclip, Xsel, Osc52]));
        let both = env(&[("DISPLAY", ":0"), ("WAYLAND_DISPLAY", "wayland-0")]);
        assert_eq!(methods("freebsd", &both), Ok(vec![WlCopy, Xclip, Xsel, Osc52]));
        // Over SSH, the terminal: the tools would copy on the remote machine.
        for ssh in [
            ("SSH_TTY", "/dev/ttys003"),
            ("SSH_CONNECTION", "10.0.0.2 51234 10.0.0.9 22"),
        ] {
            assert_eq!(methods("macos", &env(&[ssh])), Ok(vec![Osc52]));
            assert_eq!(methods("linux", &env(&[ssh, ("DISPLAY", ":0")])), Ok(vec![Osc52]));
        }
    }

    #[test]
    fn the_environment_overrides_the_choice() {
        let ssh = ("SSH_TTY", "/dev/pts/1");
        let with = |v: &str| env(&[("FSS_MQTT_CLIPBOARD", v), ssh]);
        assert_eq!(methods("macos", &with("osc52")), Ok(vec![Osc52]));
        assert_eq!(
            methods("macos", &with("native")),
            Ok(vec![Pbcopy]),
            "native wins over SSH"
        );
        assert!(
            methods("linux", &with("native"))
                .unwrap_err()
                .contains("no clipboard tool")
        );
        assert!(methods("macos", &with("off")).unwrap_err().contains("off"));
        assert!(
            methods("macos", &with("pbpaste"))
                .unwrap_err()
                .contains("osc52, native or off")
        );
    }

    #[test]
    fn osc52_sequence() {
        assert_eq!(osc52("hi").unwrap(), "\x1b]52;c;aGk=\x07");
        assert_eq!(osc52("").unwrap(), "\x1b]52;c;\x07");
        assert_eq!(osc52("ø/#").unwrap(), "\x1b]52;c;w7gvIw==\x07");
        // The cap is on the sequence as sent: 768 KB of text.
        let most = "x".repeat(786_426);
        assert_eq!(osc52(&most).unwrap().len(), OSC52_MAX);
        let err = osc52(&format!("{most}x")).unwrap_err();
        assert_eq!(err, "768.0 KB is more than the terminal takes (768.0 KB at most)");
    }

    #[cfg(unix)]
    #[test]
    fn tools_report_why_they_failed() {
        let soon = || Instant::now() + TOOL_WAIT;
        assert_eq!(pipe("cat", &[], "text", soon()), Ok(()));
        assert!(pipe("false", &[], "text", soon()).unwrap_err().starts_with("failed"));
        assert_eq!(
            pipe("fss-mqtt-no-such-tool", &[], "text", soon()),
            Err("not installed".into())
        );
        // A tool that reads everything: the text arrives whole.
        let big = "0123456789".repeat(20_000);
        assert_eq!(pipe("sh", &["-c", "test $(wc -c) -eq 200000"], &big, soon()), Ok(()));
        // One that exits fine without reading it all: not a copy.
        let huge = "x".repeat(1 << 20);
        assert_eq!(
            pipe("head", &["-c", "10"], &huge, soon()),
            Err("didn't take all the text".into())
        );
    }

    /// A tool that hangs is stopped at the deadline: with a small text, and
    /// with one bigger than the pipe holds, whose write gets stuck too.
    #[cfg(unix)]
    #[test]
    fn tools_that_hang_are_stopped_at_the_deadline() {
        for text in [String::new(), "x".repeat(1 << 20)] {
            let t = Instant::now();
            let deadline = t + Duration::from_millis(200);
            assert_eq!(pipe("sleep", &["5"], &text, deadline), Err("didn't finish".into()));
            let took = t.elapsed();
            assert!(took < Duration::from_secs(1), "{} B: took {took:?}", text.len());
        }
    }

    #[test]
    fn sinks_in_tests_record() {
        let mut s = Sink::default();
        assert_eq!(s.put("a/b"), Ok("recorded"));
        assert_eq!(s.recorded, Some(vec!["a/b".to_string()]));
    }
}
