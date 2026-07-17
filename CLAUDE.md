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
cargo clippy -p dv --no-default-features --all-targets -- -D warnings
#   ^ the SHIPPED build shape (automation feature off) — CI checks it and
#     it drifts silently if only the default shape is linted locally
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
failure), `2` usage error (bad flags), `3` timeout (`review wait` only).

`dv review wait [<id>] [--timeout <secs>]` blocks (no polling — it rides
the store watcher) until review activity changes, then emits
`{"wait":{"outcome":"changed","changes":[...]}}` (kinds: `created`/
`deleted`/`updated` with `comments_added`/`status_changed`/
`comments_updated`/`state_changed`) and exits 0; timeout emits
`{"wait":{"outcome":"timeout"}}` and exits 3. `dv skill
<install|show|path>` distributes the dv-review agent skill
(`crates/cli/skill/SKILL.md`, embedded in the binary) to
`~/.claude/skills/dv-review/` — that skill, not this file, is how agents
in *reviewed* repos learn the workflow.

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

## WSL host (dv-host)

`crates/host` (`dv-host`) is a headless stdio server, spawned per WSL distro
via `wsl.exe -d <distro> --exec`, that runs git/fs work distro-side instead
of one `wsl.exe` per command (docs/phase-5-implementation-plan.md). Only
`RepoLocation::Wsl` repos ever route through it; local repos never touch
`crates/core/src/remote/`.

Dev loop — build inside Ubuntu (NOT under `/mnt/d/...`'s 9P; keep the target
dir on ext4 or the build itself is punishingly slow):

```
wsl.exe -d Ubuntu --exec bash -lc "cd /mnt/d/Software/diff && \
  CARGO_TARGET_DIR=\$HOME/.cache/dv-target cargo build -p dv-host"
```

then point dv at the freshly built binary and skip the sidecar/install flow
and hash check entirely (the handshake itself — proto version — still
validates):

```
DV_HOST_PATH=/home/kyle/.cache/dv-target/debug/dv-host cargo run -p dv
```

`DV_NO_HOST=1` force-disables host routing for the process regardless of
`DV_HOST_PATH`/sidecar — the A/B lever for comparing against Stage-A
(`wsl.exe`-per-command) behavior without a separate build.

Without `DV_HOST_PATH`, `dv` looks for a `dv-host-linux-x64` sidecar next to
`dv.exe` (`current_exe()`-adjacent — `DV_HOST_SIDECAR` overrides the search
path, a test seam) and auto-installs it into
`~/.local/share/dv/host/<sidecar-sha256-prefix>/` inside the distro (content
hash, not a version string — see `crates/core/src/remote/install.rs`'s
module doc for why), verified via a client-side sha256 + marker file. No
sidecar next to `dv.exe` means this whole path is silently inert — today's
per-command `wsl.exe` spawns keep working exactly as before.

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
Phase 5 complete (WSL host, docs/phase-5-wsl.md § Acceptance results):
all five slices shipped per docs/phase-5-implementation-plan.md —
protocol + HostClient (S1), transport swap with zero per-command wsl.exe
spawns + blob/get (S2), sidecar auto-install with content-hash upgrade +
CI musl artifact (S3), inotify store/worktree watching replacing the 1s
digest poll — CLI comment → GUI 139ms (S4), and robustness + acceptance
(S5): fs/* store methods off sh -c (locale-proof remote store),
badge-walk running-host-only policy (no distro boot at launch),
bounded install-command timeouts, host-stderr surfacing on connection
loss. Live-verified against the real Ubuntu distro. Acceptance benchmark
reframed the win honestly: per-file diff and cold startup are UI-bound
(tree-sitter), NOT git-I/O-bound, so the host holds per-file parity but
its one-time spawn makes cold startup ~175ms slower; the real win is
capability — native live watching (worktree watch has no Stage-A
equivalent) + zero per-command spawns. DEFERRED to backlog (P3s, not
Phase-5 gate): resubscribe-on-respawn after a mid-session host crash and
moving watch subscribe/unsubscribe RPCs off the GUI thread. Backlog also
carries the deferred S3/S4 review P3s and two Phase-7 startup leads
(host spawn on the cold-start critical path; tree-sitter startup cost).
Phase 6 complete (review navigator, docs/phase-6-review-navigator.md §
Acceptance results): six slices S6a–S6f — a headless cross-repo review
index in dv-core cached in the app-data dir (S6a), shell two-phase
WSL-gated hydration (S6b), review-centric two-line sidebar cards with
explicit switching incl. submitted-read-only (S6c), grouping (repo/
status/PR) + filtering persisted in settings (S6d), out-of-diff comment
surfacing with jump-switches-source (S6e), and read-only GitHub thread
pull via `gh api graphql` with resolved-state sync onto own submitted
threads (S6f). Closes the "my comment vanished" priority cluster — the
incident replay (comment on a non-default review, switch away/back)
passes, and github.com resolve → dv-shows-resolved was live-verified
against kylekz/difftest PR#1. A phase-boundary integration review caught
4 cross-slice bugs (a watcher-reload wrong-review race the most serious)
that per-slice reviews couldn't see; all fixed. DEFERRED to backlog
(P3s): read-only thread mutation buttons render but no-op (dim/hide);
card absolute-timestamp hover tooltip (no tooltip idiom in this crate);
location-canonicalization double-hydrate; flaky host watch test.
Phase 7 complete (performance, docs/phase-7-performance.md § Acceptance
results): seven slices S7-0..S7-6 — fix-first PR-picker Enter regression
(focus the workspace handle, not the shell; + `state.focus` + committed
keyboard-dispatch regression scripts under crates/app/tests/automation/)
(S7-0), perf instrumentation last_switch_ms/last_pr_list_ms/last_pr_open_ms
(S7-1), PR-picker list cache with stale-while-revalidate (warm ctrl-g
last_pr_list_ms=0 vs cold ~2378, no spinner) (S7-2), workspace-state LRU
keeping Workspace entities alive — dual count(6)/byte(128MiB) cap, keyed by
review id — for instant reactivation (last_switch_ms ~2ms vs cold rebuild)
(S7-3), background revalidation on reactivation catching Local worktree
edits (S7-4), content-addressed PR-reopen diff cache keyed by
(merge_base,head_oid) (S7-5), and audit+eviction-policy docs (S7-6). Honest
finding: the PR-reopen ROUND-TRIP stays network-bound (content-addressing
must fetch head_oid to validate) — the win is skipping the tree-sitter diff
recompute, not a network-free reopen. Caches wire eviction to the existing
Phase-2 store watcher + Phase-5 worktree watcher (no polling); the LRU
keeps a parked entity's watchers alive so Phase-2/5 live-update holds while
parked. Capstone integration review caught 2 cross-slice bugs (stale-theme
diff on parked-WSL-no-host reactivation; a revalidate/store-watch review-id
race) — both fixed. DEFERRED (backlog): background the pr_meta fetch for a
truly instant reopen; runtime-exercise the byte-triggered eviction with a
large diff.
Phase 8 complete (LSP, distribution, macOS; docs/phase-8-lsp-and-polish.md
§ Acceptance results): nine slices S8a–S8i — automation gated behind a
default-on cargo feature so shipped binaries build with `--no-default-
features` (S8a), a gpui-free `dv-cli` crate + native Linux binary (S8b),
the content-hash installer generalized to dv-host + dv-cli with a
per-(component,distro) install lock (S8c), provisioning detection/consent/
`ConsistencyReport` in dv-core, boot-storm-safe by contract (S8d), the
first-run "Welcome to dv" onboarding page + per-launch consistency check
with silent dv-host/dv-cli self-repair on drift and consent-gated vtsls
install (S8e), a vtsls LSP client + ctrl-click go-to-definition + read-only
target viewer with back/forward history, honest-view gated (S8f), hover
popover + find-references core API + automation click modifiers/mouse_move
(S8g), a tag-triggered release pipeline — written, deliberately never
triggered (S8h), and macOS menu/cmd-accelerator conventions written blind
against real gpui source, no Mac in this sandbox (S8i). Live-verified
against real Ubuntu throughout: vtsls uninstall→consent→reinstall, cross-
file go-to-def, hover popovers, marker-drift self-heal, and the full
exe+sidecars bundle provisioning a distro end to end. Shipped LSP scope is
WSL-repos-only, narrower than the phase doc's original local-first prose —
recorded in that doc's Acceptance results rather than left implicit. The
capstone integration review (3 rounds over the whole-phase diff) caught 10
cross-slice findings — headline P2: a ctrl-click during the up-to-180s
vtsls consent install latched code intelligence off permanently (S8e×S8f);
plus consistency-check cooldowns, persisted drift dismissal, menu-dispatch
vs target-viewer guards, and CI coverage for the shipped build shape — all
fixed. DEFERRED (backlog): find-references UI surfacing (core API is
live-tested, no call site); a local/Windows LSP spawn path; macOS runtime
is CARRIED-FORWARD-UNVERIFIED pending a real Mac hand-run (checklist in
docs/backlog.md); the real v* tag → Release cut is Kyle's to trigger.