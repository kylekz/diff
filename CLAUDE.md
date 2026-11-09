# dv

Native desktop app (Rust + GPUI) for reviewing diffs locally, above all a coding agent's work before
it is committed, and for reviewing and submitting large GitHub PRs. Windows first with first-class
WSL; macOS and Linux supported. Agents read and answer reviews through the `dv` CLI.

- Spec: `docs/PLAN.md` (decisions D1–D10, architecture, phases; cited as `PLAN §N` / `D<n>`) and
  `docs/DESIGN.md` (the Glass tokens, layout, screens, keyboard map; cited as `DESIGN §N`). Read the
  relevant sections before changing anything; when code and spec disagree, fix one of them in the
  same change.
- `legacy/` holds the first implementation: **read-only reference**. Never build it, never edit it,
  never add it to the workspace. Code comes across only through the salvage list (PLAN §15): read
  it, keep what is listed, rewrite the rest.
- Reference app for patterns: the owner's sibling GPUI app, same GPUI stack (where it lives:
  `CLAUDE.local.md`). Its code may be copied freely; never modify anything in that checkout.
- Shared crates: toolbox-rs (`~/code/toolbox-rs`, github.com/kylekz/toolbox-rs, MIT, the owner's
  own), a git dependency pinned by tag in the root `Cargo.toml`. dv takes its automation crates
  (its `docs/automation.md`), `toolbox_process` (every program dv runs on this machine, its
  `docs/process.md`) and `gpui_appearance` (light/dark and the window materials, its
  `docs/appearance.md`); not `toolbox_stills` or `toolbox_store`. A change to them goes to toolbox-rs first
  (its own `script/check`, then a new tag), then the tag here; never a branch, never a committed
  `[patch]`.
- Machine-specific notes (the sibling app's path, the Mac's ssh destination for `DV_MAC_HOST`)
  live in the git-ignored `CLAUDE.local.md` beside this file, never in the repository:
  @CLAUDE.local.md

## Toolchain and platform

- Rust 1.98.1 (pinned in `rust-toolchain.toml`), edition 2024, resolver 3.
- Windows 11 is the primary platform. Linux, the headless build and the Linux app (X11 and
  Wayland) alike, is built and tested inside WSL Ubuntu (below). macOS is built and tested on the
  owner's Mac over SSH (`script/check-mac`, macOS below, PLAN §13.2).
- `git` and `gh` are on `PATH`. dv shells out to both and never links a git library (PLAN D4, D7).

## Crates (`crates/`)

| Crate | Role | gpui? |
|---|---|---|
| `dv` | lib + bins `dv` (`dv.exe`, the console program) and `dv-gui` (the app's own binary, with `gui`: windows-subsystem on Windows, the app bundle's on macOS), thin `main`s over `dv::main`: entry for the GUI (the `gui` feature), the CLI and `dv backend --stdio`, in three shapes: default (`gui` + `automation`), the shipped app (`--no-default-features --features gui`) and headless (`--no-default-features`: no gpui, the static musl build for WSL distros, whose `dv` and `dv open` exit 2 saying to open the repo from Windows, `windowless`); one clap tree (`cli`, the headless commands flattened in from `dv_cli` and run before any gpui code), the GUI started detached (`dv-gui` when a packaged `dv` has it beside it) on an absolute `open` line and what `dv open` opens (`launch`); Velopack (`installer`, PLAN D9): on Windows its install and uninstall hooks, on Windows and macOS an installed copy's updates, which ask before they close other dv processes (`updates`, `processes`); the icons (`resources/windows/dv.svg` → `dv.ico`, the Linux `dv.png` and the macOS `dv.icns`, `tests/app_icon.rs`) and Windows' version resource (`build.rs`); automation host (`automation` feature, on by default), which also seeds scenario repos with `dv_test_support` (in process, or through a loopback `dv backend --stdio` it can kill and block: `backends`) and plays a scenario's agent through its own CLI (`cli`, `cli_spawn`, `cli_join`: this executable as a child process) | with `gui` |
| `dv_ui` | views (`shell`, `switcher`, `palette`, `file_list`, `comments_list`, `diff_pane`, `thread`, `submit`, …) and their keys (`keymap`, `actions`), services (`Settings`, `Locations` with the app's `Connections` and how WSL is reached, `WslAccess`; `Reviews`, `ReviewUi`), the open review (`session::ReviewSession`: its comparison, interdiff or all changes, where each thread shows, through `dv_core`'s placement, and its remote backend's connection, `session::connection`), highlighting (`syntax`, token kinds) and comment Markdown (`markdown`, sanitised), the file view (`diff_pane::file_view`: one file whole in the stream's pane, a one-file scope), the file finder (`file_finder`, `Ctrl P`) and the history of positions (`history`, `Alt ←`/`→`), code intelligence (`intel`: `CodeIntel` per review, the hover card, the References tab, and the `LanguageServers` service, one server per worktree, spec and executable, through `dv_lsp`; the shell's navigation and status bar items, `shell::intel`), the Glass theme (`theme`), light/dark and the window material (`appearance`: the settings narrowed by the palette, over toolbox-rs's `gpui_appearance`), assets, the `system` and `automation` seams (PLAN §11) | yes |
| `dv_core` | locations, the `Backend` trait and `LocalBackend` (PLAN §5.1; `probe_path`, the one read-only op for an absolute path outside the worktree, for the language servers' resolvers) with the processes a backend runs where its repo lives (`backend::process`: `spawn_stdio`, a process group or job of its own, PLAN §18.3), the job object children that die with dv join on Windows (`job`, over `toolbox_process`), targets and their labels (`label`), git runner, diff pipeline, one side's files whole (`diff::side_file`: a side's paths and one file's text as the pipeline decodes it), review event store, anchors, relocation and placement (`review::relocate`, a file the review doesn't change included: `place_unlisted`), a thread's round snapshot read for renames and where its code went (`review::history`), the diff around a thread's code now (`review::excerpt`), GitHub via `gh` (PLAN §6–§9) | **never** |
| `dv_backend` | the stdio protocol (`proto`: JSON lines, bytes base64, `WireError` for every `BackendError`), the `dv backend --stdio` server loop (`server`), how a server starts (`launch`: `Launcher::Command` a child process such as a loopback `dv backend --stdio`, `Launcher::Wsl` a distro's through `wsl.exe`, `Launcher::InProcess` a thread over in-memory pipes for tests), `RemoteBackend` over a `Connection` with its lifecycle (`client`: state and `subscribe`, reconnect with backoff 1/2/5 s then `Failed` until `retry`, worktrees re-opened and watches resubscribed with a synthetic rescan, remote processes ended by a death, `kill` as a test seam) and the app's `Connections` registry, build hashes (`build`), WSL (`wsl`: the distros, the `/bin/sh` scripts run through `wsl.exe` as a `Shell`, the launch with its markers) and this app's Linux builds and their install, prune and check in a distro (`install`) (PLAN §5); a test helper bin, never shipped: `dv_test_child`, the process the conformance suite spawns and its child-process loopback server | never |
| `dv_cli` | the headless commands (`args`, clap) and what they run with (`Context`: the command line, cwd, env, streams, git env, so tests run them in-process), the repository and which review a command means (PLAN §10.2), a WSL repository's command run inside its distro on Windows (`reexec`, PLAN §10.4), who it acts as, thread placement, JSON DTOs (`dto`), exit and error codes (`error`), the `dv-review` agent skill (`skill/SKILL.md`, PLAN §10.5), the setup checks of every environment (`checks`: dv in a distro, the CLI on PATH with the user `Path` behind a seam, `user_path`, the agents' skills, git identity, `gh`; a distro's run as its own Linux dv, `checks::distro`) behind `dv doctor` and `dv setup [agents | path | wsl | all]` (PLAN §10, §12) | never |
| `dv_lsp` | language servers where the repo lives (PLAN §18.4): the LSP client (`client`: `LspClient` over a `Backend::spawn_stdio` process, Content-Length framing (`framing`), requests with timeouts and `$/cancelRequest` on drop, the server's own requests answered, progress, registered file watchers, UTF-8 columns with UTF-16 converted (`text`), worktree paths and `file:` URIs (`paths`)), `LanguageServerSpec` as data (`spec`) and the registry (`languages`: `typescript`, TypeScript 7's native `tsc --lsp --stdio`; `tailwindcss` and `css`, JavaScript servers), the `Resolver` (`resolve`: the `DV_LSP_<ID>` override, the project's own copy by Node's resolution through the backend, above the worktree root and through links that leave it (`resolve::node`, `Backend::probe_path`; npm, pnpm and bun layouts), the native CPU first (`resolve::platform`), `settings.json`'s `lsp.<id>.command` and `lsp.runtime` (`settings`), `PATH` and the home (`resolve::host`; a distro's read from its user's interactive login shell through `spawn_stdio`, `resolve::shell`), JavaScript servers' entry scripts, bun's global folder and the runtime, bun then node (`resolve::js`), `files_changed` / `resolve_fresh` for installs), the restart schedule (`restarts`) | never |
| `dv_e2e` | bin `e2e`: the scenario runner, toolbox-rs's `gpui_automation_runner` with dv's profile (`DV_` variables, `DV_DATA_DIR`, Windows, Linux and macOS) and its extensions (`fixture`: the header's `gh` and `lsp` played by `mock_gh` and `mock_lsp`, the `gh_log` and `lsp_log` actions, `GIT_CEILING_DIRECTORIES`), tests/e2e/README.md (PLAN §14) | never |
| `mock_gh` | bin: fake `gh` for tests (PLAN §14.2) | never |
| `mock_lsp` | bin: a scripted language server for tests and scenarios (`DV_LSP_<ID>` names it; scenario `--scenario` / `DV_MOCK_LSP`, log `--log` / `DV_MOCK_LSP_LOG`, PLAN §18.7) | never |
| `dv_test_support` | dev-only lib: `FixtureRepo` and shared test helpers (PLAN §14.2) | never |
| `dv_package` | dev-only lib + bin: `script/package`'s release version rule (PLAN §13.2) | never |

Direction: `dv` → {`dv_ui`, `dv_cli`, `dv_core`}; `dv_ui` → {`dv_cli`, `dv_backend`, `dv_lsp`,
`dv_core`, `gpui-component`}; `dv_cli` → {`dv_backend`, `dv_lsp`, `dv_core`}; `dv_lsp` → `dv_core`; `dv_backend` →
`dv_core`; `dv_e2e` → toolbox-rs's `gpui_automation_runner`. `dv_core`, `dv_backend`, `dv_cli` and
`dv`'s `automation` take toolbox-rs's `toolbox_process` (gpui-free); `dv_ui` and `dv` with `gui`
its `gpui_appearance`. `dv_ui` depends on toolbox-rs's
`gpui_automation` always: without its `enabled` feature it is only the no-op stand-ins the views
call (`automation_target`, `open_window`, …), re-exported by `dv_ui::automation`. `dv_ui`'s and
`dv`'s `automation` features turn on `gpui_automation/enabled`, the driver; `dv` depends on it only
with `automation`, so its headless build stays gpui-free. `dv`'s `automation` also brings
`dv_test_support` (gpui-free) for the scenario repos it seeds and `dv_backend` for their
loopback, never in the shipped build. A gpui
dependency in a "never" crate, or in `dv` built without `gui`, fails `script/check` (step 3,
`cargo tree`), because those run headless: in the CLI, as the static musl backend inside WSL, and
in fast tests.

GPUI naming (PLAN D1): `gpui` = crates.io `gpui-pre =0.3.11`, `gpui_platform` =
`gpui-pre-platform =0.3.11`, components = gpui-kit's `gpui-component =0.9.0` and its base
`gpui-base =0.9.0` (the window's text selection, which `dv_ui` copies through the clipboard
seam). toolbox-rs owns this pin: its tag's gpui-pre and gpui-kit versions are the ones dv uses,
and a gpui bump starts there. Don't guess APIs: grep the sources under
`~/.cargo/registry/src/index.crates.io-*/gpui-pre-0.3.11/` and `gpui-component-0.9.0/`.

## Rules

- **Dependencies.** Declare every dependency once in the root `[workspace.dependencies]` (keep it
  alphabetical) and use `foo.workspace = true` in crates. crates.io only (`deny.toml`), with one
  exception: toolbox-rs, a git dependency pinned by tag. Exact pins for the gpui family, the
  versions toolbox-rs's tag pins; bump them together with a toolbox-rs tag that has them, then
  build and run the e2e scenarios before keeping the new `Cargo.lock`.
- **Licensing.** `gpui` and `gpui-component` are Apache-2.0; learn from and build on them freely.
  Most other Zed crates (editor, project, worktree, …) are GPL/AGPL: read them for architecture,
  never copy their code here.
- **Errors.** `anyhow` in app/CLI code, `thiserror` enums in libraries. No `unwrap()`/`expect()`
  outside tests unless the message states the invariant.
- **Style.** rustfmt defaults (a hook runs `cargo fmt --all` after every edit); clippy clean with
  `-D warnings` in dv's three shapes (default, the shipped app, headless). Files stay under
  ~1,500 lines, split by responsibility
  (`mod.rs` / `render.rs` / `tests.rs` per view).
- **Comments** say what something is and why, in the present tense, citing `PLAN §N` / `DESIGN §N`
  for spec decisions. History (review findings, phase labels, "used to…") belongs in commit
  messages, never in comments.
- **The user's repo is read-only.** dv writes only `git fetch`, `.git/dv/**`, `refs/dv/**` and the
  objects those refs need; no commit, checkout, stash, reset or index change, ever (PLAN §2.4).
- **GitHub only through `gh`** (PLAN D7), with `GH_PROMPT_DISABLED=1` and the `DV_GH` override.
  Tests and scenarios point `DV_GH` at `mock_gh`; nothing automated talks to GitHub. If `gh auth
  status` fails, ask the owner to run `gh auth login`.
- **CI checks after the push, `script/check` before the commit.** `.github/workflows/ci.yml`
  runs the gates on Windows, Linux and macOS for pushes to main and pull requests (PLAN §13.4);
  it doesn't replace running `script/check` locally. A workflow change keeps to PLAN §13.4's
  hardening (actions pinned to a commit, read-only tokens, no secrets, no
  `pull_request_target`, outside values only through `env:`) and passes
  `script/check-workflows` (actionlint and zizmor).
- **Releases are the owner's.** No agent pushes a `v*` tag, approves the `release` environment,
  runs or re-runs `release.yml`, edits or deletes a GitHub release, or changes the repository's
  rulesets, environments or Actions settings without the owner asking for that change.
- **Windows and the OS go through seams.** Every window opens through
  `dv_ui::automation::open_window`, and nothing calls `App::activate`, `Window::activate_window`,
  `Window::resize` or `Window::zoom_window` while `dv_ui::automation::is_background()` (each one
  raises and activates the window on Windows). Every browser link, reveal-in-folder, folder
  picker and clipboard write goes through `dv_ui::system` (`open_url`, `reveal`, `pick_folder`,
  `write_clipboard`), never gpui's `open_url` / `reveal_path` / `prompt_for_paths` /
  `write_to_clipboard`: under automation it records the request (`state.system`) instead of
  opening anything, so a test run never replaces the owner's clipboard.
- **Colours come from the theme.** Views read `cx.dv_theme()` (`dv_ui::theme`, DESIGN §1 tokens
  for the appearance × material in use) and never hard-code a colour. Shared sizes (DESIGN §1.5:
  control and bar heights, radii, the spacing scale, type) come from the theme too; a one-off
  pixel value copied from the design board may stay inline next to the element it sizes.
- **Installing and updating are the owner's.** No agent, test or script runs `Setup.exe`,
  `Update.exe` or a dv executable with Velopack's hook arguments (`--veloapp-install`,
  `--veloapp-updated`, `--veloapp-uninstall`, any `--veloapp-*`): they write the real
  `%LOCALAPPDATA%\dv`, the real user `Path` and the Start menu. The hooks' logic is tested through
  the `Path` seam (`crates/dv/src/installer.rs`). `target/release` is the owner's own build: build
  releases with `script/package` (its `dist` profile, `target/dist`), never `--release` into the
  default target dir, and never write `target/release`. No agent passes `script/package
  --publish`: it writes `target/package/releases`, the feed the owner's installed copy updates
  from, so any release there reaches his app; an agent's run is a trial (`target/package/try`).
- **Live checks are the owner's.** `DV_LIVE_WSL=1` and `DV_LIVE_GH=1` (PLAN §14.3) touch a real
  distro's home and the `kylekz/difftest` repo; no agent, script or scenario sets them.
- **The owner's WSL home is off limits.** Nothing an agent, a test or a script runs installs
  into, writes or deletes anything under the distro home's `~/.local/bin/dv`,
  `~/.local/share/dv/**` or `~/.claude` / `~/.agents` / `~/.codex` (the first implementation's
  install lives there). Every real `wsl.exe` run that installs or launches dv goes through the
  sandbox seam `DV_WSL_HOME` (beside `DV_GH`): a POSIX path, a fresh folder under the distro's
  `/tmp`, which every launch, install, prune and check then uses as `$HOME` (`… --exec
  /usr/bin/env HOME=<it> /bin/sh -c …`, PLAN §5.2), removed afterwards. One check decides what
  its value means (`dv_backend::wsl::sandbox`): set to anything but a plain absolute POSIX path
  naming a folder other than `/` (blank, whitespace, `..` parts and `/.` included) it is an
  error, never the real home, and no sandbox for an automation run. Tests that hand the CLI a
  stand-in Linux build to check a guard name a distro that can't exist (`dv-no-such-distro`),
  so a guard that regresses reaches no real distro. Never `wsl --shutdown`,
  `-t`, `--unregister`, `--install` or `--set-default`, never `sudo`/`apt` in the distro, and
  never run anything in a distro other than `Ubuntu`, even one already running (Docker
  Desktop's `docker-desktop` is; `--wsl docker-desktop:…` crosses into it). The app holds to this under automation itself: an
  `--automation` run without `DV_WSL_HOME` naming a scratch home reaches WSL only through a
  stand-in that lists the distros a scenario names (`wsl_distros`) and refuses to start dv in
  any (the e2e runner drops an inherited `DV_WSL_HOME`; only a scenario header's `env` sets one,
  which dv checks as above), and `dv_ui`'s tests can't reach the real WSL at all (`WslAccess`,
  PLAN §5.2). The app itself never boots a distro the owner stopped: only an explicit open does
  (PLAN §5.2's connecting rule).

## Commands

```bash
script/check                 # every gate (PLAN §13.2): fmt, clippy x3, gpui boundary, tests, e2e
script/check --fast          # the same without the e2e scenarios
script/check --e2e-only      # the e2e scenarios alone (step 5; CI's job beside --fast)
script/check --foreground    # also tests/e2e/foreground: takes the focus for a few seconds
script/check --linux         # also step 6: the headless crates and dv tested and linted in WSL
                             # Ubuntu, script/build-linux --debug-only, the real WSL sandboxed under /tmp
script/check --linux-gui     # also step 7: the Linux app in WSL Ubuntu (~/.cache/dv-target-gui):
                             # clippy in its GUI shapes, dv/dv_ui/dv_e2e tests, e2e on Xvfb
script/check-mac [--sync-only] [-- <check args>]  # the Mac ($DV_MAC_HOST, ~/code/dv) made this working
                             # tree, then script/check run there; its exit code (macOS below)
script/build-linux [--aarch64] [--debug-only]   # the static musl dv for WSL:
                             # target/{debug,release}/dv-linux-x86_64 (--debug-only: debug's alone;
                             # --out <dir> [--profile dist]: only there, script/package's form)
script/package [--version <semver>] [--aarch64]  # a trial Windows release (PLAN §13.2): target/dist,
                             # target/package/{linux,stage,try}; Setup.exe and a feed nothing reads
                             # (needs vpk; never run the installer it makes; --publish, which writes
                             # the installed app's feed target/package/releases, is the owner's)
script/package --linux-app   # a trial AppImage of the Linux app: built in WSL Ubuntu (dist profile,
                             # ~/.cache/dv-target-gui), `vpk "[linux]" pack` on Windows, into
                             # target/package/try (dv.AppImage); a trial only, never --publish
                             # (--feed, --linux-dir, --out: the release workflow's, PLAN §13.4)
                             # on a Mac it makes the macOS release (.pkg, osx feed) instead: the
                             # release workflow's macOS job only (needs .NET, which the Mac lacks)
script/check-workflows       # actionlint + zizmor over .github (winget: rhysd.actionlint, zizmor.zizmor)
script/release-check <tag>   # what release.yml checks about a tag: annotated v<semver> on main,
                             # the workspace's version; read-only
act push -W .github/workflows/ci.yml -j <job> -P ubuntu-24.04=catthehacker/ubuntu:act-24.04
                             # a Linux CI job in Docker (act; Windows and macOS jobs can't run in it)

cargo build                  # the dv binary (default member)
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p dv --no-default-features --features gui --all-targets -- -D warnings   # the shipped app
cargo clippy -p dv --no-default-features --all-targets -- -D warnings   # headless (the WSL build)
cargo test -p dv --no-default-features   # headless tests; relinks target/debug/dv without the app
cargo test -p dv --test backend_loopback # the built `dv backend --stdio` as a child: the
                             # handshake, a kill and the reconnect, Failed and retry, no leaked server
cargo fmt --all
cargo build -p dv -p mock_gh -p mock_lsp                           # the runner starts these; it doesn't build them
cargo run -p dv_e2e --bin e2e -- tests/e2e    # background run; output in target/e2e/<scenario>/
DV_TEST_TSC=<TypeScript 7's lib/tsc> cargo test -p dv_lsp --test real_tsc
                             # the real TypeScript check (PLAN §18.7): read-only, a temp fixture;
                             # in WSL, a project's node_modules/.bun/@typescript+typescript-linux-x64@*/…/lib/tsc
DV_TEST_JS_RUNTIME=<bun or node> DV_TEST_TAILWIND=<its bin script> cargo test -p dv_lsp --test real_js
                             # the same for Tailwind CSS (DV_TEST_CSS: the CSS server's)
```

`dv` has three shapes (PLAN §13.1): the default one (`gui` + `automation`) for development and
the e2e runner, the app as it ships (`--no-default-features --features gui`, automation off) and
the headless build (`--no-default-features`: the CLI and `dv backend --stdio`, no gpui). Each
drifts silently if only the default shape is linted, so `script/check` lints all three. Unit
tests in `dv` that need gpui are gated on `gui`; `crates/dv/tests/*` run in every shape.

Run `cargo build -p dv -p mock_gh -p mock_lsp` right before the e2e runner: a `cargo test` build relinks the
binaries with gpui's test support, and a headless build (`--no-default-features`) relinks `dv`
without the app, neither of which the runner may start. `script/check` does this itself.

### The dv CLI

```bash
dv status --json             # the repo, who you act as, the open reviews and the current one
dv review list [--all]       # dv review show [<id>], dv comment list [--status open] [--file p]
dv comment show <thread>     # --json on any command: one {"schema": 1, …} document on stdout
dv comment reply <thread> --body-file -    # also add, annotate, address, resolve, reopen (PLAN §10.1, §19)
dv review new | done | submit | close | delete | wait     # wait: exit 3 on --timeout
dv setup agents --home <dir> --yes         # the dv-review skill; never without --home in tests
dv setup wsl <distro> --yes  # Windows: this build into the distro; only with DV_WSL_HOME=/tmp/… here
dv doctor [--json]           # every environment's setup checks (exit 1: something to fix); reads only
dv setup path | all --yes    # PATH (the user Path on Windows) / every fix: only with the seams below
dv --wsl Ubuntu:/tmp/x status # Windows: runs inside the distro (installs dv there first); the same rule
dv open <path>               # the GUI, detached; `dv` alone opens the working directory's repo
```

Every command but `dv` and `dv open` is headless; its exit codes are PLAN §10.3's (2 usage or
an unclear review, 3 a wait's timeout, 4 a WSL distro dv can't reach). On Windows a command for
a WSL repository (`--wsl`, `--repo \\wsl.localhost\…`, or a working directory there) runs
inside the distro as this build, installing it into the distro home first when it is missing
(PLAN §10.4): from this shell that is the owner's real home, so run such a command only with
`DV_WSL_HOME=/tmp/…` (and `MSYS_NO_PATHCONV=1`, WSL below). Unless stdout and stderr are both
a terminal, the crossing copies the distro's output into dv's own streams (`wsl.exe` would
write the two over each other in one file, as this shell captures them), so `--json` output is
whole here. The CLI's tests can't cross by
accident: `dv_cli`'s `Fixture` and `crates/dv/tests/cli.rs` give every command a
`DV_LINUX_DIR` without a Linux build, and the automation host's `cli` children get one unless
the app is sandboxed. Test commands in-process (`dv_cli::main_with` with a `Context`, as
`crates/dv_cli/src/tests/` does), or spawn the built binary with a headless subcommand only:
`dv` alone or `dv open` opens a window on the owner's desktop, so no test runs them (the
startup decisions are pure functions in `crates/dv/src/launch.rs`). `dv setup agents`
writes into the home directory: tests pass the hidden `--home <temp dir>`, and nothing runs it
against the real home (`~/.claude`, `~/.agents`, `~/.cursor`, or `~/.codex` / `$CODEX_HOME`,
which it reads to see whether Codex is installed). `dv setup wsl` writes into a distro's home:
tests set `DV_WSL_HOME` to a folder under its `/tmp` (WSL below). `dv setup path` (and `setup
all`) writes the Windows user `Path` in the registry and broadcasts `WM_SETTINGCHANGE`: every
test, scenario and agent run sets `DV_USER_PATH_FILE` to a scratch text file that stands in for
it (`dv_cli`'s own tests refuse the registry without it; the automation host gives the app and
its `cli` children `<data_dir>/user-path.txt`), and `HOME`/`USERPROFILE` to a scratch home, and
`DV_GH` to `mock_gh` or a program that isn't there (`DV_MACHINE_PATH_FILE` stands in for the
machine's `Path`, read for a `dv.exe` that shadows ours); no test reads or writes the real
registry. `DV_ONLY_DISTROS` (set by the automation host for its `cli` children, empty unless
sandboxed) keeps `dv doctor` / `dv setup` from reaching any distro but the sandbox's.
`dv doctor` itself only reads, but runs `gh auth status` and git: the same seams in tests. Every command line in
`crates/dv_cli/skill/SKILL.md` is run by `crates/dv_cli/src/tests/skill.rs`; change both
together.

This shell runs inside Claude Code, which exports `CLAUDECODE=1`: any `dv` command run from it
acts as the agent `claude-code` (PLAN §10.1). Run the CLI as the reviewer with
`env -u CLAUDECODE dv …`, and have every test and scenario that runs it remove `CLAUDECODE`,
`DV_AUTHOR` and `DV_REVIEW` from the child's environment, setting them only on purpose. A
scenario plays the agent with the automation host's `cli` / `cli_spawn` / `cli_join`, which do
this themselves (`"env": {"CLAUDECODE": "1"}` makes the agent Claude Code;
`tests/e2e/workflow_a_agent.jsonl` is PLAN §3 workflow A end to end that way).

## GUI runs

App windows open on the owner's real desktop.

- Run the app only through the e2e runner (background mode, the default: windows never take focus
  and stay behind everything; on macOS they open unfocused but in front, since macOS stops drawing
  a covered window) or `--automation` scripts that end with `quit`. Never leave `dv.exe`
  or `mock_gh.exe` running; if a run fails, kill what you started (`taskkill //F //IM dv.exe` from
  bash). Scenarios that must take the focus (`tests/e2e/foreground/`) run only on demand.
- **Linux GUI runs go on an Xvfb display, never WSLg's.** Inside WSL the desktop's display
  (`WAYLAND_DISPLAY=wayland-0`, `DISPLAY=:0`) is WSLg's, whose windows open on the owner's
  Windows desktop. The e2e runner on Linux starts an Xvfb display of its own for the run and
  starts the app there under X11 alone (PLAN §14.2); run the Linux app only through it (from
  the Linux GUI target dir: `CARGO_TARGET_DIR=$HOME/.cache/dv-target-gui cargo run -p
  dv_e2e --bin e2e -- tests/e2e`, or `script/check --linux-gui`). Its `--host-display`
  (WSLg's display) is for an explicit exploratory look the owner asked for: a short scenario
  under `target/tmp/` that quits. `script/check --linux-gui` runs the Linux `cargo test`
  with `DISPLAY` and `WAYLAND_DISPLAY` removed (gpui's test platform needs neither); do the
  same by hand.
- Background screenshots show the window inactive and miss Acrylic's blur (translucent surfaces
  come out over black); judge Acrylic and Mica's active tint from the foreground scenario's screen
  captures (tests/e2e/README.md, "Screenshots and window materials").
- Never point the app at the real data dir (nor, on Linux, the distro's `~/.config/dv`).
  `%APPDATA%\dv` holds the first implementation's files.
  Every run uses `--data-dir` / `DV_DATA_DIR` under `target/` (the runner gives each scenario its
  own); under `--automation` without one, dv uses a fresh temp dir (removed when it exits), never
  the real one. dv reads that folder's `settings.json` leniently and keeps keys it doesn't know,
  because the first implementation's file has the same name; dv's repo list is
  `known_repos.json`, never the first implementation's `repos.json`, its per-review choices
  (split or unified) are `review_ui.json`, and its first-run marker is `onboarding.json`, never
  the first implementation's `setup.json`. An automation run's setup checks use `<data_dir>/home`
  and `<data_dir>/user-path.txt`, and show the first-run screen only with `DV_FIRST_RUN=1`.
- Verify UI by looking: read the PNGs a run writes and inspect them against DESIGN.md. `state` is
  the assertion surface; screenshots are for style. The dev display runs at 150% scaling, so
  screenshots are physical pixels (a 1440×900 window captures as ~2160×1350).
- Exploratory runs (looking at something no scenario covers yet) are scratch scenarios outside
  `tests/e2e`, e.g. `target/tmp/explore.jsonl`, run with the runner and their own `--out`.
  toolbox-rs's `docs/automation.md` documents the protocol, the scenario format and the runner;
  `tests/e2e/README.md` dv's own extensions, commands and `state`.
  The connection states (reconnecting, failed, the banner) are the loopback's in a scenario
  (`seed_repo` with `"backend": "loopback"`, `backend_kill`, `backend_block`); a real WSL repo
  only in an exploratory run (WSL below).

## WSL

- Build and test Linux code inside Ubuntu, never on Windows paths through `/mnt/d` for the target
  dir: 9P makes the build itself punishingly slow. Keep the target dir on the distro's ext4 disk:

  ```bash
  wsl.exe -d Ubuntu --cd 'D:\Software\diff' --exec bash -lc \
    'CARGO_TARGET_DIR=$HOME/.cache/dv-target cargo test -p dv_core'
  ```

  `script/check --linux` runs the headless crates' and the headless `dv`'s tests and clippy this
  way. rustup installs the pinned toolchain inside the distro on first use. Run inside a distro,
  `script/check --linux` and `script/build-linux` work there directly (they test for Linux
  first) and skip the real-WSL step, which needs Windows.
- A WSL repo always goes through the Linux `dv`: in the app as `dv backend --stdio` in the
  distro, one `wsl.exe` child per distro, with no per-command `wsl.exe` fallback (PLAN D6, §5);
  in the CLI on Windows as the whole command run there, one `wsl.exe` for it (PLAN §10.4).
- **The Linux build** is `script/build-linux` (`--aarch64` adds the arm build): the headless `dv`
  for `x86_64-unknown-linux-musl`, built in Ubuntu, copied beside the Windows builds as
  `target/debug/dv-linux-x86_64` and `target/release/dv-linux-x86_64` (`--debug-only`, as
  `script/check --linux` runs it, leaves the release copy, a release app's, alone). The app and `dv setup
  wsl` find it beside their executable, or in `DV_LINUX_DIR` (a Windows folder), and install it
  into a distro by its sha256 (PLAN §5.2, §13.2).
- **The real-WSL tests** run only when `DV_TEST_WSL_DISTRO` names a distro, and then only in
  scratch homes under `DV_WSL_HOME`, which must be a folder under the distro's `/tmp` (anything
  else fails the test rather than touch a real home); `script/check --linux` makes a fresh one
  and runs them after `script/build-linux`. By hand, from Git Bash (`MSYS_NO_PATHCONV=1` keeps it
  from rewriting `/tmp/…` into a Windows path, which the tests refuse):

  ```bash
  script/build-linux
  MSYS_NO_PATHCONV=1 DV_TEST_WSL_DISTRO=Ubuntu DV_WSL_HOME=/tmp/dv-check-$$ \
    cargo test -p dv_backend --test wsl -- --test-threads=1
  # the same for `dv setup wsl` and the CLI run inside the distro:
  #   cargo test -p dv_cli --lib -- setup_wsl reexec doctor --test-threads=1
  #     (dv_cli's tests run `mock_gh`: `cargo build -p mock_gh` first, unless a
  #     `cargo test --workspace` built it)
  #   cargo test -p dv --test wsl_cli -- --test-threads=1
  #   cargo test -p dv_lsp --test wsl_shell -- --test-threads=1
  ```

  Each test removes its own folder; remove the empty `DV_WSL_HOME` with `wsl.exe -d Ubuntu
  --exec rmdir /tmp/dv-check-…`. Exploring the app against WSL: after `script/build-linux`, a
  scratch scenario whose header sets `"env": {"DV_WSL_HOME": "/tmp/dv-explore-home"}` (a fresh
  folder; the header's env reaches the app without Git Bash rewriting it) and opens a scratch
  repo made under the distro's `/tmp` (`\\wsl.localhost\Ubuntu\tmp\…`); remove both folders
  inside the distro afterwards.
- **The owner's live check** (`crates/dv_backend/tests/live_wsl.rs`, PLAN §14.3) installs into
  the real home: `DV_LIVE_WSL=1 cargo test -p dv_backend --test live_wsl -- --ignored
  --test-threads=1` after `cargo build -p dv` and `script/build-linux` (it ends with
  `target/debug/dv.exe` running a command inside the distro), `DV_LIVE_WSL_DISTRO` naming the
  distro (default `Ubuntu`). The owner runs it; no agent does.

## macOS

- **The Mac** is the owner's Mac (Apple silicon, macOS 27), `ssh $DV_MAC_HOST` (key
  auth, `BatchMode`), its checkout `~/code/dv`, zsh. Its screen is the owner's test desktop:
  dv's windows may show there. Screen Recording is granted to SSH sessions, so `screencapture
  -x` and the automation's captures work over SSH.
- **Build there, never cross-build here**: no cargo for macOS on Windows. `script/check-mac`
  syncs this working tree (a bundle for a commit the Mac lacks, never through GitHub; the
  Mac's own uncommitted changes and untracked files are discarded, `target/` stays) and runs
  `script/check` there; `--sync-only` stops after the sync, for running one thing by hand:
  `ssh $DV_MAC_HOST 'cd ~/code/dv && zsh -lc "cargo test -p dv_ui"'`. Copy screenshots back with
  `scp` into `target/tmp/mac/` and look at them (Retina: physical pixels at 2x).
- **Its rules**: install nothing on it but the pinned toolchain through rustup (no brew, no
  .NET, no Xcode components such as the Metal toolchain: the build uses gpui's runtime
  shaders), never `sudo`, never touch its real data dir (`~/Library/Application Support/dv`) or
  its `~/.claude`, `~/.agents`, `~/.codex`, `~/.cursor`. An exploratory run sets `DV_DATA_DIR`
  (or `--data-dir`) under `target/tmp/`, quits through automation or is killed by its pid, never
  by name. The display sleeps after a minute: the e2e runner keeps it awake while it runs
  (`caffeinate`); for a `screencapture` by hand, `caffeinate -u -t 2` first.

## Disk

D: holds the checkout and every target dir. Check `df -h /d` before anything big (a fresh build,
copying a target dir) and stop if D: would drop below 30 GB free. `target/debug/incremental` is the
first thing to delete when space runs low. Never `cargo clean` without the owner's say-so (gpui
takes many minutes to rebuild).
