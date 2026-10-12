"""Writes small Avro fixtures for the payload decoder tests.
Regenerate: docker run --rm -v "$PWD":/w -w /w python:3.12-slim sh -c "pip -q install fastavro==1.9.7 cramjam==2.8.3 zstandard==0.23.0 && python make_avro.py"
"""
import datetime as dt
import decimal
import io
import struct

import fastavro

schema = fastavro.parse_schema({
    "type": "record",
    "name": "TurbineReading",
    "namespace": "energy.wind",
    "doc": "One reading from a wind turbine",
    "fields": [
        {"name": "Timestamp", "type": {"type": "long", "logicalType": "timestamp-millis"}},
        {"name": "TurbineId", "type": "string", "doc": "Turbine tag"},
        {"name": "ActivePower", "type": "double"},
        {"name": "WindSpeed", "type": ["null", "float"], "default": None},
        {"name": "Rpm", "type": "int"},
        {"name": "Running", "type": "boolean"},
        {"name": "Status", "type": {"type": "enum", "name": "Status", "symbols": ["RUNNING", "IDLE", "FAULT"]}},
        {"name": "Price", "type": {"type": "bytes", "logicalType": "decimal", "precision": 9, "scale": 2}},
        {"name": "Day", "type": {"type": "int", "logicalType": "date"}},
        {"name": "Tags", "type": {"type": "array", "items": "string"}},
        {"name": "Readings", "type": {"type": "map", "values": "double"}},
        {"name": "Location", "type": {"type": "record", "name": "Location",
                                      "fields": [{"name": "lat", "type": "double"}, {"name": "lon", "type": "double"}]}},
        {"name": "Previous", "type": ["null", "Location"], "default": None},
    ],
})

n = 300
base = dt.datetime(2026, 10, 8, 10, 20, tzinfo=dt.timezone.utc)
records = [{
    "Timestamp": base + dt.timedelta(seconds=i),
    "TurbineId": f"T{(i % 8) + 1:02d}",
    "ActivePower": (i * 37.5) % 3600 - 12.5,
    "WindSpeed": None if i % 50 == 7 else 2.0 + (i % 130) / 10,
    "Rpm": 9 + i % 7,
    "Running": i % 9 != 0,
    "Status": "RUNNING" if i % 9 else "IDLE",
    "Price": decimal.Decimal(i) / 100,
    "Day": dt.date(2026, 10, 8),
    "Tags": ["north", "row-a"] if i % 2 == 0 else [],
    "Readings": {"temp": 21.5, "vib": 0.25} if i % 3 == 0 else {},
    "Location": {"lat": 59.91, "lon": 10.75},
    "Previous": None if i % 2 else {"lat": 1.5, "lon": -2.0},
} for i in range(n)]
meta = {"ProviderName": "TurbineFastlog", "Version": "1.0.0"}

for codec in ["null", "deflate", "snappy", "zstandard"]:
    with open(f"turbine-{codec}.avro", "wb") as f:
        fastavro.writer(f, schema, records, codec=codec, sync_interval=2000, metadata=meta)

# One record as a Kafka/Confluent producer frames it: magic 0, schema id 42, no schema.
buf = io.BytesIO()
fastavro.schemaless_writer(buf, schema, records[0])
with open("turbine-confluent.avro", "wb") as f:
    f.write(b"\x00" + struct.pack(">I", 42) + buf.getvalue())
