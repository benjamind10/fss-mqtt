# fss-mqtt

A fast, keyboard-driven MQTT explorer for the terminal. Subscribe to a broker and get a
live topic tree you narrow down just by typing.

![fss-mqtt: browsing two brokers, filtering, opening messages; Parquet, Avro, Arrow, SQLite, SQLite changeset, Protobuf, MessagePack, CBOR, CSV and gzip payloads decoded; pausing a subscription](docs/demo.gif)

## Install

Pre-built binaries for Linux, macOS and Windows are on the GitHub releases page.

```sh
# Debian / Ubuntu
sudo apt install ./fss-mqtt_<version>-1_amd64.deb
# Fedora / RHEL / openSUSE
sudo dnf install ./fss-mqtt-<version>-1.x86_64.rpm
# Homebrew (macOS, Linux)
brew install mbilling/tap/fss-mqtt
# Windows
scoop bucket add mbilling https://github.com/mbilling/scoop-bucket
scoop install fss-mqtt
```

The Linux binaries are static and run on any distribution. winget isn't set up yet
(see [RELEASING.md](RELEASING.md)).

From source (Rust 1.88+):

```sh
cargo build --release        # → target/release/fss-mqtt (~1.1 MB)
cargo install --path .       # or install it to ~/.cargo/bin
```

## Usage

```sh
fss-mqtt broker.example.com                # connect (and save the connection)
fss-mqtt mqtts://broker:8883 -u me         # TLS (system trust store); asks for the password
fss-mqtt broker:8883 --cafile ca.pem       # TLS with your own CA
fss-mqtt broker:8883 --cafile ca.pem --cert me.pem --key me.key   # mutual TLS
fss-mqtt broker -t 'v1/#' -t '$SYS/#'      # several subscriptions
fss-mqtt                                   # connect the saved connections marked autoconnect
fss-mqtt -c local -c staging               # connect these saved connections
```

Connects with MQTT v5 (so user properties are visible) and reconnects automatically.
TCP and TLS only; WebSocket brokers (`ws://`, `wss://`) are not supported.
`FSS_MQTT_BROKER`, `FSS_MQTT_USERNAME`, `FSS_MQTT_PASSWORD`, `FSS_MQTT_CAFILE` and
`FSS_MQTT_KEY_PASSWORD` (the `--key` passphrase) are read from the environment.
With no saved connections and no broker given, it connects to `mqtt://localhost:1883` (not saved).

## Connections

Several brokers can be connected at once. Each is a top-level branch in the topic tree and keeps its
own topics and history while you connect, disconnect or pause its subscriptions. Filters match
below the broker level, so `devices/#` shows matches from every broker.

Connections are kept in a small file you can edit:
`~/.config/fss-mqtt/connections.toml` (Linux, macOS; `$XDG_CONFIG_HOME` is honoured) or
`%APPDATA%\fss-mqtt\connections.toml` (Windows); `--config` picks another file.

```toml
[[connection]]
name         = "local"
url          = "mqtts://127.0.0.1:8883"
cafile       = "~/secrets/ca.pem"          # also: cert, key, insecure = true
username     = "backend"
password_env = "BROKER_PW"                 # read from this environment variable
client_id    = "fss-mqtt-ci"               # optional; random by default
qos          = 1                           # optional; 0 by default
topics       = ["devices/+/up/#", "$SYS/#"]
paused       = ["$SYS/#"]                  # listed, but not subscribed
capture      = ["devices/d1/#"]            # written to disk; see Capture to disk
autoconnect  = true                        # connect when started without -c or a broker
```

- **Adding:** connect from the command line (the entry is added or updated, named `user@host:port`
  or `--name`; `--no-save` skips this), or add a block to the file. `--cafile`, `--cert` and
  `--key` are saved as absolute paths, so the entry works from any directory; in the file you can
  also write `~/…`.
- **Passwords are never written.** Set `password_env`, or fss-mqtt asks when it connects and keeps
  the password in memory only. `FSS_MQTT_PASSWORD` is saved as `password_env`; `-P` is not.
  The passphrase of an encrypted `--key` works the same way: `key_password_env` in the file
  (`FSS_MQTT_KEY_PASSWORD` is saved as that), or it's asked for, and asked again if it's wrong.
  Supported: PKCS#8 `ENCRYPTED PRIVATE KEY` with PBKDF2 and AES or 3DES (what `openssl genpkey
  -aes256` and `openssl pkcs8 -topk8` write) and the older OpenSSL `Proc-Type: 4,ENCRYPTED`
  RSA/EC keys; not scrypt or PKCS#5 v1.5.
- **Saving** rewrites only that connection's block, so your comments elsewhere in the file stay.
  Pausing or resuming a subscription in the panel is saved the same way.
- **Live:** edits to the file apply while fss-mqtt runs (it checks once a second). New connections
  appear (and connect if `autoconnect`), removed ones disconnect and leave the tree, changed
  subscriptions are (un)subscribed on the open connection, and any other change reconnects with
  the new settings. A file with an error is reported and not applied. The panel's bottom border
  shows the file's path.

The panel at the top lists each connection with its subscriptions. `tab` moves between the panel,
the topic tree and the message list; in the panel, `space` or `⏎` connects or disconnects a
connection, or turns a subscription (subscribed and unsubscribed live) or a capture rule on or off.

| Flag | Default | |
|---|---|---|
| `-c, --connection` | | saved connection to connect (repeatable) |
| `--name` | `user@host:port` | name to save the broker under |
| `--no-save` | | don't add the broker to the connections file |
| `--config` | see above | connections file |
| `-t, --topic` | `#` | topic filter to subscribe to (repeatable) |
| `--history` | `10` | messages kept per topic |
| `--max-payload` | `4KiB` | payload bytes kept per message; the rest is discarded on arrival |
| `--inline` | `64` | payloads up to this size are shown inline in the tree; larger ones show only their size |
| `-q, --qos` | `0` | subscription QoS |
| `--cafile` | | PEM file of CA certificates to trust (implies TLS) |
| `--cert`, `--key` | | client certificate and key (PEM) for mutual TLS; the key may be encrypted |
| `--insecure` | | skip TLS certificate verification |
| `--capture` | | topic filter to capture to disk (repeatable; see [Capture to disk](#capture-to-disk)) |
| `--capture-dir` | current directory | where capture files go (also `FSS_MQTT_CAPTURE_DIR`) |

## Filtering

Just type — the tree updates on every keystroke and the cursor jumps to the first match.

- **Pattern** (input contains `/`, `+` or `#`): MQTT-style. `v1/#`, `v1/+/temp`, `+/+/status`.
  The last level is a case-insensitive *prefix*, so `v1/pl` already shows `v1/plant`, and
  everything below a match is included.
- **Search** (anything else): case-insensitive substring over the full topic path; matches are
  underlined.

`backspace` deletes a character, `ctrl+w` deletes back to the previous level, `esc`/`ctrl+u`
clears. Clearing keeps the tree open at the topic you were on.

## Keys

| Where | Key | |
|---|---|---|
| anywhere | `tab` / `shift+tab` | next / previous pane: connections, topics, messages |
| connections | `↑ ↓` | move |
| connections | `space` `⏎` | connect / disconnect, or subscription / capture rule on / off |
| tree | `↑ ↓ pgup pgdn home end` | move |
| tree | `→` | expand; on a topic with messages, enter its message list |
| tree | `←` | collapse, or jump to parent |
| tree | `⏎` | open the latest message full screen |
| tree, messages | `ctrl+r` | capture this topic and everything below it to disk; again to stop |
| messages | `↑ ↓` | pick one of the last 10 messages (properties and payload follow) |
| messages | `⏎` | open it full screen |
| messages | `← esc` | back to the tree |
| full screen | `↑ ↓ pgup pgdn` | scroll |
| full screen | `← →` | older / newer message |
| full screen | `shift ←→` `<` `>` | pan wide tables (CSV, Parquet) |
| full screen | `tab` | cycle formatted / raw / hex |
| full screen | `esc q ⏎` | close |
| anywhere | `ctrl+c` | quit |

A green `•`/`▸` means the topic (or something under it) received a message in the last 1.5 s.

Colours follow `$COLORFGBG` for light terminals (e.g. `export COLORFGBG='0;15'`) and default to dark.
Terminals without 24-bit colour (Apple Terminal, for one) get the nearest colours from the
256-colour palette instead; 24-bit colour is used when `COLORTERM=truecolor` is set, in iTerm2,
WezTerm, VS Code, Ghostty and on Windows. `FSS_MQTT_COLOR=truecolor` or `=256` overrides the guess.

## Payload formats

The payload is shown formatted, in the details pane (as much as fits) and full screen (`⏎`). The
format comes from the `content-type` property when there is one, otherwise from the payload itself:

| Format | Shown as |
|---|---|
| JSON, JSON lines | indented and coloured; a cut-off document is formatted as far as it goes |
| XML | indented, short elements on one line, tags/attributes/values coloured |
| CSV, TSV (`,` `;` tab `|`) | an aligned table, numbers right-aligned |
| Parquet | row count, row groups, writer and file metadata; the schema with types, null counts and min/max; the first rows as a table |
| Avro container files | schema, record count and codec, the first records as a table; schema-registry framing is recognised and explained |
| Arrow IPC (file and stream) | schema, record batches and row count, the first rows as a table |
| SQLite database | file header, tables with columns and row counts, indexes/views/triggers, the first rows of each table |
| MessagePack, CBOR (by `content-type`) | decoded and shown as JSON |
| gzip, zstd | unpacked on arrival; what's inside is detected and formatted like any other payload |
| Protobuf (by content-type) | raw field numbers and values, nested messages indented; no schema needed |
| SQLite changeset / patchset | per table, the inserts, updates (old → new) and deletes |
| other text / binary | text / hex dump |

Binary formats are decoded when the message arrives, while the whole payload is still there; what's
kept is the decoded result (about 4 KiB) next to the first `--max-payload` bytes. If a payload looks
like one of these formats but can't be read, the reason is shown above a hex dump of its first bytes.

- **Parquet:** flat columns with PLAIN and dictionary encodings, Snappy, Gzip, Zstd and LZ4
  compression; nested columns and rarer encodings show the schema with a note.
- **gzip, zstd:** at most 64 MiB is unpacked; the first 4 KiB of what's inside is kept, or the decoded
  view if it's another binary format (e.g. a zstd-compressed Parquet file).
- **Protobuf:** shown without a schema, like `protoc --decode_raw` (a gRPC message frame is stripped
  first): each length-delimited field is shown as text, a nested message, a packed list of small
  numbers or bytes, whichever it most likely is.
- **Avro:** container files are read with the schema in their header (uncompressed, Deflate, Snappy
  or Zstandard), nested values shown as compact JSON; Avro without its schema (Confluent
  schema-registry framing, single-object encoding) can't be decoded and says so.
- **Arrow IPC:** file and stream formats, LZ4 or Zstd compressed buffers, dictionary-encoded
  columns; nested values are shown inline (`[a, b]`, `{x: 1}`); Union, view types and run-end
  encoded columns are noted.
- **SQLite:** read directly from the file format: tables with columns and row counts, indexes,
  views and triggers, and the first rows of each table; WITHOUT ROWID tables, overflow pages and
  UTF-16 databases are read; in WAL mode, changes still in the `-wal` file aren't part of the payload.
- **SQLite changesets / patchsets** (from the session extension): recognised by a `content-type`
  containing `changeset` or `patchset`, or without one when the whole payload reads as one; the
  format doesn't carry column names, so columns are numbered (`pk c0`, `c1`, …).

## Capture to disk

Nothing is written to disk unless you ask for it, per topic. `ctrl+r` on a topic in the tree or the
message list captures that topic and everything below it (`topic/#`; on a connection's row, all of
it), and `ctrl+r` again stops. A topic inside a subtree that's already captured says so rather than
getting a rule of its own. The rules are listed in the connections panel, where `space` turns one off
and on; captured topics get a red `●` in the tree, and the header shows `● capturing` while anything
is.

Rules are saved with the connection (`capture = ["site/oslo/#"]`, and `capture_paused` for those
turned off), so they apply again next time; `--capture FILTER` (repeatable) sets them for the broker
on the command line. Editing them in the connections file applies live, without reconnecting.

Each connection writes one file per run, created when the first matching message arrives:
`fss-mqtt-<connection>-<YYYYMMDD-HHMMSS>.jsonl` in the current directory, or in `--capture-dir`
(`FSS_MQTT_CAPTURE_DIR`). Its full path is shown when it's created and in the connection's details;
on Unix it's readable by you only. Payloads are written whole, however large: `--max-payload` only
limits what's kept in memory. A thread of its own writes each file, so a slow disk never holds up
the connection: lines reach the file at least once a second, and quitting waits up to 5 s for what's
still queued. If writing fails (a full disk, no permission), or more than 64 MB is waiting because
the disk can't keep up, capture stops for that connection and says why; turning a rule off and on
again resumes it (in a new file after a failed write).

One message per line ([JSON Lines](https://jsonlines.org)):

```json
{"time":"2026-10-15T08:20:30.123Z","topic":"site/oslo/turbine/T01/telemetry","qos":1,"retain":false,"content_type":"application/json","user":[["TurbineId","T01"]],"size":34,"payload":"{\"power_kw\":2265.8,\"wind_ms\":11.5}"}
```

`time` is when it arrived (UTC). `content_type`, `response_topic`, `correlation_data` (base64),
`message_expiry`, `payload_format` and `user` appear when the message has them. The payload is
`payload` when it's UTF-8 text and `payload_base64` otherwise; `size` is its length in bytes.

```sh
# T01's power readings, one number per line
jq -r 'select(.topic | endswith("T01/telemetry")) | .payload | fromjson | .power_kw' fss-mqtt-oslo-*.jsonl
```

## Performance notes

Messages are written straight into an in-memory tree; the UI redraws at ~15 fps rather than per
message, and only the visible rows are rendered. Large payloads are never rendered in the tree. Each
topic keeps its last `--history` messages (10), and each message keeps only the first `--max-payload`
bytes (4 KiB); the rest is discarded as it arrives, so a topic costs at most about 40 KiB of payload
however large or frequent its messages are (captured topics are written to disk whole, as they
arrive). Filtering 100k topics takes a few milliseconds.

The MQTT client (`src/mqtt.rs`) is a small MQTT v5 subscriber on blocking I/O with one thread and
no async runtime, which keeps the binary around 1 MB. TLS uses rustls.

## Development

CI (`.github/workflows/ci.yml`) runs fmt, clippy and tests on Linux, macOS and Windows, checks the
minimum Rust version, runs the end-to-end test against mosquitto (plain, TLS and mutual TLS, and capture to disk), and guards the
release binary size.

```sh
cargo test
# end-to-end: starts mosquitto on 127.0.0.1:18830 (plain), localhost:18883 (TLS) and
# localhost:18884 (mutual TLS, with an encrypted client key)
scripts/e2e-brokers.sh /tmp/fss-brokers
FSS_TEST_CA=/tmp/fss-brokers/ca.pem cargo test --release e2e -- --ignored --nocapture
# filter benchmark over 100k topics:
cargo test --release bench_filter -- --ignored --nocapture
```

The demo GIF is recorded with [VHS](https://github.com/charmbracelet/vhs) against two throwaway
brokers fed with simulated wind-farm data, all in Docker: build a static Linux binary for your
Docker architecture (e.g. `cargo build --release --target aarch64-unknown-linux-musl`), then run
`docs/demo/record.sh <that binary>`. The script is `docs/demo/demo.tape`; the sample payloads it
opens, one per format, are generated by `docs/demo/samples/make_samples.py`.
