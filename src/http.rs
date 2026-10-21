//! Just enough HTTPS to ask GitHub about releases and fetch one: GET over
//! TLS (the system's trusted certificates), redirects, bodies by
//! `Content-Length`, in chunks or to the end, and a size limit.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;

const CONNECT_WAIT: Duration = Duration::from_secs(10);
const READ_WAIT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;
const MAX_HEAD: usize = 64 * 1024;

pub struct Response {
    pub status: u16,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// GETs an https `url`, following redirects when `follow` (to https only).
/// `progress` hears how much of the body has come, and of how much.
pub fn get(
    url: &str,
    follow: bool,
    limit: usize,
    progress: &mut dyn FnMut(usize, Option<usize>),
) -> Result<Response, String> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let r = fetch(&url, limit, progress)?;
        if follow && matches!(r.status, 301 | 302 | 303 | 307 | 308) {
            let to = r.header("location").ok_or("a redirect without a location")?;
            url = absolute(&url, to)?;
            continue;
        }
        return Ok(r);
    }
    Err("too many redirects".into())
}

/// (host, port, path) of an https URL.
fn split(url: &str) -> Result<(&str, u16, &str), String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| format!("not an https URL: {url}"))?;
    let (authority, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| format!("a bad port in {url}"))?),
        None => (authority, 443),
    };
    if host.is_empty() {
        return Err(format!("no host in {url}"));
    }
    Ok((host, port, path))
}

/// Where a redirect from `base` to `to` goes: https only, never down to http.
fn absolute(base: &str, to: &str) -> Result<String, String> {
    if to.starts_with("https://") {
        return Ok(to.to_string());
    }
    if to.starts_with('/') && !to.starts_with("//") {
        let (host, port, _) = split(base)?;
        return Ok(match port {
            443 => format!("https://{host}{to}"),
            p => format!("https://{host}:{p}{to}"),
        });
    }
    Err(format!("a redirect elsewhere than https: {to}"))
}

/// The system's trusted certificates, read once.
fn tls() -> Result<Arc<rustls::ClientConfig>, String> {
    static CONFIG: OnceLock<Result<Arc<rustls::ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            for c in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(c);
            }
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots)
                .with_no_client_auth();
            Ok(Arc::new(config))
        })
        .clone()
}

fn fetch(url: &str, limit: usize, progress: &mut dyn FnMut(usize, Option<usize>)) -> Result<Response, String> {
    let (host, port, path) = split(url)?;
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("{host}: {e}"))?
        .collect();
    let mut tcp = Err(format!("{host}: no address"));
    for a in &addrs {
        tcp = TcpStream::connect_timeout(a, CONNECT_WAIT).map_err(|e| format!("{host}: {e}"));
        if tcp.is_ok() {
            break;
        }
    }
    let tcp = tcp?;
    let _ = tcp.set_read_timeout(Some(READ_WAIT));
    let _ = tcp.set_write_timeout(Some(READ_WAIT));
    let name = ServerName::try_from(host.to_string()).map_err(|e| format!("{host}: {e}"))?;
    let conn = rustls::ClientConnection::new(tls()?, name).map_err(|e| format!("{host}: {e}"))?;
    let mut s = rustls::StreamOwned::new(conn, tcp);
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: fss-mqtt/{}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        env!("CARGO_PKG_VERSION")
    );
    s.write_all(request.as_bytes()).map_err(|e| format!("{host}: {e}"))?;
    read_response(&mut s, limit, progress).map_err(|e| format!("{host}: {e}"))
}

/// Reads a response to its end (the request asked the server to close).
fn read_response(
    r: &mut impl Read,
    limit: usize,
    progress: &mut dyn FnMut(usize, Option<usize>),
) -> Result<Response, String> {
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > MAX_HEAD {
            return Err("a response head too long".into());
        }
        let n = read_some(r, &mut chunk)?;
        if n == 0 {
            return Err("the connection closed before a response".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buf[..head_end]).map_err(|_| "a response head that isn't text")?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or("a response without a status")?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let mut res = Response {
        status,
        headers,
        body: buf[head_end..].to_vec(),
    };
    let chunked = res
        .header("transfer-encoding")
        .is_some_and(|t| t.to_ascii_lowercase().contains("chunked"));
    let length = if chunked {
        None
    } else {
        res.header("content-length")
            .and_then(|n| n.trim().parse::<usize>().ok())
    };
    if length.is_some_and(|n| n > limit) {
        return Err(format!("more than {} to download", crate::fmt::human_bytes(limit)));
    }
    loop {
        if let Some(n) = length
            && res.body.len() >= n
        {
            res.body.truncate(n);
            break;
        }
        let n = read_some(r, &mut chunk)?;
        if n == 0 {
            break;
        }
        res.body.extend_from_slice(&chunk[..n]);
        // Chunked bodies carry a little framing too.
        if res.body.len() > limit + limit / 16 + 4096 {
            return Err(format!("more than {} to download", crate::fmt::human_bytes(limit)));
        }
        progress(res.body.len(), length);
    }
    if let Some(n) = length
        && res.body.len() < n
    {
        return Err("the connection closed before the end".into());
    }
    if chunked {
        res.body = dechunk(&res.body)?;
    }
    Ok(res)
}

/// Some bytes, or 0 at the end: a server that closes without TLS's
/// close_notify ends the body all the same (its length or its last chunk
/// says whether it all came).
fn read_some(r: &mut impl Read, buf: &mut [u8]) -> Result<usize, String> {
    loop {
        match r.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// The body of a chunked one: each chunk is its length in hex, a line, the
/// bytes and a line; a chunk of 0 ends it.
fn dechunk(b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut p = 0;
    loop {
        let line_end = b[p..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("a chunked body cut short")?;
        let size_text = std::str::from_utf8(&b[p..p + line_end]).map_err(|_| "a bad chunk size")?;
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| "a bad chunk size")?;
        p += line_end + 2;
        if size == 0 {
            return Ok(out);
        }
        let end = p
            .checked_add(size)
            .filter(|&e| e <= b.len())
            .ok_or("a chunked body cut short")?;
        out.extend_from_slice(&b[p..end]);
        p = end + 2; // the line after the chunk
        if p > b.len() {
            return Err("a chunked body cut short".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(raw: &[u8], limit: usize) -> Result<Response, String> {
        read_response(&mut &raw[..], limit, &mut |_, _| {})
    }

    #[test]
    fn bodies_by_length_in_chunks_or_to_the_end() {
        let r = read(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nX: y\r\n\r\nhelloEXTRA", 100).unwrap();
        assert_eq!(
            (r.status, r.body.as_slice(), r.header("x")),
            (200, &b"hello"[..], Some("y"))
        );
        let r = read(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n6;x=y\r\npedia \r\n0\r\n\r\n",
            100,
        )
        .unwrap();
        assert_eq!(r.body, b"Wikipedia ");
        let r = read(b"HTTP/1.0 200 OK\r\n\r\nto the end", 100).unwrap();
        assert_eq!(r.body, b"to the end");
        let r = read(
            b"HTTP/1.1 302 Found\r\nLocation: https://github.com/o/r/releases/tag/v1.2.3\r\nContent-Length: 0\r\n\r\n",
            100,
        )
        .unwrap();
        assert_eq!(r.status, 302);
        assert_eq!(r.header("LOCATION"), Some("https://github.com/o/r/releases/tag/v1.2.3"));
    }

    #[test]
    fn short_long_and_broken_responses() {
        let err = |raw: &[u8], limit| read(raw, limit).err().unwrap();
        assert!(err(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort", 100).contains("before the end"));
        assert!(err(b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n", 100).contains("more than"));
        assert!(err(b"", 100).contains("before a response"));
        assert!(err(b"garbage\r\n\r\n", 100).contains("without a status"));
        assert!(err(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWi", 100).contains("cut short"));
        assert!(err(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n", 100).contains("chunk size"));
    }

    #[test]
    fn urls_and_redirects() {
        assert_eq!(split("https://github.com/a/b"), Ok(("github.com", 443, "/a/b")));
        assert_eq!(split("https://h:8443"), Ok(("h", 8443, "/")));
        assert!(split("http://h/x").is_err() && split("https:///x").is_err() && split("https://h:x/").is_err());
        assert_eq!(absolute("https://a/x", "https://b/y").unwrap(), "https://b/y");
        assert_eq!(absolute("https://a:8443/x", "/y?z").unwrap(), "https://a:8443/y?z");
        assert!(absolute("https://a/x", "http://b/y").is_err(), "never down to http");
        assert!(absolute("https://a/x", "//b/y").is_err());
    }
}
