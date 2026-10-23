//! Broker addresses and the MQTT client settings built from a connection.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::hash_map::RandomState;
#[cfg(not(target_arch = "wasm32"))]
use std::hash::{BuildHasher, Hasher};

use crate::config::{self, ConnCfg};
use crate::mqtt;

pub struct Broker {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    /// Over WebSockets (ws://, wss://): the path to ask for, with any query.
    pub ws: Option<String>,
    /// The URL without credentials, as a connection keeps it.
    pub url: String,
    /// The URL as shown: without a query either (a signed URL's holds a signature).
    pub display: String,
}

/// Accepts a URL or host[:port]. A bare host uses TLS when TLS options were
/// given or the port is 8883. A WebSocket URL without a path asks for /mqtt.
pub fn parse_broker(s: &str, tls_wanted: bool) -> Result<Broker, String> {
    let (scheme, rest) = match s.split_once("://") {
        Some((sc, r)) => (sc.to_lowercase(), r),
        None => (
            if tls_wanted || s.ends_with(":8883") {
                "mqtts"
            } else {
                "mqtt"
            }
            .to_string(),
            s,
        ),
    };
    let (tls, websocket) = match scheme.as_str() {
        "mqtt" | "tcp" => (false, false),
        "mqtts" | "ssl" | "tls" | "tcps" | "mqtt+ssl" => (true, false),
        "ws" => (false, true),
        "wss" => (true, true),
        other => return Err(format!("broker: unsupported scheme {other:?}")),
    };
    // Credentials (never used to log in) off first. Over TCP a URL has no
    // query, so they run to the last @ before a path: a ? or # in an
    // unencoded password is still the password's. A WebSocket URL's query
    // is sent and may hold an @, so there they end where RFC 3986 ends them.
    let stop: &[char] = if websocket { &['/', '?', '#'] } else { &['/'] };
    let auth = &rest[..rest.find(stop).unwrap_or(rest.len())];
    let rest = auth.rfind('@').map_or(rest, |i| &rest[i + 1..]);
    let rest = rest.split('#').next().unwrap_or("");
    let (hostport, path) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
    let ws = websocket.then(|| match path {
        "" => "/mqtt".to_string(),
        p if p.starts_with('?') => format!("/mqtt{p}"),
        p => p.to_string(),
    });
    let (host, port) = if let Some(r) = hostport.strip_prefix('[') {
        // [ipv6]:port
        let (h, p) = r.split_once(']').ok_or("broker: bad IPv6 address")?;
        (h.to_string(), p.strip_prefix(':').map(str::to_string))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.to_string())),
            None => (hostport.to_string(), None),
        }
    };
    if host.is_empty() {
        return Err(format!("broker: missing host in {s:?}"));
    }
    let port = match port {
        Some(p) => p.parse().map_err(|_| format!("broker: bad port {p:?}"))?,
        None if websocket && tls => 443,
        None if websocket => 80,
        None if tls => 8883,
        None => 1883,
    };
    let shown_host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.clone()
    };
    let url = match &ws {
        Some(p) => format!("{}://{shown_host}:{port}{p}", if tls { "wss" } else { "ws" }),
        None => format!("{}://{shown_host}:{port}", if tls { "mqtts" } else { "mqtt" }),
    };
    Ok(Broker {
        display: url.split('?').next().unwrap_or("").to_string(),
        url,
        host,
        port,
        tls,
        ws,
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub fn random_client_id() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    format!("fss-mqtt-{:08x}", h.finish() as u32)
}

/// A page has no process id, nor random hash keys.
#[cfg(target_arch = "wasm32")]
pub fn random_client_id() -> String {
    format!("fss-mqtt-{:08x}", (js_sys::Math::random() * f64::from(u32::MAX)) as u32)
}

/// The MQTT client's settings for connection `idx` (its index in the store).
pub fn mqtt_config(
    idx: usize,
    c: &ConnCfg,
    password: Option<String>,
    key_password: Option<String>,
) -> Result<mqtt::Config, String> {
    let b = parse_broker(&c.url, c.cafile.is_some() || c.cert.is_some())?;
    let ws = match b.ws {
        Some(path) => Some(crate::ws::Upgrade {
            path,
            headers: c
                .headers
                .iter()
                .map(|h| crate::ws::header(h))
                .collect::<Result<_, _>>()?,
            headers_env: c
                .headers_env
                .iter()
                .map(|h| crate::ws::header(h))
                .collect::<Result<_, _>>()?,
        }),
        None if c.headers.is_empty() && c.headers_env.is_empty() => None,
        None => return Err("headers are sent with a WebSocket handshake: the url needs ws:// or wss://".into()),
    };
    Ok(mqtt::Config {
        conn: idx,
        host: b.host,
        port: b.port,
        tls: b.tls || c.cafile.is_some() || c.cert.is_some(),
        ca_file: c.cafile.as_deref().map(config::expand),
        cert_file: c.cert.as_deref().map(config::expand),
        key_file: c.key.as_deref().map(config::expand),
        key_password,
        insecure: c.insecure,
        client_id: c.client_id.clone().unwrap_or_else(random_client_id),
        username: c.username.clone(),
        password,
        qos: c.qos,
        ws,
        proxy: c
            .proxy
            .as_deref()
            .map(|u| crate::proxy::Proxy::parse(u, c.proxy_password_env.clone()))
            .transpose()?,
    })
}

/// The broker URL as shown in the UI (no credentials).
pub fn display_url(c: &ConnCfg) -> Result<String, String> {
    Ok(parse_broker(&c.url, c.cafile.is_some() || c.cert.is_some())?.display)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_urls() {
        let b = parse_broker("127.0.0.1:8883", false).unwrap();
        assert!(b.tls && b.port == 8883 && b.host == "127.0.0.1");
        let b = parse_broker("broker", true).unwrap();
        assert!(b.tls && b.port == 8883);
        let b = parse_broker("mqtt://u:p@host", false).unwrap();
        assert!(!b.tls && b.port == 1883 && b.host == "host" && b.display == "mqtt://host:1883");
        let b = parse_broker("mqtts://[::1]:9000", false).unwrap();
        assert!(b.tls && b.port == 9000 && b.host == "::1");
        assert!(parse_broker("ftp://x", false).is_err());
    }

    #[test]
    fn websocket_urls() {
        let b = parse_broker("wss://broker.example.com", false).unwrap();
        assert!(b.tls && b.port == 443 && b.ws.as_deref() == Some("/mqtt"));
        assert_eq!(b.display, "wss://broker.example.com:443/mqtt");
        let b = parse_broker("ws://u:p@host:8080/ws", false).unwrap();
        assert!(!b.tls && b.port == 8080 && b.host == "host" && b.ws.as_deref() == Some("/ws"));
        let b = parse_broker("ws://host", false).unwrap();
        assert!(b.port == 80 && b.ws.as_deref() == Some("/mqtt"));
        // A query (a signed URL's) is sent, and not shown.
        let b = parse_broker("wss://iot.example.com/mqtt?X-Amz-Signature=abc#frag", false).unwrap();
        assert_eq!(b.ws.as_deref(), Some("/mqtt?X-Amz-Signature=abc"));
        assert_eq!(b.url, "wss://iot.example.com:443/mqtt?X-Amz-Signature=abc");
        assert_eq!(b.display, "wss://iot.example.com:443/mqtt");
        let b = parse_broker("wss://[::1]:8884?token=t", false).unwrap();
        assert!(b.host == "::1" && b.port == 8884 && b.ws.as_deref() == Some("/mqtt?token=t"));
        assert_eq!(b.display, "wss://[::1]:8884/mqtt");
        // Not for MQTT over TCP: a path there means nothing.
        assert!(parse_broker("mqtt://host/x", false).unwrap().ws.is_none());
        // Over TCP an unencoded ? or # in a password is still the password's;
        // in a WebSocket URL an @ in the query is the query's.
        let b = parse_broker("mqtt://me:pa?ss@host", false).unwrap();
        assert!(b.host == "host" && b.port == 1883, "{}", b.url);
        let b = parse_broker("mqtts://u:p#w@host:8884", false).unwrap();
        assert!(b.host == "host" && b.port == 8884, "{}", b.url);
        let b = parse_broker("wss://host?token=a@b", false).unwrap();
        assert!(
            b.host == "host" && b.ws.as_deref() == Some("/mqtt?token=a@b"),
            "{}",
            b.url
        );
        let b = parse_broker("ws://u:p@host:8080/ws?k=v@w", false).unwrap();
        assert!(b.host == "host" && b.port == 8080 && b.ws.as_deref() == Some("/ws?k=v@w"));
    }
}
