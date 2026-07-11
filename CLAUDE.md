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
   storage). `gh` is not installed on this machine yet — required from Phase 3.

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

UI/style work must be verified by looking at the running app, not by assuming:

1. Launch in the background: `cargo run -p dv` (or `target/debug/dv.exe`)
2. Screenshot the window (Windows-MCP / computer-use screenshot tools) and
   actually inspect the image
3. Kill it when done: `Stop-Process -Name dv`

Phase 1 adds `dv --automation` (JSON-over-stdio: dump element tree, dispatch
actions, click, screenshot-to-file) as the primary iteration loop — prefer it
over desktop screenshots once it exists. See docs/architecture.md § Testing.

## Status

Phase 0 (scaffold) complete. Next: Phase 1 — read-only diff viewer
(docs/phase-1-diff-viewer.md).
