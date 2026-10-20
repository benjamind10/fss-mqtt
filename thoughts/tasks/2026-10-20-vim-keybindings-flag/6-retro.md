---
date: 2026-10-20
git_commit: 54a5485
branch: feat/vim-keybindings-flag
repository: fss-mqtt
type: retro
---

# Retro: Vim-style keys behind `--vim`

## Struggles
- The `.gitignore` `/thoughts/` edit was made outside the logged sessions. The track-task-issue hook said "`thoughts/` is gitignored" when it was not, and the structure phase had to add a whole Phase 0 plus one AskUserQuestion to settle it. Trace: `session 8fce7743 @ 14:31:14`, `session 0e968f47 @ 15:31`, `session be68aded @ 15:42:20`.
- The structure checkpoint `rg "Options \{ inline_bytes"` stopped matching once rustfmt wrapped the two-field literal across lines, so the check proved nothing. Trace: `session 0e968f47 @ 15:32:59`, `4-structure.md § Phase 1 Completed`.
- The manual step `cargo run -- --vim -c local` named a connection the user doesn't have, which cost one failed run and a round-trip. Trace: `session be68aded @ 16:34:56`.
- The design put the D1 shim after `panel_key`, and it was approved that way. bd-structure caught the error and corrected it after approval. Trace: `session 0e968f47 @ 15:33:15`, `4-structure.md § D1`.
- The implementer edited with Python `open().write`. That turned `app.rs`/`main.rs` into CRLF files, which needed a `sed` fix before fmt. Trace: `session be68aded @ 15:55:17`.
- A Python-heredoc progress edit to `4-structure.md` and its verify grep failed, but `| tail` hid the failure and `git commit` ran anyway. Trace: `session be68aded @ 16:39:51`–`16:40:04`.
- A test hard-coded `⏎`, but `glyph::ENTER` is `"enter"` on Windows, so the test failed once. `cargo fmt --check` then failed and needed a full re-run. Trace: `session be68aded @ 16:41:35`, `16:42:14`.
- The user ran the installed 0.7.0 binary for the Phase 2 manual check ("unknown flag --vim"). The narrow-terminal check was then recorded from a general "all works". Trace: `session be68aded @ 16:44:18`, `16:47:44`.
- The user wanted the PR on their fork (`benjamind10/fss-mqtt`), not the upstream parent `mbilling/fss-mqtt`. No skill covers forks, and `gh pr create` defaults to the parent. Trace: `session be68aded @ 16:46:30`.

Evidence: task folder (0–4), `main..HEAD` (2 commits, no fix-ups), Step 1 findings (no failures; stale `filtering` after `tab` observed), 5 session logs.

## Guardrail
`cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings` and `cargo test --locked` run in CI (`.github/workflows/ci.yml`) on every PR, with tests on ubuntu, macOS and Windows. There is no pre-commit hook. `core.autocrlf=input` locally, and there is no `.gitattributes`.

## Candidates, most severe first

### 1. Pin LF line endings in the repo
- Category: automated check
- Fixes: the CRLF struggle (be68aded @ 15:55:17)
- Lands in: `.gitattributes`
- Change: `* text=auto eol=lf`. Then a tool that writes CRLF on Windows can't put mixed endings in a commit, whatever the contributor's `core.autocrlf` is.

## Skill edits

### 1. bd-pull-request: open the PR on the repo the user named, which on a fork is not `gh`'s default
- Instruction: `skills/bd-pull-request/SKILL.md` § 4a, "`gh pr create --base <base> --head <branch> …`"
- What happened: silent. The origin is a fork, and `gh pr create` without `--repo` targets the parent. The user had to correct this by hand in the implement session.
- Edit: add to Step 0: "Run `gh repo view <origin-owner>/<name> --json isFork,parent`. If it is a fork, ask whether the PR targets the fork or the parent (unless the user already said), and pass `--repo <owner>/<name>` to every `gh pr` call in Steps 0 and 4."

### 2. bd-structure: grep checkpoints must survive the formatter
- Instruction: `skills/bd-structure/SKILL.md` § Checkpoint, "callers re-checked: … `rg -n` …"
- What happened: followed and it misled. The narrowed regex assumed a one-line literal, and rustfmt wrapped it.
- Edit: "A grep checkpoint matches a token the formatter can't move (a type or field name alone), never a multi-token span that a formatter may wrap."

### 3. bd-structure: manual steps use the user's real names, or a placeholder
- Instruction: `skills/bd-structure/SKILL.md` § Checkpoint, "Manual:"
- What happened: silent. The step used `-c local` without checking the user's connections, and named no binary.
- Edit: "Manual steps run the branch build (`cargo run -- …`, never an installed binary). They name a resource (connection, profile, host) only after reading the user's config, and otherwise write `<your-connection>`."

### 4. outline-implementer-agent: edit with Edit, and never pipe a gate command
- Instruction: `agents/outline-implementer-agent.md` § implement / verify
- What happened: worked around. Python-heredoc writes caused the CRLF churn, and a verify grep piped through `| tail` hid a failure before `git commit`.
- Edit: "Edit files with Edit/Write, never with a script that rewrites the file. Run any command whose exit code gates a commit unpiped, or with `set -o pipefail`."

### 5. bd-implement-outline: record each manual check's own answer
- Instruction: `skills/bd-implement-outline/SKILL.md` § manual verification
- What happened: followed loosely. "all works" was recorded as the narrow-terminal result, which was never answered specifically.
- Edit: "Record each manual check against what the human said about that check. If a reply doesn't address it, ask once more, or record it as `waived`."

## What held up
- bd-structure's file:line re-check caught the D1 placement error before any code was written.
- The per-phase checkpoints caught the glyph and fmt slips inside the implement session. Nothing broken was committed, and the branch has no fix-up commits.
