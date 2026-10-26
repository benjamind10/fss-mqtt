#!/usr/bin/env python3
"""Makes a tape's GIF from VHS's video of it, with the keys it presses in a
strip below the terminal when the tape has a "# keys.py" line (the keys after
that line are shown).

Usage: keys.py <tape> <video> <gif>   (runs in the VHS image, which has
python3, ffmpeg and the font; record.sh runs it). The GIF is made in two
passes, its palette first, so no frame is held in memory however long the
tape. It needs ffmpeg 7.0 or later, which reads the filters from a file
(-/filter_complex); the key caps need 6.1 (drawtext's boxw, boxh, text_align
and y_align). The image has 7.1.

When each key was pressed is worked out from the tape as VHS plays it: Type
waits TypingSpeed (or its @time) after each character, Enter, Escape and the
like wait too, ctrl+<key> doesn't, Sleep sleeps, and nothing counts while
hidden. The video comes out a little shorter than that (VHS misses frames),
so the times are scaled to its length.

Text a Type writes at the typing speed (what goes into the filter, say) is
one key cap that grows as it's typed; a Type@<time> presses keys, a cap each.
The keys of a part of the tape (up to the next comment) line up side by
side, the newest one lit, until a pause of a few seconds.
"""

import json
import os
import re
import subprocess
import sys
import tempfile

FONT = "/usr/share/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf"
SIZE = 20  # font size of the keys
CELL = SIZE * 0.6  # JetBrains Mono's advance is 0.6 em
STRIP = 52  # height of the strip
CAP_H = 32
PAD = 10  # each side of a cap's label
GAP = 8  # between caps
LEFT = 72  # first cap, after the "keys" label
PAUSE = 2.0  # seconds without a key that clear the caps
HOLD = 1.5  # how long the caps stay after the last key
NAMES = {
    "Enter": "⏎",
    "Escape": "esc",
    "Tab": "tab",
    "Backspace": "⌫",
    "Space": "space",
    "Up": "↑",
    "Down": "↓",
    "Left": "←",
    "Right": "→",
    "PageUp": "pgup",
    "PageDown": "pgdn",
}


def seconds(s):
    m = re.fullmatch(r"([\d.]+)(ms|s|m)", s)
    if not m:
        sys.exit(f"keys.py: can't read the time {s!r}")
    return float(m.group(1)) * {"ms": 0.001, "s": 1, "m": 60}[m.group(2)]


def presses(tape):
    """[(time, label, text, part)] for the keys after the "# keys.py" line
    (none without one), the tape's length, and whether it has that line:
    `text` numbers the Type a character of text is from, `part` the comment
    the key comes after."""
    speed, t, shown, on, part = 0.05, 0.0, True, False, 0
    out = []
    for n, raw in enumerate(open(tape, encoding="utf-8")):
        line = raw.strip()
        if line.startswith("# keys.py"):
            on = True
        if line.startswith("#"):
            part = n
        if not line or line.startswith("#"):
            continue
        cmd, _, rest = line.partition(" ")
        name, _, at = cmd.partition("@")
        wait = seconds(at) if at else speed
        if name == "Set" and rest.startswith("TypingSpeed "):
            speed = seconds(rest.split()[1])
        elif name in ("Hide", "Show"):
            shown = name == "Show"
        elif not shown:
            continue
        elif name == "Sleep":
            t += seconds(rest)
        elif name == "Type":
            text = rest[1:-1]
            for c in text:
                if on:
                    out.append((t, "␣" if c == " " else c, n if not at and len(text) > 1 else None, part))
                t += wait
        elif name.startswith("Ctrl+"):
            if on:
                out.append((t, "ctrl+" + name[5:].lower(), None, part))
        elif name.startswith("Shift+") and name[6:] in NAMES:
            if on:
                out.append((t, "shift+" + NAMES[name[6:]], None, part))
            t += wait
        elif name in NAMES:
            for _ in range(int(rest or 1)):
                if on:
                    out.append((t, NAMES[name], None, part))
                t += wait
    return out, t, on


def background(tape):
    """The terminal's background in the tape's Set Theme, for the strip to
    match (as 0xrrggbb); black without one."""
    for line in open(tape, encoding="utf-8"):
        if line.startswith("Set Theme {"):
            colour = json.loads(line[len("Set Theme "):]).get("background", "#000000")
            return "0x" + colour.lstrip("#")
    return "black"


def caps(keys, width):
    """[(start, stop, x, label, newest)]: each key cap while it shows."""
    shown = []  # the row of caps: [x, label, text, since, newest]
    out = []

    def close(at):
        for c in shown:
            if c[3] < at:
                out.append((c[3], at, c[0], c[1], c[4]))

    last, row = None, None
    for t, label, text, part in keys:
        grow = shown and text is not None and shown[-1][2] == text
        if not grow:
            x = shown[-1][0] + len(shown[-1][1]) * CELL + 2 * PAD + GAP if shown else LEFT
            full = x + len(label) * CELL + 2 * PAD > width - 16
            if last is None or part != row or t - last > PAUSE or full:
                close(min(t, last + HOLD) if last is not None else t)
                shown, x, row = [], LEFT, part
        # Every cap shown so far ends its segment here: the newest one
        # changes (or stops being the newest).
        close(t)
        for c in shown:
            c[3], c[4] = t, False
        if grow:
            shown[-1][1] += label
            shown[-1][4] = True
        else:
            shown.append([x, label, text, t, True])
        last = t
    if last is not None:
        close(last + HOLD)
    return out


def probe(gif, entries, stream=()):
    cmd = ["ffprobe", "-v", "error", *stream, "-show_entries", entries, "-of", "csv=p=0", gif]
    return subprocess.check_output(cmd).decode().strip()


def main():
    tape, video, gif = sys.argv[1:4]
    keys, length, shown = presses(tape)
    scale = float(probe(video, "format=duration")) / length if length else 1
    w, h = (int(v) for v in probe(video, "stream=width,height", ["-select_streams", "v:0"]).split(","))
    y = h + (STRIP - CAP_H) // 2
    font = f"fontfile={FONT}:expansion=none:y_align=font"
    f = []
    if shown:
        f += [f"pad=iw:ih+{STRIP}:0:0:{background(tape)}", f"drawtext={font}:text=keys:fontsize=17:fontcolor=0x5a5a5a:x=18:y={y + 1}"]
    with tempfile.TemporaryDirectory() as work:
        for i, (start, stop, x, label, newest) in enumerate(caps([(t * scale, *k) for t, *k in keys], w)):
            # From a file: the labels need no escaping that way.
            path = os.path.join(work, f"{i}.txt")
            with open(path, "w", encoding="utf-8") as out:
                out.write(label)
            fg, bg = ("0xffffff", "0x3a3a3a") if newest else ("0xa8a8a8", "0x222222")
            f.append(
                f"drawtext={font}:textfile={path}:fontsize={SIZE}:fontcolor={fg}:box=1:boxcolor={bg}"
                f":boxw={round(len(label) * CELL + 2 * PAD)}:boxh={CAP_H}:text_align=C+M"
                f":x={round(x)}:y={y}:enable='between(t,{start:.3f},{stop:.3f})'"
            )
        drawn = ",".join(f) or "null"
        # The filters go in files: a cap each, they're more than one
        # argument may hold. The palette comes from every pixel of every
        # frame (a video's colours, thin text's included, don't survive one
        # picked from changes alone).
        def graph(name, text):
            path = os.path.join(work, name)
            with open(path, "w", encoding="utf-8") as out:
                out.write(text)
            return path

        palette = os.path.join(work, "palette.png")
        ffmpeg = ["ffmpeg", "-v", "error", "-y", "-i", video]
        first = graph("palette.txt", f"[0:v]{drawn},palettegen=stats_mode=full")
        subprocess.run([*ffmpeg, "-/filter_complex", first, palette], check=True)
        second = graph("gif.txt", f"[0:v]{drawn}[v];[v][1:v]paletteuse=dither=none")
        subprocess.run([*ffmpeg, "-i", palette, "-/filter_complex", second, gif], check=True)
    print(f"keys.py: {gif}, {len(keys)} keys shown (times scaled by {scale:.3f})")


if __name__ == "__main__":
    main()
