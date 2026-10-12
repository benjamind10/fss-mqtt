"""Writes small gzip and zstd fixtures for the compressed-payload tests.
Regenerate: docker run --rm -v "$PWD":/w -w /w python:3.12-slim sh -c "pip -q install zstandard==0.23.0 && python make_compressed.py"
"""
import gzip
import json
import random
import struct
import zstandard

zstd = zstandard.ZstdCompressor(level=19, write_checksum=True)


def write(name, data):
    assert len(data) < 64 * 1024, name
    with open(name, "wb") as f:
        f.write(data)


doc = {
    "site": "oslo",
    "turbines": [{"id": f"T{i:02d}", "power": i * 37.5, "running": i % 3 != 0} for i in range(1, 41)],
}
doc_json = json.dumps(doc).encode()
write("doc.json.gz", gzip.compress(doc_json, mtime=0))
write("doc.json.zst", zstd.compress(doc_json))

csv = "turbine,power,status\n" + "".join(
    f"T{i:02d},{i * 12.5},{'RUNNING' if i % 4 else 'IDLE'}\n" for i in range(1, 61)
)
write("turbines.csv.gz", gzip.compress(csv.encode(), mtime=0))

with open("turbine-snappy.parquet", "rb") as f:
    write("turbine-snappy.parquet.zst", zstd.compress(f.read()))

# Two gzip members back to back (what `cat a.gz b.gz` gives).
write("multi.txt.gz", gzip.compress(b"first member\n", mtime=0) + gzip.compress(b"second member\n", mtime=0))

# Two zstd frames with a skippable frame between them.
skippable = struct.pack("<II", 0x184D2A50, 4) + b"skip"
write("frames.json.zst", zstd.compress(b'{"part":1,') + skippable + zstd.compress(b'"done":true}'))

long_text = "".join(f"line {i:04d}: the quick brown fox jumps over the lazy dog\n" for i in range(400))
write("long.txt.gz", gzip.compress(long_text.encode(), mtime=0))

rng = random.Random(7)
write("random.bin.zst", zstd.compress(bytes(rng.getrandbits(8) for _ in range(3000))))

# A decompression bomb: 100 MB of zeros.
write("zeros.zst", zstd.compress(bytes(100 * 1024 * 1024)))
