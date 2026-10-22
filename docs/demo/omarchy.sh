# Sourced by omamqtt.tape, out of sight: Omarchy's theme files the way it
# keeps them, Tokyo Night the current theme, and `omamqtt` (fss-mqtt by that
# name) started with a timer that switches the theme to Osaka Jade partway
# through, by pointing the current theme at it, as Omarchy does.
mkdir -p ~/.config/fss-mqtt ~/.config/omarchy/themes ~/.config/omarchy/current ~/bin
cp docs/demo/connections.toml ~/.config/fss-mqtt/
cp -R docs/demo/omarchy/tokyo-night docs/demo/omarchy/osaka-jade ~/.config/omarchy/themes/
ln -sfn ../themes/tokyo-night ~/.config/omarchy/current/theme
ln -sf "$(command -v fss-mqtt)" ~/bin/omamqtt
export PATH="$HOME/bin:$PATH" PS1='$ '
omamqtt() {
    # Started from a subshell, the timer is no job of this shell's to report.
    ( (sleep 15 && ln -sfn ../themes/osaka-jade ~/.config/omarchy/current/theme) >/dev/null 2>&1 & )
    command omamqtt "$@"
}
