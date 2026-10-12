#!/bin/sh
# Publishes plausible wind-farm traffic for the README demo.
# Usage: publish.sh <broker-host> <site> <turbines>   (runs in the eclipse-mosquitto image)
set -u
host=$1 site=$2 n=$3
pub() { mosquitto_pub -h "$host" -V mqttv5 "$@"; }
num() { awk -v seed="$(od -An -N4 -tu4 /dev/urandom)" -v lo="$1" -v hi="$2" -v d="${3:-1}" \
    'BEGIN { srand(seed); printf "%." d "f", lo + rand() * (hi - lo) }'; }
turbines=$(i=1; while [ "$i" -le "$n" ]; do printf "T%02d " "$i"; i=$((i + 1)); done)

# A large XML snapshot, published now and then.
xml=/tmp/scada.xml
{
    echo '<?xml version="1.0"?><DataStatus><StationStatus site="'"$site"'">'
    for i in $(seq 1 400); do
        echo "<Point id=\"$i\" name=\"WTG.$((i % n + 1)).Signal$i\" quality=\"Good\">$(num 0 100 2)</Point>"
    done
    echo '</StationStatus></DataStatus>'
} > $xml

until pub -t "site/$site/online" -r -m true 2>/dev/null; do sleep 0.5; done
for t in $turbines; do pub -r -t "site/$site/turbine/$t/status" -m RUNNING; done

# One retained sample of each binary format the viewer decodes (see samples/make_samples.py).
sample() { # <topic> <file> <content-type> [more mosquitto_pub options]
    topic=$1 file=$2 ct=$3
    shift 3
    pub -r -t "site/$site/$topic" -f "/demo/samples/$file" -D publish content-type "$ct" "$@"
}
sample turbine/T01/fastlog fastlog.parquet application/vnd.apache.parquet -D publish user-property TurbineId T01
sample turbine/T02/events events.avro application/avro -D publish user-property TurbineId T02
sample turbine/T03/vibration samples.arrow application/vnd.apache.arrow.file -D publish user-property TurbineId T03
sample turbine/T04/reading reading.pb application/x-protobuf -D publish user-property TurbineId T04
sample turbine/T05/state state.msgpack application/msgpack -D publish user-property TurbineId T05
sample turbine/T02/cms vibration.cbor application/cbor -D publish user-property TurbineId T02
sample config/turbines.db turbines.db application/vnd.sqlite3 -D publish user-property Version 3
sample config/setpoints setpoints.changeset application/vnd.sqlite3.changeset -D publish user-property Version 4
sample export/daily.csv daily.csv text/csv -D publish user-property Date 2026-10-07
sample scada/summary summary.json.gz application/gzip -D publish user-property ProviderName SiteSummary

i=0
while true; do
    i=$((i + 1))
    for t in $turbines; do
        pub -t "site/$site/turbine/$t/telemetry" \
            -m "{\"ts\":\"$(date -u +%Y-%m-%dT%H:%M:%SZ)\",\"power_kw\":$(num 900 3600),\"wind_ms\":$(num 4 14),\"rpm\":$(num 9 16),\"pitch_deg\":$(num 0 6)}" \
            -D publish content-type application/json \
            -D publish user-property TurbineId "$t" \
            -D publish user-property Quality Good \
            -D publish user-property Version 1.0.0 \
            -D publish user-property PublishReason Scheduled &
    done
    pub -t "site/$site/grid/frequency_hz" -m "$(num 49.95 50.05 3)" &
    pub -t "site/$site/grid/voltage_kv" -m "$(num 32.4 33.6 2)" &
    wait
    if [ $((i % 6)) -eq 0 ]; then
        pub -r -t "site/$site/scada/online/data" -f $xml -D publish content-type application/xml \
            -D publish user-property ProviderName OnlineData -D publish user-property Quality Good
    fi
    if [ $((i % 9)) -eq 0 ]; then
        t=T0$(( (i / 9) % n + 1 ))
        pub -r -t "site/$site/turbine/$t/alarm" \
            -m "{\"code\":\"PITCH_DEVIATION\",\"severity\":\"warning\",\"value_deg\":$(num 6 9)}" \
            -D publish content-type application/json -D publish user-property TurbineId "$t"
    fi
    if [ $((i % 11)) -eq 0 ]; then
        t=T0$(( (i / 11) % n + 1 ))
        pub -r -t "site/$site/turbine/$t/fastlog" -f /demo/samples/fastlog.parquet \
            -D publish content-type application/vnd.apache.parquet -D publish user-property TurbineId "$t"
    fi
    if [ $((i % 13)) -eq 0 ]; then
        t=T0$(( (i / 13) % n + 1 ))
        pub -r -t "site/$site/turbine/$t/status" -m "$( [ $((i % 2)) -eq 0 ] && echo IDLE || echo RUNNING)"
    fi
    sleep 1
done
