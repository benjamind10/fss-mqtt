//! Noticing that a newer fss-mqtt was installed while this one runs (a
//! `brew upgrade`, a new build), so it can restart into it with the session.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const CHECK_EVERY: Duration = Duration::from_secs(5);
/// How long the new binary may take to say its version.
const PROBE_WAIT: Duration = Duration::from_secs(2);

/// What a path points at: (resolved path, modified, size).
type Stamp = (PathBuf, SystemTime, u64);

fn stamp(p: &Path) -> Option<Stamp> {
    let m = std::fs::metadata(p).ok()?;
    let real = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    Some((real, m.modified().ok()?, m.len()))
}

pub struct Watch {
    /// The command as started (on $PATH, or the path given), not resolved,
    /// so an upgrade that points a link elsewhere is seen.
    pub path: PathBuf,
    stamp: Option<Stamp>,
    checked: Instant,
    /// The last probe's answer, filled in by its thread: the version, or
    /// `None` when the binary couldn't say (being written, say).
    probe: Arc<Mutex<Option<Option<String>>>>,
    probing: bool,
    /// The version installed now, once it differs from what's running,
    /// and the file as it was when it said so.
    installed: Option<String>,
    probed: Option<Stamp>,
    /// Whether, at the last look, the file was still the one that answered
    /// (what the header offers; ctrl+n looks again with `ready`).
    offer: bool,
}

impl Watch {
    /// Watches the fss-mqtt this process was started as.
    pub fn new() -> Option<Watch> {
        Some(Watch::at(launch_path()?))
    }

    pub fn at(path: PathBuf) -> Watch {
        Watch {
            stamp: stamp(&path),
            path,
            checked: Instant::now(),
            probe: Arc::default(),
            probing: false,
            installed: None,
            probed: None,
            offer: false,
        }
    }

    /// A watch that has already found `version` (for tests).
    #[cfg(test)]
    pub fn found(path: PathBuf, version: &str) -> Watch {
        let mut w = Watch::at(path);
        w.installed = Some(version.into());
        w.probed = w.stamp.clone();
        w.offer = true;
        w
    }

    /// The version installed in our place, once it has changed: ours when
    /// the same version was rebuilt.
    pub fn installed(&self) -> Option<&str> {
        self.installed.as_deref()
    }

    /// Looks again (every few seconds); true when a change was just found.
    pub fn check(&mut self) -> bool {
        if self.probing {
            let Some(answer) = self.probe.lock().unwrap().take() else {
                return false;
            };
            self.probing = false;
            // No answer: not a working binary (yet); the next change to the
            // file is looked at again.
            let Some(v) = answer else { return false };
            let fresh = self.installed.as_deref() != Some(&v);
            self.installed = Some(v);
            self.probed = self.stamp.clone();
            self.offer = true;
            return fresh;
        }
        if self.checked.elapsed() < CHECK_EVERY {
            return false;
        }
        self.checked = Instant::now();
        let now = stamp(&self.path);
        // Not offered while the file isn't the one that said its version.
        self.offer = self.installed.is_some() && now.is_some() && now == self.probed;
        if now.is_none() || now == self.stamp {
            return false; // gone (mid-upgrade?) or unchanged
        }
        self.stamp = now;
        // Ask the new binary what it is, without holding up the UI.
        let (path, slot) = (self.path.clone(), self.probe.clone());
        self.probing = true;
        std::thread::spawn(move || {
            let v = probe(&path);
            *slot.lock().unwrap() = Some(v);
        });
        false
    }

    /// Looks at the next `check` rather than when it's time: something was
    /// just installed.
    pub fn look_now(&mut self) {
        self.checked = Instant::now() - CHECK_EVERY;
    }

    /// The version to offer a restart into: installed, and at the last look
    /// still the file that said so.
    pub fn offering(&self) -> Option<&str> {
        self.installed.as_deref().filter(|_| self.offer)
    }

    /// Whether the file is still the one that told its version, so it's
    /// safe to start (not changing again under a new build).
    pub fn ready(&self) -> bool {
        self.installed.is_some() && self.probed.is_some() && stamp(&self.path) == self.probed
    }
}

/// `fss-mqtt --version` of the binary at `path`: "0.7.0".
pub(crate) fn probe(path: &Path) -> Option<String> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + PROBE_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    out.trim().strip_prefix("fss-mqtt ").map(str::to_string)
}

/// The fss-mqtt command this process was started as: a path as given, or
/// the name found on $PATH.
fn launch_path() -> Option<PathBuf> {
    let arg0 = PathBuf::from(std::env::args_os().next()?);
    if arg0.components().count() > 1 {
        return std::path::absolute(&arg0).ok();
    }
    let exts: &[&str] = if cfg!(windows) { &["", ".exe"] } else { &[""] };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|dir| {
        exts.iter()
            .map(|e| dir.join(format!("{}{e}", arg0.display())))
            .find(|p| p.is_file())
    })
}

/// The arguments to restart with: as started, but continuing the session
/// (which holds what `--open` read) rather than opening files again.
pub fn restart_args(args: &[OsString], resume: bool) -> Vec<OsString> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.to_str() {
            Some("--resume") => {}
            Some("--open") => {
                it.next();
            }
            Some(s) if s.starts_with("--open=") => {}
            _ => out.push(a.clone()),
        }
    }
    if resume {
        out.push("--resume".into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_keeps_the_arguments_and_continues() {
        let args: Vec<OsString> = [
            "-c",
            "x",
            "--resume",
            "--open",
            "f.jsonl",
            "--open=g.jsonl",
            "-P",
            "pw",
            "mqtt://b",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let s = |v: Vec<OsString>| v.into_iter().map(|a| a.into_string().unwrap()).collect::<Vec<_>>();
        assert_eq!(
            s(restart_args(&args, true)),
            ["-c", "x", "-P", "pw", "mqtt://b", "--resume"]
        );
        assert_eq!(s(restart_args(&args, false)), ["-c", "x", "-P", "pw", "mqtt://b"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_binary_is_noticed_with_its_version() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fss-mqtt-test-{}-upgrade", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fss-mqtt");
        let install = |v: &str| {
            // As a package manager does: a new file in place of the old one.
            let tmp = dir.join("new");
            std::fs::write(&tmp, format!("#!/bin/sh\necho 'fss-mqtt {v}'\n")).unwrap();
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&tmp, &bin).unwrap();
        };
        install("0.6.0");
        let mut w = Watch::at(bin.clone());
        let due = |w: &mut Watch| w.checked = Instant::now() - CHECK_EVERY;
        due(&mut w);
        assert!(!w.check() && w.installed().is_none(), "unchanged");

        std::thread::sleep(Duration::from_millis(20)); // a new modification time
        install("10.0.0");
        due(&mut w);
        let start = Instant::now();
        while !w.check() {
            assert!(start.elapsed() < Duration::from_secs(5), "never noticed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(w.installed(), Some("10.0.0"));
        assert!(w.ready());
        // Noticed once; nothing more until it changes again.
        due(&mut w);
        assert!(!w.check());

        // Caught while being written (it can't say its version): not
        // announced, and not started; the next change is looked at again.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&bin, "\x7fELF half a binary").unwrap();
        assert!(!w.ready(), "changed since it said its version");
        due(&mut w);
        w.check();
        assert_eq!(w.offering(), None, "not offered while it's broken");
        let start = Instant::now();
        while w.probing || start.elapsed() < Duration::from_millis(50) {
            assert!(!w.check(), "a binary that can't answer isn't news");
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(w.installed(), Some("10.0.0"));
        assert!(!w.ready());
        std::thread::sleep(Duration::from_millis(20));
        install("11.0.0");
        due(&mut w);
        let start = Instant::now();
        while !w.check() {
            assert!(start.elapsed() < Duration::from_secs(5), "never noticed");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(w.installed() == Some("11.0.0") && w.ready());
        assert_eq!(w.offering(), Some("11.0.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_binary_that_hangs_is_given_up_on() {
        use std::os::unix::fs::PermissionsExt;
        let p = std::env::temp_dir().join(format!("fss-mqtt-test-{}-hang", std::process::id()));
        std::fs::write(&p, "#!/bin/sh\nsleep 10\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let start = Instant::now();
        assert_eq!(probe(&p), None);
        assert!(start.elapsed() < PROBE_WAIT + Duration::from_secs(1));
        let _ = std::fs::remove_file(&p);
    }
}
