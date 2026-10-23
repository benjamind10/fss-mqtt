//! Demo data for the web version: two wind farm sites, as the README demo's
//! publisher sends them (docs/demo/publish.sh), made in the page instead of
//! by a broker. Each site's connection has one retained sample of every
//! binary format the viewer decodes, then turbine telemetry every second,
//! grid values, and now and then an alarm, a status change, a fresh fast
//! log and the SCADA snapshot. Messages go through the same decoding on
//! arrival as a broker's.

use crate::capture::matches;
use crate::mqtt::Outgoing;
use crate::store::{ConnState, Props, Store, Sub};
use web_time::{SystemTime, UNIX_EPOCH};

const SITES: [&str; 2] = ["oslo", "bergen"];
const TURBINES: usize = 5;

const FASTLOG: &[u8] = include_bytes!("../docs/demo/samples/fastlog.parquet");
const SAMPLES: [(&str, &[u8], &str, &str, &str); 11] = [
    (
        "turbine/T01/fastlog",
        FASTLOG,
        "application/vnd.apache.parquet",
        "TurbineId",
        "T01",
    ),
    (
        "turbine/T01/daylog",
        include_bytes!("../docs/demo/samples/daylog.parquet"),
        "application/vnd.apache.parquet",
        "TurbineId",
        "T01",
    ),
    (
        "turbine/T02/events",
        include_bytes!("../docs/demo/samples/events.avro"),
        "application/avro",
        "TurbineId",
        "T02",
    ),
    (
        "turbine/T03/vibration",
        include_bytes!("../docs/demo/samples/samples.arrow"),
        "application/vnd.apache.arrow.file",
        "TurbineId",
        "T03",
    ),
    (
        "turbine/T04/reading",
        include_bytes!("../docs/demo/samples/reading.pb"),
        "application/x-protobuf",
        "TurbineId",
        "T04",
    ),
    (
        "turbine/T05/state",
        include_bytes!("../docs/demo/samples/state.msgpack"),
        "application/msgpack",
        "TurbineId",
        "T05",
    ),
    (
        "turbine/T02/cms",
        include_bytes!("../docs/demo/samples/vibration.cbor"),
        "application/cbor",
        "TurbineId",
        "T02",
    ),
    (
        "config/turbines.db",
        include_bytes!("../docs/demo/samples/turbines.db"),
        "application/vnd.sqlite3",
        "Version",
        "3",
    ),
    (
        "config/setpoints",
        include_bytes!("../docs/demo/samples/setpoints.changeset"),
        "application/vnd.sqlite3.changeset",
        "Version",
        "4",
    ),
    (
        "export/daily.csv",
        include_bytes!("../docs/demo/samples/daily.csv"),
        "text/csv",
        "Date",
        "2026-10-07",
    ),
    (
        "scada/summary",
        include_bytes!("../docs/demo/samples/summary.json.gz"),
        "application/gzip",
        "ProviderName",
        "SiteSummary",
    ),
];

/// The demo's two connections and where its traffic is.
pub struct Demo {
    conns: Vec<usize>,
    step: u64,
    seed: u64,
}

/// The two sites' connections, connected, with their retained messages.
pub fn start(st: &mut Store) -> Demo {
    let mut d = Demo {
        conns: Vec::new(),
        step: 0,
        seed: 0x9e37_79b9_7f4a_7c15,
    };
    for site in SITES {
        let sub = Sub {
            topic: "site/#".into(),
            enabled: true,
            err: None,
        };
        let c = st.add_conn(site, &format!("mqtt://demo-{site}:1883"), None, vec![sub]);
        st.conns[c].status.state = ConnState::Connected;
        d.conns.push(c);
    }
    for (i, site) in SITES.iter().enumerate() {
        let c = d.conns[i];
        add(st, c, &format!("site/{site}/online"), b"true", true, None, &[]);
        for t in 1..=TURBINES {
            let topic = format!("site/{site}/turbine/T{t:02}/status");
            add(st, c, &topic, b"RUNNING", true, None, &[]);
        }
        for (topic, bytes, ct, k, v) in SAMPLES {
            add(st, c, &format!("site/{site}/{topic}"), bytes, true, Some(ct), &[(k, v)]);
        }
        d.scada(st, c, site);
    }
    d
}

impl Demo {
    /// A number between `lo` and `hi`, as text with `places` decimals.
    fn num(&mut self, lo: f64, hi: f64, places: usize) -> String {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        let r = (self.seed >> 11) as f64 / (1u64 << 53) as f64;
        format!("{:.places$}", lo + r * (hi - lo))
    }

    /// A site's SCADA snapshot: 400 points, fresh values each time.
    fn scada(&mut self, st: &mut Store, conn: usize, site: &str) {
        let mut x = format!("<?xml version=\"1.0\"?><DataStatus><StationStatus site=\"{site}\">");
        for i in 1..=400 {
            let v = self.num(0.0, 100.0, 2);
            x += &format!(
                "<Point id=\"{i}\" name=\"WTG.{}.Signal{i}\" quality=\"Good\">{v}</Point>",
                i % TURBINES + 1
            );
        }
        x += "</StationStatus></DataStatus>";
        let topic = format!("site/{site}/scada/online/data");
        let props = [("ProviderName", "OnlineData"), ("Quality", "Good")];
        add(st, conn, &topic, x.as_bytes(), true, Some("application/xml"), &props);
    }

    /// One second's traffic, as the publisher sends it.
    pub fn second(&mut self, st: &mut Store) {
        self.step += 1;
        let i = self.step;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let ts = crate::values::timestamp(now as i64, 0, true).replace(' ', "T");
        for (s, site) in SITES.iter().enumerate() {
            let c = self.conns[s];
            for t in 1..=TURBINES {
                let id = format!("T{t:02}");
                let p = format!(
                    "{{\"ts\":\"{ts}\",\"power_kw\":{},\"wind_ms\":{},\"rpm\":{},\"pitch_deg\":{}}}",
                    self.num(900.0, 3600.0, 1),
                    self.num(4.0, 14.0, 1),
                    self.num(9.0, 16.0, 1),
                    self.num(0.0, 6.0, 1),
                );
                let props = [
                    ("TurbineId", id.as_str()),
                    ("Quality", "Good"),
                    ("Version", "1.0.0"),
                    ("PublishReason", "Scheduled"),
                ];
                let topic = format!("site/{site}/turbine/{id}/telemetry");
                add(st, c, &topic, p.as_bytes(), false, Some("application/json"), &props);
            }
            let f = self.num(49.95, 50.05, 3);
            add(
                st,
                c,
                &format!("site/{site}/grid/frequency_hz"),
                f.as_bytes(),
                false,
                None,
                &[],
            );
            let v = self.num(32.4, 33.6, 2);
            add(
                st,
                c,
                &format!("site/{site}/grid/voltage_kv"),
                v.as_bytes(),
                false,
                None,
                &[],
            );
            if i.is_multiple_of(9) {
                let id = format!("T{:02}", (i / 9) as usize % TURBINES + 1);
                let p = format!(
                    "{{\"code\":\"PITCH_DEVIATION\",\"severity\":\"warning\",\"value_deg\":{}}}",
                    self.num(6.0, 9.0, 1)
                );
                let topic = format!("site/{site}/turbine/{id}/alarm");
                let props = [("TurbineId", id.as_str())];
                add(st, c, &topic, p.as_bytes(), true, Some("application/json"), &props);
            }
            if i.is_multiple_of(11) {
                let id = format!("T{:02}", (i / 11) as usize % TURBINES + 1);
                let topic = format!("site/{site}/turbine/{id}/fastlog");
                let props = [("TurbineId", id.as_str())];
                add(
                    st,
                    c,
                    &topic,
                    FASTLOG,
                    true,
                    Some("application/vnd.apache.parquet"),
                    &props,
                );
            }
            if i.is_multiple_of(13) {
                let id = format!("T{:02}", (i / 13) as usize % TURBINES + 1);
                let state: &[u8] = if i.is_multiple_of(2) { b"IDLE" } else { b"RUNNING" };
                add(
                    st,
                    c,
                    &format!("site/{site}/turbine/{id}/status"),
                    state,
                    true,
                    None,
                    &[],
                );
            }
            if i.is_multiple_of(15) {
                self.scada(st, c, site);
            }
        }
    }
}

/// What the demo's broker does with a message published to it: it reports
/// the publish as a broker's QoS 0 is, and sends it back when the
/// connection subscribes to its topic.
pub fn publish(st: &mut Store, conn: usize, out: Outgoing) {
    let c = &st.conns[conn];
    let what = format!("{} · {}", c.name, out.topic);
    let back = c.subs.iter().any(|s| s.enabled && matches(&s.topic, &out.topic));
    let ct = out.props.content_type.clone();
    let decoded = crate::pretty::decode_on_arrival(ct.as_deref(), &out.payload);
    if back {
        st.add_decoded(conn, &out.topic, &out.payload, out.qos, out.retain, out.props, decoded);
    }
    st.notify(format!("published to {what} (QoS {})", out.qos), false);
}

/// A message as it arrives from a broker: decoded now, kept as the store keeps it.
fn add(
    st: &mut Store,
    conn: usize,
    topic: &str,
    payload: &[u8],
    retain: bool,
    ct: Option<&str>,
    user: &[(&str, &str)],
) {
    let props = Props {
        content_type: ct.map(String::from),
        user: user.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..Default::default()
    };
    let decoded = crate::pretty::decode_on_arrival(ct, payload);
    st.add_decoded(conn, topic, payload, 0, retain, props, decoded);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_sites_with_every_format_then_traffic() {
        let mut st = Store::new(10, 4 * 1024);
        let mut d = start(&mut st);
        assert_eq!(st.conns.len(), 2);
        let first = st.total();
        // online + 5 statuses + 11 samples + the SCADA snapshot, per site
        assert_eq!(first, 2 * (1 + TURBINES + SAMPLES.len() + 1) as u64);
        // Every binary sample decodes as what it is.
        let formats: std::collections::HashSet<&str> = st
            .nodes
            .iter()
            .filter_map(|n| n.latest())
            .filter_map(|m| m.decoded.as_deref().map(|v| v.format))
            .collect();
        for f in [
            "Parquet",
            "Avro",
            "Arrow",
            "Protobuf",
            "MessagePack",
            "CBOR",
            "SQLite",
            "SQLite changeset",
        ] {
            assert!(formats.contains(f), "{f} missing: {formats:?}");
        }
        for _ in 0..13 {
            d.second(&mut st);
        }
        // Each site's SCADA snapshot names the site.
        let n = st.find(1, "site/bergen/scada/online/data").unwrap();
        let xml = String::from_utf8_lossy(&st.nodes[n].latest().unwrap().payload).into_owned();
        assert!(xml.contains("site=\"bergen\""), "{}", &xml[..80]);
        // Telemetry and grid every second; alarm, fast log and status now and then.
        let per_second = 2 * (TURBINES + 2) as u64;
        assert_eq!(st.total(), first + 13 * per_second + 2 * 3);
    }

    #[test]
    fn a_publish_comes_back_when_subscribed() {
        let mut st = Store::new(10, 4 * 1024);
        start(&mut st);
        let out = |topic: &str| Outgoing {
            topic: topic.into(),
            payload: b"42".to_vec(),
            qos: 1,
            retain: false,
            props: Props::default(),
        };
        let n = st.total();
        publish(&mut st, 0, out("site/oslo/test"));
        assert_eq!(st.total(), n + 1);
        publish(&mut st, 0, out("elsewhere"));
        assert_eq!(st.total(), n + 1);
        let notices = st.take_notices();
        assert_eq!(notices[1], ("published to oslo · elsewhere (QoS 1)".into(), false));
    }
}
