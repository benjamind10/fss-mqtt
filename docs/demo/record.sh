#!/usr/bin/env bash
# Records docs/demo.gif with VHS against two throwaway mosquitto brokers fed by
# publish.sh. Needs Docker. Usage (from the repo root): docs/demo/record.sh <fss-mqtt linux binary>
set -euo pipefail
bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
root=$(cd "$(dirname "$0")/../.." && pwd)
net=fss-demo
cleanup() {
    docker rm -f fss-demo-oslo fss-demo-bergen fss-demo-pub-oslo fss-demo-pub-bergen >/dev/null 2>&1 || true
    docker network rm $net >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup
docker network create $net >/dev/null
for site in oslo bergen; do
    docker run -d --name fss-demo-$site --network $net eclipse-mosquitto:2 \
        mosquitto -c /mosquitto-no-auth.conf >/dev/null
done
docker run -d --name fss-demo-pub-oslo --network $net -v "$root/docs/demo:/demo:ro" \
    eclipse-mosquitto:2 sh /demo/publish.sh fss-demo-oslo oslo 8 >/dev/null
docker run -d --name fss-demo-pub-bergen --network $net -v "$root/docs/demo:/demo:ro" \
    eclipse-mosquitto:2 sh /demo/publish.sh fss-demo-bergen bergen 5 >/dev/null
sleep 12 # let a few large payloads and alarms arrive first
docker run --rm --network $net -v "$root:/vhs" -v "$bin:/usr/local/bin/fss-mqtt:ro" \
    ghcr.io/charmbracelet/vhs docs/demo/demo.tape
# The theme needs few colours; fewer in the palette roughly halves the file.
docker run --rm -v "$root/docs:/docs" alpine:3 \
    sh -c 'apk add -q gifsicle && gifsicle -b -O3 --colors 32 /docs/demo.gif'
ls -lh "$root/docs/demo.gif"
