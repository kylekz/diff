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

Fallback when automation can't answer it (window chrome, OS integration):
launch `cargo run -p dv` in the background, use Windows-MCP full-desktop
Screenshot (reliable), and kill with `Stop-Process -Name dv`. Ad-hoc
per-window capture via PowerShell P/Invoke has repeatedly grabbed the wrong
window — don't trust it.

## Status

Phases 0–1 complete: read-only diff viewer (unified + split), WSL routing,
review-navigator shell, keyboard nav, `--automation`, acceptance validated
on a zed-sized diff (see docs/phase-1-diff-viewer.md § Acceptance results).
Next: Phase 2 — review layer (docs/phase-2-review-layer.md).
