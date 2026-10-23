#!/usr/bin/env bash
# Records a tape's GIF (docs/demo.gif by default) with VHS against two throwaway
# mosquitto brokers fed by publish.sh. Needs Docker. Usage (from the repo root):
#   docs/demo/record.sh <fss-mqtt linux binary> [tape]   (default docs/demo/demo.tape)
set -euo pipefail
bin=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
tape=${2:-docs/demo/demo.tape}
root=$(cd "$(dirname "$0")/../.." && pwd)
# The tape's Output line: its GIF, as a path from the repo root without quotes.
gif=$(sed -n 's/^Output //p' "$root/$tape")
# What VHS records instead: a video. Making a GIF itself, it holds every
# frame in memory, which a long tape runs out of.
rec="$(dirname "$tape")/.recording.tape"
video="$(dirname "$gif")/.recording.mp4"
net=fss-demo
cleanup() {
    docker rm -f fss-demo-oslo fss-demo-bergen fss-demo-pub-oslo fss-demo-pub-bergen >/dev/null 2>&1 || true
    docker network rm $net >/dev/null 2>&1 || true
    rm -f "$root/$rec" "$root/$video"
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
sed "s|^Output .*|Output $video|" "$root/$tape" >"$root/$rec"
docker run --rm --network $net -v "$root:/vhs" -v "$bin:/usr/local/bin/fss-mqtt:ro" \
    ghcr.io/charmbracelet/vhs "$rec"
# The GIF from the video, with the keys pressed below the terminal for a
# tape with a "# keys.py" line.
docker run --rm -v "$root:/vhs" --entrypoint python3 ghcr.io/charmbracelet/vhs \
    docs/demo/keys.py "$tape" "$video" "$gif"
# A theme needs few colours; fewer in the palette roughly halves the file.
# A tape showing more than one theme says how many it needs: "# colours: 96".
colours=$(sed -n 's/^# colours: \([0-9]*\)$/\1/p' "$root/$tape")
docker run --rm -v "$root:/vhs" alpine:3 \
    sh -c "apk add -q gifsicle && gifsicle -b -O3 --colors ${colours:-32} '/vhs/$gif'"
ls -lh "$root/$gif"
