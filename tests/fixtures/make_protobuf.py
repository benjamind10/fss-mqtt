"""Writes small Protobuf fixtures for the payload decoder tests.
Regenerate: docker run --rm -v "$PWD":/w -w /w python:3.12-slim sh -c "pip -q install protobuf==5.28.3 grpcio-tools==1.66.2 && python make_protobuf.py"
turbine.txt is `protoc --decode_raw < turbine.pb`, the reference the decoder's output is compared with.
"""
import struct
import subprocess
import sys

subprocess.run([sys.executable, "-m", "grpc_tools.protoc", "-I.", "--python_out=/tmp", "turbine.proto"], check=True)
sys.path.insert(0, "/tmp")
import turbine_pb2 as pb  # noqa: E402

m = pb.TurbineReading(
    turbine_id="T01",
    active_power=2265.8,
    wind_speed=11.5,
    power_delta=-42,
    yaw_offset=-3,
    running=True,
    status=pb.FAULT,
    location=pb.Location(lat=59.91, lon=10.75),
    rpm=[9, 10, 11],
    tags=["north", "offshore"],
    raw=bytes([0x00, 0x01, 0xFE, 0xFF]),
)
m.temps["gearbox"] = 61.5
m.temps["nacelle"] = 24.25
data = m.SerializeToString(deterministic=True)
with open("turbine.pb", "wb") as f:
    f.write(data)
# gRPC length-prefixed message: uncompressed flag, 4-byte big-endian length.
with open("turbine.grpc", "wb") as f:
    f.write(b"\x00" + struct.pack(">I", len(data)) + data)
r = subprocess.run([sys.executable, "-m", "grpc_tools.protoc", "--decode_raw"], input=data, capture_output=True, check=True)
with open("turbine.txt", "wb") as f:
    f.write(r.stdout)
