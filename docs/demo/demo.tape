# README demo: navigation, charts, anomalies, subscribing and the payload formats. Regenerate with
# docs/demo/record.sh <fss-mqtt linux binary> (needs Docker).
Output docs/demo.gif

Set Shell "bash"
Set FontSize 15
Set Width 1280
Set Height 720
Set Padding 16
Set Framerate 20
Set TypingSpeed 90ms
Set Theme { "name": "fss", "black": "#000000", "red": "#ff6b6b", "green": "#5fbf77", "yellow": "#e5c07b", "blue": "#7ab4e8", "magenta": "#b1a8f5", "cyan": "#7fd1c7", "white": "#e8e6e3", "brightBlack": "#4e4e4e", "brightRed": "#ff6b6b", "brightGreen": "#5fbf77", "brightYellow": "#e5c07b", "brightBlue": "#7ab4e8", "brightMagenta": "#b1a8f5", "brightCyan": "#7fd1c7", "brightWhite": "#ffffff", "background": "#000000", "foreground": "#e8e6e3", "selection": "#333333", "cursor": "#e8e6e3" }
Env COLORTERM "truecolor"

Hide
Type "mkdir -p ~/.config/fss-mqtt && cp docs/demo/connections.toml ~/.config/fss-mqtt/ && export PS1='$ ' && clear"
Enter
Sleep 500ms
Show

Type "fss-mqtt"
Sleep 400ms
Enter
Sleep 2.5s

# keys.py shows the keys pressed from here on.

# The topic tree: a site down to its turbines; green marks topics that just updated.
Down
Right
Sleep 400ms
Down
Right
Sleep 400ms
Down@300ms 6
Right
Sleep 400ms
Down
Right
Sleep 2s

# Typing filters: every turbine's telemetry, on both brokers.
Type "telemetry"
Sleep 1.2s
Down@400ms 2
Sleep 1.2s

# Its last messages, and one full screen.
Tab
Sleep 700ms
Down@450ms 2
Sleep 600ms
Enter
Sleep 2.2s
Escape
Sleep 600ms

# The numbers its messages carry, charted live; down picks the next field.
Tab
Sleep 1.8s
Down@1s 2
Sleep 1.2s
Escape
Sleep 300ms
Escape
Sleep 500ms

# A day of minutes in one Parquet message: a column charted, scrolled
# through with left and right; down, the next column; enter, the chart
# over the whole screen, and down the next column there.
Type "daylog"
Sleep 700ms
Tab
Sleep 300ms
Tab
Sleep 1.5s
Right@500ms 4
Sleep 600ms
Down
Sleep 1.2s
Enter
Sleep 2s
Down
Sleep 1.8s
Escape
Sleep 300ms
Escape
Sleep 300ms
Escape
Sleep 500ms

# Payloads decoded on arrival: Parquet, Avro, Arrow, SQLite and its
# changesets, Protobuf without a schema, MessagePack, CBOR, CSV, gzip.
Type "fastlog"
Sleep 400ms
Enter
Sleep 2.4s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type "events"
Sleep 400ms
Enter
Sleep 1.8s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type "vibration"
Sleep 400ms
Enter
Sleep 1.8s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type "turbines.db"
Sleep 400ms
Enter
Sleep 2s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type "setpoints"
Sleep 400ms
Enter
Sleep 1.8s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type "reading"
Sleep 400ms
Enter
Sleep 1.8s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type "state"
Sleep 400ms
Enter
Sleep 1.8s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type "cms"
Sleep 400ms
Enter
Sleep 1.8s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type "daily"
Sleep 400ms
Enter
Sleep 1.8s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type "summary"
Sleep 400ms
Enter
Sleep 1.8s
Escape
Sleep 200ms
Escape
Sleep 300ms

# Anomalies flagged as they happen: a power spike, a wind sensor stuck at
# one value, a turbine gone quiet. "!" lists them, each with its reason.
Type "!"
Sleep 2.5s
Down@700ms 2
Sleep 2s
Down@300ms 6
Sleep 2.2s
Escape
Sleep 500ms

# Subscribe to more from the connections panel: + and a topic filter.
Shift+Tab
Sleep 600ms
Down@300ms 3
Sleep 400ms
Type "+"
Sleep 700ms
Type "$SYS/broker/clients/#"
Sleep 500ms
Enter
Sleep 1.5s
Escape
Sleep 300ms
Type "clients"
Sleep 2s
Escape
Sleep 500ms

# MQTT patterns match below each broker.
Type "site/+/grid"
Sleep 2s
Escape
Sleep 600ms

# Quitting says how to carry on where you left off.
Ctrl+C
Sleep 2.5s
