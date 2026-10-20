"""Writes small Arrow IPC fixtures (file and stream format) for the payload decoder tests.
Regenerate: docker run --rm -v "$PWD":/w -w /w python:3.12-slim sh -c "pip -q install pyarrow==17.0.0 polars==1.9.0 && python make_arrow.py"
"""
import datetime as dt
import decimal
import numpy as np
import pyarrow as pa
import pyarrow.ipc as ipc

n = 500
base = dt.datetime(2026, 10, 8, 10, 20, tzinfo=dt.timezone.utc)
table = pa.table({
    "Timestamp": pa.array([base + dt.timedelta(seconds=i) for i in range(n)], pa.timestamp("ms", tz="UTC")),
    "TurbineId": pa.array([f"T{(i % 8) + 1:02d}" for i in range(n)]),
    "ActivePower": pa.array([(i * 37.5) % 3600 - 12.5 for i in range(n)], pa.float64()),
    "WindSpeed": pa.array([None if i % 50 == 7 else 2.0 + (i % 130) / 10 for i in range(n)], pa.float32()),
    "Rpm": pa.array([9 + i % 7 for i in range(n)], pa.int32()),
    "Counter": pa.array([10_000_000_000 + i for i in range(n)], pa.int64()),
    "Running": pa.array([i % 9 != 0 for i in range(n)]),
    "Status": pa.array(["RUNNING" if i % 9 else "IDLE" for i in range(n)]).dictionary_encode(),
    "Day": pa.array([dt.date(2026, 10, 8) for _ in range(n)], pa.date32()),
    "Price": pa.array([decimal.Decimal(i) / 100 for i in range(n)], pa.decimal128(9, 2)),
    "Tags": pa.array([["ok"] if i % 9 else ["idle", "check"] for i in range(n)], pa.list_(pa.string())),
})
meta = {b"ProviderName": b"TurbineFastlog", b"Version": b"1.0.0"}
table = table.replace_schema_metadata(meta)
batches = table.to_batches(max_chunksize=200)

for codec in [None, "lz4", "zstd"]:
    opts = ipc.IpcWriteOptions(compression=codec)
    name = codec or "none"
    with ipc.new_file(f"turbine-{name}.arrow", table.schema, options=opts) as w:
        for b in batches:
            w.write_batch(b)
    with ipc.new_stream(f"turbine-{name}.arrows", table.schema, options=opts) as w:
        for b in batches:
            w.write_batch(b)

# One row of each of the rarer types.
types = pa.table({
    "null": pa.array([None, None], pa.null()),
    "u8": pa.array([255, None], pa.uint8()),
    "i16": pa.array([-300, 7], pa.int16()),
    "u64": pa.array([2**64 - 1, 0], pa.uint64()),
    "half": pa.array(np.array([1.5, -2.0], dtype=np.float16)),
    "t32": pa.array([3723, 0], pa.time32("s")),
    "t64": pa.array([3723_000_001, 0], pa.time64("us")),
    "tsns": pa.array([1_791_455_400_123_456_789, 0], pa.timestamp("ns")),
    "d64": pa.array([1_791_417_600_000, 0], pa.date64()),
    "big": pa.array(["large", None], pa.large_string()),
    "bin": pa.array([b"\x00\x01\xff", b"ok"], pa.binary()),
    "lbin": pa.array([b"\x02", b""], pa.large_binary()),
    "fsb": pa.array([b"\xde\xad\xbe\xef", b"abcd"], pa.binary(4)),
    "dur": pa.array([1500, -1], pa.duration("ms")),
    "d256": pa.array([decimal.Decimal("-12345678901234567890.12345"), None], pa.decimal256(40, 5)),
    "point": pa.array([{"x": 1, "y": 2}, None], pa.struct([("x", pa.int8()), ("y", pa.int8())])),
    "attrs": pa.array([[("k", 1)], []], pa.map_(pa.string(), pa.int32())),
    "span": pa.array([pa.MonthDayNano([1, 2, 3]), None], pa.month_day_nano_interval()),
    "either": pa.UnionArray.from_dense(pa.array([0, 1], pa.int8()), pa.array([0, 0], pa.int32()),
                                       [pa.array([5]), pa.array(["x"])], ["n", "s"]),
})
with ipc.new_file("types.arrow", types.schema) as w:
    w.write_table(types)

# View, run-end encoded and union columns, in two batches built separately so
# their view columns have different numbers of data buffers: the first holds
# a 13-byte string (out of line), the second only inline ones (none).
views = pa.schema([
    ("sv", pa.string_view()),
    ("bv", pa.binary_view()),
    ("lv", pa.list_view(pa.int32())),
    ("llv", pa.large_list_view(pa.string_view())),
    ("ree", pa.run_end_encoded(pa.int32(), pa.string())),
    ("su", pa.sparse_union([pa.field("i", pa.int64()), pa.field("s", pa.string())])),
    ("du", pa.dense_union([pa.field("f", pa.float64()), pa.field("t", pa.string())], type_codes=[5, 9])),
])


def batch(sv, bv, lv, llv, ree, su, du):
    return pa.record_batch([sv, bv, lv, llv, ree, su, du], schema=views)


first = batch(
    pa.array(["short", "exactly12byt", "thirteen byte"], pa.string_view()),
    pa.array([b"\x00\x01", b"x" * 12, b"y" * 13 + b"\xff"], pa.binary_view()),
    # Out of order and overlapping: [13, 14], [10, 11, 12], [].
    pa.ListViewArray.from_arrays(pa.array([3, 0, 1], pa.int32()), pa.array([2, 3, 0], pa.int32()),
                                 pa.array([10, 11, 12, 13, 14], pa.int32())),
    pa.LargeListViewArray.from_arrays(pa.array([0, 1, 0], pa.int64()), pa.array([1, 1, 2], pa.int64()),
                                      pa.array(["a", "a value longer than twelve bytes"], pa.string_view())),
    pa.RunEndEncodedArray.from_arrays(pa.array([2, 3], pa.int32()), pa.array(["a", None])),
    pa.UnionArray.from_sparse(pa.array([0, 1, 0], pa.int8()),
                              [pa.array([7, 0, -1], pa.int64()), pa.array(["", "s1", ""])], ["i", "s"]),
    pa.UnionArray.from_dense(pa.array([9, 5, 9], pa.int8()), pa.array([0, 0, 1], pa.int32()),
                             [pa.array([1.5]), pa.array(["p", "q"])], ["f", "t"], type_codes=[5, 9]),
)
second = batch(
    pa.array([None, "", "inline only"], pa.string_view()),
    pa.array([None, b"", b"\xde\xad\xbe\xef"], pa.binary_view()),
    pa.ListViewArray.from_arrays(pa.array([1, 0, 0], pa.int32()), pa.array([1, 2, 0], pa.int32()),
                                 pa.array([20, 21], pa.int32()), mask=pa.array([False, False, True])),
    pa.LargeListViewArray.from_arrays(pa.array([0, 0, 0], pa.int64()), pa.array([0, 1, 0], pa.int64()),
                                      pa.array(["b"], pa.string_view())),
    pa.RunEndEncodedArray.from_arrays(pa.array([1, 3], pa.int32()), pa.array(["b", "c"])),
    pa.UnionArray.from_sparse(pa.array([1, 1, 0], pa.int8()),
                              [pa.array([0, 0, 42], pa.int64()), pa.array(["s2", None, ""])], ["i", "s"]),
    pa.UnionArray.from_dense(pa.array([5, 5, 9], pa.int8()), pa.array([0, 1, 0], pa.int32()),
                             [pa.array([2.0, None]), pa.array(["r"])], ["f", "t"], type_codes=[5, 9]),
)
for b in (first, second):
    b.validate(full=True)
with ipc.new_file("views.arrow", views) as w:
    w.write_batch(first)
    w.write_batch(second)
with ipc.new_stream("views-zstd.arrows", views, options=ipc.IpcWriteOptions(compression="zstd")) as w:
    w.write_batch(first)
    w.write_batch(second)

# Polars writes strings and binary as views by default: a real-world check.
import polars as pl

pl.DataFrame({
    "name": ["T01", "a value longer than twelve bytes", None],
    "data": [b"\x00", b"z" * 20, None],
    "tags": [["a"], [], ["b", "another value longer than twelve"]],
    "power": [1.5, None, 3.25],
}).write_ipc("polars.arrow", compression="zstd")
