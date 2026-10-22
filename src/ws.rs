//! MQTT over WebSockets (RFC 6455; MQTT 5, section 6): an opening handshake
//! that asks for the "mqtt" subprotocol, then MQTT's bytes in binary frames,
//! masked going out (as a client's must be) and unpacked coming in, whatever
//! their boundaries: a packet may span frames, or a frame hold several.
//! Pings are answered; a close ends the stream. Under a read timeout, a frame
//! that arrives in parts is kept until it's whole.

use std::io::{self, Read, Write};

use ring::rand::SecureRandom;

const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const MAX_HEAD: usize = 16 * 1024;
/// Larger than any MQTT packet (256 MiB).
const MAX_FRAME: u64 = 512 << 20;
/// Headers the handshake sets itself.
const OURS: [&str; 6] = [
    "host",
    "upgrade",
    "connection",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
];

/// What the opening request asks for: the path (with any query) and the
/// connection's own headers, some with their values in environment
/// variables (read when connecting, not when the file is).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Upgrade {
    pub path: String,
    pub headers: Vec<(String, String)>,
    /// (name, environment variable)
    pub headers_env: Vec<(String, String)>,
}

impl Upgrade {
    /// All the headers to send, those from the environment read now.
    pub fn headers(&self) -> Result<Vec<(String, String)>, String> {
        let mut out = self.headers.clone();
        for (name, var) in &self.headers_env {
            match std::env::var(var) {
                Ok(v) if !v.is_empty() => out.push(header(&format!("{name}: {v}"))?),
                _ => return Err(format!("the {name} header is to come from ${var}, which isn't set")),
            }
        }
        Ok(out)
    }
}

/// A header from the connections file, "Name: value": a name a request can
/// carry, and not one the handshake sets itself.
pub fn header(s: &str) -> Result<(String, String), String> {
    let (name, value) = s
        .split_once(':')
        .ok_or_else(|| format!("{s:?} is not \"Name: value\""))?;
    let (name, value) = (name.trim(), value.trim());
    let token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
    if name.is_empty() || !name.chars().all(token) {
        return Err(format!("{name:?} is not a header name"));
    }
    if OURS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(format!("{name} is set by the WebSocket handshake itself"));
    }
    if value.chars().any(|c| c.is_control() && c != '\t') {
        return Err(format!("{name}: the value has a control character"));
    }
    Ok((name.to_string(), value.to_string()))
}

/// A WebSocket over `inner` (a TCP or TLS stream), once the handshake is done.
pub struct Ws<S> {
    pub inner: S,
    /// Bytes read, not yet a whole frame (from `start`).
    raw: Vec<u8>,
    start: usize,
    /// Frames' data, not yet read (from `pos`).
    data: Vec<u8>,
    pos: usize,
    closed: bool,
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("websocket: {}", msg.into()))
}

fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut b = [0u8; N];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| io::Error::other("no random numbers"))?;
    Ok(b)
}

/// The Sec-WebSocket-Accept a server answers `key` with.
fn accept(key: &str) -> String {
    let d = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        format!("{key}{GUID}").as_bytes(),
    );
    crate::base64::encode(d.as_ref())
}

/// Opens the WebSocket on `s`, to `host`:`port` (`tls` for wss): the
/// request, then the server's 101 checked. `s` should have a read timeout.
pub fn handshake<S: Read + Write>(
    mut s: S,
    host: &str,
    port: u16,
    tls: bool,
    up: &Upgrade,
    headers: &[(String, String)],
) -> Result<Ws<S>, String> {
    let key = crate::base64::encode(&random::<16>().map_err(|e| e.to_string())?);
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let host = match (tls, port) {
        (false, 80) | (true, 443) => host,
        _ => format!("{host}:{port}"),
    };
    let mut req = format!(
        "GET {} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: mqtt\r\n",
        up.path,
    );
    // The connection's own User-Agent, if it sets one (a gateway may look at it).
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("user-agent")) {
        req += &format!("User-Agent: fss-mqtt/{}\r\n", env!("CARGO_PKG_VERSION"));
    }
    for (k, v) in headers {
        req += &format!("{k}: {v}\r\n");
    }
    req += "\r\n";
    s.write_all(req.as_bytes())
        .and_then(|_| s.flush())
        .map_err(|e| e.to_string())?;

    let mut buf = Vec::new();
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err("websocket: the broker's answer is too long".into());
        }
        let mut tmp = [0u8; 4096];
        match s.read(&mut tmp) {
            Ok(0) => return Err("websocket: the broker closed the connection during the handshake".into()),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Err("websocket: timed out waiting for the broker's handshake".into());
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let head = String::from_utf8_lossy(&buf[..end]);
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    if status.split_whitespace().nth(1) != Some("101") {
        let why = status.split_once(' ').map_or(status, |(_, r)| r);
        return Err(format!("websocket: the broker refused the upgrade: HTTP {why}"));
    }
    let headers: Vec<(String, &str)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim()))
        .collect();
    let get = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| *v);
    let upgraded = get("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        && get("connection").is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("upgrade")));
    if !upgraded {
        return Err("websocket: the broker's 101 doesn't say it upgraded (Upgrade, Connection)".into());
    }
    if get("sec-websocket-accept") != Some(accept(&key).as_str()) {
        return Err("websocket: the broker's handshake doesn't match the request (Sec-WebSocket-Accept)".into());
    }
    // A broker may leave the subprotocol unsaid; it may not pick another.
    if let Some(p) = get("sec-websocket-protocol")
        && !p.eq_ignore_ascii_case("mqtt")
    {
        return Err(format!("websocket: the broker chose the subprotocol {p:?}, not mqtt"));
    }
    Ok(Ws {
        inner: s,
        raw: buf[end..].to_vec(),
        start: 0,
        data: Vec::new(),
        pos: 0,
        closed: false,
    })
}

impl<S: Read + Write> Ws<S> {
    /// Sends a frame: final, masked, with opcode `op`.
    fn send(&mut self, op: u8, payload: &[u8]) -> io::Result<()> {
        let mut f = Vec::with_capacity(payload.len() + 14);
        f.push(0x80 | op);
        match payload.len() {
            n if n < 126 => f.push(0x80 | n as u8),
            n if n <= 0xffff => {
                f.push(0x80 | 126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(0x80 | 127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        let key = random::<4>()?;
        f.extend_from_slice(&key);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
        self.inner.write_all(&f)?;
        self.inner.flush()
    }

    /// Takes one whole frame from what was read, if there is one: data kept
    /// for reading, a ping answered, a close noted. False when the next
    /// frame isn't all here yet.
    fn frame(&mut self) -> io::Result<bool> {
        let b = &self.raw[self.start..];
        if b.len() < 2 {
            return Ok(false);
        }
        if b[0] & 0x70 != 0 {
            return Err(bad("a frame with reserved bits set"));
        }
        let op = b[0] & 0x0f;
        if op >= 0x8 && (b[0] & 0x80 == 0 || b[1] & 0x7f > 125) {
            return Err(bad("a control frame in parts, or over 125 bytes"));
        }
        let (len, mut at) = match b[1] & 0x7f {
            126 if b.len() >= 4 => (u16::from_be_bytes([b[2], b[3]]) as u64, 4),
            127 if b.len() >= 10 => (u64::from_be_bytes(b[2..10].try_into().unwrap()), 10),
            126 | 127 => return Ok(false),
            n => (n as u64, 2),
        };
        if len > MAX_FRAME {
            return Err(bad(format!("a frame of {len} bytes")));
        }
        let mask = if b[1] & 0x80 != 0 {
            let Some(k) = b.get(at..at + 4) else { return Ok(false) };
            at += 4;
            Some([k[0], k[1], k[2], k[3]])
        } else {
            None
        };
        let len = len as usize;
        if b.len() < at + len {
            return Ok(false);
        }
        let mut payload = b[at..at + len].to_vec();
        if let Some(k) = mask {
            payload.iter_mut().enumerate().for_each(|(i, x)| *x ^= k[i % 4]);
        }
        self.start += at + len;
        if self.start == self.raw.len() {
            self.raw.clear();
            self.start = 0;
        } else if self.start > 1 << 16 {
            self.raw.drain(..self.start);
            self.start = 0;
        }
        match op {
            // Continuation, text (MQTT sends binary; taken all the same), binary.
            0x0..=0x2 => self.data.extend_from_slice(&payload),
            0x8 => {
                // Answered with its status code, then the stream ends.
                let status = payload.get(..2).unwrap_or_default().to_vec();
                let _ = self.send(0x8, &status);
                self.closed = true;
            }
            0x9 => self.send(0xA, &payload)?,
            0xA => {}
            _ => return Err(bad(format!("a frame with opcode {op}"))),
        }
        Ok(true)
    }
}

impl<S: Read + Write> Read for Ws<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.data.len() {
                let n = buf.len().min(self.data.len() - self.pos);
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                if self.pos == self.data.len() {
                    self.data.clear();
                    self.pos = 0;
                }
                return Ok(n);
            }
            if self.closed {
                return Ok(0);
            }
            if self.frame()? {
                continue;
            }
            // A read timeout here leaves a frame's first part kept, for later.
            let mut tmp = [0u8; 16 * 1024];
            let n = self.inner.read(&mut tmp)?;
            if n == 0 {
                return Ok(0);
            }
            self.raw.extend_from_slice(&tmp[..n]);
        }
    }
}

impl<S: Read + Write> Write for Ws<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.send(0x2, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    /// A frame as a server sends it: unmasked.
    fn frame(op: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0x80 | op];
        match payload.len() {
            n if n < 126 => f.push(n as u8),
            n if n <= 0xffff => {
                f.push(126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        f.extend_from_slice(payload);
        f
    }

    /// Reads a client's frame from `s`: (opcode, unmasked payload).
    fn read_frame(s: &mut impl Read) -> (u8, Vec<u8>) {
        let mut h = [0u8; 2];
        s.read_exact(&mut h).unwrap();
        assert_eq!(h[1] & 0x80, 0x80, "a client's frames are masked");
        let len = match h[1] & 0x7f {
            126 => {
                let mut l = [0u8; 2];
                s.read_exact(&mut l).unwrap();
                u16::from_be_bytes(l) as usize
            }
            127 => {
                let mut l = [0u8; 8];
                s.read_exact(&mut l).unwrap();
                u64::from_be_bytes(l) as usize
            }
            n => n as usize,
        };
        let mut k = [0u8; 4];
        s.read_exact(&mut k).unwrap();
        let mut p = vec![0u8; len];
        s.read_exact(&mut p).unwrap();
        p.iter_mut().enumerate().for_each(|(i, x)| *x ^= k[i % 4]);
        (h[0] & 0x0f, p)
    }

    /// Reads the client's opening request; the headers, lowercased.
    fn read_request(s: &mut TcpStream) -> (String, Vec<(String, String)>) {
        let mut buf = Vec::new();
        let mut b = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).unwrap();
            buf.push(b[0]);
        }
        let text = String::from_utf8(buf).unwrap();
        let mut lines = text.split("\r\n");
        let first = lines.next().unwrap().to_string();
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        (first, headers)
    }

    fn get<'a>(h: &'a [(String, String)], k: &str) -> &'a str {
        &h.iter().find(|(n, _)| n == k).unwrap().1
    }

    /// A server on a local port running `serve` for one client; the client's
    /// stream, with a short read timeout as the MQTT client has.
    fn pair(serve: impl FnOnce(TcpStream) + Send + 'static) -> (TcpStream, thread::JoinHandle<()>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let t = thread::spawn(move || serve(l.accept().unwrap().0));
        let c = TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        (c, t)
    }

    fn up(path: &str) -> Upgrade {
        Upgrade {
            path: path.into(),
            headers: vec![("CF-Access-Client-Id".into(), "abc.access".into())],
            ..Default::default()
        }
    }

    #[test]
    fn the_rfc_example_key() {
        assert_eq!(accept("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn handshake_then_frames_whatever_their_boundaries() {
        let big: Vec<u8> = (0..70_000u32).map(|i| i as u8).collect();
        let big2 = big.clone();
        let (c, server) = pair(move |mut s| {
            let (first, h) = read_request(&mut s);
            assert_eq!(first, "GET /mqtt?x=1 HTTP/1.1");
            assert_eq!(get(&h, "host"), "localhost:1884", "a port that isn't 80 is named");
            assert_eq!(get(&h, "sec-websocket-protocol"), "mqtt");
            assert_eq!(get(&h, "sec-websocket-version"), "13");
            assert_eq!(get(&h, "cf-access-client-id"), "abc.access");
            let answer = accept(get(&h, "sec-websocket-key"));
            // The 101 and the first frame together; then a frame in two parts
            // with a pause between (longer than the client's read timeout).
            let mut out = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {answer}\r\nSec-WebSocket-Protocol: mqtt\r\n\r\n"
            )
            .into_bytes();
            out.extend(frame(0x2, b"\x20\x03"));
            s.write_all(&out).unwrap();
            let f = frame(0x2, b"\x00\x00\x00");
            s.write_all(&f[..2]).unwrap();
            thread::sleep(Duration::from_millis(250));
            s.write_all(&f[2..]).unwrap();
            // A ping, answered by a pong with its payload.
            s.write_all(&frame(0x9, b"hi")).unwrap();
            assert_eq!(read_frame(&mut s), (0xA, b"hi".to_vec()));
            // 16- and 64-bit lengths.
            s.write_all(&frame(0x2, &big2[..300])).unwrap();
            s.write_all(&frame(0x2, &big2)).unwrap();
            // What the client writes comes as masked binary frames.
            assert_eq!(read_frame(&mut s), (0x2, b"\xc0\x00".to_vec()));
            assert_eq!(read_frame(&mut s), (0x2, big2.clone()));
            // A close is answered with its status code.
            s.write_all(&frame(0x8, &1000u16.to_be_bytes())).unwrap();
            assert_eq!(read_frame(&mut s), (0x8, 1000u16.to_be_bytes().to_vec()));
        });
        let mut ws = handshake(c, "localhost", 1884, false, &up("/mqtt?x=1"), &up("").headers).unwrap();
        // CONNACK's bytes, across the two frames and the pause.
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        while got.len() < 5 {
            match ws.read(&mut buf) {
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(got, b"\x20\x03\x00\x00\x00");
        let mut rest = Vec::new();
        while rest.len() < 300 + big.len() {
            match ws.read(&mut buf) {
                Ok(n) => rest.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(&rest[..300], &big[..300]);
        assert_eq!(&rest[300..], &big[..]);
        ws.write_all(b"\xc0\x00").unwrap();
        ws.write_all(&big).unwrap();
        // The close: the end of the stream.
        let end = loop {
            match ws.read(&mut buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => panic!("{e}"),
            }
        };
        assert_eq!(end, 0);
        server.join().unwrap();
    }

    #[test]
    fn a_refusal_or_a_wrong_answer_is_an_error() {
        let (c, server) = pair(|mut s| {
            read_request(&mut s);
            s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        let e = handshake(c, "h", 80, false, &up("/nope"), &[]).err().unwrap();
        assert!(e.contains("refused the upgrade: HTTP 404 Not Found"), "{e}");
        server.join().unwrap();

        let (c, server) = pair(|mut s| {
            read_request(&mut s);
            let r = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                     Sec-WebSocket-Accept: nope\r\n\r\n";
            s.write_all(r.as_bytes()).unwrap();
        });
        let e = handshake(c, "h", 80, false, &up("/mqtt"), &[]).err().unwrap();
        assert!(e.contains("Sec-WebSocket-Accept"), "{e}");
        server.join().unwrap();

        let (c, server) = pair(|mut s| {
            let (_, h) = read_request(&mut s);
            assert_eq!(get(&h, "host"), "h", "the default port goes unnamed");
            let r = format!(
                "HTTP/1.1 101 OK\r\nUpgrade: websocket\r\nConnection: keep-alive, Upgrade\r\n\
                 Sec-WebSocket-Accept: {}\r\nSec-WebSocket-Protocol: chat\r\n\r\n",
                accept(get(&h, "sec-websocket-key"))
            );
            s.write_all(r.as_bytes()).unwrap();
        });
        let e = handshake(c, "h", 80, false, &up("/mqtt"), &[]).err().unwrap();
        assert!(e.contains("\"chat\""), "{e}");
        server.join().unwrap();

        // A 101 that doesn't say it upgraded.
        let (c, server) = pair(|mut s| {
            let (_, h) = read_request(&mut s);
            let r = format!(
                "HTTP/1.1 101 OK\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                accept(get(&h, "sec-websocket-key"))
            );
            s.write_all(r.as_bytes()).unwrap();
        });
        let e = handshake(c, "h", 80, false, &up("/mqtt"), &[]).err().unwrap();
        assert!(e.contains("Upgrade, Connection"), "{e}");
        server.join().unwrap();
    }

    #[test]
    fn its_own_user_agent_and_bad_control_frames() {
        let (c, server) = pair(|mut s| {
            let (_, h) = read_request(&mut s);
            assert_eq!(h.iter().filter(|(k, _)| k == "user-agent").count(), 1);
            assert_eq!(get(&h, "user-agent"), "plant-gateway/2");
            let r = format!(
                "HTTP/1.1 101 OK\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
                accept(get(&h, "sec-websocket-key"))
            );
            s.write_all(r.as_bytes()).unwrap();
            // A ping without FIN: a protocol error.
            s.write_all(&[0x09, 0x00]).unwrap();
            thread::sleep(Duration::from_millis(300));
        });
        let agent = [("User-Agent".to_string(), "plant-gateway/2".to_string())];
        let mut ws = handshake(c, "h", 80, false, &up("/mqtt"), &agent).unwrap();
        let mut buf = [0u8; 8];
        let e = loop {
            match ws.read(&mut buf) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {}
                other => break other.unwrap_err(),
            }
        };
        assert!(e.to_string().contains("control frame"), "{e}");
        server.join().unwrap();
    }

    #[test]
    fn headers_from_the_environment_when_connecting() {
        let var = format!("FSS_MQTT_TEST_HEADER_{}", std::process::id());
        let up = Upgrade {
            headers: vec![("X-A".into(), "1".into())],
            headers_env: vec![("Authorization".into(), var.clone())],
            ..Default::default()
        };
        let e = up.headers().unwrap_err();
        assert!(
            e.contains("Authorization") && e.contains(&var) && e.contains("isn't set"),
            "{e}"
        );
        // SAFETY: the variable is this test's own, read by nothing else.
        unsafe { std::env::set_var(&var, "Bearer t") };
        assert_eq!(
            up.headers().unwrap(),
            vec![("X-A".into(), "1".into()), ("Authorization".into(), "Bearer t".into())]
        );
        unsafe { std::env::remove_var(&var) };
    }

    #[test]
    fn headers_from_the_file() {
        assert_eq!(
            header("Authorization: Bearer x:y").unwrap(),
            ("Authorization".into(), "Bearer x:y".into())
        );
        assert!(header("no colon").is_err());
        assert!(header("Bad Name: x").is_err());
        assert!(
            header("Sec-WebSocket-Protocol: mqttv3.1")
                .unwrap_err()
                .contains("handshake itself")
        );
        assert!(header("X-A: one\rtwo").is_err());
    }
}
