"""Writes the sample payloads publish.sh sends for the README demo, one per decoded format.
Regenerate (from docs/demo/samples):
docker run --rm -v "$PWD":/w -w /w python:3.12-slim sh -c "pip -q install pyarrow==17.0.0 fastavro==1.9.7 msgpack==1.1.0 cbor2==5.6.5 apsw==3.53.4.0 && python make_samples.py"
"""
import datetime as dt
import gzip
import io
import json
import math
import os
import struct

import apsw
import cbor2
import fastavro
import msgpack
import pyarrow as pa
import pyarrow.ipc as ipc
import pyarrow.parquet as pq

base = dt.datetime(2026, 10, 8, 10, 20, tzinfo=dt.timezone.utc)
turbines = [f"T{i:02d}" for i in range(1, 9)]


# fss-mqtt keeps this much of each payload by default (--max-payload), so a
# sample this size or smaller is shown whole, without a truncation note.
MAX_PAYLOAD = 4 * 1024


def check(name):
    size = os.path.getsize(name)
    assert size <= MAX_PAYLOAD, (name, size)


def write(name, data):
    with open(name, "wb") as f:
        f.write(data)
    check(name)


# Parquet: a 1 Hz fast log from one turbine.
n = 60
fastlog = pa.table({
    "Timestamp": pa.array([base + dt.timedelta(seconds=i) for i in range(n)], pa.timestamp("ms", tz="UTC")),
    "TurbineId": pa.array(["T01"] * n),
    "ActivePower": pa.array([round(2400 + 900 * math.sin(i / 17) + (i * 7919 % 50), 1) for i in range(n)]),
    "WindSpeed": pa.array([round(10 + 2.5 * math.sin(i / 17) + (i * 104729 % 10) / 20, 2) for i in range(n)], pa.float32()),
    "RotorRpm": pa.array([round(13 + 2 * math.sin(i / 17), 2) for i in range(n)], pa.float32()),
    "PitchDeg": pa.array([round(max(0.0, 3 * math.sin(i / 23)), 2) for i in range(n)], pa.float32()),
    "Status": pa.array(["RUNNING"] * n),
}).replace_schema_metadata({b"ProviderName": b"TurbineFastlog", b"Version": b"1.0.0"})
pq.write_table(fastlog, "fastlog.parquet", compression="snappy", row_group_size=n, store_schema=False)
check("fastlog.parquet")

# Avro container file: the turbine event log.
schema = fastavro.parse_schema({
    "type": "record", "name": "TurbineEvent", "namespace": "energy.wind",
    "fields": [
        {"name": "Timestamp", "type": {"type": "long", "logicalType": "timestamp-millis"}},
        {"name": "TurbineId", "type": "string"},
        {"name": "Code", "type": "string"},
        {"name": "Severity", "type": {"type": "enum", "name": "Severity", "symbols": ["INFO", "WARNING", "FAULT"]}},
        {"name": "Value", "type": ["null", "double"], "default": None},
    ],
})
codes = [("PITCH_DEVIATION", "WARNING", 7.2), ("YAW_MISALIGNMENT", "WARNING", 11.5), ("GRID_LOSS", "FAULT", None),
         ("RESTART", "INFO", None), ("GEARBOX_TEMP_HIGH", "WARNING", 82.4), ("SERVICE_MODE", "INFO", None)]
events = [{"Timestamp": base + dt.timedelta(minutes=7 * i), "TurbineId": turbines[i * 3 % 8], "Code": c,
           "Severity": s, "Value": v} for i, (c, s, v) in enumerate(codes * 4)]
buf = io.BytesIO()
fastavro.writer(buf, schema, events, codec="deflate")
write("events.avro", buf.getvalue())

# Arrow IPC file: vibration samples from the drive train.
m = 150
samples = pa.table({
    "Timestamp": pa.array([base + dt.timedelta(milliseconds=20 * i) for i in range(m)], pa.timestamp("ms", tz="UTC")),
    "Sensor": pa.array([["main_bearing", "gearbox", "generator"][i % 3] for i in range(m)]).dictionary_encode(),
    "AccelG": pa.array([round(0.4 * math.sin(i / 3) + 0.05 * (i % 7), 3) for i in range(m)], pa.float32()),
    "FreqHz": pa.array([[12.5, 37.5, 115.0][i % 3] for i in range(m)], pa.float32()),
}).replace_schema_metadata({b"ProviderName": b"CMS"})
out = pa.BufferOutputStream()
with ipc.new_file(out, samples.schema, options=ipc.IpcWriteOptions(compression="zstd")) as w:
    for b in samples.to_batches(max_chunksize=75):
        w.write_batch(b)
write("samples.arrow", out.getvalue().to_pybytes())

# SQLite database: the site configuration.
setup = """
PRAGMA page_size = 512;
CREATE TABLE turbines (id TEXT PRIMARY KEY, model TEXT NOT NULL, rated_kw REAL, commissioned TEXT);
CREATE TABLE setpoints (turbine TEXT REFERENCES turbines(id), name TEXT, value REAL, PRIMARY KEY (turbine, name));
CREATE INDEX idx_setpoints_name ON setpoints(name);
CREATE VIEW derated AS SELECT turbine, value FROM setpoints WHERE name = 'max_power_kw' AND value < 3600;
"""
rows = "".join(f"INSERT INTO turbines VALUES ('{t}', 'V117-3.6', 3600, '2019-0{i % 6 + 3}-1{i}');\n"
               for i, t in enumerate(turbines))
rows += "".join(f"INSERT INTO setpoints VALUES ('{t}', 'max_power_kw', {3600 if i % 3 else 3000});\n"
                f"INSERT INTO setpoints VALUES ('{t}', 'cut_out_ms', 25);\n" for i, t in enumerate(turbines))
if os.path.exists("turbines.db"):
    os.remove("turbines.db")
con = apsw.Connection("turbines.db")
con.execute(setup + rows)
con.close()
check("turbines.db")

# SQLite changeset: a setpoint change pushed to the site.
con = apsw.Connection(":memory:")
con.execute(setup + rows)
s = apsw.Session(con, "main")
s.attach()
con.execute("""
UPDATE setpoints SET value = 3600 WHERE turbine = 'T01' AND name = 'max_power_kw';
UPDATE setpoints SET value = 2800 WHERE turbine = 'T05' AND name = 'max_power_kw';
UPDATE setpoints SET value = 22 WHERE name = 'cut_out_ms' AND turbine IN ('T02', 'T03');
INSERT INTO turbines VALUES ('T09', 'V136-4.2', 4200, '2026-10-01');
INSERT INTO setpoints VALUES ('T09', 'max_power_kw', 4200);
""")
write("setpoints.changeset", s.changeset())


# Protobuf: one telemetry reading, encoded by hand (no schema travels with it).
def varint(v):
    out = bytearray()
    while True:
        b = v & 0x7F
        v >>= 7
        out.append(b | (0x80 if v else 0))
        if not v:
            return bytes(out)


def key(field, wire):
    return varint(field << 3 | wire)


def ld(field, data):
    return key(field, 2) + varint(len(data)) + data


location = key(1, 1) + struct.pack("<d", 59.9139) + key(2, 1) + struct.pack("<d", 10.7522)
temps = varint(61) + varint(58) + varint(74) + varint(23)
reading = (ld(1, b"T04") + key(2, 0) + varint(int(base.timestamp() * 1000)) + key(3, 1) + struct.pack("<d", 2265.8)
           + key(4, 5) + struct.pack("<f", 11.5) + key(5, 0) + varint(14) + key(6, 0) + varint(1)
           + ld(7, location) + ld(8, temps) + ld(9, b"north") + ld(9, b"offshore"))
write("reading.pb", reading)

# MessagePack and CBOR: compact status documents.
write("state.msgpack", msgpack.packb({
    "turbine": "T05", "state": "RUNNING", "since": "2026-10-08T06:12:40Z", "power_kw": 3120.5,
    "yaw": {"position_deg": 247.5, "error_deg": 1.8}, "alarms": [], "derated": False,
}))
write("vibration.cbor", cbor2.dumps({
    "turbine": "T02", "ts": "2026-10-08T10:20:00Z", "rms_g": {"main_bearing": 0.21, "gearbox": 0.34, "generator": 0.12},
    "peaks_hz": [12.5, 37.5, 115.0], "ok": True,
}))

# CSV: yesterday's production per turbine.
csv = "turbine,energy_mwh,availability_pct,avg_wind_ms,max_power_kw,alarms\n" + "".join(
    f"{t},{58.4 + i * 3.7 % 9:.1f},{99.6 - i * 0.37 % 2.5:.1f},{9.1 + i * 0.23:.1f},{3600 if i % 3 else 3000},{i % 3}\n"
    for i, t in enumerate(turbines))
write("daily.csv", csv.encode())

# gzip: a JSON site summary, unpacked by the viewer before it's formatted.
summary = {
    "site": "wind-farm", "ts": "2026-10-08T10:20:00Z", "power_mw": 22.4, "wind_ms": 10.8,
    "turbines": [{"id": t, "state": "RUNNING" if i != 4 else "IDLE", "power_kw": round(2900 + i * 61.5, 1)}
                 for i, t in enumerate(turbines)],
}
write("summary.json.gz", gzip.compress(json.dumps(summary).encode(), mtime=0))
