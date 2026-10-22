//! Reaching a broker through a proxy: an HTTP proxy's CONNECT tunnel, or a
//! SOCKS5 proxy's (RFC 1928, with RFC 1929's login). Either way what comes
//! out is a TCP stream to the broker, and TLS, WebSockets and MQTT run over
//! it as they would without one. The broker's name is the proxy's to
//! resolve.

use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream};
use std::time::Duration;

const MAX_HEAD: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Http,
    Socks5,
}

/// A proxy from a connection's settings: http://[user@]host:port or
/// socks5://[user@]host:port. The password, for a user, is the environment
/// variable `password_env`'s, read when connecting.
#[derive(Clone, Debug, PartialEq)]
pub struct Proxy {
    pub kind: Kind,
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub password_env: Option<String>,
}

impl Proxy {
    /// The proxy at `url`; a password there is refused, as in the rest of
    /// the connections file.
    pub fn parse(url: &str, password_env: Option<String>) -> Result<Proxy, String> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| format!("proxy {url:?}: give it as http://host:port or socks5://host:port"))?;
        let kind = match scheme.to_ascii_lowercase().as_str() {
            "http" => Kind::Http,
            "socks5" | "socks5h" => Kind::Socks5,
            other => return Err(format!("proxy: {other}:// isn't a kind it knows (http, socks5)")),
        };
        let rest = rest.split(['/', '?', '#']).next().unwrap_or("");
        let (user, hostport) = match rest.rsplit_once('@') {
            Some((u, _)) if u.contains(':') => {
                return Err("proxy: a password isn't read from the url; set proxy_password_env".into());
            }
            Some((u, h)) => (Some(u.to_string()), h),
            None => (None, rest),
        };
        let (host, port) = match hostport.strip_prefix('[') {
            Some(r) => r
                .split_once("]:")
                .ok_or_else(|| format!("proxy {url:?}: the port is missing"))?,
            None => hostport
                .rsplit_once(':')
                .ok_or_else(|| format!("proxy {url:?}: the port is missing"))?,
        };
        let port = port.parse().map_err(|_| format!("proxy {url:?}: bad port {port:?}"))?;
        if host.is_empty() {
            return Err(format!("proxy {url:?}: the host is missing"));
        }
        Ok(Proxy {
            kind,
            host: host.to_string(),
            port,
            user: user.filter(|u| !u.is_empty()),
            password_env,
        })
    }

    /// The password to log in with, read now; an error when there's a user
    /// and its variable isn't set.
    pub fn password(&self) -> Result<Option<String>, String> {
        let Some(user) = &self.user else { return Ok(None) };
        let var = self.password_env.as_deref().ok_or_else(|| {
            format!("proxy: {user}'s password is to come from an environment variable: set proxy_password_env")
        })?;
        match std::env::var(var) {
            Ok(p) if !p.is_empty() => Ok(Some(p)),
            _ => Err(format!(
                "proxy: {user}'s password is to come from ${var}, which isn't set"
            )),
        }
    }

    /// A TCP stream to `host`:`port` through the proxy (dialled with `dial`),
    /// which has to answer within `wait`.
    pub fn connect(
        &self,
        password: Option<&str>,
        host: &str,
        port: u16,
        dial: impl Fn(&str, u16) -> Result<TcpStream, String>,
        wait: Duration,
    ) -> Result<TcpStream, String> {
        let at = format!("proxy {}:{}", self.host, self.port);
        let mut s = dial(&self.host, self.port).map_err(|e| format!("{at}: {e}"))?;
        let _ = s.set_read_timeout(Some(wait));
        let done = match self.kind {
            Kind::Http => self.http(&mut s, password, host, port),
            Kind::Socks5 => self.socks5(&mut s, password, host, port),
        };
        done.map_err(|e| format!("{at}: {e}"))?;
        Ok(s)
    }

    fn http(&self, s: &mut TcpStream, password: Option<&str>, host: &str, port: u16) -> Result<(), String> {
        let target = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let mut req = format!(
            "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nUser-Agent: fss-mqtt/{}\r\n",
            env!("CARGO_PKG_VERSION")
        );
        if let Some(user) = &self.user {
            let login = format!("{user}:{}", password.unwrap_or(""));
            req += &format!(
                "Proxy-Authorization: Basic {}\r\n",
                crate::base64::encode(login.as_bytes())
            );
        }
        req += "\r\n";
        s.write_all(req.as_bytes()).map_err(io)?;
        // A byte at a time: what follows the answer is the broker's.
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() > MAX_HEAD {
                return Err("the answer to CONNECT is too long".into());
            }
            let mut b = [0u8; 1];
            match s.read(&mut b) {
                Ok(0) => return Err("closed the connection instead of answering CONNECT".into()),
                Ok(_) => head.push(b[0]),
                Err(e) => return Err(io(e)),
            }
        }
        let head = String::from_utf8_lossy(&head);
        let status = head.lines().next().unwrap_or("");
        let code = status.split_whitespace().nth(1).unwrap_or("");
        if code.starts_with('2') {
            return Ok(());
        }
        let why = status.split_once(' ').map_or(status, |(_, r)| r);
        Err(match (code, &self.user) {
            ("407", None) => "wants a login (407): give the user in the proxy url, and proxy_password_env".into(),
            ("407", Some(u)) => format!("refused {u}'s login (407)"),
            _ => format!("refused the tunnel: HTTP {why}"),
        })
    }

    fn socks5(&self, s: &mut TcpStream, password: Option<&str>, host: &str, port: u16) -> Result<(), String> {
        let greet: &[u8] = if self.user.is_some() { &[5, 2, 0, 2] } else { &[5, 1, 0] };
        s.write_all(greet).map_err(io)?;
        let mut r = [0u8; 2];
        s.read_exact(&mut r).map_err(io)?;
        match r {
            [5, 0] => {}
            [5, 2] => {
                let (Some(user), Some(pw)) = (&self.user, password) else {
                    return Err("wants a login: give the user in the proxy url, and proxy_password_env".into());
                };
                if user.len() > 255 || pw.len() > 255 {
                    return Err("SOCKS5 takes a user and password of 255 bytes at most".into());
                }
                let mut auth = vec![1, user.len() as u8];
                auth.extend_from_slice(user.as_bytes());
                auth.push(pw.len() as u8);
                auth.extend_from_slice(pw.as_bytes());
                s.write_all(&auth).map_err(io)?;
                s.read_exact(&mut r).map_err(io)?;
                if r[1] != 0 {
                    return Err(format!("refused {user}'s login"));
                }
            }
            [5, 0xff] if self.user.is_none() => {
                return Err("wants a login: give the user in the proxy url, and proxy_password_env".into());
            }
            [5, 0xff] => return Err("accepts none of the ways to log in that fss-mqtt has".into()),
            _ => return Err("isn't a SOCKS5 proxy (its greeting is wrong)".into()),
        }
        let mut req = vec![5, 1, 0];
        match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(a)) => {
                req.push(1);
                req.extend_from_slice(&a.octets());
            }
            Ok(IpAddr::V6(a)) => {
                req.push(4);
                req.extend_from_slice(&a.octets());
            }
            Err(_) if host.len() <= 255 => {
                req.extend_from_slice(&[3, host.len() as u8]);
                req.extend_from_slice(host.as_bytes());
            }
            Err(_) => return Err("SOCKS5 takes a host name of 255 bytes at most".into()),
        }
        req.extend_from_slice(&port.to_be_bytes());
        s.write_all(&req).map_err(io)?;
        let mut head = [0u8; 4];
        s.read_exact(&mut head).map_err(io)?;
        if head[0] != 5 {
            return Err("isn't a SOCKS5 proxy (its answer is wrong)".into());
        }
        if head[1] != 0 {
            let why = match head[1] {
                1 => "a general failure",
                2 => "not allowed by its rules",
                3 => "the network is unreachable",
                4 => "the host is unreachable",
                5 => "the connection was refused",
                6 => "the connection timed out",
                7 => "it doesn't do CONNECT",
                8 => "it doesn't take that kind of address",
                _ => "an unknown reason",
            };
            return Err(format!("couldn't reach {host}:{port}: {why}"));
        }
        // The address it connected from, then its port: read past them.
        let len = match head[3] {
            1 => 4,
            4 => 16,
            3 => {
                let mut n = [0u8; 1];
                s.read_exact(&mut n).map_err(io)?;
                n[0] as usize
            }
            _ => return Err("isn't a SOCKS5 proxy (its answer is wrong)".into()),
        };
        let mut rest = vec![0u8; len + 2];
        s.read_exact(&mut rest).map_err(io)?;
        Ok(())
    }
}

fn io(e: std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => "no answer in time".into(),
        std::io::ErrorKind::UnexpectedEof => "closed the connection".into(),
        _ => e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn dial(host: &str, port: u16) -> Result<TcpStream, String> {
        TcpStream::connect((host, port)).map_err(|e| e.to_string())
    }

    /// A proxy on a local port running `serve` for one client.
    fn proxy(
        kind: &str,
        user: &str,
        serve: impl FnOnce(TcpStream) + Send + 'static,
    ) -> (Proxy, thread::JoinHandle<()>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let t = thread::spawn(move || serve(l.accept().unwrap().0));
        (
            Proxy::parse(&format!("{kind}://{user}127.0.0.1:{port}"), Some("PW".into())).unwrap(),
            t,
        )
    }

    fn head(s: &mut TcpStream) -> String {
        let mut h = Vec::new();
        let mut b = [0u8; 1];
        while !h.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).unwrap();
            h.push(b[0]);
        }
        String::from_utf8(h).unwrap()
    }

    /// The tunnel carries bytes both ways once it's open.
    fn echo(s: &mut TcpStream) {
        let mut b = [0u8; 4];
        s.read_exact(&mut b).unwrap();
        s.write_all(&b).unwrap();
    }

    fn through(s: &mut TcpStream) {
        s.write_all(b"ping").unwrap();
        let mut b = [0u8; 4];
        s.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"ping");
    }

    #[test]
    fn proxy_urls() {
        let p = Proxy::parse("http://alice@proxy.corp:8080", Some("PW".into())).unwrap();
        assert_eq!(
            (p.kind, p.host.as_str(), p.port, p.user.as_deref()),
            (Kind::Http, "proxy.corp", 8080, Some("alice"))
        );
        let p = Proxy::parse("socks5h://[::1]:1080/", None).unwrap();
        assert_eq!(
            (p.kind, p.host.as_str(), p.port, p.user),
            (Kind::Socks5, "::1", 1080, None)
        );
        assert!(
            Proxy::parse("http://alice:secret@p:8080", None)
                .unwrap_err()
                .contains("proxy_password_env")
        );
        assert!(Proxy::parse("http://p", None).unwrap_err().contains("port is missing"));
        assert!(Proxy::parse("ftp://p:21", None).unwrap_err().contains("ftp"));
        assert!(Proxy::parse("p:8080", None).is_err());
        let p = Proxy::parse("http://alice@p:8080", None).unwrap();
        assert!(p.password().unwrap_err().contains("proxy_password_env"));
        assert_eq!(Proxy::parse("http://p:8080", None).unwrap().password(), Ok(None));
    }

    #[test]
    fn http_connect_with_a_login() {
        let (p, server) = proxy("http", "alice@", |mut s| {
            let h = head(&mut s);
            assert!(h.starts_with("CONNECT broker.example.com:8883 HTTP/1.1\r\n"), "{h}");
            assert!(h.contains("\r\nHost: broker.example.com:8883\r\n"));
            let login = crate::base64::encode(b"alice:s3cret");
            assert!(
                h.contains(&format!("\r\nProxy-Authorization: Basic {login}\r\n")),
                "{h}"
            );
            s.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").unwrap();
            echo(&mut s);
        });
        let mut s = p
            .connect(Some("s3cret"), "broker.example.com", 8883, dial, Duration::from_secs(5))
            .unwrap();
        through(&mut s);
        server.join().unwrap();
    }

    #[test]
    fn http_refusals_say_why() {
        for (answer, user, want) in [
            (
                "HTTP/1.1 407 Proxy Authentication Required\r\n\r\n",
                "",
                "wants a login (407)",
            ),
            (
                "HTTP/1.1 407 Proxy Authentication Required\r\n\r\n",
                "alice@",
                "refused alice's login",
            ),
            (
                "HTTP/1.1 403 Forbidden\r\n\r\n",
                "",
                "refused the tunnel: HTTP 403 Forbidden",
            ),
        ] {
            let (p, server) = proxy("http", user, move |mut s| {
                head(&mut s);
                s.write_all(answer.as_bytes()).unwrap();
            });
            let e = p
                .connect(Some("x"), "[::1]", 1883, dial, Duration::from_secs(5))
                .unwrap_err();
            assert!(e.starts_with("proxy 127.0.0.1:") && e.contains(want), "{e}");
            server.join().unwrap();
        }
    }

    #[test]
    fn socks5_with_and_without_a_login() {
        // No login; a name for the proxy to resolve.
        let (p, server) = proxy("socks5", "", |mut s| {
            let mut b = [0u8; 3];
            s.read_exact(&mut b).unwrap();
            assert_eq!(b, [5, 1, 0]);
            s.write_all(&[5, 0]).unwrap();
            let mut r = [0u8; 5];
            s.read_exact(&mut r).unwrap();
            assert_eq!(r, [5, 1, 0, 3, 18]);
            let mut name = [0u8; 18 + 2];
            s.read_exact(&mut name).unwrap();
            assert_eq!(&name[..18], b"broker.example.com");
            assert_eq!(u16::from_be_bytes([name[18], name[19]]), 8884);
            s.write_all(&[5, 0, 0, 1, 10, 0, 0, 1, 0x1f, 0x90]).unwrap();
            echo(&mut s);
        });
        let mut s = p
            .connect(None, "broker.example.com", 8884, dial, Duration::from_secs(5))
            .unwrap();
        through(&mut s);
        server.join().unwrap();

        // A login, an IPv4 address, then a refusal.
        let (p, server) = proxy("socks5", "bob@", |mut s| {
            let mut b = [0u8; 4];
            s.read_exact(&mut b).unwrap();
            assert_eq!(b, [5, 2, 0, 2]);
            s.write_all(&[5, 2]).unwrap();
            let mut auth = [0u8; 2 + 3 + 1 + 2];
            s.read_exact(&mut auth).unwrap();
            assert_eq!(&auth, b"\x01\x03bob\x02pw");
            s.write_all(&[1, 0]).unwrap();
            let mut r = [0u8; 10];
            s.read_exact(&mut r).unwrap();
            assert_eq!(r, [5, 1, 0, 1, 10, 1, 2, 3, 0x07, 0x5b]);
            s.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
        });
        let e = p
            .connect(Some("pw"), "10.1.2.3", 1883, dial, Duration::from_secs(5))
            .unwrap_err();
        assert!(
            e.contains("couldn't reach 10.1.2.3:1883: the connection was refused"),
            "{e}"
        );
        server.join().unwrap();
    }
}
