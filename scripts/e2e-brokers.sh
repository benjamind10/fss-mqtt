#!/usr/bin/env bash
# Starts the two mosquitto brokers the e2e test expects, with a throwaway CA:
#   127.0.0.1:18830  plain
#   localhost:18883  TLS (server cert for localhost, signed by $dir/ca.pem)
#   localhost:18884  mutual TLS: requires a client cert ($dir/client.pem, with the
#                    key $dir/client.key encrypted with the passphrase "s3cret")
# Usage: scripts/e2e-brokers.sh <dir>   then: FSS_TEST_CA=<dir>/ca.pem cargo test --release e2e -- --ignored
set -euo pipefail
dir=$(mkdir -p "$1" && cd "$1" && pwd)
cd "$dir"
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 2 -subj "/CN=fss-mqtt test CA" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr -subj "/CN=localhost" 2>/dev/null
printf "subjectAltName=DNS:localhost,IP:127.0.0.1" > san.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out server.pem -days 2 -extfile san.ext 2>/dev/null
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -aes256 -pass pass:s3cret -out client.key 2>/dev/null
openssl req -new -key client.key -passin pass:s3cret -out client.csr -subj "/CN=fss-mqtt e2e" 2>/dev/null
openssl x509 -req -in client.csr -CA ca.pem -CAkey ca.key -CAcreateserial -out client.pem -days 2 2>/dev/null
openssl pkey -in client.key -passin pass:s3cret -out client-plain.key # for mosquitto_pub

cat > plain.conf <<CONF
listener 18830 127.0.0.1
allow_anonymous true
CONF
cat > tls.conf <<CONF
listener 18883 127.0.0.1
allow_anonymous true
cafile $dir/ca.pem
certfile $dir/server.pem
keyfile $dir/server.key
CONF
cat > mtls.conf <<CONF
listener 18884 127.0.0.1
allow_anonymous true
cafile $dir/ca.pem
certfile $dir/server.pem
keyfile $dir/server.key
require_certificate true
CONF
mosquitto -d -c plain.conf
mosquitto -d -c tls.conf
mosquitto -d -c mtls.conf
sleep 1
echo "brokers up; CA at $dir/ca.pem"
