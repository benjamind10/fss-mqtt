//! Broker addresses and the MQTT client settings built from a connection.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

use crate::config::{self, ConnCfg};
use crate::mqtt;

pub struct Broker {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub display: String, // URL without credentials
}

/// Accepts a URL or host[:port]. A bare host uses TLS when TLS options were
/// given or the port is 8883.
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
    let tls = match scheme.as_str() {
        "mqtt" | "tcp" => false,
        "mqtts" | "ssl" | "tls" | "tcps" | "mqtt+ssl" => true,
        "ws" | "wss" => return Err("WebSocket brokers (ws://, wss://) are not supported".into()),
        other => return Err(format!("broker: unsupported scheme {other:?}")),
    };
    let hostport = rest.split('/').next().unwrap_or("");
    let hostport = hostport.rsplit_once('@').map(|(_, h)| h).unwrap_or(hostport);
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
        None if tls => 8883,
        None => 1883,
    };
    let shown_host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.clone()
    };
    Ok(Broker {
        display: format!("{}://{shown_host}:{port}", if tls { "mqtts" } else { "mqtt" }),
        host,
        port,
        tls,
    })
}

pub fn random_client_id() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    format!("fss-mqtt-{:08x}", h.finish() as u32)
}

/// The MQTT client's settings for connection `idx` (its index in the store).
pub fn mqtt_config(
    idx: usize,
    c: &ConnCfg,
    password: Option<String>,
    key_password: Option<String>,
) -> Result<mqtt::Config, String> {
    let b = parse_broker(&c.url, c.cafile.is_some() || c.cert.is_some())?;
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
        assert!(parse_broker("wss://x", false).is_err());
    }
}
