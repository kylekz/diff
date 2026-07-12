# dv — local diff viewer / PR reviewer

Native GPUI (Rust) app for reviewing large diffs locally: fast diff browsing,
line/range comments, GitHub review submission via `gh`, WSL-aware. The phased
plan lives in `docs/` — read `docs/architecture.md` first.

## Commands

```
cargo build                                        # debug build
cargo run -p dv                                    # launch the app
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all                                    # also auto-runs via PostToolUse hook
```

First-ever build fetches the zed monorepo as a git dependency and compiles
gpui — expect many minutes. Incremental builds are fast.

## Workspace layout

- `crates/core` (`dv-core`) — headless domain logic: git access, diff model,
  review store, eventually the agent-facing CLI. **No gpui imports here, ever.**
  This crate is the seam for the future WSL host-process split.
- `crates/app` (`dv`) — the GPUI UI binary.

## Architecture principles (don't violate casually)

1. **All repo operations shell out to `git`** (and later `gh`). Never link
   libgit2/gix for repo access — subprocess routing is what makes WSL support
   nearly free (`wsl.exe -d <distro> git ...` is the same code path).
2. **dv-core stays headless and testable.** The UI consumes it through narrow
   interfaces so a client/server split can land without a rewrite.
3. **GitHub goes through the `gh` CLI** (auth/SSO/enterprise for free, no token
   storage). Installed on this machine (2.96); if `gh auth status` fails, ask
   the user to run `gh auth login`. Use `gh run list/view/watch` for CI —
   the repo is private, so unauthenticated API polling does not work.

## GPUI / dependency policy

- `gpui` must be declared with the **exact same git spec** gpui-component uses
  internally (currently un-pinned zed main). A `rev =` on our side creates a
  second, incompatible copy of gpui. Version pinning lives in the committed
  `Cargo.lock`.
- To upgrade: bump zed + gpui-component **together** (`cargo update -p gpui
  -p gpui_platform -p gpui-component`), then build AND launch the app before
  committing the new lockfile.
- GPUI docs are sparse; read real source. Cargo checks zed out under
  `~/.cargo/git/checkouts/`. For heavier reference work, clone into the
  gitignored `refs/` dir:
  `git clone --depth 1 https://github.com/zed-industries/zed refs/zed`
  `git clone --depth 1 https://github.com/longbridge/gpui-component refs/gpui-component`
- **Licensing — important:** `gpui` and `gpui-component` are Apache-2.0: build
  on them, learn from them freely. Most *other* zed crates (editor, project,
  worktree, …) are GPL/AGPL: read them for architecture, **never copy their
  code into this repo**.

## Visual verification (for agents)

UI/style work must be verified by looking at the running app, not by
assuming. The primary loop is **`dv --automation`** (JSON-over-stdio; see
docs/architecture.md § Testing): write a command script, pipe it through the
app, then Read the PNG it produced and actually inspect the image.

```bash
# bash (preferred — PowerShell > redirects write UTF-16); one JSON per line
cat > cmds.jsonl <<'EOF'
{"id":1,"cmd":"wait_ready","timeout_ms":20000}
{"id":2,"cmd":"state"}
{"id":3,"cmd":"action","name":"ToggleSplit"}
{"id":4,"cmd":"select_file","index":2}
{"id":5,"cmd":"wait_ready"}
{"id":6,"cmd":"screenshot","path":"C:/path/to/shot.png"}
{"id":7,"cmd":"quit"}
EOF
./target/debug/dv.exe <repo-path> --automation < cmds.jsonl > out.jsonl
```

Notes that save debugging time:
- Use **forward slashes** in JSON paths (backslashes are JSON escapes).
- Commands run strictly in order, each completing before the next; use
  `wait_ready` after anything that loads (launch, `open`, select_file) so
  one-shot scripts are deterministic. Stdin EOF quits the app (after the
  in-flight command; waits are capped at 60s) — no orphan windows.
- `state` is the semantic dump (files, selection, view mode, row counts):
  assert behavior there, reserve screenshots for style. `actions` lists
  every dispatchable action name. `key`/`click` exercise real input paths
  (click coords are logical px; `state` reports the scale factor).
- The New Review folder picker is disabled under automation (a native
  dialog would block the executor and wedge the channel); scripts use
  `{"cmd":"open","path":"D:/some/repo"}` instead.
- `resize` is fire-and-forget (the OS applies it async) — poll `state`
  until the size matches before asserting on it; `screenshot`'s built-in
  settle delay usually covers it.
- Screenshots are self-captures of the app's own window (PrintWindow by
  PID) at physical resolution — immune to focus/wrong-window mixups.
- A `click` immediately after a state-mutating command hit-tests the stale
  frame (clicks the old UI); insert a `screenshot` (forces a draw + settle)
  or a short `wait` before position-sensitive clicks.

Fallback when automation can't answer it (window chrome, OS integration):
launch `cargo run -p dv` in the background, use Windows-MCP full-desktop
Screenshot (reliable), and kill with `Stop-Process -Name dv`. Ad-hoc
per-window capture via PowerShell P/Invoke has repeatedly grabbed the wrong
window — don't trust it.

## Review CLI (for agents)

`dv review <list|show|create|delete>` and `dv comment <add|reply|resolve|
unresolve|list>` (`crates/app/src/cli.rs`) are the headless, agent-facing
side of the review layer — no gpui, no window. `dv comment list --status
open --json` is the canonical "what does the reviewer want from me" query.

Global flags, anywhere after the subcommand: `--repo <path>` (default: cwd,
also takes `\\wsl.localhost\<distro>\<path>`), `--wsl <distro>:<posix-path>`,
`--json`. With no `--review <id>`, `comment add` targets the most recent
draft review (auto-creating one if none exists); `reply`/`resolve`/
`unresolve`/`comment list` search every review for the comment id instead.

Always pass `--json` for machine parsing — it's a stable schema (`review`/
`reviews`/`comment`/`comments`/`review_id`/`comment_id` keys). Errors print
to stderr always, plus `{"error":"..."}` on stdout in `--json` mode. Exit
codes: `0` success, `1` operation error (unknown id, no repo, store
failure), `2` usage error (bad flags).

`dv pr <list|view|create|fetch>` (`crates/app/src/cli/pr_cmd.rs`) wrap `gh`
for GitHub PRs: `list`/`view <n>` emit `{"prs":[...]}`/`{"pr":{...}}`;
`create --title <t> [--body][--base][--draft]` emits `{"pr":{"number","url"}}`;
`fetch <n>` fetches the PR's head/base and emits `{"pr_range":{"number",
"base_oid","head_oid","merge_base","range"}}` — `range` is the
`merge_base..head_oid` diff range the GUI will reuse. `dv review submit
[<id>] --pr <n> [--verdict comment|approve|request-changes] [--body]
[--include-resolved]` maps local comments onto a GitHub review (validating
every anchor and line-in-diff first, aborting with the full list of
problems on any failure) and emits `{"submission":{"review_id","pr",
"event","comments","url"}}`.

## Status

Phases 0–1 complete: read-only diff viewer (unified + split), WSL routing,
review-navigator shell, keyboard nav, `--automation`, acceptance validated
on a zed-sized diff (see docs/phase-1-diff-viewer.md § Acceptance results).
Phase 2 complete: ReviewStore + agent CLI + GUI commenting (selection,
editor, threads with reply/edit/resolve/delete), live store watching,
summary panel with finish-review verdicts, sidebar badges, stale-anchor
flagging — acceptance loop verified end to end (docs/phase-2-review-layer.md
§ Acceptance results). Phase 3 complete: GitHub via gh — `dv pr` (list/
view/create/fetch + GUI launch), PR picker + header, validated review
submission from both CLI and GUI (blob anchors, rename-aware paths,
line-in-diff), sidebar PR status icons, comment author = GitHub login —
live-accepted against kylekz/difftest (docs/phase-3-github.md § Acceptance
results). Phase 4 complete: settings & appearance —
JetBrains Mono bundled, four themes (Aura Dark, Dracula, Claude Dark,
Claude Light) with ctrl-shift-t picker + follow-OS, settings.json +
ctrl-, panel (view mode, context lines, fonts with row-height scaling),
drag-to-resize sidebar/summary with persisted widths, thread-card/button
polish pass (docs/phase-4-settings-and-theming.md § Acceptance results).
Next: Phase 5 — WSL host process (docs/phase-5-wsl.md).