# Vim keys demo (fss-mqtt --vim). Regenerate with
# docs/demo/record.sh <fss-mqtt linux binary> docs/demo/vim.tape (needs Docker).
Output docs/vim.gif

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

Type "fss-mqtt --vim"
Sleep 400ms
Enter
Sleep 2.5s

# keys.py shows the keys pressed from here on.

# j moves down, l opens: down to a site's turbines.
Type@350ms "jl"
Sleep 400ms
Type@350ms "jl"
Sleep 400ms
Type@250ms "jjjjjj"
Sleep 200ms
Type "l"
Sleep 500ms
Type@350ms "jl"
Sleep 500ms
Type@450ms "jj"
Sleep 1.2s

# G to the last row, g back to the first.
Type "G"
Sleep 900ms
Type "g"
Sleep 900ms

# / starts the filter; enter keeps it and goes back to moving.
Type "/"
Sleep 500ms
Type "telemetry"
Sleep 1s
Enter
Sleep 500ms
Type@400ms "jj"
Sleep 800ms

# l into the messages, j / k pick one, enter shows it full screen;
# h / l step to an older and a newer one, q goes back.
Type "l"
Sleep 600ms
Type@450ms "jjk"
Sleep 600ms
Enter
Sleep 1.2s
Type "h"
Sleep 900ms
Type "l"
Sleep 900ms
Type "q"
Sleep 700ms

# y yanks the message; e opens a copy to edit and publish (esc closes it).
Type "y"
Sleep 1.4s
Type "e"
Sleep 2.5s
Escape
Sleep 600ms

# h back to the tree, esc clears the filter; a longer message (a
# Parquet table) full screen.
Type "h"
Sleep 400ms
Escape
Sleep 800ms
Type "/"
Sleep 400ms
Type "fastlog"
Sleep 900ms
Enter
Sleep 400ms
Enter
Sleep 1.2s

# j scrolls, ctrl+d / ctrl+u page, G / g jump to the end and back.
Type@300ms "jjjj"
Sleep 400ms
Ctrl+D
Sleep 800ms
Ctrl+D
Sleep 800ms
Ctrl+U
Sleep 800ms
Type "G"
Sleep 1s
Type "g"
Sleep 800ms

# q goes back, q again quits and says how to continue.
Type "q"
Sleep 800ms
Type "q"
Sleep 2.5s
