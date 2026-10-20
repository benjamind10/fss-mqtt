---
date: 2026-10-20
git_commit: 309ada6
branch: main
repository: fss-mqtt
type: structure
status: implemented
---

# Structure: Vim-style keys behind `--vim`

## Approach
`--vim` (or `FSS_MQTT_KEYS=vim`) sets `app::Options.vim`. A `vim_key` translation step in `App::key` rewrites vim letters into the `KeyCode`s the panel, tree and messages handlers already handle. A new `App.filtering` flag opens the filter arm to letters only after `/`. With the flag off, every code path is the one that runs today. This is the same direction as `3-design.md`. One placement correction is recorded under D1.

## Desired End State
- `fss-mqtt --vim`: in the tree, `j k h l g G ctrl+d ctrl+u` navigate, `q` quits, `e` publishes and `t y` copy. In the connections pane, `j k` move the panel cursor. In messages, `j k` pick a message and `h` goes back.
- `/` opens filtering. Letters then extend the filter, `⏎` keeps it and returns to navigation, and `esc` clears it and returns. A paste opens filtering.
- The bottom hints and the filter-box placeholder show the vim keys. In filter mode they show `⏎ keep · esc clear`.
- The Windows `Continue session:` line includes `--vim` when it was on. `--help` lists the flag and the env var, and the README documents both.
- Without `--vim`, `cargo test --locked` passes with no existing test edited except the one `Options { … }` literal. A new guard test proves `j` still types into the filter.

## Implementation Progress
- [x] Phase 0: Settle the working tree on a task branch
- [x] Phase 1: `--vim` makes letters navigate and `/` filter
- [x] Phase 2: Vim mode shows its own keys in hints, placeholder and README

## Resolved Decisions

### D1: A translation shim maps vim keys onto the existing handlers
**Decision:** `vim_key` rewrites keys into existing `KeyCode`s. It runs after `Tab`/`BackTab` and **before `panel_key`**. The design's diagram placed it after `panel_key`, but `panel_key` only understands `↑ ↓ Home End` (`src/app.rs:630-633`) and returns `false` for `j` (`:641`). After `panel_key`, `j` in the connections pane would move the tree cursor instead of the panel cursor. Running the shim first gives the panel the translated `↓`. This keeps the design's intent ("the panel handlers already understand").
**Rationale:** It is a small diff that reuses tested handlers (`src/app.rs:585-619`, `:1181-1205`, `:627-644`), and the default path is unchanged.

### D2: `⏎` keeps the filter, `esc` clears it, and both leave `/` mode
**Decision:** This matches yazi/ranger (research §9) and the tree's `esc` today (`src/app.rs:586`). In navigation, `esc` still clears the filter.
**Rationale:** One key undoes a filter, and `esc` keeps its current meaning.

### D3: Vim mode binds `j k h l g G ctrl+d ctrl+u / q e t y`
**Decision:**
- `j k h l` → `↓ ↑ ← →`, `g`/`G` → `Home`/`End`, and `ctrl+d`/`ctrl+u` → `PgDn`/`PgUp`.
- `/` filter, `q` quit, `e` publish, `t y` copy.
- A single `g` goes to the top; there is no `gg` state.
- `ctrl+u` loses "clear filter" in navigation. It keeps that meaning while filtering, where readline semantics apply (research §9).
**Rationale:** These are the letters the viewer already uses (`src/viewer.rs:144-171`). No key-sequence infrastructure exists (research §4).

### D4: `--vim` flag plus `FSS_MQTT_KEYS=vim`, kept in the resume command
**Decision:**
- Parse `--vim` like `--insecure` (`src/main.rs:205`).
- Read the env var like `FSS_MQTT_SESSION` (`:157`).
- `resume_command` (`:689-702`) appends `--vim`.
- On unix, `restart_args` already forwards the raw argv (`src/upgrade.rs:170-187`).
**Rationale:** This gives an always-on option without a prefs file. The connections file rejects global keys (`src/config.rs:243-248`).

## Not Doing
- No action enum or keymap table (rejected in D1, too large a refactor for one flag).
- No `gg` prefix state, no `?` help overlay, no configurable keymap file.
- No vim modes in the composer or the password prompt, and no change to the viewer's keys.
- No change to default (non-`--vim`) behaviour.
- No tests for `parse_args`, which reads `std::env::args()` directly (research §5).

## Phase Overview

### Phase 0: Settle the working tree on a task branch ✅
**Goal:** Implementation starts on a clean task branch, not `main` with a pending `.gitignore` edit.
**Files:** `.gitignore`: the uncommitted `+/thoughts/` line. Commit it on its own, or keep it out of the task branch. This is the human's call at implement time; it is not an open question.
**Depends on:** nothing
**Checkpoint:**
- Automated: `git status --porcelain` is empty, and `git branch --show-current` prints the task branch (e.g. `vim-keys`), not `main`.

**Completed:** 2026-10-20, inline. The human chose to keep the line local: `/thoughts/` moved to `.git/info/exclude`, `.gitignore` reverted, branch `feat/vim-keybindings-flag` created from `main` at 309ada6 (in sync with `origin/main`). `git status --porcelain` is empty. No commit (no tracked changes).
**Manual verification:** n/a

### Phase 1: `--vim` makes letters navigate and `/` filter ✅
**Goal:** With `--vim`, every key in D3 works in all three panes and the `/` filter round-trip works. The hints still show today's text. Phase 2 fixes that.

**Files:**
```diff
 src/
 ├── main.rs        ~ Args.vim; --vim arm + FSS_MQTT_KEYS env default; Options literal (:599);
 │                    resume_command appends --vim; USAGE: --vim line by --inline, FSS_MQTT_KEYS in the env list
 ├── app.rs         ~ Options.vim; App.filtering; vim_key(); filter-arm gate; paste opens filtering
 └── ui.rs          ~ test fixture Options literal (:1241) gains vim: false
+                     + sample_vim() helper (sample + app.opts.vim = true)
+                     + vim-mode key tests (see Test seams)
```

**Interfaces:**
```diff
 // src/app.rs
 pub struct Options {
     pub inline_bytes: usize,
+    pub vim: bool, // --vim / FSS_MQTT_KEYS=vim: letters navigate, `/` filters
 }
 pub struct App {
+    pub filtering: bool, // vim mode: `/` was pressed, letters go to the filter (ui reads it in Phase 2)
 }
+/// Vim mode only: rewrites a vim key into the KeyCode the panes already handle,
+/// or acts and returns None (`/` `q` `e` `t` `y`, and ⏎/esc while filtering).
+fn vim_key(&mut self, k: KeyEvent) -> Option<KeyEvent>
```

How `App::key` (`src/app.rs:484-622`) changes:
````text
ctrl+c · ctrl+n · prompt · composer · ctrl+e/viewer-e · viewer     (unchanged)
Tab / BackTab                                                      (unchanged)
+ if opts.vim: k = vim_key(k)?   ← before panel_key (D1)
    filtering:  ⏎ → filtering=false · esc → filtering=false + set_filter("") · else pass k through
    navigating: j k h l g G ^d ^u → translated KeyEvent (modifiers cleared)
                / → filtering=true, focus=Tree, msg_sel=None
                q → quit · e → open_composer · t/y → copy_topic/copy_message (not in Conns, like ^t/^y)
                anything else → pass k through untouched (space still reaches panel_key)
panel_key · ^t/^y                                                   (unchanged)
filter arm: Char(c) case gated on `!opts.vim || filtering`          (Backspace, ^w, ^u, ^r unchanged)
msg_key / tree match                                                (unchanged)
````
- `App::paste` (`:450-460`) sets `filtering = true` in vim mode on the filter branch.
- `filtering` is not saved in the session. `restore_view` leaves it `false`.

**Depends on:** Phase 0
**Checkpoint:**
- Automated: `cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked` passes.
- Automated: these tests exist and pass. Each one fails if the shim is skipped or misplaced.
  - Default guard: without vim, `typ("j")` leaves `filter_text == "j"`.
  - Vim: `typ("j")` leaves `filter_text == ""` and the selected path changes. `G` then `g` reach the last and first rows. `l` expands the selected node, and `h` collapses it.
  - Vim, connections pane: focus `Conns`, `typ("j")` makes `panel_cursor == 1` and the tree selection is unchanged. This catches the D1 placement.
  - Vim filter round-trip: `/` + `typ("v1/+/line1")` filters (the frame lacks `line2`). `⏎` gives `filtering == false` with the filter kept, and `j` then moves. `/` + `esc` gives `filter_text == ""`.
  - Vim `ctrl+u` while navigating with a filter set: the filter is kept (D3). While filtering, `ctrl+u` clears it.
  - Vim `q` sets `app.quit`. Vim `e` opens the composer. `t`/`y` copy, with the same notice as `ctrl+t`/`ctrl+y` in `copy_topic_and_message` (`src/ui.rs:1841`).
- Automated: `git diff --stat src/ui.rs` touches no existing test body. Only the `:1241` literal changes and new tests are added.
- Manual: on Windows, run `cargo run -- --vim -c local`, quit with `q`, and check that the `Continue session:` line contains `--vim`. Run `$env:FSS_MQTT_KEYS='vim'; cargo run` and check that `j` navigates. `resume_command` has no test seam because it reads `std::env::args()`.
- callers re-checked: `app::Options` literals at `src/main.rs:599` and `src/ui.rs:1241`. `rg -n "Options \{ inline_bytes" src` returns only those two, and `pub struct Options` is the definition.

**Test seams:**

| Behaviour | Seam | Test location | What the seam can't show |
|---|---|---|---|
| vim translation + filter mode | `app.key(KeyEvent)` via `typ`/`press`/`ctrl` on a real `App` | new tests in `src/ui.rs` test module, `sample_vim()` | real terminal key repeat / shift reporting of `G` (crossterm sends `Char('G')` + SHIFT; the shim matches on the char) |
| flag/env parsing, resume line | none (`parse_args`/`resume_command` read process args) | — | all of it → Manual step above |

**Completed:** `src/app.rs` (`Options.vim`, `App.filtering`, `vim_key` shim before `panel_key`, filter-arm gate, paste opens filtering), `src/main.rs` (`--vim` flag, `FSS_MQTT_KEYS` env default, Options literal, `resume_command` appends `--vim`, USAGE lines), `src/ui.rs` (fixture `vim: false`, `sample_vim`, 6 vim-mode key tests). Adaptations: both Options literals are multi-line after rustfmt (the `main.rs` one is now at `:605`), so the checkpoint's `rg "Options \{ inline_bytes"` matches nothing; `rg "Options \{"` finds the definition plus the two literals. `vim_key` passes Alt and other ctrl keys through. In Msgs, `^d`/`^u` jump to the last/first message (existing PgDn/PgUp behaviour of `msg_key`).
**Manual verification:** ran — the human ran `cargo run -- --vim -c ignition` (the outline's `-c local` doesn't exist on this machine), quit with `q`, and saw `--vim` in the `Continue session:` line; with `FSS_MQTT_KEYS=vim`, `j` navigated the tree.

### Phase 2: Vim mode shows its own keys in hints, placeholder and README ✅
**Goal:** A `--vim` user can find every D3 key from the screen and the README. Default hints are byte-identical.

**Files:**
```diff
 src/
 └── ui.rs          ~ hints(): vim lists per focus + filtering list; input_box(): vim placeholder,
                      no edit cursor while navigating
+                     + hint/placeholder frame tests
 README.md          ~ Keys: a "Vim keys (--vim)" table after the main one; Flags table row for --vim;
                      FSS_MQTT_KEYS beside FSS_MQTT_SESSION
```

**Interfaces:** `hints()` (`src/ui.rs:922-961`) picks a list from `(app.opts.vim, app.filtering, app.focus)`. Ranks follow today's scheme.

| State | Hint pairs (rank) |
|---|---|
| vim, filtering (any focus) | `⏎ keep`(0) · `esc clear`(0) · `^w level`(2) |
| vim, Tree | `j k move`(3) · `h l collapse/expand`(3) · `⏎ open`(2) · `/ filter`(1) · `e publish`(1) · `t y copy`(1) · `^r capture`(1) · `esc clear filter`(2) · `tab pane`(2) |
| vim, Msgs | `j k messages`(3) · `⏎ open`(2) · `e publish`(1) · `t y copy`(1) · `^r capture`(1) · `h esc back`(2) · `tab pane`(2) |
| vim, Conns | `j k move`(3) · `space ⏎ connect / on-off`(1) · `tab pane`(2) · `/ filter`(1) |
| vim, any non-filtering | trailing `q quit`(0) in place of `^c quit` |
| not vim | today's lists, unchanged |

`input_box()` (`:821-843`), when `opts.vim` is set:
- Not filtering, empty filter: the faint placeholder `/ to filter  ·  v1/#   sensor/+/temp   or any text`, with no cursor cell.
- Not filtering, non-empty filter: the text, with no trailing cursor cell.
- Filtering: today's rendering, with a cursor cell.

**Depends on:** Phase 1 (`opts.vim`, `filtering`)
**Checkpoint:**
- Automated: `cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked` passes. The existing default-hint asserts still pass unchanged: `"^e publish"` at `src/ui.rs:1704` and `"^n restart · ^c quit"` at `:2311`.
- Automated: new frame tests.
  - Vim tree frame contains `j k move`, `/ filter` and `q quit`, and does not contain `^c quit`.
  - After `/`, it contains `⏎ keep · esc clear`.
  - The vim empty-filter frame contains `/ to filter` and not `ype to filter`.
  - The default frame still contains `ype to filter`. Each assert fails if the branch is wired to the wrong state.
- Manual: in a narrow terminal (`--vim`, about 60 columns), check that the lower-rank vim hints drop before `q quit`. No test asserts rank truncation (research §7).

**Completed:** src/ui.rs (`hints()` vim/filtering lists, `input_box()` vim placeholder with no cursor while navigating, new test `vim_hints_and_placeholder`); README.md (`--vim` flag row, Vim keys section, `FSS_MQTT_KEYS` beside `FSS_MQTT_SESSION`). Adaptations: the test uses `glyph::ENTER` (renders `enter` in this build) instead of a literal `⏎`; `^n restart` kept in the vim lists, including while filtering; README has no env-var list, so `FSS_MQTT_KEYS` sits in the Sessions prose, the Flags row and the Vim keys section; README `g`/`G` row includes connections.
**Manual verification:** ran — the human ran the branch build with `--vim` in a narrow terminal and reported "all works" (lower-rank hints drop before `q quit`).

## Open Questions
None.

## Rollback
Revert the task branch's commits. The flag defaults to off, and nothing it adds is persisted to the session or the connections file.

## File-Disjoint Check
Not parallel. Both phases edit `src/ui.rs` (Phase 1 the fixture and tests, Phase 2 `hints`/`input_box` and tests), and Phase 2 reads Phase 1's fields. Run them in sequence.

## Approval
Approved by the user on 2026-10-20; line refs checked at 309ada6. Conditions: none. All recommendations were accepted as proposed. These include the D1 move of `vim_key` ahead of `panel_key` and the filled-in details: `/` focuses the tree, `t`/`y` are inert in Conns, `filtering` is not persisted, vim hints show `q quit`, and there is no edit cursor until `/`.
