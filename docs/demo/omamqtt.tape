# OmaMQTT: fss-mqtt started as omamqtt wears the current Omarchy theme's
# colours, and follows Omarchy to another theme (Tokyo Night, then Osaka
# Jade, 15 seconds in). Regenerate with
# docs/demo/record.sh <fss-mqtt linux binary> docs/demo/omamqtt.tape (needs Docker).
# colours: 96
Output docs/omamqtt.gif

Set Shell "bash"
Set FontSize 15
Set Width 1280
Set Height 720
Set Padding 16
Set Framerate 20
Set TypingSpeed 90ms
Set Theme { "name": "omarchy-tokyo-night", "black": "#15161e", "red": "#f7768e", "green": "#9ece6a", "yellow": "#e0af68", "blue": "#7aa2f7", "magenta": "#bb9af7", "cyan": "#7dcfff", "white": "#a9b1d6", "brightBlack": "#414868", "brightRed": "#f7768e", "brightGreen": "#9ece6a", "brightYellow": "#e0af68", "brightBlue": "#7aa2f7", "brightMagenta": "#bb9af7", "brightCyan": "#7dcfff", "brightWhite": "#c0caf5", "background": "#1a1b26", "foreground": "#a9b1d6", "selection": "#292e42", "cursor": "#c0caf5" }
Env COLORTERM "truecolor"

Hide
Type ". docs/demo/omarchy.sh && clear"
Enter
Sleep 500ms
Show

Type "omamqtt"
Sleep 400ms
Enter
Sleep 2.5s

# keys.py shows the keys pressed from here on.

# OmaMQTT in Omarchy's Tokyo Night: a site down to its turbines.
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
Sleep 1.5s

# Every turbine's telemetry: a topic's messages, and their numbers charted.
Type "telemetry"
Sleep 1s
Down@400ms 2
Sleep 1s
Tab
Sleep 300ms
Tab
Sleep 1s
Down
Sleep 1s

# Omarchy switches to its Osaka Jade theme; OmaMQTT follows within a second.
# (The switch is omarchy.sh's timer, 15 s after the Enter above.)
Sleep 5s

# The rest in the new colours: a day of minutes scrolled, a message in full.
Escape
Sleep 300ms
Escape
Sleep 500ms
Type "daylog"
Sleep 700ms
Tab
Sleep 300ms
Tab
Sleep 1.2s
Right@500ms 3
Sleep 1s
Escape
Sleep 300ms
Escape
Sleep 500ms
Type "events"
Sleep 400ms
Enter
Sleep 2.4s
Escape
Sleep 200ms
Escape
Sleep 500ms

# Quitting says how to carry on.
Ctrl+C
Sleep 2.5s
