# README demo. Regenerate with docs/demo/record.sh (needs Docker).
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

# Open one site down to its turbines; green marks topics that just updated.
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
Sleep 2.5s

# Search: every turbine's telemetry, across both brokers.
Type "telemetry"
Sleep 1.5s
Down@350ms 3
Sleep 1.2s

# Browse the last messages; properties and payload follow the selection.
Right
Sleep 800ms
Down@500ms 2
Sleep 1s

# Full screen: pretty JSON, raw, hex.
Enter
Sleep 2s
Tab
Sleep 1s
Tab
Sleep 1.5s
Escape
Sleep 400ms
Escape
Sleep 400ms
Escape
Sleep 600ms

# Binary formats are decoded on arrival: Parquet, Avro, Arrow, SQLite and its changesets,
# Protobuf without a schema, MessagePack, CBOR, CSV, and gzip unpacked first.
Type@50ms "fastlog"
Sleep 500ms
Enter
Sleep 2.5s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type@50ms "events"
Sleep 500ms
Enter
Sleep 1.9s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type@50ms "vibration"
Sleep 500ms
Enter
Sleep 2.2s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type@50ms "turbines.db"
Sleep 500ms
Enter
Sleep 2.2s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type@50ms "setpoints"
Sleep 500ms
Enter
Sleep 2.2s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type@50ms "reading"
Sleep 500ms
Enter
Sleep 1.9s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type@50ms "state"
Sleep 500ms
Enter
Sleep 1.9s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type@50ms "cms"
Sleep 500ms
Enter
Sleep 1.9s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type@50ms "daily"
Sleep 500ms
Enter
Sleep 1.9s
Escape
Sleep 200ms
Escape
Sleep 300ms
Type@50ms "summary"
Sleep 500ms
Enter
Sleep 1.9s
Escape
Sleep 200ms
Escape
Sleep 300ms

# MQTT patterns match below each broker.
Type "site/+/grid"
Sleep 2.5s
Escape
Sleep 800ms

# Connections: pause and resume a subscription.
Shift+Tab
Sleep 800ms
Down@500ms 2
Sleep 600ms
Space
Sleep 1.8s
Space
Sleep 1.5s
