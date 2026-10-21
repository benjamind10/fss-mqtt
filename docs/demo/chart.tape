# Charts demo. Regenerate with
# docs/demo/record.sh <fss-mqtt linux binary> docs/demo/chart.tape (needs Docker).
Output docs/chart.gif

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


Type "fss-mqtt --history 60"
Sleep 400ms
Enter
Sleep 2.5s

# keys.py shows the keys pressed from here on.

# A turbine's telemetry: each JSON field over the messages kept, live.
Type "telemetry"
Sleep 3s

# tab to the messages, tab to the chart; down picks the next field.
Tab
Sleep 700ms
Tab
Sleep 1.2s
Down
Sleep 1.5s
Down
Sleep 1.5s
Down
Sleep 1.5s

# Another turbine: the field picked stays picked.
Escape
Sleep 500ms
Down@500ms 2
Sleep 2.5s

# A Parquet fast log: each number column, row by row.
Escape
Sleep 500ms
Type "fastlog"
Sleep 2s
Tab
Sleep 600ms
Tab
Sleep 1s
Down@1.2s 3
Sleep 1.5s

# A CSV report: its columns, one turbine per row.
Escape
Sleep 400ms
Escape
Sleep 400ms
Type "daily"
Sleep 2s
Tab
Sleep 600ms
Tab
Sleep 1s
Down@1.2s 2
Sleep 2s

Ctrl+C
Sleep 2s
