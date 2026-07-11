# dv

A fast, native diff viewer and PR reviewer. Rust + [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui).

Born from the pain of reviewing large PRs on github.com: slow to load, hard
to jump around, no code intelligence — and no good way to review an agent's
work locally *before* pushing.

- Local-first: review any diff in any repo, no PR required
- Line & range comments, threads, resolution — GitHub-style
- Submit reviews to GitHub (comment / approve / request changes) via `gh`
- Agent-friendly: reviews are drivable by CLI, so coding agents can read
  your comments, act on them, and resolve them
- Windows (with first-class WSL support) and macOS

## Building

```
cargo run -p dv
```

First build compiles GPUI from the zed monorepo — expect it to take a while.
