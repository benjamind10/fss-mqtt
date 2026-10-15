"""Writes Sparkplug B fixtures for the payload decoder tests, built with the official
sparkplug_b.proto (Eclipse Tahu, fetched here: it's EPL-2.0, so it isn't kept in this repository).
Regenerate: docker run --rm -v "$PWD":/w -w /w python:3.12-slim sh -c "pip -q install protobuf==5.28.3 grpcio-tools==1.66.2 && python make_sparkplug.py"

Payloads don't carry their topic. Group Plant1, edge node Gateway1, device Meter3:
  nbirth.bin       spBv1.0/Plant1/NBIRTH/Gateway1         every type, aliases, a data set, templates
  ndata.bin        spBv1.0/Plant1/NDATA/Gateway1          aliases only, one unknown (99)
  dbirth.bin       spBv1.0/Plant1/DBIRTH/Gateway1/Meter3
  ddata.bin        spBv1.0/Plant1/DDATA/Gateway1/Meter3   aliases only
  ndeath.bin       spBv1.0/Plant1/NDEATH/Gateway1         bdSeq
  ncmd.bin         spBv1.0/Plant1/NCMD/Gateway1           rebirth request
  ndata-gzip.bin   ndata.bin, compressed the way Eclipse Tahu does (uuid SPBV1.0_COMPRESSED)

Values are encoded as Sparkplug 3.0 says (and Eclipse Tahu's Java client does): signed integers
as two's complement in int_value/long_value, arrays packed little-endian in bytes_value.
"""
import datetime as dt
import gzip
import os
import struct
import subprocess
import sys
import urllib.request

PROTO = "https://raw.githubusercontent.com/eclipse-tahu/tahu/master/sparkplug_b/sparkplug_b.proto"
os.makedirs("/tmp/spb", exist_ok=True)
urllib.request.urlretrieve(PROTO, "/tmp/spb/sparkplug_b.proto")
subprocess.run(
    [sys.executable, "-m", "grpc_tools.protoc", "-I/tmp/spb", "--python_out=/tmp/spb", "sparkplug_b.proto"],
    check=True,
)
sys.path.insert(0, "/tmp/spb")
import sparkplug_b_pb2 as spb  # noqa: E402

P = spb.Payload
T0 = int(dt.datetime(2026, 10, 15, 10, 20, 30, 123000, tzinfo=dt.timezone.utc).timestamp() * 1000)

# DataType numbers (the enum in sparkplug_b.proto)
(INT8, INT16, INT32, INT64, UINT8, UINT16, UINT32, UINT64, FLOAT, DOUBLE, BOOLEAN, STRING,
 DATETIME, TEXT, UUID, DATASET, BYTES, FILE, TEMPLATE) = range(1, 20)
(INT8A, INT16A, INT32A, INT64A, UINT8A, UINT16A, UINT32A, UINT64A, FLOATA, DOUBLEA, BOOLA, STRINGA,
 DATETIMEA) = range(22, 35)


def bool_array(values):
    """A little-endian uint32 count, then the values packed into bytes, first value in the top bit."""
    bits = "".join("1" if v else "0" for v in values)
    bits += "0" * (-len(bits) % 8)
    return struct.pack("<I", len(values)) + bytes(int(bits[i:i + 8], 2) for i in range(0, len(bits), 8))


ARRAYS = {INT8A: "b", INT16A: "h", INT32A: "i", INT64A: "q", UINT8A: "B", UINT16A: "H", UINT32A: "I",
          UINT64A: "Q", FLOATA: "f", DOUBLEA: "d", DATETIMEA: "Q"}


def set_value(m, datatype, value):
    """Puts `value` in the field Sparkplug uses for `datatype` (a Metric, a Template.Parameter or a
    PropertyValue: the scalar fields have the same names)."""
    if datatype in (INT8, INT16, INT32):
        m.int_value = value & 0xFFFFFFFF
    elif datatype in (UINT8, UINT16, UINT32):
        m.int_value = value
    elif datatype in (INT64, UINT64, DATETIME):
        m.long_value = value & 0xFFFFFFFFFFFFFFFF
    elif datatype == FLOAT:
        m.float_value = value
    elif datatype == DOUBLE:
        m.double_value = value
    elif datatype == BOOLEAN:
        m.boolean_value = value
    elif datatype in (STRING, TEXT, UUID):
        m.string_value = value
    elif datatype in (BYTES, FILE):
        m.bytes_value = value
    elif datatype in ARRAYS:
        m.bytes_value = struct.pack(f"<{len(value)}{ARRAYS[datatype]}", *value)
    elif datatype == BOOLA:
        m.bytes_value = bool_array(value)
    elif datatype == STRINGA:
        m.bytes_value = b"".join(s.encode() + b"\0" for s in value)
    else:
        raise ValueError(datatype)


def metric(p, name=None, alias=None, datatype=None, value=None, **kw):
    m = p.metrics.add()
    if name is not None:
        m.name = name
    if alias is not None:
        m.alias = alias
    if datatype is not None:
        m.datatype = datatype
    if value is not None:
        set_value(m, datatype, value)
    for k, v in kw.items():
        setattr(m, k, v)
    return m


def write(name, p):
    with open(name, "wb") as f:
        f.write(p.SerializeToString(deterministic=True))


# NBIRTH: every scalar and array type, with aliases; a data set; a template definition and instance.
nbirth = P(timestamp=T0, seq=0)
metric(nbirth, "bdSeq", None, INT64, 3)
metric(nbirth, "Node Control/Rebirth", 1, BOOLEAN, False)
metric(nbirth, "Inputs/Int8", 10, INT8, -23)
metric(nbirth, "Inputs/Int16", 11, INT16, -30000)
metric(nbirth, "Inputs/Int32", 12, INT32, -1)
metric(nbirth, "Inputs/Int64", 13, INT64, -4270929666821191986)
metric(nbirth, "Inputs/UInt8", 14, UINT8, 250)
metric(nbirth, "Inputs/UInt16", 15, UINT16, 52360)
metric(nbirth, "Inputs/UInt32", 16, UINT32, 3293969225)
metric(nbirth, "Inputs/UInt64", 17, UINT64, 16444743074749521625)
power = metric(nbirth, "Inputs/Float", 18, FLOAT, 1.23)
power.properties.keys.extend(["engUnit", "Quality"])
power.properties.values.add(type=STRING, string_value="kW")
power.properties.values.add(type=INT32, int_value=192)
metric(nbirth, "Inputs/Double", 19, DOUBLE, 12.354213)
metric(nbirth, "Inputs/Boolean", 20, BOOLEAN, True)
metric(nbirth, "Inputs/String", 21, STRING, "hello")
metric(nbirth, "Inputs/DateTime", 22, DATETIME, 1256102875335)
metric(nbirth, "Inputs/Text", 23, TEXT, "longer text", is_transient=True)
metric(nbirth, "Inputs/UUID", 24, UUID, "1b4e28ba-2fa1-11d2-883f-0016d3cca427")
metric(nbirth, "Inputs/Bytes", 25, BYTES, bytes([0x00, 0x01, 0xFE, 0xFF]))
setpoints = b"step,temp\n1,20.5\n2,21.0\n"
f = metric(nbirth, "Inputs/File", 26, FILE, setpoints)
f.metadata.file_name = "setpoints.csv"
f.metadata.content_type = "text/csv"
f.metadata.size = len(setpoints)
metric(nbirth, "Inputs/Null", 27, INT32, None, is_null=True)
metric(nbirth, "Arrays/Int8", 30, INT8A, [-23, 123])
metric(nbirth, "Arrays/Int16", 31, INT16A, [-30000, 30000])
metric(nbirth, "Arrays/Int32", 32, INT32A, [-1, 315338746])
metric(nbirth, "Arrays/Int64", 33, INT64A, [-4270929666821191986, -3601064768563266876])
metric(nbirth, "Arrays/UInt8", 34, UINT8A, [23, 250])
metric(nbirth, "Arrays/UInt16", 35, UINT16A, [30, 52360])
metric(nbirth, "Arrays/UInt32", 36, UINT32A, [52, 3293969225])
metric(nbirth, "Arrays/UInt64", 37, UINT64A, [52, 16444743074749521625])
metric(nbirth, "Arrays/Float", 38, FLOATA, [1.23, 89.341])
metric(nbirth, "Arrays/Double", 39, DOUBLEA, [12.354213, 1022.9123213])
metric(nbirth, "Arrays/Boolean", 40, BOOLA,
       [False, False, True, True, False, True, False, False, True, True, False, True])
metric(nbirth, "Arrays/String", 41, STRINGA, ["ABC", "hello"])
metric(nbirth, "Arrays/DateTime", 42, DATETIMEA, [1256102875335, 1656107875000])

recipe = metric(nbirth, "Recipes/Current", 50, DATASET)
ds = recipe.dataset_value
ds.num_of_columns = 4
ds.columns.extend(["step", "temp", "ok", "label"])
ds.types.extend([UINT32, FLOAT, BOOLEAN, STRING])
for step, temp, ok, label in [(1, 20.5, True, "heat"), (2, 21.25, True, "hold"), (3, -4.5, False, "cool")]:
    row = ds.rows.add()
    row.elements.add(int_value=step)
    row.elements.add(float_value=temp)
    row.elements.add(boolean_value=ok)
    row.elements.add(string_value=label)

definition = metric(nbirth, "_types_/Motor", None, TEMPLATE)
t = definition.template_value
t.version = "1.0"
t.is_definition = True
for name, datatype in [("RPM", FLOAT), ("Amps", INT32)]:
    t.metrics.add(name=name, datatype=datatype)
t.parameters.add(name="Index", type=INT32, int_value=0)

motor = metric(nbirth, "Motors/Motor1", 51, TEMPLATE)
t = motor.template_value
t.version = "1.0"
t.template_ref = "Motor"
t.is_definition = False
set_value(t.metrics.add(name="RPM", datatype=FLOAT), FLOAT, 1450.5)
set_value(t.metrics.add(name="Amps", datatype=INT32), INT32, -12)
p = t.parameters.add(name="Index", type=INT32)
set_value(p, INT32, 2)
write("nbirth.bin", nbirth)

# NDATA: aliases only, no names or types, as the spec asks; one alias the birth didn't define.
ndata = P(timestamp=T0 + 1000, seq=1)
metric(ndata, None, 10).int_value = -24 & 0xFFFFFFFF
metric(ndata, None, 18).float_value = 2.5
metric(ndata, None, 21).string_value = "world"
metric(ndata, None, 13, None, None, is_historical=True, timestamp=T0 - 60_000).long_value = -5 & 0xFFFFFFFFFFFFFFFF
metric(ndata, None, 30).bytes_value = struct.pack("<2b", 1, -1)
metric(ndata, None, 99).int_value = 7
write("ndata.bin", ndata)

# A device and its data.
dbirth = P(timestamp=T0 + 2000, seq=2)
metric(dbirth, "Power", 100, DOUBLE, 1234.5)
metric(dbirth, "Energy", 101, UINT64, 987654321)
metric(dbirth, "Status", 102, STRING, "RUNNING")
write("dbirth.bin", dbirth)

ddata = P(timestamp=T0 + 3000, seq=3)
metric(ddata, None, 100).double_value = 1250.25
metric(ddata, None, 102).string_value = "IDLE"
write("ddata.bin", ddata)

# Death and a command: by name, with a type, no aliases.
ndeath = P(timestamp=T0 + 4000)
metric(ndeath, "bdSeq", None, INT64, 3)
write("ndeath.bin", ndeath)

ncmd = P(timestamp=T0 + 5000)
metric(ncmd, "Node Control/Rebirth", None, BOOLEAN, True)
write("ncmd.bin", ncmd)

# Compressed: the whole NDATA payload gzipped into the body of a wrapper payload.
packed = P(timestamp=T0 + 1000, seq=1, uuid="SPBV1.0_COMPRESSED",
           body=gzip.compress(ndata.SerializeToString(deterministic=True), mtime=0))
metric(packed, "algorithm", None, STRING, "GZIP")
write("ndata-gzip.bin", packed)
