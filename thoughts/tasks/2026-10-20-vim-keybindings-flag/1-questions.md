---
date: 2026-10-20
repository: fss-mqtt
type: questions
---

# Questions: Vim-style keybindings behind a CLI flag

## Context
fss-mqtt is a ratatui/crossterm TUI MQTT client. The task adds a command-line flag that switches the TUI
to vim-style navigation keys, like Ranger and similar TUI programs. Today the bindings are inline `match`
arms spread across several input contexts, and printable characters typed in the main panes go into a
live topic filter.

## Key Context Pointers

- Repositories: fss-mqtt (this repo, `C:\dev\fss-mqtt`)
- Libraries / dependencies: `ratatui = 0.30` (crossterm feature), crossterm; no clap (CLI parsing is hand-rolled)
- Filepaths:
  - `src/main.rs` — `USAGE` (47), `struct Args` (96), `parse_args()` (131), `cli_connection()` (252), `run()` (312), event loop (726), restart args (648)
  - `src/config.rs` — connections file parser, `ConnCfg` (23), `set()` (385)
  - `src/app.rs` — `App::key` (484), tree pane keys (586-611), `panel_key` (627), `prompt_key` (1134), `msg_key` (1181), `composer_key` (1330), `viewer_key` (1394), `Focus` enum (97)
  - `src/viewer.rs` — `Viewer::key` (140)
  - `src/composer.rs` — `Composer::key` (863), `enum Action` (677)
  - `src/ui.rs` — `hint()` (865), `hints()` (922), test module (1092)
  - `src/upgrade.rs` — `restart_args` / re-exec (127, 156)
  - `README.md` — CLI flag table (100-115), Filtering (117-128), Keys table (130-155)
- Prior work: none — no earlier `thoughts/` documents exist in this repo.

## Research Questions

### Q1: How does `App::key` dispatch input, and how do printable characters reach the filter?
**Unknown:** The exact order of handling in `App::key`: which global keys are checked first, when typed
characters go to the filter and when they go to the focused pane, and how `Backspace`, `ctrl+w`, `ctrl+u`
and `Esc` change the filter. It is also unclear whether any plain letters are already bound in the tree,
messages or panel panes.
**Why it matters:** Vim-style letter keys such as `h j k l g G /` overlap directly with
type-to-filter. That makes this dispatch order the main constraint on the feature.
**Research needed:** Read `src/app.rs` `App::key` (484 onward) end to end. Trace how a `KeyCode::Char` press
flows to the filter (around 535) and to each focus target (`Focus` enum at 97). List every plain-letter
binding that exists in each `Focus` state today.

### Q2: What input contexts exist, and what keys does each bind today?
**Unknown:** A complete inventory of the input contexts and of each context's bindings.
- Contexts: tree, messages, connections/panel, text prompt, composer, full-screen viewer, and any popups
  or confirmations.
- For each binding: navigation (up/down/top/bottom/page/left/right), action, and text-entry keys.
**Why it matters:** This shows which contexts are navigation surfaces and which are text-entry
surfaces. It also shows where vim-like keys already exist; for example, `Viewer::key` already uses
`j/k/g/G/h/l/b/f/q`.
**Research needed:** Go through `src/app.rs` (`panel_key` 627, `prompt_key` 1134, `msg_key` 1181,
`composer_key` 1330, `viewer_key` 1394), `src/viewer.rs:140` and `src/composer.rs:863`. Build a table of
context → key → effect. Note any modal or confirmation states, such as the composer's discard confirmation
at 871.

### Q3: How do CLI flags flow from `parse_args` into the running app?
**Unknown:**
- How `Args` fields are defined and parsed, and how a boolean flag reaches `App` or the UI state.
- How flags are documented in `USAGE`.
- Whether flags survive the `ctrl+n` restart/re-exec path in `upgrade.rs` / `main.rs:648`.
- Whether any per-user settings, as distinct from per-connection ones, are persisted anywhere, such as
  `config.rs`.
**Why it matters:** This decides where a mode switch would be stored and passed in. It also shows
whether the mode would be lost on restart.
**Research needed:**
- In `src/main.rs`, trace `parse_args()` (131) → `Args` (96) → `run()` (312) → `App` construction.
- Find an existing boolean flag such as `--inline`, `-q` or `--no-save` and follow it end to end.
- Trace `upgrade::restart_args` and `main.rs:648`/`690` to see which args are forwarded on restart.
- In `src/config.rs`, check whether the file format supports any global, non-connection keys.

### Q4: How are the bottom-bar key hints built, and how do they relate to the real bindings?
**Unknown:** How `hints()` chooses the tuples for each focus, how priority and width truncation work,
and whether any other user-visible text names specific keys. That other text could include notices,
prompts or the README.
**Why it matters:** A different keymap needs matching hints, or the UI will show the wrong keys.
**Research needed:**
- Read `src/ui.rs` `hint()` (865), `hint_room` (898) and `hints()` (922 to about 1044).
- Grep for string literals that name keys, such as `"esc"`, `"^"`, `"enter"` and `"tab"`, in `ui.rs`, `app.rs`
  and notice text.
- Note the README sections at 100-155, 164, 178-208, 271 and 353 that document keys.

### Q5: How is key handling tested?
**Unknown:** The test helpers (`press()`, the ctrl helper), how tests build an `App`, and how they assert
on state and hint text. It is also unknown whether `parse_args` is unit-tested and whether `viewer.rs`
has any coverage.
**Why it matters:** This decides how a second keymap can be tested side by side with the default.
**Research needed:** Read the `src/ui.rs` test module (1092 onward, helpers at 1260 and 1695, key-sequence tests
at 1355-1915), `src/composer.rs` tests (985, helper at 1004) and `src/main.rs` tests (743-764).

### Q6: What does "vim-like" mean in Ranger and comparable TUIs?
**Unknown:** The conventional vim-style bindings in Ranger, lazygit, htop, k9s and similar tools:
- navigation: `h/j/k/l`, `gg`/`G`, `ctrl+d/u/f/b`
- filter and search: `/`, `n/N`
- quit and back: `q`
- multi-key sequences: `gg`, `dd`

It is also unknown how these tools handle a filter or search mode next to normal-mode navigation, and
whether they offer the mode as a flag, a config option or both.
**Why it matters:** This is the reference for what users expect from "vim-like".
**Research needed:** Use `web-search-researcher` to collect Ranger's default navigation and search
keys. Check one or two other popular TUIs for how they present vim mode versus default mode and how
filtering is entered and left.

## Decisions to Resolve

### D1: How does filtering work in vim mode?
**Decision:** Letters can't both navigate and filter at once.
**Options:**
- A) `/` enters a filter-input mode; `Esc` or `Enter` leaves it.
- B) Keep type-to-filter but reserve a small set of letters for navigation.
- C) Another modal scheme.

**Needs:** Human input, informed by Q1 and Q6.

### D2: Which contexts does vim mode cover?
**Decision:** Which contexts change: only the navigation panes (tree, messages, connections), or also
the viewer and the composer?
**Options:**
- A) Navigation panes only. The viewer already has vim keys, and the composer stays a plain editor.
- B) Also add modal (normal/insert) editing to the composer.

**Needs:** Human input, informed by Q2.

### D3: How is the mode set?
**Decision:** Whether the mode is set only by a CLI flag or also persisted. Also choose the flag name.
**Options:**
- A) CLI flag only, such as `--vim`.
- B) Flag plus a persisted setting.
- C) Generic `--keys vim|default`.

**Needs:** Human input, informed by Q3.

### D4: Are the default bindings kept in vim mode?
**Decision:** Whether arrows, PgUp/PgDn and the existing ctrl-chords keep working in vim mode or are
replaced. Also decide what happens to any ctrl-chords that clash with vim conventions, such as `ctrl+u`
and `ctrl+d`.
**Options:**
- A) Vim mode adds bindings on top of the defaults.
- B) Vim mode remaps the clashing chords.

**Needs:** Human input, informed by Q2 and Q6.

### D5: How are hints and docs presented in vim mode?
**Decision:** Whether the bottom-bar hints and the README show vim keys when the mode is on.
**Options:**
- A) Hints switch with the mode, and the README gets a vim-mode section.
- B) Hints stay as they are.

**Needs:** Human input, informed by Q4.

## Research Plan
1. Q1: the dispatch and filter conflict. It unblocks D1 and D4 and is the core constraint.
2. Q2: the per-context key inventory, which unblocks D2 and D4.
3. Q3: CLI flag plumbing and restart forwarding, which unblocks D3.
4. Q6: external vim and Ranger conventions, in parallel with the above.
5. Q4 and Q5: hints and test patterns, which shape structure and verification.
