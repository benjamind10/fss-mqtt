//! Updating fss-mqtt from inside it: noticing a newer release on GitHub (at
//! start, at most once a day; never with FSS_MQTT_UPDATE=off), and
//! installing it the way this copy was installed. Restarting into it is
//! upgrade.rs's, as for any newer fss-mqtt installed in our place.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::http;

const REPO: &str = "mbilling/fss-mqtt";
const TAP_FORMULA: &str = "mbilling/tap/fss-mqtt";
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// How often GitHub is asked; a failed ask is tried again sooner.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
const RETRY_AFTER: Duration = Duration::from_secs(3600);
const MAX_DOWNLOAD: usize = 64 << 20;

/// Whether FSS_MQTT_UPDATE turns looking for releases off.
pub fn off() -> bool {
    is_off(std::env::var("FSS_MQTT_UPDATE").ok().as_deref())
}

fn is_off(v: Option<&str>) -> bool {
    v.is_some_and(|v| ["off", "0", "no", "false"].contains(&v.trim().to_ascii_lowercase().as_str()))
}

pub fn release_url(v: &str) -> String {
    format!("https://github.com/{REPO}/releases/tag/v{v}")
}

fn parse(v: &str) -> Option<[u64; 3]> {
    let mut it = v.split('.');
    let out = [
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
        it.next()?.parse().ok()?,
    ];
    it.next().is_none().then_some(out)
}

/// Whether release `v` is newer than `than` (x.y.z; nothing else is).
pub fn newer(v: &str, than: &str) -> bool {
    matches!((parse(v), parse(than)), (Some(a), Some(b)) if a > b)
}

/// "0.8.1" from where the latest release is (…/releases/tag/v0.8.1).
fn tag_version(location: &str) -> Option<String> {
    let (_, tag) = location.rsplit_once("/tag/")?;
    let v = tag.strip_prefix('v').unwrap_or(tag).split(['?', '#']).next()?;
    parse(v)?;
    Some(v.to_string())
}

/// The newest release: where GitHub's …/releases/latest sends you. No API,
/// no token, nothing but the request.
fn latest() -> Result<String, String> {
    let url = format!("https://github.com/{REPO}/releases/latest");
    let r = http::get(&url, false, 1 << 20, &mut |_, _| {})?;
    let to = r
        .header("location")
        .ok_or_else(|| format!("no release (HTTP {})", r.status))?;
    tag_version(to).ok_or_else(|| format!("not a release: {to}"))
}

/// What the last look found, and when ("1760000000 0.8.1").
fn load(path: &Path) -> Option<(SystemTime, String)> {
    let text = std::fs::read_to_string(path).ok()?;
    let (at, v) = text.trim().split_once(' ')?;
    parse(v)?;
    Some((UNIX_EPOCH + Duration::from_secs(at.parse().ok()?), v.to_string()))
}

fn save(path: &Path, v: &str) {
    let at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, format!("{at} {v}\n"));
}

/// Looks for a newer release now and then, in the background.
pub struct Checker {
    /// Where the last look is kept between runs.
    cache: Option<PathBuf>,
    /// The newest release seen.
    latest: Option<String>,
    due: Instant,
    answer: Arc<Mutex<Option<Result<String, String>>>>,
    asking: bool,
}

impl Checker {
    /// Knows what the last run found, and looks again once that's a day old.
    pub fn new(cache: Option<PathBuf>) -> Checker {
        let mut c = Checker {
            cache,
            latest: None,
            due: Instant::now(),
            answer: Arc::default(),
            asking: false,
        };
        if let Some((at, v)) = c.cache.as_deref().and_then(load) {
            // A clock set back makes it look recent: it waits a day then.
            let age = SystemTime::now().duration_since(at).unwrap_or_default();
            c.due = Instant::now() + CHECK_EVERY.saturating_sub(age);
            c.latest = Some(v);
        }
        c
    }

    /// One that has seen `version` and won't look again (for tests).
    #[cfg(test)]
    pub fn found(version: &str) -> Checker {
        Checker {
            cache: None,
            latest: Some(version.into()),
            due: Instant::now() + CHECK_EVERY * 365,
            answer: Arc::default(),
            asking: false,
        }
    }

    /// Looks when it's time; true when that just found a release newer
    /// than this one.
    pub fn check(&mut self) -> bool {
        if self.asking {
            let Some(answer) = self.answer.lock().unwrap().take() else {
                return false;
            };
            self.asking = false;
            let Ok(v) = answer else {
                self.due = Instant::now() + RETRY_AFTER;
                return false;
            };
            self.due = Instant::now() + CHECK_EVERY;
            if let Some(p) = &self.cache {
                save(p, &v);
            }
            let fresh = self.latest.as_deref() != Some(v.as_str()) && newer(&v, CURRENT);
            self.latest = Some(v);
            return fresh;
        }
        if Instant::now() < self.due {
            return false;
        }
        self.asking = true;
        let slot = self.answer.clone();
        std::thread::spawn(move || {
            let r = latest();
            *slot.lock().unwrap() = Some(r);
        });
        false
    }

    /// The newest release, when it's newer than this one.
    pub fn available(&self) -> Option<&str> {
        self.latest.as_deref().filter(|v| newer(v, CURRENT))
    }

    /// Whether a look has found out yet.
    pub fn known(&self) -> bool {
        self.latest.is_some()
    }
}

/// How this copy was installed, and so how it's updated.
#[derive(Debug, PartialEq, Eq)]
pub enum How {
    /// By Homebrew, whose `brew` this is.
    Brew(PathBuf),
    /// By Scoop, which doesn't update a program while it runs.
    Scoop,
    /// By a package manager (.deb, .rpm, Nix, snap), where it's updated.
    Package,
    /// Built from the source.
    Source,
    /// Any other way (a release archive, cargo install): replaced in place.
    Replace(PathBuf),
}

/// How the copy at `exe` (resolved, not a link to it) was installed.
pub fn how(exe: &Path) -> How {
    let s = exe.to_string_lossy().replace('\\', "/");
    let lower = s.to_ascii_lowercase();
    if let Some(i) = s.find("/Cellar/fss-mqtt/") {
        return How::Brew(Path::new(&s[..i]).join("bin").join("brew"));
    }
    let scoop_root = std::env::var("SCOOP")
        .ok()
        .map(|d| d.replace('\\', "/").trim_end_matches('/').to_ascii_lowercase())
        .filter(|d| !d.is_empty());
    if lower.contains("/scoop/apps/fss-mqtt/")
        || scoop_root.is_some_and(|d| lower.contains(&format!("{d}/apps/fss-mqtt/")))
    {
        return How::Scoop;
    }
    if lower.contains("/target/release/") || lower.contains("/target/debug/") {
        return How::Source;
    }
    if ["/usr/bin/", "/usr/sbin/", "/bin/", "/sbin/", "/nix/store/", "/snap/"]
        .iter()
        .any(|d| s.starts_with(d))
    {
        return How::Package;
    }
    How::Replace(exe.to_path_buf())
}

/// What installing a release is doing; then whether it worked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Working(String),
    Done,
    Failed(String),
}

/// A release being installed, in the background.
pub struct Job {
    pub version: String,
    step: Arc<Mutex<Step>>,
}

impl Job {
    pub fn step(&self) -> Step {
        self.step.lock().unwrap().clone()
    }

    /// One that has come to `step` (for tests).
    #[cfg(test)]
    pub fn ended(version: &str, step: Step) -> Job {
        Job {
            version: version.into(),
            step: Arc::new(Mutex::new(step)),
        }
    }
}

/// Starts installing `version` the way this copy was installed, or says
/// what to do instead when that can't be done from here.
pub fn start(how: &How, version: &str) -> Result<Job, String> {
    let step = Arc::new(Mutex::new(Step::Working("starting".into())));
    let job = Job {
        version: version.to_string(),
        step: step.clone(),
    };
    let v = version.to_string();
    match how {
        How::Brew(brew) => {
            let brew = brew.clone();
            run(step, move |say| brew_upgrade(&brew, say));
        }
        How::Replace(exe) => {
            let exe = exe.clone();
            run(step, move |say| replace(&exe, &v, say));
        }
        How::Scoop => {
            return Err(
                "Scoop doesn't update fss-mqtt while it runs: quit, run `scoop update fss-mqtt`, \
                 then `fss-mqtt --resume` to continue"
                    .into(),
            );
        }
        How::Package if crate::brand::oma() => {
            return Err(format!(
                "OmaMQTT {version} comes with Omarchy's updates: omarchy-update"
            ));
        }
        How::Package => {
            return Err(format!(
                "this fss-mqtt came with a package manager: update it there, or get {version} from {}",
                release_url(version)
            ));
        }
        How::Source => return Err("this fss-mqtt was built from source: pull and build it again".into()),
    }
    Ok(job)
}

/// Does `work` on a thread of its own, saying how it goes in `step`.
fn run(step: Arc<Mutex<Step>>, work: impl FnOnce(&dyn Fn(String)) -> Result<(), String> + Send + 'static) {
    std::thread::spawn(move || {
        let say = |s: String| *step.lock().unwrap() = Step::Working(s);
        let end = match work(&say) {
            Ok(()) => Step::Done,
            Err(e) => Step::Failed(e),
        };
        *step.lock().unwrap() = end;
    });
}

/// `brew update`, so the tap has the new formula, then `brew upgrade`.
fn brew_upgrade(brew: &Path, say: &dyn Fn(String)) -> Result<(), String> {
    let run = |args: &[&str]| -> Result<(), String> {
        let out = Command::new(brew)
            .args(args)
            .env("HOMEBREW_NO_ENV_HINTS", "1")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("{}: {e}", brew.display()))?;
        if out.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
        Err(format!("brew {} failed: {last}", args[0]))
    };
    say("updating Homebrew".into());
    run(&["update"])?;
    say("upgrading with Homebrew".into());
    run(&["upgrade", TAP_FORMULA])
}

/// The release archive for this platform, if there's one.
fn asset(v: &str) -> Option<String> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        // The x86_64 build runs on Windows on Arm too.
        ("windows", _) => return Some(format!("fss-mqtt-{v}-x86_64-pc-windows-msvc.zip")),
        _ => return None,
    };
    Some(format!("fss-mqtt-{v}-{target}.tar.gz"))
}

/// Downloads the release archive, checks it against the release's
/// SHA256SUMS, takes the program out and puts it in place of `exe`.
fn replace(exe: &Path, version: &str, say: &dyn Fn(String)) -> Result<(), String> {
    let name = asset(version).ok_or("there's no release build for this platform")?;
    let base = format!("https://github.com/{REPO}/releases/download/v{version}");
    say("downloading".into());
    let sums = http::get(&format!("{base}/SHA256SUMS"), true, 1 << 20, &mut |_, _| {})?;
    if sums.status != 200 {
        return Err(format!("SHA256SUMS: HTTP {}", sums.status));
    }
    let want =
        sum_for(&String::from_utf8_lossy(&sums.body), &name).ok_or_else(|| format!("{name} isn't in SHA256SUMS"))?;
    let file = http::get(&format!("{base}/{name}"), true, MAX_DOWNLOAD, &mut |got, of| {
        say(match of {
            Some(n) if n > 0 => format!("downloading {}%", got * 100 / n),
            _ => format!("downloading {}", crate::fmt::human_bytes(got)),
        })
    })?;
    if file.status != 200 {
        return Err(format!("{name}: HTTP {}", file.status));
    }
    if sha256(&file.body) != want {
        return Err(format!("{name} doesn't match its SHA256SUMS"));
    }
    say("installing".into());
    let program = if name.ends_with(".zip") {
        unzip(&file.body, "fss-mqtt.exe")?
    } else {
        untar_gz(&file.body, "fss-mqtt")?
    };
    install(exe, &program, version)
}

fn sha256(b: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, b)
        .as_ref()
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// The checksum SHA256SUMS lists for `name` ("<hex>  <name>" lines).
fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (sum, file) = l.trim().split_once(char::is_whitespace)?;
        (file.trim().trim_start_matches('*') == name).then(|| sum.to_ascii_lowercase())
    })
}

/// The file called `name` (in any folder) in a .tar.gz.
fn untar_gz(b: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let mut tar = Vec::new();
    crate::gzip::Reader::new(b)
        .read_to_end(&mut tar)
        .map_err(|e| format!("unpacking the archive: {e}"))?;
    let mut p = 0;
    while let Some(h) = tar.get(p..p + 512) {
        if h.iter().all(|&c| c == 0) {
            break;
        }
        let text = |a: usize, n: usize| {
            let f = &h[a..a + n];
            String::from_utf8_lossy(&f[..f.iter().position(|&c| c == 0).unwrap_or(n)]).into_owned()
        };
        let mut path = text(0, 100);
        if &h[257..262] == b"ustar" && h[345] != 0 {
            path = format!("{}/{path}", text(345, 155));
        }
        let size = usize::from_str_radix(text(124, 12).trim(), 8).map_err(|_| "a damaged archive")?;
        let start = p + 512;
        let end = start
            .checked_add(size)
            .filter(|&e| e <= tar.len())
            .ok_or("a cut-off archive")?;
        let file = h[156] == b'0' || h[156] == 0;
        if file && (path == name || path.ends_with(&format!("/{name}"))) {
            return Ok(tar[start..end].to_vec());
        }
        p = start + size.div_ceil(512) * 512;
    }
    Err(format!("no {name} in the archive"))
}

/// The file called `name` (in any folder) in a zip.
fn unzip(b: &[u8], name: &str) -> Result<Vec<u8>, String> {
    let u16_at = |i: usize| b.get(i..i + 2).map(|x| u16::from_le_bytes([x[0], x[1]]) as usize);
    let u32_at = |i: usize| b.get(i..i + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]));
    let damaged = || "a damaged archive".to_string();
    // The end of the central directory, which lists the entries.
    let eocd = (0..b.len().saturating_sub(21))
        .rev()
        .find(|&i| b[i..].starts_with(b"PK\x05\x06"))
        .ok_or("not a zip archive")?;
    let count = u16_at(eocd + 10).ok_or_else(damaged)?;
    let mut p = u32_at(eocd + 16).ok_or_else(damaged)? as usize;
    for _ in 0..count {
        if !b.get(p..).is_some_and(|x| x.starts_with(b"PK\x01\x02")) {
            return Err(damaged());
        }
        let field = |o: usize| u16_at(p + o).ok_or_else(damaged);
        let (method, name_len, extra_len, comment_len) = (field(10)?, field(28)?, field(30)?, field(32)?);
        let crc = u32_at(p + 16).ok_or_else(damaged)?;
        let packed = u32_at(p + 20).ok_or_else(damaged)? as usize;
        let size = u32_at(p + 24).ok_or_else(damaged)? as usize;
        let local = u32_at(p + 42).ok_or_else(damaged)? as usize;
        let entry = b.get(p + 46..p + 46 + name_len).ok_or_else(damaged)?;
        let entry = String::from_utf8_lossy(entry);
        if entry == name || entry.ends_with(&format!("/{name}")) {
            if !b.get(local..).is_some_and(|x| x.starts_with(b"PK\x03\x04")) {
                return Err(damaged());
            }
            let skip = 30 + u16_at(local + 26).ok_or_else(damaged)? + u16_at(local + 28).ok_or_else(damaged)?;
            let data = b.get(local + skip..local + skip + packed).ok_or_else(damaged)?;
            let out = match method {
                0 => data.to_vec(),
                8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, size).map_err(|_| damaged())?,
                m => return Err(format!("a zip compressed with method {m}")),
            };
            if out.len() != size || crate::crc32::crc32(&out) != crc {
                return Err(damaged());
            }
            return Ok(out);
        }
        p += 46 + name_len + extra_len + comment_len;
    }
    Err(format!("no {name} in the archive"))
}

/// Puts `program` in place of `exe`, once it has said it's `version` from
/// beside it: so it surely runs here.
fn install(exe: &Path, program: &[u8], version: &str) -> Result<(), String> {
    let dir = exe.parent().ok_or("nowhere to install")?;
    let new = dir.join(if cfg!(windows) {
        "fss-mqtt.new.exe"
    } else {
        ".fss-mqtt.new"
    });
    std::fs::write(&new, program).map_err(|e| match e.kind() {
        // Another user's, or a package manager's.
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => format!(
            "can't write in {}: install {version} the way you installed this one, or from {}",
            dir.display(),
            release_url(version)
        ),
        _ => format!("can't write in {}: {e}", dir.display()),
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(exe).map_or(0o755, |m| m.permissions().mode() & 0o7777) | 0o100;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("{}: {e}", new.display()))?;
    }
    match crate::upgrade::probe(&new) {
        Some(v) if v == version => {}
        said => {
            let _ = std::fs::remove_file(&new);
            return Err(match said {
                Some(v) => format!("the download says it's {v}, not {version}"),
                None => "the download doesn't run here".into(),
            });
        }
    }
    swap(exe, &new)
}

#[cfg(not(windows))]
fn swap(exe: &Path, new: &Path) -> Result<(), String> {
    std::fs::rename(new, exe).map_err(|e| {
        let _ = std::fs::remove_file(new);
        format!("can't replace {}: {e}", exe.display())
    })
}

/// Windows won't replace a program that's running, but lets it be renamed:
/// the old one steps aside, and goes next time (`clean_up`).
#[cfg(windows)]
fn swap(exe: &Path, new: &Path) -> Result<(), String> {
    let old = old_copy(exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(exe, &old).map_err(|e| format!("can't move {} aside: {e}", exe.display()))?;
    std::fs::rename(new, exe).map_err(|e| {
        let _ = std::fs::rename(&old, exe);
        format!("can't replace {}: {e}", exe.display())
    })
}

#[cfg(windows)]
fn old_copy(exe: &Path) -> PathBuf {
    exe.with_extension("old.exe")
}

/// Removes what an update on Windows left beside this program.
pub fn clean_up() {
    #[cfg(windows)]
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_copy(&exe));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn versions_and_tags() {
        assert!(newer("0.8.1", "0.8.0") && newer("0.10.0", "0.9.9") && newer("1.0.0", "0.99.99"));
        assert!(!newer("0.8.0", "0.8.0") && !newer("0.7.9", "0.8.0"));
        assert!(!newer("0.9.0-rc1", "0.8.0") && !newer("0.9", "0.8.0") && !newer("x", "0.8.0"));
        let t = |l: &str| tag_version(l);
        assert_eq!(
            t("https://github.com/mbilling/fss-mqtt/releases/tag/v0.8.1").as_deref(),
            Some("0.8.1")
        );
        assert_eq!(t("/mbilling/fss-mqtt/releases/tag/0.9.0?x=1").as_deref(), Some("0.9.0"));
        assert_eq!(t("https://github.com/mbilling/fss-mqtt/releases"), None);
        assert_eq!(t("https://github.com/o/r/releases/tag/nightly"), None);
        assert!(is_off(Some("off")) && is_off(Some(" OFF ")) && is_off(Some("0")) && is_off(Some("false")));
        assert!(!is_off(None) && !is_off(Some("on")) && !is_off(Some("")));
    }

    #[test]
    fn how_it_was_installed() {
        let h = |p: &str| how(Path::new(p));
        assert_eq!(
            h("/opt/homebrew/Cellar/fss-mqtt/0.8.0/bin/fss-mqtt"),
            How::Brew("/opt/homebrew/bin/brew".into())
        );
        assert_eq!(
            h("/home/linuxbrew/.linuxbrew/Cellar/fss-mqtt/0.8.0/bin/fss-mqtt"),
            How::Brew("/home/linuxbrew/.linuxbrew/bin/brew".into())
        );
        assert_eq!(h(r"\\?\C:\Users\me\scoop\apps\fss-mqtt\0.8.0\fss-mqtt.exe"), How::Scoop);
        assert_eq!(h("/home/me/code/fss-mqtt/target/release/fss-mqtt"), How::Source);
        assert_eq!(h("/usr/bin/fss-mqtt"), How::Package);
        assert_eq!(h("/nix/store/abc-fss-mqtt-0.8.0/bin/fss-mqtt"), How::Package);
        assert_eq!(h("/snap/fss-mqtt/12/bin/fss-mqtt"), How::Package);
        assert_eq!(
            h("/home/me/.cargo/bin/fss-mqtt"),
            How::Replace("/home/me/.cargo/bin/fss-mqtt".into())
        );
        assert!(matches!(start(&How::Scoop, "9.9.9"), Err(e) if e.contains("scoop update fss-mqtt")));
        assert!(
            matches!(start(&How::Package, "9.9.9"), Err(e) if e.contains("update it there") && e.contains("/releases/tag/v9.9.9"))
        );
        assert!(matches!(start(&How::Source, "9.9.9"), Err(e) if e.contains("built from source")));
    }

    #[test]
    fn archive_names_and_checksums() {
        let a = asset("1.2.3");
        if cfg!(any(target_os = "macos", target_os = "linux", windows)) {
            let a = a.unwrap();
            assert!(a.starts_with("fss-mqtt-1.2.3-"), "{a}");
            assert!(a.ends_with(if cfg!(windows) { ".zip" } else { ".tar.gz" }), "{a}");
        }
        let sums = "aa11  fss-mqtt-1.2.3-x.tar.gz\nBB22 *fss-mqtt-1.2.3-y.zip\n";
        assert_eq!(sum_for(sums, "fss-mqtt-1.2.3-x.tar.gz").as_deref(), Some("aa11"));
        assert_eq!(sum_for(sums, "fss-mqtt-1.2.3-y.zip").as_deref(), Some("bb22"));
        assert_eq!(sum_for(sums, "fss-mqtt-1.2.3-x.tar"), None);
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = Vec::new();
        for (name, data) in entries {
            let mut h = [0u8; 512];
            h[..name.len()].copy_from_slice(name.as_bytes());
            h[100..108].copy_from_slice(b"0000755\0");
            h[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
            h[156] = b'0';
            h[257..263].copy_from_slice(b"ustar\0");
            tar.extend_from_slice(&h);
            tar.extend_from_slice(data);
            tar.resize(tar.len().div_ceil(512) * 512, 0);
        }
        tar.extend_from_slice(&[0; 1024]);
        let mut w = crate::gzip::Writer::new(Vec::new()).unwrap();
        w.write_all(&tar).unwrap();
        w.finish().unwrap()
    }

    #[test]
    fn the_program_out_of_a_tar_gz() {
        let program: &[u8] = b"\x7fELF the program";
        let b = tar_gz(&[
            ("fss-mqtt-1.2.3-x/README.md", b"read me"),
            ("fss-mqtt-1.2.3-x/fss-mqtt", program),
        ]);
        assert_eq!(untar_gz(&b, "fss-mqtt").unwrap(), program);
        assert!(untar_gz(&b, "fss-mqtt.exe").unwrap_err().contains("no fss-mqtt.exe"));
        assert!(untar_gz(&b[..b.len() / 2], "fss-mqtt").is_err(), "cut off");
        assert!(untar_gz(b"not gzip", "fss-mqtt").is_err());
    }

    fn zip(entries: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let (mut out, mut dir) = (Vec::new(), Vec::new());
        for &(name, data, deflate) in entries {
            let packed = if deflate {
                miniz_oxide::deflate::compress_to_vec(data, 6)
            } else {
                data.to_vec()
            };
            let crc = crate::crc32::crc32(data);
            let method: u16 = if deflate { 8 } else { 0 };
            let at = out.len() as u32;
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&[20, 0, 0, 0]);
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&packed);
            dir.extend_from_slice(b"PK\x01\x02");
            dir.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
            dir.extend_from_slice(&method.to_le_bytes());
            dir.extend_from_slice(&[0; 4]);
            dir.extend_from_slice(&crc.to_le_bytes());
            dir.extend_from_slice(&(packed.len() as u32).to_le_bytes());
            dir.extend_from_slice(&(data.len() as u32).to_le_bytes());
            dir.extend_from_slice(&(name.len() as u16).to_le_bytes());
            dir.extend_from_slice(&[0; 12]);
            dir.extend_from_slice(&at.to_le_bytes());
            dir.extend_from_slice(name.as_bytes());
        }
        let (dir_at, dir_len) = (out.len() as u32, dir.len() as u32);
        out.extend_from_slice(&dir);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&dir_len.to_le_bytes());
        out.extend_from_slice(&dir_at.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out
    }

    #[test]
    fn the_program_out_of_a_zip() {
        let program = b"MZ the program, the program, the program".repeat(20);
        let b = zip(&[("README.md", b"read me", false), ("fss-mqtt.exe", &program, true)]);
        assert_eq!(unzip(&b, "fss-mqtt.exe").unwrap(), program);
        assert_eq!(unzip(&b, "README.md").unwrap(), b"read me");
        assert!(unzip(&b, "nope").unwrap_err().contains("no nope"));
        // A flipped byte in the program: its CRC says so.
        let mut bad = b.clone();
        let at = bad.windows(4).position(|w| w == b"PK\x01\x02").unwrap() - 3;
        bad[at] ^= 0xff;
        assert!(unzip(&bad, "fss-mqtt.exe").is_err());
        assert!(unzip(b"PK\x05\x06 too short", "x").is_err() && unzip(b"plain", "x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_program_goes_in_place_once_it_runs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fss-mqtt-test-{}-install", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("fss-mqtt");
        std::fs::write(&exe, "#!/bin/sh\necho 'fss-mqtt 0.0.1'\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o750)).unwrap();
        let program = |v: &str| format!("#!/bin/sh\necho 'fss-mqtt {v}'\n").into_bytes();

        // Says another version, or doesn't run: left out, nothing changed.
        let e = install(&exe, &program("1.0.0"), "9.9.9").unwrap_err();
        assert!(e.contains("says it's 1.0.0"), "{e}");
        assert!(install(&exe, b"\x7fELF not for here", "9.9.9").is_err());
        assert!(std::fs::read(&exe).unwrap().ends_with(b"0.0.1'\n"));
        assert!(!dir.join(".fss-mqtt.new").exists(), "nothing left behind");

        install(&exe, &program("9.9.9"), "9.9.9").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), program("9.9.9"));
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o750, "as it was");

        // A folder this user can't write in: says how else.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let e = install(&exe, &program("9.9.10"), "9.9.10");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if let Err(e) = e {
            assert!(e.contains("the way you installed this one"), "{e}");
        } // (root may write anyway)
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Against GitHub itself: `cargo test release_from_github -- --ignored`.
    #[test]
    #[ignore]
    fn release_from_github() {
        let v = latest().unwrap();
        assert!(parse(&v).is_some(), "{v}");
        let dir = std::env::temp_dir().join(format!("fss-mqtt-test-{}-github", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join(if cfg!(windows) { "fss-mqtt.exe" } else { "fss-mqtt" });
        std::fs::write(&exe, "old").unwrap();
        replace(&exe, &v, &|s| eprintln!("{s}")).unwrap();
        assert_eq!(crate::upgrade::probe(&exe).as_deref(), Some(v.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_look_is_kept_for_a_day() {
        let dir = std::env::temp_dir().join(format!("fss-mqtt-test-{}-check", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cache = dir.join("update-check");
        save(&cache, "99.0.0");
        let c = Checker::new(Some(cache.clone()));
        assert_eq!(c.available(), Some("99.0.0"));
        assert!(c.known() && c.due > Instant::now() + CHECK_EVERY - Duration::from_secs(60));
        // Two days old: due now.
        let old = SystemTime::now() - 2 * CHECK_EVERY;
        let at = old.duration_since(UNIX_EPOCH).unwrap().as_secs();
        std::fs::write(&cache, format!("{at} 0.0.1\n")).unwrap();
        let c = Checker::new(Some(cache.clone()));
        assert!(c.due <= Instant::now());
        assert_eq!(c.available(), None, "0.0.1 isn't newer");
        // Nothing kept, or nonsense: due now, nothing known.
        std::fs::write(&cache, "garbage").unwrap();
        let c = Checker::new(Some(cache));
        assert!(!c.known() && c.due <= Instant::now());
        assert_eq!(Checker::found("99.0.0").available(), Some("99.0.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
