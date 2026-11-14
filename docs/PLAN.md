# dv: rewrite plan

dv is a native desktop app (Rust + GPUI) for reviewing diffs locally — above all an AI coding agent's
work before it is committed — and for reviewing and submitting large GitHub PRs that github.com handles
badly. Windows first with first-class WSL; macOS and Linux supported. Coding agents read and answer the
review through the `dv` CLI.

- **Visual spec:** [DESIGN.md](DESIGN.md), plus the design canvas (row 2, "Glass", is the chosen
  direction): (a private canvas)
- **Status:** plan, written 2026-10-04 after a full review of the first implementation (§15). Nothing
  here is built yet. Sections are cited from code and commits as `PLAN §N` / `D<n>`.

Comment and doc rule for the rewrite: code comments explain *what something is and why*, in the present
tense, citing `PLAN §N` / `DESIGN §N` where a spec decision is involved. History (review findings, phase
labels, "used to…") belongs in commit messages, never in comments.

---

## 0. Goals and non-goals

**Goals (v1)**
1. Review an agent's uncommitted or branch work in a local or WSL repo: fast diff, viewed tracking,
   line/range comments, threads, rounds with "changes since last round" (§2, §3).
2. A CLI that lets agents in Windows *or* inside WSL list, answer and address review threads and wait
   for the next round (§10).
3. Open a GitHub PR as a review, comment on it, pull GitHub's own threads, and submit one GitHub review
   with a verdict (§9).
4. WSL repos behave exactly like local ones: native speed, live updates while the agent edits (§5).
5. First-run onboarding and a re-runnable setup/doctor that install the WSL backend, put `dv` on PATH,
   install agent skills and check `gh` — per environment (§12).
6. A per-user installer with delta auto-updates (Velopack); Linux builds; macOS verified on the
   owner's Mac (§13).
7. Hermetic automated testing: fixture repos, a fake `gh`, GUI e2e scenarios (§14).
8. Read the code around a change as an editor would: select and find text, view whole files, and
   hover, go to definition and find references through the project's own language servers, run
   where the repo lives (§18).
9. An agent explains its change as it hands it over: annotations beside the code, so round 1
   isn't spent asking why (§19).

**Non-goals (v1):** AI features inside dv, editing files (and so a language server's diagnostics,
completion, rename and code actions, §18.1), committing or any other mutation of the user's repo
(§2.4), cross-machine review sync, GitHub Enterprise-specific work
beyond what `gh` gives for free, merge-conflict badges.

---

## 1. Decisions

### D1. UI stack: `gpui-pre` + `gpui-component` 0.9 (gpui-kit), exact pins (same as the sibling apps)
`gpui = { package = "gpui-pre", version = "=0.3.11" }` (+ `gpui-pre-platform`), `gpui-component =0.9.0`
and `gpui-base =0.9.0` (gpui-kit's crates, each pinned on its own rather than through the
`gpui-kit` umbrella),
declared once in `[workspace.dependencies]`, bumped together. The first implementation sat on zed git
main + `gpui-component 0.5.2`, a line that is no longer maintained, and every `cargo update` was a
big-bang risk. Same stack as the sibling apps means their shared automation crates (toolbox-rs's
`gpui_automation`, §14.2) and their lessons carry over directly; toolbox-rs owns this pin, so a
gpui bump starts there. Grep APIs under `~/.cargo/registry/src/*/gpui-pre-0.3.11/` and `gpui-component-0.9.0/`.

### D2. Rewrite in place, salvage deliberately
Same repository, fresh crates (§4), with the first implementation kept read-only under `legacy/`.
Port code from it only through the salvage list (§15) — read it, keep what is listed, rewrite the
rest. `legacy/` is deleted when v1 reaches parity on workflows A–C (§3).

### D3. The review is the unit you open
A review owns exactly one fixed target. There is no "open a repo and guess which review" step.
Full model in §2.

### D4. git computes the diff
Per file, dv runs `git diff --no-ext-diff --no-textconv --diff-algorithm=myers --indent-heuristic
--submodule=short --ignore-submodules=untracked --no-color --src-prefix=a/ --dst-prefix=b/ -U<N> -M
<old> <new> -- :(literal)<path>` (an untracked file: `git diff --no-index … -- /dev/null <path>`,
where exit code 1 means "differs", and a file named `-`, which would mean stdin there, is passed as
`./-`) and parses the result. `N` exceeds the file's line count — a file of `n` bytes has at most
`n + 1` lines, so `N` comes from the size check (§6 step 2) — and with context larger than the file
the output *is* both complete, aligned files, so dv collapses unchanged runs into expandable gaps
itself. This gives git's semantics for free: `core.autocrlf`, `.gitattributes` (eol, `-diff`,
`binary`, clean filters) and rename detection. `--no-textconv`, because comment anchors and GitHub's
line numbers refer to the real file's lines, not converted text; `--diff-algorithm=myers` overrides
a user's `diff.algorithm` and a driver's `diff.<driver>.algorithm` (which beats the git policy's
`-c diff.algorithm=myers`, §5.1), so dv shows what GitHub shows; `--indent-heuristic` (git's
default) overrides `diff.indentHeuristic=false`, which would mark a different run of a block that
can slide than GitHub marks, and so move comment anchors off the lines GitHub shows as changed;
`--submodule=short` keeps a gitlink's patch the `Subproject commit` lines dv reads whatever
`diff.submodule` says, and `--ignore-submodules=untracked` (git's own default) keeps a
`diff.ignoreSubmodules` or `.gitmodules` `ignore` setting from hiding a submodule bump; the fixed
prefixes override `diff.noprefix` / `diff.mnemonicPrefix`. The file list (§6 step 1) carries the
same pinned flags, so its totals match the hunks. Binary detection still follows `.gitattributes`.
dv adds only intraline word ranges and syntax highlighting. (The first implementation diffed raw
bytes itself and showed every edited CRLF file as fully rewritten.)

### D5. Reviews are append-only event logs
`<git-common-dir>/dv/reviews/<id>.jsonl`, one JSON event per line, appended with a single `write` on a
file opened `O_APPEND`. Readers fold events into state. No lock file, no read-modify-write, so the GUI,
the CLI and any number of agents can write concurrently. Details in §7.

### D6. WSL: one Linux `dv`, mandatory backend
The Linux build of `dv` (static musl x86_64 + aarch64) is both the CLI inside the distro and the backend:
`dv backend --stdio`. The Windows GUI talks to it over the stdio of one `wsl.exe -d <distro> --exec`
child per distro. For a WSL repo this is the **only** path — no per-command `wsl.exe` fallback, no
optional accelerator. If it can't start, the repo shows an actionable banner (§12.3). The same `Backend`
interface is implemented in-process for local repos (§5), so there is one code path above it.

### D7. GitHub through `gh`, explicitly
All GitHub calls go through the `gh` CLI (auth, SSO, enterprise for free; no token storage), with
`GH_PROMPT_DISABLED=1`, and through a `DV_GH` override that tests point at `mock_gh` (§14). The GitHub
repository is resolved from `gh repo set-default`, else `upstream`, else `origin` — never hard-coded.
Every list is paginated.

### D8. Window material: Mica by default
Windows 11: `MicaBackdrop` by default, `Blurred` (Acrylic) as a setting, `Opaque` automatically on
Windows 10, when "transparency effects" is off, on Linux, and as a setting. **macOS: Opaque by
default, Vibrancy as a setting.** gpui's own `Blurred` there is an `NSVisualEffectView` with its
tint and saturation layers stripped: untinted, and with no blur at all up to gpui-pre 0.3.6 on
macOS 27. Vibrancy is toolbox-rs's `gpui_appearance` instead, which puts a plain
`NSVisualEffectView` of its own behind gpui's content: tinted for light or dark, flat while the
window is inactive, as any macOS window. Its surfaces take Acrylic's values (DESIGN §3) until
DESIGN tunes a set of its own; until then Opaque stays the default. "Reduce transparency" on
resolves it to Opaque, and a Mica or Acrylic setting made elsewhere is kept and resolves to Opaque
meanwhile (as Vibrancy does on Windows). GPUI only blurs what is *behind* the window; in-app
overlays are opaque tinted layers over a scrim (DESIGN §3). Acrylic uses the undocumented
composition API and can lag while dragging on some Windows builds — hence Mica as the default.
gpui sets Mica through `DWMWA_SYSTEMBACKDROP_TYPE`, which only build 22621+ has, so earlier
Windows 11 builds (22000–22620) resolve Mica to Opaque too; a material the platform can't show
always resolves to Opaque. `gpui_appearance` holds these rules, follows the OS's light/dark
setting and applies both to the window (`dv_ui::appearance` narrows the settings by the palette
first, DESIGN §1.3).

### D9. Packaging and updates: Velopack
- **Packages:** `vpk pack` produces a one-click per-user `Setup.exe` on Windows (no wizard, no
  admin, installs under `%LOCALAPPDATA%\dv\`), a `.pkg` on macOS and an AppImage on Linux, plus the
  full and delta update packages and a release feed, all from a build without the automation feature.
  `vpk` is a .NET tool (the .NET SDK 8 and `vpk` 1.2.161 are installed on the build machine);
  `script/package` makes a Windows release, and on a Mac the macOS one (§13.2).
- **In the app:** the `velopack` crate, pinned to the `vpk` version (`=1.2.161`; Velopack's crate
  and CLI go together), a Windows and macOS dependency of the `gui` feature (the default shape
  carries it; the headless and musl builds don't). `VelopackApp::build()…run()` is the first
  call in `main` of both binaries there (`crates/dv/src/installer.rs`; skipped for a command
  line that isn't all Unicode, which its `env::args` would panic on: the hooks' arguments are
  ASCII). Its hooks are Windows' alone (Velopack runs none on macOS, where the first-run screen's
  Add to PATH links `~/.local/bin/dv`, §12.2): `Update.exe` runs them on the main executable
  (`dv-gui.exe`) before anything else of dv, they exit without a window, and they call `dv
  setup path`'s functions for the installed copy's folder,
  Velopack's stable `current` (never a versioned one): after install the `--yes` logic, before
  uninstall the `--remove` logic, so PATH stays app-owned (§12) and the installer has no logic of
  its own. **An update leaves PATH alone**: the folder doesn't change, and a user who took it off
  with `dv setup path --remove` keeps it off. Only a packaged build that Velopack's locator finds
  installed acts on a hook; anything else does nothing. An agent never runs `Setup.exe`,
  `Update.exe` or a dv executable with `--veloapp-*` arguments; the hooks' logic is tested
  through the `Path` seam.
- **Applying an update ends every dv process running from the install:** Velopack's updater
  stops each process whose executable is under the install folder before it replaces the files —
  other dv windows, with whatever they hold unsaved (drafts live in memory), and an agent's `dv
  review wait` or `dv comment reply`. So neither binary applies a downloaded update by itself at
  startup (auto-apply is off in both); an update applies only from **Restart to update**, which
  first counts the other dv processes whose image is under the install's `current` (on macOS
  the bundle's `Contents/MacOS`; a Toolhelp snapshot and `QueryFullProcessImageNameW` on
  Windows, `proc_listallpids` and `proc_pidpath` on macOS; this process left out) and, when there
  are any or this window holds an unsent comment or the open Finish round sheet, asks: what
  will close ("2 other dv windows or commands running from this install", "your unsent comment
  in this window"), **Restart anyway** and **Not now** (DESIGN §4.1). Nothing is dropped
  without that answer.
- **Updates** (`crates/dv/src/updates.rs`): `UpdateManager` over the release feed checks a few
  seconds after the app starts and every 4 hours, downloads the newer release (its delta when
  there is one) in the background, and the status bar then shows "dv <version> is ready" with
  **Restart to update**, which (after the question above, when it asks) hands it to
  `Update.exe` — it waits for dv to quit, applies it and starts dv again on what this window
  had open, its repository and review named absolutely (`open <path> --review <id>`), since the
  restarted app starts in the install's folder — and quits. A restart that fails says so in the
  status bar ("Update failed · details", why in its tooltip) with **Retry**. Never under
  `--automation` (scenarios make one ready through the seam, `update_ready`, `state.update`), and
  never in a build that isn't installed: `UpdateManager::new` fails there and updates are off,
  silently. **The feed** is the one baked into the build (`DV_UPDATE_FEED`), read through
  Velopack's `AutoSource`: a release from the release workflow (§13.4) names the repository,
  `https://github.com/kylekz/diff`, which Velopack reads as GitHub Releases (its
  `GithubSource`: the published releases, never drafts or prereleases, each one's feed for the
  install's channel merged, `releases.win.json` on Windows and `releases.osx.json` on macOS, so
  a delta in any of them is found); on macOS the updater is the bundle's `UpdateMac`, which
  replaces the bundle in place (one in `/Applications` may ask for an administrator's password
  through AppleScript; `~/Applications` doesn't). A local release names a
  folder of `vpk pack` output, the one `script/package --publish` writes, the packaging
  checkout's `target\package\releases`, so a release the owner publishes locally is picked up
  by a locally packaged copy, while a trial run (the default, `target\package\try`) reaches
  nobody. `settings.json`'s `"update": {"feed": "<folder or URL>"}` names another (empty turns
  updates off): a locally packaged install moves to GitHub Releases with
  `"feed": "https://github.com/kylekz/diff"`, since every released version is newer than every
  `-local.` one.
- Chosen over cargo-packager (the sibling app's tool; a classic NSIS wizard and an updater that re-downloads the
  whole installer and needs manifest signing keys) for delta updates and the simpler install flow.

### D10. Code signing: none (yet)
The executables, `Setup.exe` and the macOS `.pkg` carry no certificate's signature (dv runs on
the owner's own machines). A locally built copy isn't challenged (SmartScreen and Gatekeeper check files
carrying a downloaded-from-the-internet mark); a release downloaded from GitHub Releases is.
On Windows SmartScreen warns ("Windows protected your PC", **More info** › **Run anyway**)
until the file has a reputation. On macOS Gatekeeper refuses the downloaded `.pkg` once
(System Settings › Privacy & Security › **Open Anyway**); the app it installs carries no
quarantine mark, and neither does an update Velopack downloads, so neither is asked about
again. The macOS app is sealed with an ad-hoc signature (`vpk pack --signAppIdentity -`,
codesign's identity-less one: the bundle verifies, `codesign --verify --deep --strict`, but
names no developer), so privacy grants (access to Documents, Desktop or Downloads) are tied to
each build and macOS may ask again after an update; repositories elsewhere (`~/code`) never ask. What stands in for a signature is the
release workflow's build provenance (§13.4): every released file is attested through Sigstore
as built by `release.yml` from the tagged commit (`gh attestation verify <file> --repo
kylekz/diff`). Revisit when dv has users beyond the owner: a certificate on Windows (Azure
Artifact Signing, or SignPath's free signing for open-source projects), an Apple Developer ID
with notarization on macOS. Signing changes no code: `vpk pack` takes the identities
(`--signAppIdentity`, `--signInstallIdentity`, `--notaryProfile`, `--keychain`), the
certificates and the notarization key become secrets of the `release` environment, and the
signing moves behind the owner's approval (a job in that environment), since the builds run
before it; an install without a certificate's signature updates to a signed release like any
other.

### D11. Code intelligence through the project's own language servers
Hover, go to definition and find references come from standard language servers (LSP) that dv starts
where the repo lives, through its backend (a WSL repo's run inside the distro), found in the project
first (TypeScript 7's native server ships in the `typescript` package) and never installed by dv
(§18). Each language is a spec in `dv_lsp`, so adding one doesn't touch the rest. Chosen over
tree-sitter navigation (quick, but by name only: no types, wrong across files) and over an embedded
editor (dv reads, it never edits).

---

## 2. Concept model

### 2.1 Entities

```
Repo          a clone at a Location: Local(<path>) | Wsl{distro, path}
 └─ Review    the unit of work; id "r-<ulid>"
     ├─ Target          fixed at creation, never changes (§2.2)
     ├─ Rounds          1..n snapshots of the target (§2.3)
     ├─ Viewed          per-round set of paths the reviewer marked viewed
     ├─ Threads         anchored comment + replies + state (§2.5); an agent's annotations (§19)
     └─ Status          §2.4
```

### 2.2 Targets

| Target | Old side | New side | Moves? |
|---|---|---|---|
| `working` | `HEAD` (the empty tree on an unborn branch) | working tree incl. untracked (not ignored) | yes, live |
| `staged` | `HEAD` (the empty tree on an unborn branch) | index | yes, live |
| `branch <b> [vs <base>]` | `merge-base(<base>, <b>)` | `<b>`'s tip, or the working tree of the worktree that has `<b>` checked out | yes |
| `commit <rev>` | `<rev>`'s first parent (the empty tree for a root commit) | `<rev>` | no |
| `range <a>..<b>` / `<a>...<b>` | `<a>` / merge-base | `<b>` | no |
| `pr <owner/repo#n>` | merge-base(base, head) | PR head | on push |

`<base>` defaults to the repo's default branch: what `origin/HEAD` points at (`origin/main`), else a
local `main`, else `master`. `<b>` is the ref its name stands for however it is spelled (`feat`,
`heads/feat`, `refs/heads/feat`; a local branch before a tag of the same name, as `git switch`
picks), and that one ref gives both the tip and the checked-out test. Unrelated histories (no merge
base) are an error, not an empty diff.
**Branches and stack bases** (`dv_core::git::branches`): the switcher's branch picker (DESIGN §4.1)
offers every local branch against its *stack base*, the nearest branch tip among its first-parent
ancestors outside the default branch's history (`main → feat-a → feat-b`: `feat-b`'s is `feat-a`),
else against the default branch, and lets the reviewer pick any other base. One `git log --branches
--simplify-by-decoration ^<default>` finds every branch's: it keeps only commits a branch points at,
outside the default branch, and rewrites each one's parents to its nearest such commit, so a branch's
first rewritten parent is its stack base's tip; a parent left out means the default branch, and a
branch merged into the default one has none. It and `git for-each-ref refs/heads/` run with every
listing of a repo's row (§11), two commands whatever the number of branches. A suggestion only: a
lower branch rewritten without restacking the ones above it leaves them stacked on its old commits,
which the walk can't name, so they are offered against the default branch.
`dv_core::target::Target::resolve` turns a target into a `DiffSpec` of two sides — a commit id, the
empty tree, the index or the working tree. A target stores its spec *and* the resolved oids at each
round, so a review never silently changes meaning (the first implementation froze `base` but left
`head` symbolic).

**A review's home worktree** (`dv_core::review::home`) is where its target resolves, whichever
worktree reads it: a `working` or `staged` review's is the worktree it was made in (its
`created` event's `worktree`, §7); a `branch` review's is the worktree that has `<b>` checked out:
the worktree asking, when it has; else the one the review was made in, when it has; else one
`git worktree list --porcelain` lists with it (git checks a branch out in one worktree only
unless told otherwise, `git worktree add --force`; the bare repository, worktrees whose
directory is gone, and the git dir git lists as the main worktree of a repository whose git dir
is kept apart from its files — `git init --separate-git-dir`, a submodule — are left out). A
branch checked out nowhere has none: its tip, the same from everywhere. `commit`, `range` and `pr` targets have none either.
The GUI (`Reviews::open`: every open, by id or by target, from the switcher, the source picker,
`dv open --review`, automation's `open_review`) and the CLI (`Repo::for_review`) both bind to it,
so a review lists the same files, places its threads at the same lines and snapshots the same
content from every worktree. A `working` or `staged` review whose worktree is no longer one of
the repository's (removed, moved, or deleted and waiting for `git worktree prune`) has no home
(`HomeError::Gone`): the GUI's main-area banner, the CLI's exit 1 (`target`), never a resolution
in some other worktree. In the GUI the home is decided as the review opens; a branch checked out
in another worktree after that shows once the review is opened again.

### 2.3 Rounds
A round is the target as it stood when the reviewer last handed the review over.

- **Local targets:** when the reviewer finishes a round ("Request changes", "Approve", or explicitly
  "Snapshot round"), dv snapshots the new side: a temporary index (`GIT_INDEX_FILE`) + `git add -A` +
  `write-tree` + `commit-tree`, stored at `refs/dv/reviews/<id>/<n>`. Round n+1 defaults to the interdiff
  *round n → now*, with a toggle to the full diff. The interdiff is `DiffSpec::since(&round)`: the
  round's snapshot (`Side::Commit(round.new_oid)`) as the old side, the target's new side as it
  resolves now (§6 step 6). `DiffSpec::since_checked` first makes sure the snapshot commit is still
  in the repository (`git cat-file -e <oid>^{commit}`); a missing one (a ref deleted by hand and
  pruned) is `TargetError::MissingSnapshot`, for the GUI to show as a banner while it falls back
  to all changes. A fixed target (`commit`, `range`) has an empty interdiff.
- **The comparison in the GUI** (`dv_ui::session::Comparison`: since round n, or all changes)
  is session state, never persisted. A review opens on *since its last closed round* when it
  has one, isn't finished and its target is live (`working`, `staged`, a branch), otherwise on
  all changes; a fixed target never offers the toggle, and a draft PR review offers "Since your
  last review" (and opens on it) when the PR has an earlier submitted review (§9.5; DESIGN
  §4.1). A round
  that closes moves it to *since* that round, a review that finishes to all changes. Switching
  (`I`, the panel's toggle, an "all changes" row of the comments list, "Show all changes")
  re-resolves the spec and lists again under a new generation (§11), keeping the reading
  position where the same file and line still show. Viewed marks stay per round: an
  interdiff's files are marked in the round in progress. The CLI always works on all changes
  (§10.1).
- **Snapshot detail** (`dv_core::review::snapshot`): a commit side is kept as it is. For the index
  and the working tree, the worktree's index file is copied byte for byte, modification time
  included (git's racy-entry check compares against it), into the store (`tmp/index-<ulid>`,
  `Backend::copy_index_to_store`; no index yet starts an empty one). The working tree then runs
  `git add -A --ignore-errors` into the copy (tracked + untracked, ignores honoured, the same clean
  filters and eol conversion `git diff` applies; unresolved conflicts stage as the files stand,
  markers included; a nested repo without a commit, which git can't stage, is left out, but any
  other file git can't read — one another program holds open without sharing — fails the round
  with `SnapshotError::Unreadable` naming it, because the copy would keep a version the reviewer
  wasn't shown),
  and both run `write-tree` on it (an index with unresolved conflicts has no tree:
  `SnapshotError::Unmerged`). Both calls pass `-c core.splitIndex=false` (a split index would
  otherwise write a new shared index into the git dir) and `-c core.safecrlf=false`.
  `commit-tree --no-gpg-sign` with `-c user.name=dv -c user.email=dv@dv.invalid` (works without a
  configured identity; `GIT_AUTHOR_*`/`GIT_COMMITTER_*` variables still win), the old side's
  commit as parent when it is one, message `dv review <id> round <n>`. The ref is created with
  `git update-ref <ref> <new> <zero-oid>`, never blindly overwritten: a ref at the same commit, or
  at a commit of the same tree and parent (an earlier attempt whose events never landed), is
  reused; any other commit at the ref of the round in progress (such an attempt, made before the
  content changed, or before a branch moved on from the commit the last round closed at) is
  replaced with a compare-and-swap `git update-ref <ref> <new> <old>`; the ref of a closed round
  is never replaced (`SnapshotError::RefExists`). The temp index and its `.lock` are removed
  afterwards, also on failure, and each snapshot first sweeps `tmp/index-*` older than an hour
  (by the ULID's time), which a dv killed mid-snapshot leaves. `review::ops::finish_round`
  (reviewer only; not for a finished or a PR review; the
  verdict is `approve`, `request_changes` or none) resolves the target, snapshots, then appends
  `round_closed` (old side, snapshot, verdict, summary) and the verdict's status
  (`changes_requested` / `approved`; none leaves it), both checked before either is written.
- **PR targets:** a PR review has one dv round only (no `round_closed`); the PR's rounds are its
  reviews (`pr_round`, §9.5). A draft shows the PR's head as a refresh last pinned it, and
  submitting closes the review at that head and its merge base (`submitted`'s `head_oid` and
  `merge_base_oid`). New commits on the PR offer "Start round n+1": a new draft, whose
  comparison "Since your last review" diffs the last submitted review's head against the new one.

### 2.4 Status and invariants
- **Local review:** `open` → `changes_requested` ⇄ `in_progress` (agent ran `dv review done` →
  `ready`) → `approved` | `closed`. Never "submitted". Any of `open`, `changes_requested`,
  `in_progress` and `ready` may move to any other of `changes_requested`, `in_progress`, `ready`,
  `approved` and `closed`; agents set `in_progress` and `ready`, the reviewer `changes_requested`,
  `approved` and `closed`. Nothing returns to `open`. An agent's first write on a
  `changes_requested` review (a reply, an address or a new comment through the CLI) moves it to
  `in_progress`: one more `status` event after the write, best effort, written only when the
  review is still `changes_requested` as it stands then (`ops::start_work`) and conditional on it
  in the log itself (`if_status: changes_requested`, §7), so a status set meanwhile — the
  reviewer's verdict, the agent's own `dv review done` run beside the write, even one landing
  between that check and the append — wins; the switcher's "Agent working" chip then means the
  agent has started.
- **An agent never hands back a round it hasn't seen** (`ops::hand_back`, `dv review done`).
  `done --round <n>` names the round the agent worked on (`round.n` from the `dv review wait`
  that gave it the round, or the last of the review's rounds when it started without waiting),
  which must be the last closed one: an older one is `OpError::UnseenRound` (the CLI's exit 1
  `unseen_round`, nothing written) for the newest, one that hasn't closed exit 2. Without
  `--round`, the fold's derived `Round::closed_from` (the status the review had when the round
  closed; no event of its own) decides: a round the reviewer finished while the review was
  `in_progress` (`Review::unseen_round`) came while the agent was still working on the one
  before, whose `done` would carry a note about that round and whose next `review wait` would
  block without ever hearing of the new one, so it is `unseen_round` until the agent names the
  round. The agent's writes since don't count as having seen it: nothing in the log tells a
  reply about the new round from one still going down the old round's list. A round closed from
  `open` (the first) or from `ready` (after a handback, a summary-only round fixed in code
  alone) came while the agent waited for it and needs no number, nor does a second `done` once
  the review is `ready`. The `status` event `done` writes is conditional on the closed rounds
  it was checked against (`if_round: n`, §7), so a round that closes between the check and the
  append wins: the fold makes the late `ready` a no-op, and `hand_back` reads the log back after
  appending and reports `finished` when that round finished the review, else `unseen_round`.
- **PR review:** `draft` → `submitted` (read-only, owned by GitHub from then on: every later event is
  refused). The next round is a new draft review linked to the same PR. A draft may be abandoned
  (`closed`). A PR review is the reviewer's alone: an agent reads it but never writes to it
  (§9.10). While its pending review on GitHub holds posted comments (§9.8) those threads are
  frozen until it is submitted or discarded.
- **Closing** is the reviewer's, from either side: `dv review close <id>`, or **Close review**
  on the review's row in the switcher (DESIGN §4.1; `Reviews::close_review`, the same
  `ops::set_status`), whichever review it is, the open one included, which follows its log as
  it does any write from outside. The switcher's key only asks first, since `closed` is final.
- `approved`, `closed` and `submitted` are final; every other status counts as open. A finished
  local review is read-only for everyone, the CLI included: `review::ops` refuses a new thread,
  a reply, a thread state change, an edit, a deletion or a viewed mark on an `approved` or
  `closed` review (`OpError::Finished`, the CLI's exit 1 `finished`), as the GUI shows it
  (DESIGN §4.4); a submitted PR review refuses every later event in the fold itself. A finished
  local review moving to another status (`ops::set_status`: an agent's `ready` racing the
  approval, a `closed` after it) and a round finished on one are `OpError::Finished` too.
- **A verdict's status always lands after its round.** `ops::finish_round` writes the verdict's
  `status` event even when the review already had that status as it read it: a status set while
  the round's snapshot is taken (the agent's `dv review done`) would otherwise stand after
  `round_closed` and outlive the verdict. With nothing between, the fold reads the repeat as a
  no-op.
- **At most one open review per target.** Opening a target resumes it; "New review" on a target that has
  one asks "Resume or start fresh?". `working` and `staged` targets are per worktree (each worktree has
  its own changes); the others are the same target from every worktree, and a `pr` target is the same
  PR whatever head it was fetched at (`dv_core::review::ops::open_review`). Wherever a review was
  opened, it is read in its home worktree (§2.2).
- A repo has many reviews; a PR has many reviews over time; local reviews need no PR.
- **dv never mutates the user's repo.** Allowed writes: `git fetch`, `.git/dv/**`, `refs/dv/**` and the
  objects those refs need. No commit, checkout, stash, reset or index change, ever.

### 2.5 Threads
- **Anchor:** round, path (and old path for renames), side (old/new), line range, **plus the anchored
  lines' text and 3 lines of context either side** (§8). A comment on the whole file (GitHub's
  file comment) has a **file-level anchor**: lines `0..=0`, no text, no context
  (`Anchor::is_file_level`), on the new side, or the old side for a deleted file.
- **States:** `open` (waiting on the agent/author) → `addressed` (agent says done, with an optional note)
  → `resolved` (reviewer confirmed) or back to `open` ("Reopen"). Only the reviewer resolves and
  reopens (`ops::set_thread_state` refuses an agent's resolve or reopen as `NotAllowed`, whatever
  the thread's state, saying it is the reviewer's). Agents (and the reviewer) address; addressing
  again replaces the note; a resolved thread is reopened before it can be addressed. Replies by anyone. Only an item's author edits or deletes it
  (a deletion is a tombstone).
- **Authors:** `reviewer` (git identity), `agent` (`--author`, default from the skill: `claude-code`,
  `codex`, …), `github` (GitHub's threads on a PR: shown from the PR's mirror, never written to a
  review's log, read-only; their state is GitHub's, §9.6).
- **An agent's model:** an agent says which model it runs on, in the model's own words
  (`--model 'Claude Fable 5.2'`, else `$DV_MODEL`, §10.1), and every event it writes carries
  it beside its name (`author.model`, optional, §7). It is what the app shows for the agent,
  with the model family's mark (DESIGN §1.6, §4.1), and no part of who the agent is
  (`Author::is`: kind and name): an agent that changes models still edits and deletes what it
  wrote. dv stores the name as given and reads the family from it only to draw
  (`dv_ui::agents`), so a family it learns later shows for old reviews too.
- **Annotations** (§19): an agent's explanation of its own change, anchored as a comment is,
  in state `annotation`: waiting on no one, counted apart from the threads. The reviewer's reply
  turns one into an open thread, the agent's question as §10.5 treats it.

### 2.6 Promotion (later, not v1)
When a branch reviewed as `branch feat vs main` becomes PR #n, dv offers to carry open threads into the
PR's draft review, re-anchored against the PR diff.

---

## 3. Workflows

### A. Agent's local work (the core loop)
1. The agent works in `~/code/api-server` (WSL). dv's switcher shows that repo with "uncommitted
   changes · 7 files".
2. The reviewer picks **Review** → a `working` review opens (or resumes). When the user asked the
   agent to hand its work over, the agent opened that review first, annotated its change and
   marked it ready (§19), so the reviewer starts with its reasons beside the code.
3. They read, mark files viewed (`V`), comment (select lines, `C`), then **Request changes**
   (`Ctrl Enter`). Round 1 is snapshotted; status `changes_requested`.
4. The agent (blocked in `dv review wait`) wakes, runs `dv comment list --status open`, fixes,
   `dv comment reply` / `dv comment address <id> --note …`, then `dv review done`.
5. dv shows round 2 as *round 1 → now*, addressed threads first with **Resolve** / **Reopen**.
6. **Approve** closes the review. The reviewer (or the agent, told by the skill) commits.

### B. Your branch before a PR
Workflow A with target `branch feat vs main` (includes uncommitted changes while `feat` is checked out).
Finish with `gh pr create`; promotion (§2.6) later.

### C. A GitHub PR
1. `dv open #482` / a pasted URL / `Ctrl G`. dv fetches base and head, resumes or creates the PR's draft.
2. GitHub's existing threads appear read-only alongside the reviewer's drafts (paginated, §9.2).
3. **Finish review** (`Ctrl Enter`) → summary, verdict, the list of comments to post. Each is validated
   against GitHub's hunks; anything outside them offers *Post as file comment* or *Keep local* (§9.3).
4. Submit posts one GitHub review; the draft becomes read-only, round 1 closes.
5. The author pushes; dv marks the PR updated and offers *Start round 2*.

### D. Agent-launched review (later)
An optional Claude Code `Stop` hook (installed by `dv setup agents`) runs `dv open --review-current`,
bringing dv to the front on the agent's working-tree review when the agent finishes a turn.

---

## 4. Workspace layout

| Crate | Kind | Role | gpui? |
|---|---|---|---|
| `dv_core` | lib | Location, the `Backend` trait and `LocalBackend` (§5.1), Target and its labels, git runner, diff pipeline (§6), review events + fold (§7), anchors, relocation and placement (§8), GitHub via `gh` (§9) | **never** |
| `dv_backend` | lib | The stdio protocol (§5), server loop, `Connection`s (launch, reconnect) and the `Connections` registry, `RemoteBackend` client, distro install | never |
| `dv_cli` | lib | clap command tree, JSON output DTOs, setup/doctor checks, a WSL repository's commands run inside its distro (§10, §12) | never |
| `dv_lsp` | lib | language servers where the repo lives: the LSP client, the language specs and their resolvers (§18.4) | never |
| `dv_ui` | lib | views + services (§11) | yes |
| `dv` | lib + bins `dv`, `dv-gui` | entry: GUI (the `gui` feature), CLI and `dv backend --stdio`; on Windows and macOS also the console/GUI split (`dv-gui` with `gui`) and Velopack's updates, on Windows its hooks (D9); three shapes, one of them headless (§13.1) | with `gui` |
| `dv_e2e` | bin `e2e` | the scenario runner: toolbox-rs's `gpui_automation_runner` with dv's profile and its `gh`/`lsp` fixture (§14.2) | no |
| `mock_gh` | bin | fake `gh` for tests (§14.2) | no |
| `mock_lsp` | bin | a scripted language server for tests (§18.7) | no |
| `dv_test_support` | lib (dev) | fixture repo builder, shared by every crate's tests | no |
| `dv_package` | lib + bin (dev) | `script/package`'s helper: the version of a local release (§13.2) | no |

Direction: `dv` → `dv_ui` → {`dv_cli`, `dv_backend`, `dv_lsp`, `dv_core`, `gpui-component`};
`dv_cli` → {`dv_backend`, `dv_lsp`, `dv_core`} (`dv_lsp` for the user's shell's `PATH`, §12.2); `dv_lsp` →
`dv_core`; `dv_backend` → `dv_core`. The automation crates come from toolbox-rs
(github.com/kylekz/toolbox-rs, the owner's MIT crates shared with the sibling apps), a git
dependency pinned by tag (§14.2), and so do `toolbox_process` (every program dv runs on this
machine, in a process tree of its own: `dv_core`, `dv_backend`, `dv_cli`; gpui-free, §18.3) and
`gpui_appearance` (light/dark and the window materials, D8: `dv_ui`). A gpui import outside
`dv_ui`, `dv` and toolbox-rs's `gpui_automation` and `gpui_appearance`, or in `dv` built without `gui`, is a build failure (`script/check` verifies
it with `cargo tree`, §13.2).

Rules: errors are `anyhow` in app/CLI code and `thiserror` enums in libraries; no `unwrap()`/`expect()`
outside tests unless the message states the invariant; files stay under ~1,500 lines (split by
responsibility, `mod.rs` / `render.rs` / `tests.rs` per view).

---

## 5. The backend

### 5.1 Interface
One trait, `dv_core::backend::Backend`, two implementations, so everything above it is
location-agnostic. The trait and `LocalBackend` live in `dv_core`, because the diff pipeline and the
review store there are written against it; `dv_backend` (which depends on `dv_core`) holds the stdio
protocol, the server loop, `RemoteBackend` and the distro install.

A backend is bound to one worktree, opened from a `Location`: git runs in its root, worktree paths
are relative to that root, store paths are relative to `<git-common-dir>/dv/` (§7). Every method
blocks and takes and returns plain data, so the stdio protocol serialises calls 1:1:

```rust
pub trait Backend: Send + Sync {
    fn repo(&self) -> &RepoInfo;     // location, toplevel, git dir, common dir, object format, git version
    fn git(&self, args: &[&str], opts: &GitOpts) -> Result<GitOutput, BackendError>; // stdin, timeout, ok codes
    fn worktree_read(&self, rel: &str) -> Result<Option<Vec<u8>>, BackendError>;
    fn worktree_len(&self, rel: &str) -> Result<Option<u64>, BackendError>;
    fn worktree_text_stats(&self, rel: &str) -> Result<Option<TextStats>, BackendError>; // size, lines, binary sniff (§6)
    fn probe_path(&self, path: &str, read: u64) -> Result<PathProbe, BackendError>; // an absolute path, read-only (§18.4)
    fn store_append(&self, rel: &str, line: &[u8]) -> Result<(), BackendError>;   // one write, O_APPEND (D5)
    fn store_read(&self, rel: &str) -> Result<Option<Vec<u8>>, BackendError>;
    fn store_list(&self, dir: &str) -> Result<Vec<String>, BackendError>;
    fn store_write_atomic(&self, rel: &str, bytes: &[u8]) -> Result<(), BackendError>; // temp + rename
    fn store_remove(&self, rel: &str) -> Result<bool, BackendError>;
    fn copy_index_to_store(&self, rel: &str) -> Result<bool, BackendError>; // §2.3 snapshots
    fn watch(&self, spec: WatchSpec, sink: WatchSink) -> Result<WatchHandle, BackendError>; // §5.3
    fn spawn_stdio(&self, spec: &ProcessSpec, sink: ProcessSink) -> Result<ProcessHandle, BackendError>; // §18.3
}
```

`GitOpts::index_file` names a store file (a store path, validated as one) that a single `git` call
uses as its index: the backend resolves it under `<git-common-dir>/dv/` and exports it as
`GIT_INDEX_FILE` after the policy has removed any inherited one. Round snapshots stage into a copy
of the index this way (§2.3), so the user's index is never written.

- **Paths** are validated before use: no absolute paths, no `..`, `.` or empty components, no
  backslashes (separators on Windows); store paths use only `[A-Za-z0-9._-]` components that don't
  start with `.`. Worktree reads see a file as git does: a symbolic link is never followed and
  reads as its target path, which is what git stores and diffs for a link.
- **Outside the worktree**, one read-only op: `probe_path` takes an absolute, normalised path on
  the backend's machine (no `.`, `..` or empty components; on Windows a drive or UNC path, no
  verbatim or device prefix, no `:` past the drive) and says what is there: `Missing`, `Dir`,
  `Link {target}` (never followed, the target as the OS reads it) or `File {len, contents}`, the
  bytes only when the file is at most `read` bytes, which is itself at most 4 MiB
  (`PROBE_READ_MAX`); `read` 0 only looks. The language servers' resolvers use it (§18.4): Node
  resolves packages above the worktree and through links that leave it, every probe only
  looking but the reads of a small file (`PROBE_READ_SMALL`, 1 MiB: a `package.json`, a shim,
  never an executable); so do the references list and the file view for a file outside the
  repository (up to 4 MiB, §18.1). Nothing in dv writes outside the worktree's own paths and
  `.git/dv` (§2.4).
- **Git invocation policy** (`dv_core::git::command`, the only place a git process is built):
  `LC_ALL=C`, `--no-pager` and `GIT_PAGER=cat`, `GIT_TERMINAL_PROMPT=0`, `GIT_OPTIONAL_LOCKS=0`,
  `-c core.quotepath=off -c color.ui=never -c color.diff=never -c diff.algorithm=myers
  -c diff.autoRefreshIndex=false`, the inherited variables that point git elsewhere or change how
  pathspecs and diffs are read (`GIT_DIR`, `GIT_INDEX_FILE`, `GIT_CONFIG_PARAMETERS`,
  `GIT_LITERAL_PATHSPECS`, `GIT_DIFF_OPTS`, `GIT_ATTR_SOURCE`, …) removed. It runs through
  toolbox-rs's `toolbox_process` (no console window on Windows, a process tree of its own), so a
  timeout kills git with everything it started (hooks, `ssh`, credential helpers), and what git
  leaves running when it exits is killed then. `color.diff` needs its own override because a configured `color.diff=always` beats
  `color.ui`. A driver's `diff.<driver>.algorithm` beats `-c diff.algorithm`, and only flags beat a
  `.gitmodules` `ignore` setting, so the diff commands also pin git's diff presentation with flags
  (D4). `diff.autoRefreshIndex=false` because the worktree forms of `git diff` rewrite the index
  after a stat refresh even under `GIT_OPTIONAL_LOCKS=0` (checked 2026-10, git 2.53), which would
  break §2.4 and race an agent's `git add` for `index.lock`. Errors carry the arguments, exit code
  and stderr; their `Display` is a one-line summary (DESIGN §4.4).
- The user's own git configuration otherwise applies (autocrlf, attributes, excludes). Tests are
  hermetic instead: `LocalBackendOptions::env` overlays the environment of every git the backend
  spawns, and tests pass `FixtureRepo::env_overlay` (no global or system config or attributes, fixed
  identity), so results don't depend on the developer's machine (§14.1).
- `LocalBackend` — in-process: spawns `git` directly, `notify` for watching (local repos get live
  updates too, which the first implementation lacked).
- `RemoteBackend` — JSON-lines RPC to `dv backend --stdio` in the distro; that server is a
  `LocalBackend` running natively on Linux. Binary payloads are base64. Requests carry ids;
  responses and watch events interleave on stdout.

**The wire** (`dv_backend::proto`, `PROTO = 2`): JSON, one object per line (`\n`), UTF-8, over the
server's stdin and stdout. A request is `{"id": <u64>, "op": "<name>", …args}`; a response
`{"id": n, "ok": <value>}` or `{"id": n, "err": <WireError>}`; a watch batch `{"watch": <u32>,
"event": <WatchEvent>}`; a process's output `{"proc": <u32>, "out": <base64>}`, `{"proc": n,
"err": <stderr line>}` and, last, `{"proc": n, "exit": <code or null>}` (§18.3). The server runs
each request on a thread of its own (a long `git` must not hold up a `store_read`) and answers in
the order calls finish; the client matches ids. Every
byte field (git's stdin, stdout and stderr, file contents, store lines and bytes) is base64
(standard alphabet) in dedicated wire types: `GitOpts` and `GitOutput`, whose bytes would
serialise as JSON number arrays, never cross as they are; `RepoInfo`, `WatchSpec`, `WatchEvent`,
`TextStats` and `ProcessSpec` do. The ops:
- `hello {proto, build}` → `{proto, build_ok, pid, os, arch, version}`: the client's first request.
  `build_ok` says whether the sha256 of the server's own executable is `build` (§5.2), `null` when
  none was sent; the executable is hashed only when asked.
- `open {path, env}` → `{repo, info}`: a `LocalBackend::open_with(path, {env})` kept under the
  handle `repo` (`env` is the overlay: the loopback tests send the fixture's, the app none);
  `close {repo}` forgets it.
- One op per `Backend` method, naming its `repo`: `git {args, opts}`, `worktree_read`,
  `worktree_len`, `worktree_text_stats`, `probe_path {repo, path, read}` (→ `{kind: missing | dir |
  link {target} | file {len, contents}}`, the contents base64), `store_append`, `store_read`, `store_list`,
  `store_write_atomic`, `store_remove`, `copy_index_to_store`; `watch {repo, spec}` →
  `{watch, worktree_error}`, whose batches the server holds back until that answer is out;
  `unwatch {watch}`.
- `spawn {repo, spec}` → `{proc, pid}`, whose output lines the server holds back the same way;
  `proc_write {proc, bytes}` (base64) → `null` once written, an `io` error once it has ended;
  `proc_kill {proc}` → `null`, its exit line following (§18.3).

`WireError` carries every `BackendError` variant with its fields (an `io::Error` as `{kind,
message}`), so the client rebuilds the same variant, whose `Display` and `detail()` read as they did
on the server; `protocol` answers a line that isn't a request (id 0 when none can be read), after
which the server serves on, and reaches the caller as `Unavailable`. One writer thread owns the
server's stdout and writes whole lines (logging goes to stderr). When stdin ends the server stops
its watches, kills its processes, writes out what it has queued and exits, whatever calls are still
in git: that is how a server dies with its client.

**The client** (`dv_backend::client`): a `Connection` (one per distro, or a named loopback) starts
its server through its `Launcher` (`dv_backend::launch`) on the first call and shakes hands; every
`RemoteBackend` of that server shares it, each call blocking its thread until its reply comes.
`Launcher::Command` runs a child process with piped stdio (no console window on Windows, where the
detached app has no console: `toolbox_process::Command::to_std`), such as a loopback `dv backend --stdio`, and may name the build hash
`hello` checks; `Launcher::InProcess` serves on a thread over in-memory pipes; `Launcher::Wsl`
starts a distro's server through `wsl.exe`, installing this app's Linux build there first when it is
missing (§5.2). A launcher also answers whether its host runs (`host_running`: a distro's state,
listed afresh; `None` for this machine), which every automatic start asks first (§5.2's connecting
rule). Every process a launcher starts, and every `wsl.exe` dv runs, joins a job object this process
holds with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (`dv_core::job`, over
`toolbox_process::tie_to_this_process`), so it dies with dv however dv ends. A reader thread routes replies by id and watch batches to their watch's channel, which
delivers to the sink on a thread of the watch's own, so the `WatchHandle` contract (§5.3) holds over
the wire. Before the first reply, lines that aren't the protocol are kept as diagnostics (`wsl.exe`
can print warnings on stdout first); after it, such a line, or a reply nobody asked for, is a
protocol failure. `repo().location` is `Location::Wsl { distro, path: <toplevel> }` through a distro
and the server's own path through a loopback. A server that dies (its output ends, a read or write
fails, a protocol failure, its process exits — watched on its own, since on Windows a git the server
started inherits its stdout and can keep it open) fails every call in flight at once with
`BackendError::Disconnected` ("lost the connection to dv in Ubuntu: …"); what follows is §5.2's
reconnect. A connection's `ConnectionState` (`Idle`, `Connecting`, `Installing { done, total }`,
`Connected`, `Reconnecting { attempt }`, `Stopped`, `Failed { error }`) is readable and observable
(`subscribe`, called in order on a thread of the connection's own, which the UI forwards to its
executor); `diagnostics()` gives the launcher's command line, the state history and the servers'
last stderr lines, and `kill()` kills the server as a crash would (a test seam). The app holds its
connections in a `Connections` registry, one per distro (names compared without case, as `wsl.exe`
takes them) and one per named loopback; the CLI never uses one (§10.4). Dropping the last handle on
a connection (every `RemoteBackend` and remote watch holds one) closes its server's stdin, which
ends the server, and reaps its process; so does a server with no live watch, no running process and
no call in flight for 60 s, counted from when it last became unused — its last call ended, its last
watch or its last process went (the **idle close**: the state is `Idle` again, the `RemoteBackend`s
stay valid, and the next call starts a server automatically), so a connection made for a listing
keeps no process in a distro and WSL can shut an unused one down; an open review holds a watch, so
its server stays (one whose watch couldn't start, an inotify limit in the distro say, holds none and
may idle out: the next call, its refresh on focus, starts the server again, and its status bar shows
the review's own state meanwhile, an idle connection being up). The conformance suite (§14.2) runs
the same cases against both implementations.

### 5.2 Lifecycle (WSL)
- **The connecting rule: only an explicit open boots a distro.** One connection per distro.
  A start is either *explicit* — the user opening a repo or a review in it (`dv open`, Open
  folder…, the switcher's **Start** and **Retry** below, all `Locations::open`) or the banner's
  Retry or Start, each `Connection::retry` — or *automatic*: the first call of a listing
  (`Locations::reach`, for the switcher's rows), the first call after an idle close (§5.1), and
  a respawn after a death (below). An explicit start connects whatever the state, booting a
  stopped distro and starting a failed or stopped connection once more. An automatic start
  first asks `wsl::running_now` — `wsl.exe --list` afresh, not the 2 s cache, since the death
  that prompts a respawn just happened — and when the distro isn't running launches nothing:
  the connection is `Stopped` (its calls fail with `Unavailable`, "Ubuntu isn't running") until
  an explicit start. So `wsl --shutdown` or `wsl -t Ubuntu` stays done, and nothing connects at
  launch for its own sake. A listing never starts a connection that failed or is reconnecting,
  and makes a new one only for a distro that is running (from `wsl::distros`); a `Stopped` one
  it starts again only once the distro runs again (the user started it). A listing asks a
  stopped or failed connection's distro from `wsl::running` — the 2 s cache, one `wsl.exe
  --list` shared by every listing asking at once — not afresh, so a stopped distro's rows don't
  each list WSL. An explicit start made while a reconnect is under way waits for that
  reconnect and takes its outcome; an open that fails only because its connection is down keeps
  its `RemoteBackend` for when it is back. **A picked review is never lost and never reported
  twice:** an open (`Reviews::open`: the connection, finding and loading the review, its home
  worktree) whose connection is lost under any step waits for the reconnect, starting nothing
  itself (`Connection::wait_for_reconnect`; a `retry` there would restart a failed connection,
  and a server that dies on every open would then loop), and runs once more. If it still fails
  for its connection (lost again, or the connection failed or found its distro stopped) the
  shell keeps it as the connection's **pending pick** (one at most; a newer pick or the
  Dismiss of its own banner drops it, another banner's Dismiss leaving it waiting): while the
  open review goes through the same connection the main area raises nothing, its backend
  banner and status bar saying what is wrong; otherwise the main area's banner reads "couldn't
  open <repo>" (the connection's error and diagnostics in Details) with Retry. When the
  connection comes back (or is back already when the open fails) the pick runs again by
  itself, its banner going, keeping open menus and the keyboard where they are. **Running by
  itself, it never replaces a review that holds unsaved text:** while the "Finish round" sheet
  is open or a comment composer (a new thread or a reply) holds a draft, the pick waits on, and
  runs once the sheet closes or the draft is posted or cancelled, if its connection is still up
  then (connected, or closed for idleness, which the next call starts again); one that went
  down again keeps it waiting. Its banner's Retry is the user's own pick and runs at once (an
  explicit pick replaces the open review as `Ctrl O` does, DESIGN §4.2). One whose open kills
  its connection every time runs again by itself at most three times, then fails in the banner
  (DESIGN §4.4). What a WSL repo's
  header says (DESIGN §4.1): a distro that isn't running, whatever its connection did before,
  "Ubuntu isn't running" and one row, **Start Ubuntu**, which connects (booting it) and then
  lists the repo as usual; a connection that failed, why, and one row, **Retry**; one that is
  reconnecting, "Ubuntu · reconnecting…", keeping the rows of its last listing. A loopback
  (§14.2) has no host to ask; the automation host's stands in for one (`backend_stop`). An
  automation run reaches WSL only through the sandbox seam: without `DV_WSL_HOME` naming a
  scratch home it gets a stand-in (`WslAccess::Scripted`) that lists the distros a scenario
  names (`wsl_distros`, running or not as it says, which its connections' host check reads) and
  refuses to start anything in one, and dv_ui's tests can't reach the real one at all
  (CLAUDE.md).
- **The Linux build** (`script/build-linux`, §13.2) ships beside `dv.exe` as `dv-linux-x86_64`
  and `dv-linux-aarch64` (`DV_LINUX_DIR` names another folder; `dv_backend::install`). Its
  **build hash** is the lowercase hex sha256 of that file, computed off the UI thread once per
  process per file (cached by path, length and modification time).
- **Launch** (`Launcher::Wsl`, `dv_backend::wsl`): `wsl.exe -d <distro> --cd / --exec /bin/sh -c
  <launch script> sh <x86_64 hash|-> <aarch64 hash|->`, with no shell on the Windows side and
  no login shell inside, so the arguments arrive as passed (checked against a real distro:
  spaces, `$`, both quotes, backslashes, newlines, an empty one). The script picks the hash for
  `uname -m` and, when `$HOME/.local/share/dv/bin/dv-<hash>` is there, prints `dv-exec <arch>`
  and execs it as `backend --stdio`; otherwise it prints one line and exits: `dv-missing <arch>`
  (120: the client installs it, then launches again), `dv-arch <machine>` (121: dv has no
  build for that architecture) or `dv-no-build <arch>` (122: this app has none; `Unavailable`
  naming the missing file and that `script/build-linux` makes it). The marker line tells a
  server that started from `wsl.exe` failing to start the distro (no such distro, WSL not
  installed): a launch whose output ends without one is `Unavailable` with `wsl.exe`'s own
  words (UTF-16LE, decoded). `--cd /` keeps `wsl.exe` from translating the app's working
  directory, which it warns about when it can't.
- Handshake: `hello` carries the hash of the build the launch picked; the server hashes its own
  executable. Mismatch → install it again once (the server's `arch` says which build) → if still
  mismatched, `Failed` ("the dv in Ubuntu doesn't match this app") and the banner. Another build
  is judged before the protocol: an older build speaks an older protocol too, and installing
  this app's build is what fixes both. A server of another protocol that is this build, or that
  was asked for none (a loopback), fails the connection ("dv in Ubuntu speaks protocol 1; this
  app speaks 2"). The launch runs a distro's build by its hash, so an older build already
  installed there is never the one started: the new hash is missing, and the launch installs
  it. A launch
  that can't succeed until someone acts (no build for the architecture, no such distro) fails
  the connection at once, a respawn too.
- **Install** (`dv_backend::install`): one `wsl.exe … /bin/sh -c <install script> sh <hash>`
  with the build streamed on its stdin, the connection `Installing { done, total }` meanwhile:
  into a temporary file beside `~/.local/share/dv/bin/dv-<hash>`, `chmod 755`, its sha256
  checked (a copy that isn't the build is exit 3, nothing installed), renamed into place; then
  `~/.local/bin/dv` becomes a symbolic link to it, a temporary link renamed over whatever file
  was there (the first implementation's regular file too), so the dv on the distro's `PATH` is
  an app's, under **the link rule**:
  **the link belongs to the installed copy.** A packaged build (`script/package`'s, §13.2:
  `DV_CHANNEL=release` at build time, `dv_backend::build::is_packaged`) also writes the marker
  `dv-<hash>.packaged` beside its build and always takes the link; a development build installs
  and runs its own `dv-<hash>` (the launch and the CLI's run use the build by its hash) but
  leaves the link alone while it points at a packaged build that is there (the script says
  `dv-kept`), and takes it otherwise. So a development build run beside the installed app never
  replaces the installed copy's `dv` in the distro, and the next release takes it from the last.
  The probe says `packaged` for a link to another packaged build: up to date for a development
  build, an install to do for a packaged one; `dv setup path` inside a distro leaves the same
  link alone for a development build, and the "dv in the distro" check (§12.2) says which
  build the link runs. The link is a convenience — the launch and the CLI's run use the build itself —
  so one that can't be made (a directory at `~/.local/bin/dv`, a `~/.local/bin` that isn't a
  folder dv can write) is a warning on stderr (kept in the connection's diagnostics, printed
  by the CLI), never a failed install. A script that fails before it reads all of the build
  says why itself; the pipe it closed is no part of the error.
- **Prune** after the first successful handshake per process and distro, in the background,
  best effort: every `~/.local/share/dv/bin/dv-*` but this app's builds and the one
  `~/.local/bin/dv` links to (another app's install may have pointed it there since; a running
  server keeps its deleted file), the `.packaged` marker of a build that is gone, and temporary
  copies an install left more than an hour ago. `~/.local/share/dv`'s other folders are never
  touched.
- **Sandbox** `DV_WSL_HOME` (a POSIX path): every launch, install, prune and check runs as `…
  --exec /usr/bin/env HOME=<it> /bin/sh -c …`, so tests and agents' runs install into a scratch
  folder under the distro's `/tmp`, never the user's own home. One check decides what it means,
  everywhere (`dv_backend::wsl::sandbox`: the launcher, the CLI, `dv setup wsl`, the automation
  guards): unset is the distro user's own home; set to a plain absolute POSIX path naming a
  folder other than `/` (no `..` part, no backslash, no line break, nothing around it) it is
  the scratch home, normalised (`.` parts and repeated slashes dropped); set to anything else —
  blank, whitespace, relative, `/` in any spelling (`//`, `/.`, `/./`), a path Git Bash turned
  into `C:/…`, a value that isn't Unicode — it is an error, never a fallback to the real home,
  and no sandbox an automation run may reach a real distro with.
- **Distros** (`wsl::distros`): `wsl.exe --list --quiet` and `wsl.exe --list --running --quiet`
  (UTF-16LE), never the localised `--verbose` table, reused for 2 s, everyone asking while a
  listing runs sharing it (`wsl::running_now` lists afresh); a `--running` that fails counts as
  none running. Listing boots no distro. Every
  `wsl.exe` dv starts is made without a console window (`CREATE_NO_WINDOW` through `toolbox_process`, one builder,
  `wsl::wsl_exe`), the CLI's crossing alone sharing its caller's console (§10.4). `dv setup wsl
  <distro>` does the install ahead of time (§10.1).
- Death (§5.1): in-flight requests fail with `BackendError::Disconnected`, the connection is
  `Reconnecting { attempt }` and the UI shows "reconnecting…"; the client respawns after 1 s, 2 s
  and 5 s, each an automatic start (the connecting rule above: a distro the user stopped ends the
  connection `Stopped` at once, launching nothing), calls failing at once with `Disconnected`
  meanwhile, also while an attempt installs the build again (its progress is still the state).
  When a server answers, each `RemoteBackend` opens its worktree on it again with its next call
  (a worktree's handle belongs to the server generation that issued it, and a worktree gone
  meanwhile is that call's error), every live watch subscribes again and its sink gets one
  synthetic `WatchEvent { rescan: true, .. }` for what changed while nobody watched. A watch the
  new server refuses (its worktree gone), or watches differently than its handle says (the store
  alone now, its `worktree_error`, or the worktree again), ends instead: its sink is dropped, and
  its consumer starts a new watch, whose handle says what it covers; one interrupted by another
  death waits for the next server. When the third attempt fails, or a server answers as another
  protocol or build, the connection is `Failed`: calls fail with `Unavailable` and nothing
  starts until `retry()` (the banner's Retry, an explicit start), which starts one server and
  blocks until it answers or fails. A server that dies within 10 s of a respawn continues that
  count instead of starting a new one, so one that dies on every start ends `Failed` rather
  than looping; any other start (the first, an explicit one, the first after an idle close)
  begins a fresh count. A server that can't start the first time fails the connection at
  once.

### 5.3 Watching
Worktree watch honours git's ignore rules — every `.gitignore` (nearest wins, nothing re-included
inside an ignored directory), `info/exclude`, the global excludes file — through the `ignore` crate's
matcher, loaded by a walk that skips ignored directories and re-read as `.gitignore` files change;
tracked files that match a pattern still count, as they do for git. A removed path can't be asked
whether it was a directory, so the watch remembers the ignored directories it has seen and drops the
removal of `target/` under a `target/` pattern too. A symbolic link is one path and is never
followed, by the OS watch or the watch's own listings, because git stores a link as its target path
and never descends into one (nor does a `foo/` pattern match a link named `foo`). The git dir is
dropped except `dv/` and the git state below; a linked worktree's `<git-common-dir>/info/` lies
outside its tree and is watched for `info/exclude` edits on its own. **Git state** (`WatchSpec { worktree, store, git }`):
with `git`, a batch sets `WatchEvent::git_changed` when this worktree's `HEAD` or `index`, or the
common dir's `refs/**` (except `refs/dv/**`, dv's own snapshots), `packed-refs` or `reftable/**`
change. A `working` target's old side moves when the agent commits without touching a worktree
file, and a `staged` target lives in the index. Only final names count: git writes each through a
`*.lock` renamed into place, so lock churn alone changes nothing. The directories holding them are
watched on their own where the worktree's recursive watch doesn't cover them (a linked worktree's
git dir and common dir, or a watch without the worktree); in a reftable repo a `refs/dv/` update
can't be told apart and fires too. Events coalesce for 150ms and arrive as *paths*
(`WatchEvent { worktree_paths, store_paths, git_changed, rescan }`: relative, `/`-separated,
de-duplicated), so the UI refreshes only the affected files and the file list; `rescan` means the
OS may have dropped events (or a batch would list more than 10,000 paths). A directory that appears
is listed before its batch goes out, because inotify watches a new directory only after reporting
it and a directory moved in brings its files without an event each. `<git-common-dir>/dv/` is watched separately when it lies outside the worktree (a
linked worktree's common dir). Dropping the handle stops the watch before `drop` returns. The OS
watch registers ignored directories too (inotify needs a watch per directory), so on Linux a huge
`node_modules/` can exhaust the per-user limit; a watch that includes the store then keeps
watching the store alone and its handle says why the worktree and the git state aren't watched,
so the UI falls back to refreshing those itself instead of also losing review updates.

---

## 6. Diff pipeline

1. **File list** (`dv_core::diff::file_list`): one `git diff --raw --numstat -z -M --no-abbrev
   --no-ext-diff --no-textconv --diff-algorithm=myers --indent-heuristic --submodule=short
   --ignore-submodules=untracked <old> <new>` call (D4's pinned flags) gives every file's status,
   modes and object ids and its +/− up front (the first implementation only counted files already
   opened), from one look at the working tree. Rename detection only: `-M` also overrides a
   configured `diff.renames=copies`, so a copy lists as added, as on GitHub. A working tree new side
   adds the untracked files (`git ls-files --others --exclude-standard -z`): + is the file's line
   count, read where the file lives (`Backend::worktree_text_stats`, streamed), binary by the `diff`
   attribute (`git check-attr`; `-diff`/`binary` → binary, `diff` → text, a driver name → that
   driver's `diff.<driver>.binary` when set, as git decides) or else git's heuristic (a NUL in the
   first 8000 bytes); a file git converts before it counts or sniffs is counted by git
   (`git diff --no-index --numstat`), so it agrees with the same file staged: one whose `filter`
   driver has a `clean` or `process` command (a git-lfs asset is a three-line text pointer to git,
   whatever its bytes) or one with a `working-tree-encoding` (UTF-16, say); a filter driver without
   a command converts nothing, as in git; an untracked symbolic link is one line, its target path; a
   nested repository (`dir/`) lists as a submodule with one addition, as `--numstat` counts the
   gitlink it becomes once staged or snapshotted (§2.3). A worktree-side entry that `--raw` lists as
   modified with an unhashed new side but `--numstat` doesn't list is a stat-dirty file whose
   content is unchanged (the git policy's `diff.autoRefreshIndex=false`, §5.1) and is dropped. An
   untracked file the old side holds too (a round snapshot's, step 6; or one `git rm --cached`
   left on disk) is part of the working tree side like any untracked file, but git's diff sees the
   working tree only through the index: it lists such a file as deleted (or as the source of a
   rename to a similar file added since) while `ls-files` lists it as new. When the list finds
   any, it lists again through a temp copy of the index (`<git-common-dir>/dv/tmp/`) in which
   `git update-index --index-info` registers each at its old-side mode and blob, writing no
   object: git then compares them with the files on disk, and each lists once, as modified when it
   changed, marked `FileEntry::new_untracked` so `file_diff` builds the same temp index for it.
   The list is sorted by path.
2. **Per file, on demand** (`dv_core::diff::file_diff`): cheap checks first. An unmerged entry on an
   index side is `Unmerged` (the index holds no single version); a submodule change (every side a
   gitlink) is its gitlink ids (on a working tree side git's own `Subproject commit <oid>[-dirty]`
   line). Then both sides' sizes (one `git cat-file --batch-check` for the blobs, the backend for a
   working tree file git didn't hash or hashed only in memory, as rename detection does):
   binary files (known from the list) are `Binary` with their sizes, whatever their size; over the
   cap (2 MB or 20k lines per side — the list's +/− and the output's length catch most of it before
   any line is built) is `TooLarge`, shown with a "Load anyway" row that diffs again without the
   caps. Otherwise `git diff -U<N>` (D4) is parsed into aligned lines — hunk bodies read by their
   header counts, never by what a line looks like, because a reviewed file may itself be a patch —
   with each side's "no newline at end of file", and CRLF line endings stripped for display and
   reported. A listed file whose content didn't change (renamed, or only its mode changed) gets
   no hunk from git; its text is read from the old side's blob and given as context lines on
   both sides, so the diff still holds both sides' complete text for threads to be placed
   against (§8), while the stream shows "No content change". Under `core.autocrlf` or `text eol=crlf` git compares the normalised LF form, so an
   edited CRLF checkout is exactly its edited lines. A type change (file ↔ symlink, file ↔ gitlink)
   prints as a deletion plus an addition of one path and becomes one diff.
3. **Intraline:** word-level ranges for paired changed lines (kept from the first implementation:
   the i-th removed line of a change block pairs with the i-th added one, token diff with
   histogram, pairs over 10,000 token pairs or more than 70% rewritten left unmarked).
4. **Highlight:** tree-sitter on both full sides, off the UI thread, producing **token kinds**, not
   colours. Colours resolve at paint time from the theme, so theme changes repaint without recomputing
   and cached rows never go stale. Rows appear unhighlighted first and colour in when ready.
   `dv_ui::syntax` runs gpui-component's highlighter (every grammar of its `tree-sitter-languages`
   set; SQL's crate wants `cc` ~1.2, so the lock keeps `cc` at the newest 1.2, and its Neovim-made
   query gets dv's corrections for numbers, `NULL` and option keywords) once per loaded file
   side, in the language its path names (extension or a well-known name such as `Makefile`; none:
   plain). Capture names map to ten kinds — keyword, function, string, number, type, constant,
   property, punctuation, comment, plain — through one table (`kind_for_capture`), and the
   highlighter's style resolver encodes the kind so nothing colour-shaped is cached. Caps: a side
   over 2 MB isn't highlighted, a line over 10,000 bytes keeps no tokens.
5. **Rows:** built lazily for the current view mode only; unchanged runs beyond the context setting
   become gap rows that expand in 20-line steps from either edge or fully, from the already-loaded
   content (no refetch; `dv_core::diff::{segments, expand_gap}`). A run shorter than two lines stays
   shown, since its gap row would take the same space. A gap row also heads the run that follows
   it with that run's `@@ -a,b +c,d @@` (`dv_core::diff::hunk_range`, git's header for the same
   lines). In split view, within a run of changed lines the i-th removed line faces the i-th
   added one (`dv_core::diff::split_rows`, the pairing intraline marks use).
6. **Interdiff (rounds):** the same pipeline between round n's snapshot and the current new side
   (`DiffSpec::since`, §2.3). A working tree snapshot holds the untracked files of its round, so the
   step 1 rule for untracked files the old side holds is what keeps an interdiff honest: an
   untracked file unchanged since the round isn't listed, a changed one lists once as modified,
   and a near copy staged since lists as added, not as a rename of the untracked original.
7. **Stream:** the diff pane is one virtualised list over *all* files (file header rows pinned while
   scrolling), so `J`/`K` scroll to a file instead of swapping views. `dv_ui::diff_pane::rows`
   builds each file's rows as plain data — header, code rows (one line, or one split pair), gap
   rows, a notice instead of lines, the next-file footer, and the insertion points thread cards
   and the composer take — and the gpui `list` holds one item per row, plus a spacer that lets the
   last header reach the top (the viewport less the last file's rows, a card not measured yet
   counted at the least a card can be, so the spacer is never short). A file's rows are rebuilt
   when its diff lands, a gap expands, it collapses (viewed) or the view mode changes, and only
   they are spliced; the list scrolls by item, so a file loading above the viewport moves
   nothing, and rows rebuilt under the reading position keep it on the same line. A file whose
   diff loads again with other lines (the agent saved it) has its lines followed into the new
   diff by number (`dv_core::diff::remap_lines`: on a side whose text didn't change every line
   keeps its number): the cursor, the selection, the composer's lines, the reading position, the
   context the reviewer opened and the lines on screen stay with their lines. Every row but a
   thread card, the composer and an opened failure banner has a fixed height, and those count as
   last measured, so scrolling to a line, paging and the pinned header work from heights without
   waiting for a layout. The composer's row carries its field's focus handle into the list, so a
   field scrolled out of view keeps the keyboard; one left without a row (its file collapsed or
   gone) hands the keyboard back to the stream. The stream asks
   the session for the files in view plus two ahead and one behind; a code row shapes only the
   columns in view, so a minified line costs no more than a short one.

---

## 7. Review store

`dv_core::review`: `id`, `event`, `model` (the folded state), `fold`, `store` (`ReviewStore`) and
`ops` (the checked changes the UI and the CLI call).

- **Location:** `<git-common-dir>/dv/reviews/<id>.jsonl` (resolved with `git rev-parse
  --git-common-dir`, never by hand), shared by every worktree of the repo. The `created` event records
  the worktree root it was made in (`worktree`), which is what a `working` or `staged` target means.
- **Events (v1):** `created{target, author, worktree}` (an `author` is `{kind, name, model?}`
  everywhere, `model` the one an agent named, §2.5), `round_closed{n, new_oid, old_oid, verdict?,
  body?}` (`body`: the round's summary, Markdown),
  `viewed{round, path, on}`, `thread_added{id, anchor, body, author, github_thread?, annotation?}`, `reply_added{thread, id, body,
  author, anchor?}`, `thread_state{thread, state, note?, author, anchor?}`, `body_edited{target_id,
  body}`, `deleted{target_id}`, `pr_linked{owner, repo, number}`, `submitted{github_review_id, url,
  event, head_oid?, merge_base_oid?}`, `pr_pending{github_review_id, commit}`, `pr_posted{thread, github_id}`,
  `pr_discarded{}`, `status{status, note?, author?, if_status?, if_round?}`. A reply's or state change's
  `anchor` is the thread's relocated anchor, recorded once it moved or its file was renamed (§8);
  a status's `note` is what the one who set it says, Markdown (the agent's `dv review done --note`,
  §3 workflow A step 4), `author` who that was, and `if_status` and `if_round` conditions: the
  move applies only to a review that has that status (the agent's automatic `in_progress`), or
  exactly that many closed rounds (the agent's `dv review done`), where the event lands, and is a
  no-op otherwise (§2.4). A PR review's GitHub events (§9.8): a `thread_added`'s `github_thread`
  makes the thread a draft reply to that GitHub thread (§9.6); `pr_pending` names the pending
  review on GitHub that submitting is building, `pr_posted` each draft posted into it (frozen
  from then on), `pr_discarded` the pending review deleted (the posted drafts thawed), and
  `submitted`'s `head_oid` and `merge_base_oid` the commits it went to GitHub against (§9.5; absent in
  logs written before phase 8). A `thread_added`'s `annotation: true` makes the thread an agent's
  annotation (§19). Each line is `{"v":1, "eid", "at" (ms), "kind": …, …}`: the format version, a
  unique event id and the time, beside the kind and its fields (snake_case). Optional fields are left
  out when empty; unknown fields are ignored, so an optional addition needs no version bump.
- **Writing:** one `store_append` per event (D5). A log whose last line lacks its newline ends in a
  write a crash cut off; the next append first writes a lone newline, so the new event isn't glued
  onto the torn line. Only `ReviewStore::create` writes `created`, as a log's first line.
- **Fold:** the file order is the total order. Unknown event kinds and lines with a higher `v` are
  skipped and counted (forward compatible); a line that fails to parse is reported with its line
  number and skipped (a torn final line loses nothing before it). The fold checks every event against
  the state it lands on with the rules of §2.4 and §2.5 — ids that exist and aren't deleted, allowed
  status and thread-state moves, rounds closing in order, viewed rounds that exist, nothing after
  `submitted`, an event id seen once — and skips one that doesn't apply with a warning. Writers don't
  lock, so two may race on stale state (an agent addresses a thread the reviewer just resolved); the
  fold is the final word. An event that changes nothing applies as a no-op. A log without a valid
  `created` event is unreadable. An anchor's lines are 1-based, save the file-level anchor's
  `0..=0` with no text or context (§2.5); any other anchor with a line 0 is refused. A dv older
  than file-level anchors refuses that one too, so it skips such a thread (and the events on
  it) with its usual warning: an addition no version bump covers, which is acceptable for a
  comment an older build can't show anyway (the snapshot cache's version changed with it, so
  a cache an older build folded is rebuilt). A relocated `anchor` on a reply or state change moves the thread
  there and keeps its first anchor (§8). The last `status` event that changed the review is
  `Review::status_change {status, note, author, at}`; repeating the status with another note while
  the review is open replaces the note (as addressing a thread again does), with the same note or
  none it is a no-op. The status each round closed from is derived, never written
  (`Round::closed_from`): it says when a hand-back would be of a round the agent may not have
  seen (§2.4).
- **Operations** (`review::ops`) load the review, run the fold's own check on the new event, add the
  checks that need to know who is asking (§2.4's status rules, author-only edits and deletes), and
  only then append, so an operation never writes an event the fold would skip on that state.
- **List:** every `<review id>.jsonl` in `reviews/`, newest first; a log that fails to load is listed
  as `{id, error}` — one bad file never hides the others — while one deleted between the listing and
  its load is simply gone. Listing also removes snapshots whose log is gone, which a load racing a
  delete writes back after the delete removed them.
- **Ids:** `r-<ulid>`, `t-<ulid>`, `c-<ulid>`, `e-<ulid>` in lowercase Crockford base32 (no `i l o u`,
  first digit `0`–`7`), validated by a strict parser before any path is built from one; `../x`,
  uppercase and a wrong kind letter are refused.
- **Snapshots** of the fold are a cache only (`<id>.state.json`): the folded state, its warnings and
  the log's complete-line count (the event count), length and FNV-1a hash. A snapshot answers a load
  only when all three match the log as it is now (checking them is far cheaper than parsing every
  line, and catches a log that grew or was edited in place); otherwise the log is folded and the
  snapshot rewritten atomically. Its format version changes whenever the folded state does, and a
  snapshot of another version is rebuilt, so one folded by a dv that ignored an event field is
  never taken for this build's fold. Operations fold without rewriting it: their own append makes a
  snapshot of the state they loaded stale at once. A snapshot that can't be read or written is
  ignored, logged at debug only: readers woken by one append (an agent's `dv review wait`, the
  app) rewrite it at once, and on Windows the losers' replace fails while another holds the
  file, which must not reach a successful command's stderr.
- **Deletion** of a review removes its log, its snapshot and its `refs/dv/reviews/<id>/*` (one
  `git update-ref --stdin` over what `for-each-ref` lists), log first, and finishes an interrupted
  delete when run again. A round finished while its review is deleted can make its snapshot ref
  after the delete listed the refs; its `round_closed` then finds no log, and `finish_round`
  removes that ref again (`git update-ref -d <ref> <its commit>`), so nothing is left pinning
  the snapshot.

## 8. Anchoring and relocation

A new thread's anchor comes from the selection and the side's full text
(`dv_core::review::anchor_selection`: the lines' text and up to 3 lines of context either side);
`Anchor::matches` says whether its text still stands at its recorded lines.

**Relocation** (`dv_core::review::relocate(anchor, lines) -> Relocation`, against the side's
current full text, `side_lines`) runs on every refresh, one pure function for the GUI and the CLI:
1. the anchored text still stands at its recorded lines → `Unchanged`;
2. exact matches of the anchored lines within ±200 lines of the recorded start: the one whose
   context lines also match best wins (the count of `context_before`/`context_after` lines that
   match where they belong), then the nearest, then the one above → `Moved { how: Nearby }`;
3. else an exact match anywhere in the file, if it is the only one → `Moved { how: Unique }`;
4. else (and before a slight anchor's bare matches count, below), when the anchor's whole
   context still stands together somewhere (its `context_before`
   lines, then its `context_after` lines, nothing or a few lines between; a context cut short by
   the file's edge must meet that edge; the nearest to the recorded start, then the one above),
   the anchored lines are gone from between it: with nothing between they were deleted →
   **outdated**; with up to the anchored count plus 10 lines between (`REWRITE_SLACK`) they were
   rewritten there, found at those lines when they score at least 0.80 as step 5 scores a window
   (the same number of lines compared line by line, any other number as one text each) →
   `Moved { how: Fuzzy { score } }`, else **outdated**. Never at a like line elsewhere: list-shaped
   code (match arms, rows of data, flag lists) has neighbours that score as well as the line a
   reviewer asked to remove, and a fuzzy window would land the thread on one of them;
5. else fuzzy, on whitespace-normalised lines (trimmed, inner runs of whitespace collapsed to one
   space): every window of the anchor's shape (its context before, as many lines as it anchors,
   its context after, clipped at the file's edges, where a missing line scores 0) scores the
   weighted mean of its lines' similarity to the anchor's, anchored lines weighing 2 and context
   lines 1; an anchor of more than 16 lines is scored on 16 of them, its first and last among
   them and the rest spread evenly (`FUZZY_MAX_ANCHORED`), so a window costs the same whatever
   the range's length. Line similarity is the Dice coefficient of the two lines' character
   bigrams (equal lines 1, two empty lines 1, a line shorter than 2 characters compared by
   equality). Of the windows scoring ≥ 0.80, the nearest (then the one above) within 0.05 of the
   best score wins (`FUZZY_MARGIN`: in repetitive text — rows of data, a flag list — an
   untouched window far away can score a little better than the anchored lines edited where they
   stand) → `Moved { how: Fuzzy { score } }`. A side over 20,000 lines skips this step.
6. else **outdated**: shown with its original code snippet, not inline (its card heads its file,
   placement below, or stands where its code went when its round's snapshot tells: "Round
   history" below), and flagged in the comments list.

An exact match from step 2 or 3 wins at once when its context stands around it too (every
`context_before` and `context_after` line where it belongs). One standing among other lines (a
bare match) may be no more than the anchored text repeated somewhere (`return Ok(());`, `}`, a
second debug print), and which it is depends on the anchor:
- a **slight** anchor (`review::is_slight`: fewer than 32 non-whitespace characters in all its
  lines, `SLIGHT_CHARS`; a longer line says enough to be followed where it moves, alone or not)
  says too little to tell a move from a copy that was always there, so step 4 runs before its
  bare matches count: the line a reviewer asked to remove, deleted while a twin stands
  elsewhere, is outdated, and one rewritten where it stood is found there or outdated, never
  moved onto the twin (whose lines the next event would record). A match standing between the
  closed-up context is the anchored text itself, with lines inserted beside it, and still wins
  at exactly its own lines. At a file's edge (its first or last lines: no context on that side)
  the closed-up context runs to the edge, so code added above a file's last line (a route
  before `module.exports = router;`) or below its first leaves the thread on that line. The
  price: a slight line cut and pasted out of its context is outdated, and a slight last line
  deleted while code ending in a copy of it is appended (the last `}` deleted, a function
  appended) lands on the copy, which has the same shape as code inserted above the last line
  and is far rarer;
- a fuller anchor found elsewhere was moved there, so for it step 4 is only for text found
  nowhere: a block cut and pasted elsewhere leaves its context closed up behind it and is still
  found where it went (a block moved and edited at once, whose old place closed up, is outdated:
  dv can't tell it from a deletion).

Step 5 then runs as well, and a window scoring better than the bare match's own window wins over
it, so an edit to the anchored line itself is found where it was made. An anchor of blank lines only
would match any blank run by text alone, so in steps 2–3 its matches count only where at least one
context line matches too. Each line's bigrams are computed once per side and shared by every
thread placed on it (`review::FuzzyLines`); in a release build a 20,000-line side takes about
16 ms for a 3-line anchor and about 30 ms for a 200-line one that moved far or is gone (measured
2026-10-09).

**Placement** (`review::place_thread(thread, entries, history, text) -> Placement`) decides the
file and side a thread shows on now. Renames carry anchors (`review::thread_file`): its file is
the entry at the anchor's path, else the entry renamed from it (`entry.old_path == anchor.path`),
else the entry renamed from the path the anchor's file had been renamed from (`entry.old_path ==
anchor.old_path`: renamed again since, which git reports as one rename from the first name);
when that entry is missing or lacks the anchor's side, a new-side thread of a closed round takes
the entry at the path its round's history says the file has now, by the same rule (an added file
renamed since, which no list of the review pairs with its first name: "Round history" below). The side's text comes from a callback (the GUI's loaded diff
— every file a thread shows in loads whatever the stream's viewport; where the diff shown holds
no text, a file too large to show, the review's own diff of the file without caps, read in the
background for the threads anchored there —, the CLI's `file_diff` without caps; a file renamed
or with its mode changed and nothing else carries its text as context lines, §6 step 2), which
can say it isn't loaded yet (`Pending`) or that the side has no lines (binary, a submodule,
unmerged: outdated). A thread
whose file left the list, or whose side the file no longer has (a new-side thread on a deleted
file, an old-side one on an added file), is `Outdated`; a **comment on the whole file** (a
file-level anchor, §2.5) whose file has its side is `Placement::File { file }` (`as_str()`
"file", no `lines()`, not outdated), its text never asked for and nothing relocated or traced
(its file found by the same rule, renames and round history included: a deleted file paired
with an untracked one, git's rename since the round); otherwise `relocate` decides `Unchanged`,
`Moved` or `Outdated`, and an `Outdated` new-side thread of a closed round carries its trail
when its round's snapshot tells (below). The GUI and the CLI place threads only through it, against the review's
own file list (all changes), so they never disagree (the GUI's interdiff adds one rule after it:
a thread whose file the interdiff doesn't show, and an old-side thread, isn't drawn, §2.3, §11;
it is still placed, against the review's own diff of its file read in the background, so the
comments list, "Copy as prompt" and the sheet name where it stands in all changes, as the CLI
does). An outdated thread whose file is still listed heads that file in the stream with the lines
it was written on (DESIGN §4.1), or with a trail stands where its code went, so it can still be
answered and resolved; one whose file left
the list (`Outdated` without a file) heads the stream itself, as a full card in its "Not in the
diff" section before the first file, whichever comparison is shown, so it too can be answered
and resolved there (and from its comments-list row's menu). A comment on the whole file heads
its file, ahead of its outdated cards, and goes to that section once its file leaves the list.

**Threads on files the review doesn't change.** A thread whose file left the review's list —
the file never changed (a comment from the file view), or stopped changing (the agent reverted
it) — is no longer simply outdated: `review::place_unlisted` relocates it, as above, against that
file's text at the review's new side (`diff::side_file`, read whole, without the size caps, as
the file view's text is), at the path its round's history says the file has now when it says
(`review::unlisted_path`), else its anchor's. The review doesn't change the file, so its old and
new sides are the same text, and an old-side anchor is relocated in it too. Found there, a thread
is `Placement::Unlisted { path, start_line, end_line, how }`: placed (`lines()` its lines;
`as_str()` "unchanged" or "moved", `shows_moved` and `shows_edited` as for any placement), not
outdated, in no listed file (`file()` `None`), and not in the diff (`Placement::in_diff()` false);
a comment on the whole file of a file that stands there is `Placement::UnlistedFile { path }`
(`is_file()`, `as_str()` "file", not in the diff). Its anchored text gone from the file, or the
file gone from that side (or without text: binary, a submodule), it is outdated without a file as
before. The GUI reads such a file in the background for its threads (and the file view), kept
until a refresh touches its path or the git state or a side moves, the read it had placing them
meanwhile; until its first read lands the thread waits (`ThreadPlacement::Reading`: pending,
drawn neutrally in the "Not in the diff" section, so its card, and a reply being typed in it,
stay there), then is marked "Unchanged file" with **View file** (the file view at its line) once
it is placed; in that file's file view it shows inline at its lines (either side's). The CLI
reads the same file synchronously, once per path, for the threads that need it (a file it can't
read is a warning on stderr, its threads outdated) and reports them `placement` "unchanged" /
"moved" / "edited" / "file", `in_diff: false`, `anchor.current` and `excerpt` from that file's
text, and the text output's `(not in the diff)` (`(file, not in the diff)`, "on the whole file,
which the review doesn't change"). No move is recorded for such a thread (its anchor names no
listed file); once its file joins the review again, it is placed in it as any thread.
`dv comment add` still takes only the review's changed files.

**Round history** (`review::history`). A thread's `anchor.round` is the round its line numbers
refer to; once that round has closed, its snapshot (`refs/dv/reviews/<id>/<n>`, §2.3) holds the
text the anchor was taken from, and a new-side thread the current text alone can't place (its
file not listed, or lacking its side, or relocation calling it outdated) is read against it.
`review::read_round_history(repo, resolved, round, paths)` runs blocking over `GitRepo` (so
through any backend, the WSL one included): one file list of the interdiff since the round
(`DiffSpec::since`, snapshot → now) and the texts it needs through `file_diff`. It follows each
path: an entry renamed from it names it now (git's rename detection); a path that list shows
deleted is paired with its most similar added or untracked entry, because git can't pair a file
the snapshot holds with an untracked one (dv lists those apart, §6 step 1) and an agent's new
files are untracked until committed — on substantive lines only (trimmed, at least 3
alphanumeric characters: `}`, `);` don't count), each weighed by its length, the Dice
coefficient of the two texts' lines as a multiset must be over 0.5 (`PAIR_THRESHOLD`, git's
default rename threshold) and they must share at least 3 such lines (`PAIR_MIN_SHARED_LINES`)
adding up to at least 60 characters (`PAIR_MIN_SHARED_CHARS`), so two small unrelated files
(functions sharing a `return 0;` and their braces, barrel files sharing one `export *` line)
never pair; the candidates ranked by same extension, then name likeness, then path, at most 30
read (`MAX_PAIR_READS`) within the default `DiffLimits`, a tie going to the one ranked first;
any other path is where it was. A round whose snapshot is missing says nothing. Where the code
went (`review::trace`): the anchor is found in the snapshot's text (`relocate`: at its lines or
moved), and those lines are mapped through a line diff (imara-diff, Myers) of the snapshot's
text against the current text; the change blocks overlapping them (a pure insertion counting
when it falls between two of them) give `Went::Replaced` at the new-side lines of those blocks
and the unchanged lines between, cut at the anchored count plus `REWRITE_SLACK` lines from its
start (a file rewritten whole doesn't point far down it), or, when every such block replaces
its lines with nothing, `Went::Removed` before new-side line k where the first of them stood (1
at the top, one past the last line at the end), never `Replaced` at anchored lines left
standing. A read again takes the snapshot's texts from the history it replaces (a snapshot
never changes) and reads only the interdiff's list and the candidates. Old-side anchors (a snapshot
is a new side), anchors of the round in progress and an anchor the snapshot doesn't hold get no
trail. The trail rides on the placement, `Outdated { file, trail: Some(Trail { round, went }) }`:
the thread is still outdated (`is_outdated()`, `as_str()` "outdated", no `lines()`), and no
relocated anchor is recorded from it, so answering it never re-anchors it onto other code.
`place_thread` takes the history the caller read (`History::None` for a round in progress or a
read that failed, `Pending` before it is read, `Read`); a thread that needs it before it is read
is `Pending` (its file then `None` when no entry is at its path), so it never shows outdated, in
the "Not in the diff" section or heading its file, and then jumps; in the GUI a thread placed
before keeps that placement while only its history is missing, so its card and a reply being
typed in it stay. Both callers read it only for the rounds some thread needs, for those
threads' paths: the CLI synchronously, for threads that placed without history are outdated (a
round it can't read is a warning on stderr, its threads placed without it); the GUI in the
background, one read per round at a time (paths asked for meanwhile follow it), for the paths
live threads still name, kept while the target resolves the same, the review's files (paths and
statuses) stand as read and no refresh touches a path the history read, the one it had placing
threads while it is read again, dropped when a read again against another basis fails (§11).

**Recording a move.** Relocation is display-only until the next event on the thread, which records
the new anchor: `reply_added` and `thread_state` carry an optional `anchor` (§7), the thread's
anchor re-taken at its relocated lines in the round in progress with fresh context
(`review::relocated_anchor`, through `anchor_selection`); callers pass one for `Moved`, and for a
thread standing at its lines in a file renamed since it was written (re-taken at the same lines
under the file's name now, so the thread keeps its file once the rename becomes part of the old
side — a commit, a rebase — rather than going outdated; a comment on the whole file in a
renamed file likewise, re-taken as a file-level anchor under the name now, no text read)
(`ops::reply` / `ops::set_thread_state`
take `relocated: Option<Anchor>`). The fold checks it like a
`thread_added` anchor (an invalid one skips the event with a warning), replaces `Thread::anchor`
with it and keeps the anchor the thread was opened with as `Thread::original_anchor` (set on the
first move only); an event that changes nothing records no anchor either. dv versions without
relocation ignore the field. The CLI's `comment reply | address | resolve | reopen` place the
thread first and pass the anchor the same way the GUI's writes do. A thread shows as moved (the
GUI's "· moved", the CLI's `placement: "moved"`) when the lines it shows at now start elsewhere
than the first line it was written on (`review::shows_moved`), whether relocation finds it there
or an event recorded the move: code edited where it stands (a fuzzy match starting at its own
first line, however many lines it covers now: a line rewritten into a block) and a renamed file
move nothing, so recording a move, an edit in place or a rename changes nothing anyone sees.
Code edited where it stands is `placement: "edited"` in the CLI (`review::shows_edited`: found
there by a fuzzy match, or standing at recorded lines whose text isn't the text it was written
on), so an agent reads `current_code` rather than take the comment's code as still there; the
GUI marks nothing for it. `dv comment list --json` reports `anchor.current` (relocated) and
`anchor.original`, plus the anchored text, so agents never act on a stale line number.

---

## 9. GitHub

Phase 8 (§16) makes PR reviews work end to end: §3 workflow C. Everything goes through `gh` (D7);
dv stores no token and talks to no GitHub endpoint itself. The pieces, in the order a review uses
them: which GitHub repository a clone is (§9.1), running `gh` (§9.2), opening a PR (§9.3), the
local mirror of what GitHub holds (§9.4), how a PR review follows the PR (§9.5), GitHub's
threads in dv (§9.6), Viewed sync (§9.7), submitting (§9.8), finding PRs and the PR in the app
(§9.9), the CLI and agents (§9.10), testing (§9.11) and the order of work (§9.12). Checked
against GitHub's REST and GraphQL schemas, `gh` 2.96's source and help, and the VS Code GitHub
extension and octo.nvim on 2026-10-19; what GitHub doesn't document and the owner's live check
(§14.3) must confirm is marked *(live check)*.

### 9.1 The repository on GitHub
`dv_core::github::repo` reads a clone's remotes through its backend (`git config --get-regexp
'^remote\..*\.(url|gh-resolved)$'`), so a WSL repository is read where it lives, and picks the
remote whose repository PRs live in (D7, and §15's "hard-coded `origin`" bug):
1. the remote `gh repo set-default` marked (`remote.<name>.gh-resolved`: `base`, or an
   `owner/repo` naming one of the remotes' repositories);
2. else `upstream`, else `origin`, when its URL is a GitHub one;
3. else the only remote with a GitHub URL; none, or several with none of the above, is
   `GithubError::NoRepository` naming the remotes and `gh repo set-default` as the fix.

A GitHub URL is `https://<host>/<owner>/<repo>[.git]`, `git@<host>:<owner>/<repo>[.git]` or
`ssh://git@<host>/<owner>/<repo>[.git]`, read as the remote is configured (git's
`url.<base>.insteadOf` rewriting only changes where `git fetch` goes). `<host>` is `github.com`
or a GitHub Enterprise host (`gh`'s `--hostname`). The result is `GithubRepo { host, owner,
repo, remote }`, kept for the app's session per repository and read again when its config
changes. A PR review's target names owner and repo only (§2.2); its host is the clone's.

### 9.2 Running `gh`
`dv_core::github::Gh` runs the program `DV_GH` names, else `gh` on `PATH`, with
`GH_PROMPT_DISABLED=1`, `GH_NO_UPDATE_NOTIFIER=1`, `GH_PAGER=cat`, `NO_COLOR=1` and no console
window on Windows, always naming the repository (`-R <host>/<owner>/<repo>`, `gh api --hostname
<host> repos/<owner>/<repo>/…`), never relying on the working directory. Every request body goes
on stdin as JSON (`--input -`): a REST body, and every GraphQL request whole (`gh api graphql
--input -` with `{"query", "variables"}`), never as `-f`/`-F` arguments, which can't carry nested
inputs and would meet Windows' 32K command-line limit with long comments.

- **Where it runs.** The app runs `gh` on its own machine for every repository, a WSL one too:
  GitHub calls need the repository's name, not its files, and the app's own `gh` login is the one
  its setup checks (§12.2). Only `git fetch` runs where the repository lives (its backend). The
  CLI runs `gh` where it runs: inside the distro for a WSL repository (§10.4), with the distro's
  own login, which the distro's setup checks cover. The two logins may differ, which is why the
  mirror is per login (§9.4).
- **Results.** Every call has a timeout (30 s; a posting mutation 60 s) and ends as one of: the
  parsed JSON; `GithubError::NotAuthenticated` (gh's "gh auth login" markers, or HTTP 401) with
  `gh auth login` as the fix (dv never runs it); `NotFound` (404: the PR or repository, or no
  access to a private one); `RateLimited { reset }` (403 or 429 with `retry-after` or
  `x-ratelimit-reset`, primary or secondary); `Validation { messages }` (422, GitHub's `errors[]`
  messages kept verbatim); `Graphql { messages }` (a 200 whose body has `errors[]`, which gh
  reports as a failure); `Unavailable` (5xx, a network failure, a timeout); `Gh { stderr }` for
  anything else. stderr and the response body are kept (2 KB each) for the detail line. A
  missing `gh` is the setup check's state (§12.2).
- **Pagination** (D7): REST lists through `gh api --paginate --slurp`; GraphQL connections
  through dv's own loop (`first: 100`, `after: $cursor`, `pageInfo { hasNextPage endCursor }`,
  again until `hasNextPage` is false), a nested connection that has more pages (a thread's
  comments past 100) through a follow-up query on its node; at most 50 pages each, and a cursor
  that repeats stops the loop, with a warning, so it can't spin forever. Calls run one at a time,
  and mutations at least one second apart (GitHub's secondary limits, §9.8).

### 9.3 Opening a PR
A PR is named as `482`, `#482`, `<owner>/<repo>#482` or its URL (`https://<host>/<owner>/<repo>/pull/482`,
any trailing path such as `/files`), parsed by `dv_core::github::pr_ref`. One that names another
repository than the clone's (§9.1) is `GithubError::OtherRepository` ("#482 is in acme/api;
open it from a clone of that repository").

1. **Metadata:** `gh pr view <n> -R … --json id,number,title,body,url,state,isDraft,author,
   baseRefName,headRefName,headRefOid,headRepositoryOwner,isCrossRepository,updatedAt,
   reviewDecision,statusCheckRollup,changedFiles,additions,deletions` (a null `statusCheckRollup`
   and an empty `reviewDecision` are none), and the viewer's login (`gh api user`, once per app
   session and host).
2. **Fetch** through the repository's backend, into dv's own namespace (§2.4: only `git fetch`
   and `refs/dv/**`): `git fetch --no-tags --no-write-fetch-head --refmap= <remote>
   +refs/pull/<n>/head:refs/dv/pr/<owner>/<repo>/<n>/head
   +refs/heads/<baseRefName>:refs/dv/pr/<owner>/<repo>/<n>/base` (`--refmap=` keeps git from
   also moving the remote's tracking branch, `origin/main`, which other reviews' default base
   reads). `refs/pull/<n>/head` lives on the base repository for a fork's PR too, so one remote
   always serves. Skipped when both are already there and the head is the one `gh` reported.
3. **Verify** that the fetched head is `headRefOid` (a push between the two calls refetches once,
   then fails with `GithubError::Moved`), then compute the old side locally: `merge-base(base,
   head)` of the fetched commits. `baseRefOid` isn't used: GitHub doesn't document whether it is
   the base branch's tip or the merge base. The diff is then GitHub's own "Files changed" diff
   (three dots), which stays the same when the base branch moves on unless the PR merges it in.
4. **The review:** the PR's open draft review when it has one (`ops::open_review` by PR, whatever
   head; at most one, §2.4); else its latest submitted review when that was submitted at the PR's
   head (nothing new to review, so no new round is offered); else a new draft with the target
   `pr <owner/repo#n>` at these oids and its `pr_linked` event. Opening also refreshes the PR's
   mirror (§9.4) and reconciles an interrupted submission (§9.8).

`dv open #482` and the PR picker open it in the app; `dv review new --pr 482` makes or resumes
it from the CLI (§9.10). A closed or merged PR opens like an open one (its state shows, §9.9);
GitHub accepts review comments on both *(live check)*.

### 9.4 The PR mirror
What GitHub holds about a PR is GitHub's and is never written into a review's log (§7): dv keeps
a **mirror** of it, a cache rewritten whole on every refresh, read by every review of the PR and
every worktree: `<git-common-dir>/dv/github/<host>/<owner>/<repo>/pr-<n>/<login>.json` (a host
spelled for a file name, `:` as `_`; per login, because Viewed state, pending reviews and what
the viewer may resolve are the login's), written through the backend's atomic store write, so a
WSL repository's mirror lives in the distro beside its reviews. It holds the metadata of §9.3,
the checks summarised (passing, failing and pending counts), every review thread with every
comment (§9.6), the viewer's pending review if any, with its commit and its comments (§9.8),
each file's Viewed state and the state dv last synced for it (§9.7), GitHub's own patch for
each file at the head (`gh api --paginate --slurp repos/<o>/<r>/pulls/<n>/files`: `filename`,
`previous_filename`, `status`, `patch`, up to GitHub's 3,000 files; a file without a `patch` is
binary, rename-only or too large, §9.8), the head those were read at, and when. A mirror that
fails to parse, or of another format version, is fetched again. Nothing in it is authority for
dv's own review state.

**Refresh** reads everything again and fetches a moved head (§9.3 steps 1–3): when a PR review
opens, on **Refresh** (`Ctrl R`, the PR strip, `dv review refresh`), and after every write dv
makes to GitHub (a resolve, a submission). There is no background polling: the strip says when
the mirror was read. A refresh that fails keeps the last mirror, marked stale with the error
(the strip says so). The app follows a mirror rewritten by the CLI through the store's change
events (§5.3), as it follows a review's log.

### 9.5 How a PR review follows the PR
- **One dv round per PR review.** A PR review never closes a dv round (`round_closed` is local
  reviews' only, §2.3): its anchors and viewed marks are all round 1. The PR's own rounds are its
  reviews: the PR round of a review is its place among the PR's reviews in this repository's
  store (`pr_round`, 1 for the first), which the strip and the switcher show ("Round 2").
- **A draft follows the PR's head, one refresh at a time.** Its target keeps the oids it was
  created at (§2.2: a target never changes); its sides are its **pins**, `refs/dv/reviews/<id>/pr-head`
  and `…/pr-base`, which a refresh moves to the fetched head and its merge base (a
  compare-and-swap; removed with the review, §7). So the draft shows, and submits against, the
  head it was last refreshed to, never one a refresh in another process fetched behind its back;
  the strip says what moved. After a refresh that moved the pins the draft lists again and its
  threads relocate (§8), as a branch review's do after a commit. The pins also keep a review's
  commits after a force-push drops them from GitHub's refs (whether GitHub serves a commit no ref
  reaches isn't documented, so dv never relies on it).
- **A pending review stops the draft.** While a submission is under way (the log's
  `pr_pending`, §9.8) the draft doesn't follow: GitHub fixed that pending review to a commit, so
  a refresh updates the mirror and the strip ("The PR moved: 2 new commits") but not the pins.
  The reviewer finishes the submission at that head (GitHub takes a review of an older commit)
  or discards the pending review, which lets the draft follow again.
- **Submitting closes the review.** `submitted` (§7) now also records the `head_oid` and the
  `merge_base_oid` it went to GitHub against (the pins, frozen from then on); the review is
  read-only (the fold refuses every later event, as now) and resolves at those commits.
- **The next round is the next draft.** When the PR has commits newer than its latest submitted
  review, the strip offers **Start round n+1**, which makes the new draft. A draft whose PR has
  an earlier submitted review offers the comparison toggle (§2.3), "Since your last review" and
  "All changes", opening on the first: `DiffSpec { old: Commit(<the previous review's pr-head
  pin>), new: <the draft's pr-head pin> }`, which works across a force-push because both commits
  are pinned. Viewed marks stay per review.
- **`finish_round`** stays local-only; a PR review's verdict goes through submitting (§9.8).

### 9.6 GitHub's threads in dv
The mirror's threads come from GraphQL `pullRequest.reviewThreads` (paged, §9.2): per thread `id
isResolved isOutdated path line originalLine startLine originalStartLine diffSide startDiffSide
subjectType viewerCanResolve viewerCanUnresolve viewerCanReply`, and per comment `id
fullDatabaseId author{login} body createdAt url diffHunk outdated commit{oid}
originalCommit{oid} replyTo{id} pullRequestReview{id fullDatabaseId state}`. A null author is
`ghost`. Another user's pending comments aren't visible; the viewer's own are (their review's
state is `PENDING`).

- **Shown, never stored.** GitHub's threads are a type of their own, `review::GithubThread`
  (GitHub's node id, path, side, lines, resolved and outdated, comments), not `Thread`s: they
  never enter the log, the fold or an id that names a file. The UI and the DTOs see a review's
  threads through `ThreadKey { Local(ThreadId), Github(node id) }`; the CLI spells a GitHub
  thread's key `gh:<node id>`, which `comment show` takes and every write refuses (§9.10).
- **Placement.** A file-level thread (`subjectType: FILE`) is file-level (§2.5). A line thread
  GitHub doesn't call outdated has a `line` on its comments' `commit`, the PR's current head:
  dv builds an anchor from that commit's text at those lines, with full context (§8; the head
  is fetched), and places it in the diff shown: exactly when the review shows that head,
  relocated otherwise (a draft refreshed to an older head, a submitted review). An outdated
  thread is placed by its `originalLine` when its `originalCommit` is the head the review shows
  (a submitted review of that commit), and otherwise goes to the "Not in the diff" section
  (DESIGN §4.1) as "Outdated on GitHub", its `diffHunk` shown as the code it was on. `diffSide`
  LEFT is the old side, RIGHT the new; `startLine` makes a range.
- **dv's own, once posted.** A comment dv posted into its pending review (the log's
  `pr_posted`, §9.8) comes back in the mirror as a pending GitHub comment; it is shown once, as
  the draft it was, until the review is submitted. A submitted review shows its posted drafts as
  GitHub's copies (matched by `pullRequestReview.fullDatabaseId` = the submission's id) and the
  drafts it kept local as "Kept local"; a later draft shows them as GitHub's like everyone else's.
- **What they count in.** GitHub's threads are threads like any other for `]`/`[`, the comments
  list (their own group, "On GitHub"), the outline's marks and **Copy as prompt** (an agent fixing
  PR feedback needs them); the status bar counts them apart ("2 open · 5 on GitHub").
- **Reply** (`R`, the card's field) on a draft writes a draft reply: a local thread whose
  `thread_added` names the GitHub thread (`github_thread: <node id>`, a new optional field, §7),
  anchored where the GitHub thread shows, drawn inside the GitHub card as "Your reply · posts
  with your review", edited and deleted like any draft, posted when the review is submitted
  (§9.8). On a submitted review there is no reply (its log takes nothing more): the card says
  "Start round n+1 to reply". A draft reply whose GitHub thread is gone (deleted on GitHub) shows
  in the "Not in the diff" section, "Its thread on GitHub is gone", to be deleted or copied.
- **Resolve / Unresolve** (`X`) changes GitHub at once (`resolveReviewThread` /
  `unresolveReviewThread`, offered only when `viewerCanResolve` / `viewerCanUnresolve`), then
  refreshes the mirror; it writes nothing to the log, so it works from any review of the PR, a
  submitted one included (it is GitHub's state, not the review's).
- **Open on GitHub** opens the thread's `url` (through `dv_ui::system::open_url`).

### 9.7 Viewed sync
GitHub keeps, per user, PR and file, a Viewed state (`files { path viewerViewedState }`:
`VIEWED`, `UNVIEWED`, or `DISMISSED` when the file changed since it was viewed); dv keeps viewed
marks per review (§7). A **draft** PR review syncs them both ways, as its reviewer (an agent's
`dv review refresh` reads but never writes Viewed); a submitted review never does. The mirror
keeps, per file, the state dv last saw GitHub and its own mark agree on (`synced`), and each
sync is a three-way compare against it:
- the mark changed in dv since `synced` (GitHub didn't): dv → GitHub, `markFileAsViewed` /
  `unmarkFileAsViewed` (the PR's node id; the file's new path, a deleted file's old path), in the
  background after the `viewed` event is written; one that fails is tried again at the next sync;
- GitHub changed since `synced` (dv didn't): GitHub → dv, a `viewed` event (the reviewer's) to
  match; `DISMISSED` unmarks, so a file that changed under a draft that followed the PR shows as
  not viewed again;
- both changed: dv's mark wins and is pushed;
- then `synced` becomes the state both now hold.
A sync runs after every refresh and after each mark in the app. In "Since your last review" the
files listed are the interdiff's, but Viewed is still the file's, as GitHub has no rounds.

### 9.8 Submitting
**The sheet** (`Ctrl Enter`, DESIGN §4.2) for a draft PR review: summary, verdict (Comment /
Approve / Request changes), the comments to post, each with its include checkbox and, when
GitHub can't take it where it is, its validation row; the footer's "Posting as `<login>` via gh".

- **Your own PR:** Approve and Request changes are disabled when the viewer is the PR's author
  ("You can't approve your own pull request"); Comment stays.
- **Summary:** Request changes needs one; Comment needs one or at least one comment *(live
  check: GitHub documents the body as required for both)*; Approve needs none.
- **What can be posted where.** Line comments are validated against **GitHub's own hunks**, the
  mirror's patch for each file at the head being submitted (never a diff of dv's own, which could
  cut hunks differently: §15's "Histogram-based validation" bug): a comment is postable when its
  whole range lies in one hunk on its side, context lines included (GitHub's rule; its web UI's
  comments on unchanged lines aren't open to the API yet). Otherwise its row offers **Post as
  file comment** (the body prefixed with a quoted `path:line` and the anchored line, `subjectType:
  FILE`) or **Keep local** (it stays in the review, unposted, shown as kept local after
  submitting); a comment on a file the PR doesn't touch can only be kept local, and a file without
  a patch (binary, rename-only, too large) takes only file comments. Undo puts a row back.
  **Submit** is disabled until every included comment is postable or decided.
- **Replies** to GitHub threads (§9.6) are listed with the comments ("Reply to @octocat on
  `src/api.rs:40`") and need no validation.
- **A pending review that isn't dv's.** GitHub allows each user one pending review per PR, and
  the web UI or another tool may have started one. The sheet then shows a row, "You have a
  pending review on GitHub (3 comments). Submit or discard it there first", with **Open on
  GitHub**, and Submit stays disabled until a refresh no longer finds it.

**Posting** (`dv_core::github::submit`, one engine for the app and the CLI), through GraphQL,
because REST's create-review call can't carry file comments or replies:
1. **Claim and reconcile.** The run claims the review with `git update-ref
   refs/dv/reviews/<id>/submitting <head> <zero-oid>` (an atomic create: a second run, the CLI's
   beside the app's, is refused as "already being submitted"; removed when the run ends, and one
   older than 10 minutes counts as abandoned), then reconciles (below).
2. **The head:** without a pending review yet, a fresh `gh pr view --json headRefOid` must equal
   the draft's `pr-head` pin; else nothing is posted: the sheet closes on a refresh ("The PR
   moved: 2 new commits. Check your comments against them, then submit again."), which moves the
   pins and relocates the drafts (§9.5). With dv's pending review, its commit is the head.
3. **The pending review:** dv's own (below), else a new empty one: `addPullRequestReview(input:
   {pullRequestId, commitOID: <pr-head>, body: "<!-- dv:<review id> -->"})`; dv appends
   `pr_pending { github_review_id, commit }` (a new event, §7) as soon as it has the id.
4. **Each comment, one at a time:** line and file comments with `addPullRequestReviewThread`
   (`path`, `line`, `side`, `startLine`, `startSide`, or `subjectType: FILE`), replies with
   `addPullRequestReviewThreadReply`, each body ending in the thread's own marker (`<!-- dv:t-… -->`,
   invisible on GitHub), each success appending `pr_posted { thread, github_id }` (§7). A posted
   draft is frozen (ops refuse an edit or delete with `OpError::Posting`) until the review is
   submitted or its pending review discarded. Mutations go at least a second apart, and no more
   than 60 in a minute (GitHub's secondary limits), the sheet showing progress ("Posting 12 of
   40…").
5. **Submit:** `submitPullRequestReview(pullRequestReviewId, event, body: <summary + the
   review's marker>)`, then `submitted { github_review_id, url, event, head_oid, merge_base_oid }`,
   a refresh, and the claim's ref removed. The review is read-only from then on, with "Submitted
   to GitHub · View on GitHub".

**Reconciling** (`github::submit::reconcile`, run on opening a PR review, in the app and the CLI
alike, and first in every posting run) makes the log agree with GitHub after a crash, a timeout
that hid a success, or a change made on GitHub: it finds dv's pending review by the review's
marker in the viewer's pending review's body (so one made just before a crash, without
`pr_pending` in the log, is found too, and its `pr_pending` written), matches its comments to
drafts by their thread markers (writing the `pr_posted` a crash lost, so a retry never posts a
comment twice), and when the log has `pr_pending` but GitHub has no such pending review any more:
a review of the viewer's carrying the marker was submitted (by dv before writing `submitted`, or
on GitHub's website): `submitted` is written, drafts never posted counting as kept local; none
(deleted on GitHub): `pr_discarded`.

**Failures** stop at the first error, keep everything posted so far in the pending review, and
show it where it belongs: a 422 or GraphQL error on its comment's row (GitHub's message verbatim,
never a guess such as "maybe force-pushed"), anything else (auth, rate limit, network) in the
sheet's footer, each with **Retry** (reconcile, then continue). **Discard pending review**
(`deletePullRequestReview`, after a confirmation naming how many comments it deletes from
GitHub) thaws the drafts and appends `pr_discarded {}` (§7).

### 9.9 Finding PRs, and the PR in the app
- **`Ctrl G`** (and the title bar's **PRs**) opens the PR picker over the window for the repository
  in view: a field taking a number, `#n`, `owner/repo#n` or a URL, and below it the repository's
  open PRs (`gh pr list -R … --json number,title,author,isDraft,updatedAt --limit 50`), the ones
  that request the viewer's review first and marked so (a second `gh pr list --search
  "review-requested:@me" --json number`), filtered as the field is typed. The list is kept for
  the app's session per repository and read again in the background each time the picker opens
  (stale-while-revalidate). Enter opens the PR (§9.3).
- **The switcher** lists a repository's PR reviews in their own section ("Pull requests") by
  `#482 Title`, with the review's status chip (`draft`, `submitted`) and its PR round; the title
  bar names a PR review the same way.
- **The PR strip** (DESIGN §2, §4.5: chrome, PR reviews only) under the title bar: `#482` and the
  title, the author, `base ← head` (`owner:head` for a fork), the PR's state (Open, Draft, Merged,
  Closed), the checks' counts (passing, failing, pending; a click opens the PR's checks page), the
  round ("Round 2 · since your last review"), what moved ("2 new commits since this draft's
  head", "The PR moved: 2 new commits" while a pending review holds the draft, "Read 3 min ago",
  or "Couldn't refresh · details" when stale), and **Refresh** (`Ctrl R`), **Start round n+1**
  (when the latest review is submitted and the PR has newer commits) and **Open on GitHub**.

### 9.10 The CLI and agents
- **`dv review new --pr <n | #n | owner/repo#n | URL>`** opens the PR as §9.3 (fetch, mirror,
  review) and prints the review as `review new` does; `dv open <PR>` opens it in the app. A WSL
  repository's command runs inside its distro (§10.4) with the distro's `gh`.
- **`dv review submit <id> --verdict comment|approve|request-changes [--body …]`** on a PR review
  is the engine of §9.8, run by the reviewer: every comment included; one GitHub can't take where
  it is fails the command (exit 1, `not_postable`, listing each with why) unless `--outside
  file-comment` or `--outside keep-local` decides them all; a pending review on GitHub that isn't
  dv's is exit 1, `pending_review`. Output: the submission's URL and what was posted.
  `--verdict comment` on a local review stays exit 2.
- **`dv review refresh [<id>]`** refreshes a PR review (§9.4) and, run by the reviewer, syncs its
  Viewed marks (§9.7).
- **Reading.** `dv review show`, `dv comment list` and `dv comment show` on a PR review include
  GitHub's threads (`id` `gh:<node id>`, `author.kind` `github`, `github: {url, resolved,
  outdated}`); `ReviewDto` gains `pr {owner, repo, number, url, state, head_oid, pr_round}` and
  `submissions [{github_review_id, url, event, at}]` (schema 1 grows).
- **Agents read, never write, a PR review** (the owner's choice, 2026-10-19): every write an
  agent's identity makes on a PR review (`comment add`, `annotate`, `reply`, `address`, `review done`) is
  exit 1 `not_allowed` ("PR reviews are the reviewer's; read the threads with `dv comment
  list`"), as is any write to a `gh:` thread. The skill (§10.5) says so and shows reading a PR's
  feedback to fix it locally.
- **Exit 5** (`GitHub error`) gets its error code, `github`, with `gh`'s message and the
  `GithubError` kind (`not_authenticated`, `not_found`, `rate_limited`, `validation`,
  `unavailable`) in `detail`.

### 9.11 Testing
- **The fixture GitHub** (`dv_test_support`): a bare repository standing in for GitHub, its PRs as
  `refs/pull/<n>/head` (`git update-ref`), and a clone whose `origin` is
  `https://github.com/dv-fixture/<name>.git` rewritten to the bare repository by
  `url.<file URL>.insteadOf`, so §9.1 sees GitHub and `git fetch` reaches the fixture. The
  fixture's commits have fixed identities and dates, so their oids are stable and recorded
  responses can name them.
- **`mock_gh`** answers every call with recorded responses (scrubbed from `kylekz/difftest`,
  §14.2), told apart by their arguments and, for GraphQL, by `stdin_contains` on the request
  (`addPullRequestReviewThread`, a thread's marker); sequences (`times`) answer a refresh after
  posting with the new state. Its log is how tests check exactly what was posted.
- **Unit and CLI tests** cover the remote rules (§9.1), the PR reference parser, every
  `GithubError` mapping, pagination (a nested page, a repeating cursor), placement of GitHub's
  threads (exact, relocated, outdated at its original commit and not), validation against
  recorded patches (a range across two hunks, a deleted line, a rename, a file without a patch),
  the posting engine's order, its claim, reconciling after each step (a lost `pr_pending`, a lost
  `pr_posted`, submitted on GitHub, deleted on GitHub), the pending review that stops a draft,
  the Viewed three-way sync, and the agents' refusals.
- **e2e** (`tests/e2e/workflow_c_pr.jsonl` and friends): §3 workflow C against `mock_gh` and the
  fixture: `Ctrl G`, open, GitHub's threads placed, a reply, a resolve, Viewed both ways, the
  sheet's validation rows (file comment, keep local), submit, the read-only banner, a new head,
  "Start round 2" and its interdiff; and the failure states (not authenticated, a 422 on one
  comment then Retry, rate limited, the PR moved before submitting, a pending review on GitHub).
- **The owner's live check** (`DV_LIVE_GH=1`, §14.3; `crates/dv_cli/tests/live_gh.rs`,
  `#[ignore]`): against `kylekz/difftest` and a PR the owner keeps open for it, in a scratch
  clone: open, refresh, draft a line comment, a file comment and a reply to an existing thread,
  submit as Comment, then read the review back from GitHub; mark a file viewed and back; resolve
  and unresolve a thread. It confirms the points marked *(live check)* above. No agent runs it.

### 9.12 Order within phase 8
1. **GitHub plumbing** (`dv_core::github`): §9.1's remote rules, §9.2's runner, errors and
   pagination, the PR reference parser, the mirror's format and store; the fixture GitHub.
   Unit tests only.
2. **Opening a PR** (§9.3, §9.5): fetch, verify, the pins and refresh, `review new --pr`, `review
   refresh`, `dv open <PR>`, the DTO additions and the agents' refusals.
3. **GitHub's threads** (§9.6): `GithubThread` and `ThreadKey`, placement, the cards (DESIGN
   §4.5), counts, the comments list, Copy as prompt, Resolve / Unresolve, draft replies, the
   CLI's reading.
4. **The PR in the app** (§9.9): the picker, the switcher's section, the strip, Start round n+1
   and the "since your last review" comparison.
5. **Submitting** (§9.8): the PR sheet with validation from GitHub's patches, the posting engine,
   the claim, reconciling, discard, `review submit` for PRs.
6. **Viewed sync** (§9.7).
7. **Workflow C end to end** (e2e) and the live check written for the owner; then a capstone
   review over the whole phase.

---

## 10. CLI

### 10.1 Command tree

```
dv                                 open the GUI on the current repo
dv open [<path> | <PR-url> | #<n>] [--review <id>]
                                   the ONLY GUI entry points; every other command is headless

dv status                          repo, open reviews, the current one, its round and thread counts
dv review list [--all] [--status …]
dv review new [--staged | --branch [<b>] [--base <base>] | --commit <rev> | --range <a..b> | --pr <PR>]
dv review show [<id>]
dv review wait [<id>] [--timeout <s>]      block until the reviewer finishes a round (exit 3 on timeout)
dv review done [<id>] [--note <t> | --note-file <f|->] [--round <n>]
                                           agent: round addressed, ready for re-review
dv review submit [<id>] --verdict approve|request-changes|comment [--body <t> | --body-file <f|->]
                 [--outside file-comment|keep-local]
                                           reviewer (`comment` and the PR flags: PR reviews, §9.10)
dv review refresh [<id>]                   a PR review: fetch the PR and refresh its mirror (§9.4)
dv review close | delete <id>              reviewer

dv comment list [--status open|addressed|resolved|annotation|outdated]… [--file <p>] [--oneline]
dv comment show <id>                       the thread with the diff around its code now
dv comment add --file <p> [--lines N[:M]] [--side old|new] (--body <t> | --body-file <f|->)
dv comment annotate --file <p> [--lines N[:M]] [--side old|new] (--body <t> | --body-file <f|->)
                                           agent: explain a change (§19)
dv comment edit <id> (--body <t> | --body-file <f|->)
dv comment delete <id>                     the item's author (§19.1)
dv comment reply <id> (--body <t> | --body-file <f|->)
dv comment address <id> [--note <t> | --note-file <f|->]
                                           agent
dv comment resolve | reopen <id>           reviewer

dv setup [path [--remove] | agents | wsl <distro> | all] [--yes]   alone: `all`
dv doctor [--json]                         every environment's setup checks (§12)
dv backend --stdio                         (every build; started by the app, §5.1)
```

Global flags: `--repo <path>` (also `\\wsl.localhost\<distro>\<path>`), `--wsl <distro>:<path>`,
`--review <id>`, `--json`, `--author <name>`, `--model <model>`; they go anywhere on the line.

- **One clap tree.** `dv_cli::args` holds the global flags and the headless commands; the `dv`
  binary keeps the GUI's options (`--data-dir`, `--automation…`, `--window-size`, the hidden
  `--no-detach`) and `open`, and flattens `dv_cli::Command` into its own subcommands, so `dv
  --help` lists every command. A headless command goes to `dv_cli::run(&global, command, &mut
  Context)` before any gpui code runs. `Context` carries the command line (without the app's
  options: what a command run inside a WSL distro passes on, §10.4), the working directory, the
  environment (`dv_cli::Env`: `$DV_REVIEW`, `$DV_AUTHOR`, `$DV_MODEL` and `CLAUDECODE` are read through it,
  never by clap), stdin, stdout, stderr, whether they are the process's own (a child then
  shares them) and whether they are terminals, and the environment overlay every git the
  command spawns gets (`LocalBackendOptions::env`), so tests run commands in-process and
  hermetically (`dv_cli::main_with` parses too); a few binary tests in `crates/dv/tests/` check
  the real process.
- **The repository** is `--wsl`, else `--repo` (relative to the working directory, the path
  that makes read as a location again, so a relative path from a working directory inside a
  distro, `\\wsl.localhost\…`, is a WSL location too), else the working directory's. Not
  inside a work tree: exit 1. On Windows a command for a WSL location (`\\wsl.localhost\…`,
  `\\wsl$\…`, `--wsl`) runs inside its distro (§10.4); elsewhere, a location of the distro dv
  runs in (Linux, `$WSL_DISTRO_NAME`) is its local path — a `--file` spelled through that
  distro's share (`\\wsl.localhost\Ubuntu\…`) too — and a Windows drive's path given there
  (`--repo D:\code\api`, `d:/code/api`, `dv open`'s path in a Linux build with the app) is that
  drive where WSL's default automount root mounts it (`/mnt/d/code/api`,
  `dv_core::location::mount_path`, the inverse of the headless `dv open`'s suggestion below)
  rather than a folder under the working directory; any other WSL location is exit 4
  (`backend_unavailable`), without starting anything, saying to run the command from Windows
  (`dv.exe --wsl <distro>:<path> …`, the location quoted to paste there by the rule below),
  the one dv that reaches every distro (inside a distro `wsl.exe -d` would need a dv installed
  in the other one).
- **Identity (§2.5):** an agent named by `--author <name>`, else `$DV_AUTHOR`, else
  `claude-code` when `CLAUDECODE` is set (Claude Code exports it to the commands it runs);
  otherwise the reviewer, by the repo's `user.name` (`review::reviewer_identity`, as the GUI).
  What each may do is `review::ops`'s rule. `dv status` says who a command acts as. An agent's
  model (§2.5) is `--model <model>`, else `$DV_MODEL`: one line of text, trimmed, stored as
  given. `--model` without an agent identity, or empty, is exit 2 (`usage`); `$DV_MODEL` is
  read only when the command acts as an agent, and empty it counts as unset, as `$DV_AUTHOR`
  does.
- **Placement:** `comment list` and `comment show` place every thread with
  `review::place_thread` (§8) against the review's target resolved now (all changes, never an
  interdiff) in its home worktree (§2.2: a `working` or `staged` review's own worktree, a
  `branch` review's the one with the branch checked out), whichever worktree the command runs in
  (a home worktree that is gone is exit 1, `target`), reading each anchored side's full text through `file_diff`
  without caps, so the CLI says what the GUI shows (a target that no longer resolves, such as a
  deleted branch, is exit 1, `target`, with git's words; `review submit` too). A thread found
  there carries the excerpt of that same diff around its lines (`review::excerpt`, §10.3), with
  no further git call. `--status outdated` keeps the threads whose code can't
  be found; `--file` matches a thread's file in the current diff, where its code is now, or any
  path it was written on. A `--file` (here and in `comment add`) is a path from the worktree
  root; an absolute one inside the worktree the review was made in, or the one the command runs
  in, loses that root (the longer, when both hold it), so the reviewed file's own path works from
  any worktree.
- **Writes** go through `review::ops` (or the store's delete), so the CLI writes what the GUI
  would and nothing else (§2.4: only `.git/dv/**` and `refs/dv/**`). A finished review (approved,
  closed) takes no comment writes (`add`, `annotate`, `reply`, `address`, `resolve`, `reopen`,
  `edit`, `delete`: exit 1
  `finished`, §2.4), as the GUI shows it read-only, and moves to no other status (`review done
  <id>` racing the reviewer's approval, `review close` after it, `review submit`: exit 1
  `finished` too, so an agent always hears the one stop signal). A finished review is no longer
  open, so no longer in context (§10.2): a command that names none finds no review (exit 2
  `no_review`), which is why the skill passes the review's id to `done` and `wait`:
  - `review new` opens the target's review as whoever the command acts as, or resumes its open
    one (§2.4), once the target resolves now (a mistyped revision is exit 1, not a review of
    nothing); `--branch` alone is the branch checked out (a detached `HEAD` is exit 2), compared
    with `--base` or the default branch, the switcher's own target.
  - `review done [--note | --note-file] [--round <n>]` is an agent's (exit 2 without an agent
    identity, saying to pass `--author`): status `ready` with the note (§7), through
    `ops::hand_back`. `--round <n>` names the round the agent worked on, which must be the last
    closed one; an older one, or none when the reviewer finished the last round while the agent
    was working (§2.4), is exit 1 `unseen_round`, nothing written, with what to do: read round
    n's summary (the `body` of the last of `rounds` in `dv review show <id> --json`) and its
    open threads (`dv comment list --status open`), then `dv review done <id> --round
    <n>`; one that hasn't closed is exit 2. The event is conditional on the closed rounds
    (`if_round`), and a round that closed between the check and the append makes it a no-op,
    reported as `finished` when that round finished the review, else `unseen_round`.
  - `review submit` is the reviewer's `ops::finish_round` with the verdict and the summary
    (`--body`, `--body-file`), snapshotting the review's home worktree (§2.2) whichever worktree
    it runs in; `--verdict comment` on a local review is exit 2 ("a local round approves or requests
    changes"); on a PR review it is §9.8's posting (§9.10). `review close <id>` sets `closed`;
    `review delete <id>` is the store's delete (log, snapshot cache, round refs), reviewer only
    (an agent is exit 1, `not_allowed`), and deleting again finishes an interrupted delete, then
    says the review isn't there (exit 1).
  - `comment add` anchors on a file of the review's current diff (the full target, never an
    interdiff), by its path or its rename's old path: a file the review doesn't change is exit 1
    `not_found`; a side the file lacks (`old` of an added file, `new` of a deleted one), lines
    past the side's end, or a file without lines (binary, a submodule, conflicted) exit 1
    `rejected`. The anchor is `anchor_selection` over that side's full text in the round in
    progress. Without `--lines` it is a comment on the whole file, a file-level anchor (§2.5)
    on its new side, or with `--side old` on a deleted file's old side (`rejected` on a deleted
    file's new side and on any other file's old side, saying which to use), which reads
    none of the file's text, so a binary or too large file takes one too.
  - `comment annotate` is `comment add` for an agent's annotation (§19): the same files, sides,
    lines, file-level anchor and text, `annotation: true` on its `thread_added`. It is an
    agent's: without an agent identity it is exit 2 (`usage`, saying to pass `--author`), as
    `review done` is.
  - `comment reply | address | resolve | reopen` place the thread first and record its
    relocated anchor when it moved (§8); a target that no longer resolves is exit 1 before
    anything is written, as for `comment list`.
  - **Text** comes from `--body <t>`, or `--body-file <path>` (relative to the working directory)
    / `-` (stdin): a leading byte order mark, `\r\n` and trailing whitespace go, anything else is
    kept; empty text is exit 2, a file that can't be read or isn't UTF-8 exit 1 (`io`). A note
    (`comment address`, `review done`) comes from `--note <t>` or `--note-file <path> | -`, read
    and cleaned the same way, and one left empty is no note. An inline `--body` or `--note` may
    start with `-` (a Markdown list, "-1, I disagree"): clap takes the next word as the text
    whatever it starts with, except a lone flag (`--json`, `-v`, `--author=x`: one word, a
    letter after the dashes), which means the text was left out, and a lone `-` or `--` (stdin
    meant through the inline option, or the end of the options), which the `-file` forms are
    for: both exit 2 (`usage`). A `--` taken as such a value doesn't end the options, so a
    `--json` after it still asks for the JSON error document.
  - **An agent's first write** on a `changes_requested` review moves it to `in_progress` (§2.4).
  - Each prints what it changed (`--json`: the review or thread as it is now, `changed: false`
    when a status or thread state was already so and nothing was written).
- **`review wait [<id>] [--timeout <s>]`** waits on the review's state, not for a change (the
  first implementation reported changes; state can't lose a wakeup): it returns at once, exit 0,
  when the status is one the agent acts on (`changes_requested`, `approved`, `closed`;
  `submitted` for a PR review); otherwise it registers a store watch (`WatchSpec::STORE`) first,
  then loads the review again, then blocks until a batch touches `reviews/<id>.jsonl` (or is a
  rescan) and the reloaded status is one of those, also re-checking 5 s after each load however
  many other batches (other reviews, temp indexes) arrive meanwhile (every 1 s when the store
  can't be watched). A review deleted meanwhile is exit 1 (`not_found`); `--timeout`
  (seconds, fractions allowed) running out is exit 3. It prints why it returned: `{"schema": 1,
  "outcome": <the status>, "review": …, "round": <the last closed round> | null, "threads":
  {open, addressed, resolved, annotation}}`, or `{"schema": 1, "outcome": "timeout"}`; the text is the
  review's line, the last closed round (`round 1 closed: request_changes`) with its summary in
  full, indented (§10.3), and what to do next ("1 open thread: run `dv comment list --status
  open`, …, then `dv review done <id> --round <n>`", naming the review and the round; with no
  open thread, the round's summary or the reviewer's replies, since a reviewer may request
  changes in the summary alone; on a timeout, "run `dv review wait <id>` again", naming the
  review). A
  finished review is no
  longer in context (§10.2), so a wait that may start after the verdict names the review (the
  skill passes the id `dv status` gave).
- **`setup agents [--yes]`** installs the skill (§10.5).
- **`doctor [--json]`** lists every environment's checks (§12): this machine's, then on
  Windows each WSL distro's (a running one's read by its own Linux dv, a stopped one listed as
  stopped, never booted; none when this dv has no Linux build beside it, which this machine's
  list says as a skipped `wsl` row without starting `wsl.exe`). It changes nothing. Exit 0 when
  no check needs fixing (missing, out of date, failed), else 1, its document printed either
  way. The text is a block per environment (`Windows`, `Ubuntu (WSL)`, `Debian (WSL, stopped)`),
  then `  <state>  <title>  <detail>` per check and, under one that needs fixing, `    fix:
  <command>`.
- **`setup path [--remove] [--yes]`** puts dv on `PATH` (§12.2 item 3): on Windows this
  executable's folder appended to the user `Path` (`HKCU\Environment`), `WM_SETTINGCHANGE`
  broadcast (a hidden `--dir` names another folder: tests, the installer's hooks for the
  installed copy, §13.3); on Linux and inside a distro `~/.local/bin/dv` linked to this dv
  and, when `~/.local/bin` isn't on the user's shell's `PATH`, a line in its startup file;
  macOS the same as Linux. `--remove` takes back exactly that folder, link and line. It
  asks as `setup agents` does. `--json`: `{dir, action: added | unchanged | removed | absent,
  link, rc_file}`.
- **`setup all [--yes]`** (and `dv setup` alone) applies every fix dv can apply that `doctor`
  lists, here and in each running distro (dv installed there first, then the distro's own
  fixes through its dv), and names the ones only a command fixes (`gh auth login`, `git config
  --global user.name …`). It asks as `setup agents` does, showing the plan. `--json`: `{steps:
  [{environment, check, outcome: applied | failed | manual, detail, command}]}`; exit 1 when a
  fix failed.
- **`setup wsl <distro> [--yes]`** (Windows; elsewhere exit 2, `unsupported`) installs this
  build of dv in the distro, or checks that it is installed and intact (its sha256), and points
  the distro's `~/.local/bin/dv` at it: §5.2's install, ahead of the app's first open there. It
  finds the Linux build beside `dv.exe` (`DV_LINUX_DIR`) and the distro without booting it
  (`wsl.exe --list`), then boots it to look. It asks as `setup agents` does (a terminal is shown
  the plan and asked; a pipe or `--json` without `--yes` is exit 2; "no" is exit 1,
  `cancelled`; nothing to change needs no answer). `--json`: `{distro, arch, build, installed,
  linked, path}`, `installed` saying whether this run installed it and `linked` whether
  `~/.local/bin/dv` runs it now: a link the install couldn't make (§5.2) is a warning on stderr
  saying how to fix it, and the command still succeeds. WSL, the distro or the Linux build
  unavailable (and a `DV_WSL_HOME` that isn't a POSIX path) is exit 4; an install that fails
  exit 1 (`io`).
- **`backend --stdio`** serves the protocol (§5.1) on stdin and stdout until stdin ends, then exits
  0 at once; the app starts it (inside a distro, and as the tests' loopback). `dv backend` without
  `--stdio` is exit 2 (`usage`).
- **PRs** (`review new --pr`, `review submit` and `review refresh` of a PR review, `dv open` of a
  PR): §9.10.
- **`dv` and `dv open`** start the GUI detached (§13.1) and return 0 at once; `--automation`
  and `--no-detach` run it in the same process. It opens on the repository named (the path,
  `--repo` or `--wsl`; `dv open` alone: the working directory's): the repository becomes a known
  one and the switcher opens on its uncommitted changes, as the automation `open` command does
  (`Shell::open_repo`); or, with `--review <id>` (the flag only), straight on that review through
  `Shell::open`. Bare `dv` opens the working directory's repository when it is one and nothing
  otherwise (an automation run opens nothing). Before detaching, a path that isn't a repository
  and a review that isn't there are errors in the terminal (exit 1); in the app they are the main
  area's banner (DESIGN §4.4). The headless commands' `--json` and `--author` mean nothing to the
  app, which prints no document and always acts as the reviewer: on a `dv` or `dv open` line
  they are exit 2 (`usage`, a JSON error document with `--json`), so an agent that asked for JSON
  gets JSON rather than a window. Starting detached hands the app none of this process's
  standard handles (§13.1), so a caller capturing `dv open`'s output gets its end at once. A
  build without the app (§13.1's headless shape, the Linux `dv` inside WSL) has none of the app's
  options, and its `dv` and `dv open` exit 2 (`unsupported`): inside a distro
  (`$WSL_DISTRO_NAME`) saying to open the repository from Windows, `dv open <path>` with the
  path as Windows spells it: `--wsl`'s, a path through a distro's share as it is (any
  distro's), a Windows path as it is, else the POSIX path named (or the working directory's)
  made absolute, as `\\wsl.localhost\<distro>\<path>` — but a path on a Windows drive under
  WSL's default automount root (`/mnt/d/code/api`, in `--wsl` too) as that drive's path
  (`D:\code\api`), which the app opens locally rather than through the distro over 9P; quoted
  only when it must be, in double quotes, which PowerShell and cmd read alike, or in
  PowerShell's literal single quotes for a path with `$`, a backtick or a double quote, which
  PowerShell expands or ends inside double quotes (`dv_cli::output::windows_arg`: the one rule
  for every command dv suggests to paste on Windows, the "another distro" error's too).
  Elsewhere it says that this build has no window.

### 10.2 Which review a command means
1. `--review <id>` (or the command's own `<id>` argument; naming two different reviews is a usage
error); 2. else `$DV_REVIEW`; 3. else the single open review whose target matches the current
context, the one open review whose home (§2.2) is the cwd's worktree
(`dv_core::review::ops::reviews_in_context` — a `working` or `staged` review made in this worktree,
or a `branch` review whose branch, however it is spelled, is checked out here: the rule a review's
target resolves by, so the review in context here is the one read here); 4. else exit 2 with
the candidates listed: every open review of the repository (`id`, label, status, round and the
`worktree` it was made in; a `working` or `staged` review labelled by its own worktree's `HEAD`,
"working tree vs feature", whichever worktree the command runs in), in the message and as
`error.candidates`, with the code `ambiguous_review` (several matched) or
`no_review` (none did). Never a silent "newest draft" guess. A named review that doesn't exist is
exit 1 (`not_found`); a finished one may still be named.

Commands that take a thread id (`comment show | reply | address | resolve | reopen`, and
`comment edit | delete`, whose id may be a reply's, §19.1) find the thread's review themselves
(thread and reply ids are unique across a repository's reviews), so an agent
never needs `--review` for them; `--review` narrows the search to that review, `$DV_REVIEW`
doesn't. A deleted thread is not found.

### 10.3 Output and exit codes
- `--json` prints one JSON document on stdout, compact on one line, `"schema": 1` first: the
  documented DTOs of `dv_cli::dto` (its module docs list every document and field), never the
  internal event or state structs. snake_case throughout; timestamps in ms since the epoch;
  every key present (`null` when empty). A review: `id`, `target` (`kind`, the spec's fields,
  `label`; a `working` or `staged` target is labelled with the `HEAD` of the worktree the review
  was made in, `dv_core::label::Labels`, as the GUI's switcher labels it), `status`, `round`, `rounds` (`{n, verdict, body, closed_at}`), `threads` (`{open,
  addressed, resolved, annotation}`, §19), `worktree`, `created_at`, `updated_at`, `status_note` (the note the last
  status change carried, `{status, note, author, at}`, or `null`). A thread: `id`, `review_id`,
  `annotation` (`true` for one an agent opened as an annotation, which it stays once a reply
  opened it, §19), `state` (`open`, `addressed`, `resolved`, `annotation`), `placement` (`unchanged`; `moved` — relocated now, or standing where an earlier
  write recorded its move, `review::shows_moved`; `edited` — standing where it was written, its
  text changed there, `review::shows_edited`; `outdated`; or `file`, a comment on the whole
  file standing in it), `outdated`, `file_level` (`true` for a comment on the whole file: its
  `anchor.original` lines are 0, `code` is empty and `anchor.current`, `code_context`,
  `current_code`, `excerpt` and `trail` are `null`; `outdated` once its file is gone or
  deleted), `path` (its file in the current diff, a rename followed), `side`, `anchor: {original: {round, path, side, start_line, end_line},
  current: {path, side, start_line, end_line} | null}`, `code` (the lines it was written on, as
  they were), `code_context` (`{before, after}`: up to `ANCHOR_CONTEXT` lines that stood around
  `code` then, from its first anchor; non-null exactly when `anchor.current` is `null`, a
  comment on the whole file aside),
  `current_code` (the lines at `anchor.current` now), `excerpt` (the review's diff around
  `anchor.current` now, the rows the text prints, below, joined by `\n`; non-null exactly when
  `anchor.current` is), `trail` (an outdated thread's code since its round, §8 "Round history":
  `{round, went, path, start_line, end_line, before_line, code, excerpt}`, `went` `replaced` —
  lines `start_line..end_line` of `path` replaced it, `code` those lines now and `excerpt` the
  diff around them as `excerpt` is for a found thread — or `removed`, standing before
  `before_line` now, `code` and `excerpt` `null`; else `null`; `placement` stays `outdated`),
  `body`, `author: {kind, name, model}` (`model` the one an agent named, else `null`, §2.5), `created_at`, `note` (the last state change's), `replies: [{id,
  author, body, created_at}]`. The writes print: `review new` `{created, review}`; `review done` and `close`
  `{changed, review}`; `review submit` `{review, round}`; `review delete` `{deleted}`; `comment
  add`, `comment annotate` and `comment edit` `{thread}` (an edited reply: `{reply, thread}`);
  `comment delete` `{deleted, thread}` (`thread` `null` once the thread itself is deleted); `comment reply` `{reply, thread}`; `comment address | resolve | reopen`
  `{changed, thread}`; `setup agents` `{agents: [{agent, path, action: installed | updated |
  unchanged | skipped, reason}]}`; `setup wsl` `{distro, arch, build, installed, linked,
  path}`; `doctor` `{ok, environments: [{name, kind: windows | macos | linux | wsl, running,
  note, checks: [{id, title, state: ok | missing | out_of_date | failed | skipped, detail, fix:
  {action, command} | null}]}]}`; `setup path` and `setup all` as in §10.1;
  `review wait` as in §10.1.
- Within a schema a document only grows: readers ignore keys they don't know; a key removed,
  renamed or retyped, or a value whose meaning changes, is a new schema number.
- Exit codes: `0` ok, `1` operation error (and `dv doctor` with a check to fix), `2` usage / ambiguous review, `3` wait timeout, `4` backend
  unavailable (the repository's WSL distro can't be reached: WSL, the distro or this app's Linux
  build unavailable, §10.4), `5` GitHub error (with `gh`'s message, code `github`, §9.10). A command run inside a
  distro (§10.4) exits with the code of the dv there, whatever it is.
- Errors: stderr always (`dv: <message>`, then the candidates and the failed git command); with
  `--json` also `{"schema": 1, "error": {"code", "message", "detail"?, "candidates"?}}` on stdout.
  Codes (`dv_cli::error::code`): `usage`, `unsupported`, `ambiguous_review`, `no_review`,
  `not_found`, `not_a_repository`, `not_allowed`, `rejected`, `finished`, `unseen_round`,
  `cancelled`, `backend_unavailable` (exit 4), `target` (also a review whose home worktree is gone),
  `corrupt`, `git`, `io`, `internal`, `github` (exit 5), `not_postable`, `pending_review` (§9.10). clap's own parse errors
  are `usage` documents too whenever `--json` (or a `--json=…` spelling, which clap refuses for a
  flag) is anywhere on the line before a bare `--`, their message naming
  what is wrong (the missing arguments after "the following required arguments were not
  provided:"; a command group named alone, `dv --json review`, for which clap prints the group's
  help on stderr, "'dv review' requires a subcommand but one was not provided"); `--help` and
  `--version` print on stdout and exit 0. `--version` is `dv <version>`, followed by
  `(development build)` unless `script/package` built it (§13.2); `dv status --json` names the
  same as `dv: {version, channel}`, `channel` `release` or `development`
  (`dv_backend::build`).
- Human output: short stable lines, fields two spaces apart, no colour. A thread's place reads
  `path:12`, `path:14-16 (moved from 12-14)` or `path (outdated, was 12)` (with its trail,
  `path (outdated, was 12, replaced by 40-45)` or `(outdated, was 12, removed before 40)`), and a
  thread on the old
  side says so first (`path:12 (old)`, `path:14 (old, moved from 12)`), as the GUI names it
  (DESIGN §4.1). A comment on the whole file reads `path (file)`, `path (file, old)`,
  `path (file, outdated)`. The candidates of an `ambiguous_review` / `no_review` error follow the message,
  one per line: `id  label  status  round n  worktree`. No line ends in whitespace.
- **A thread's block** (`comment list`, one per thread after the review's line, each after a
  blank line; `comment show`: the review's line, a blank line, the block; `comment list
  --oneline`: one `id  state  place  <the comment's first line>` per thread instead):
  - The header `id  state  place`, the place as above.
  - A comment on the whole file has no code: one line, `on the whole file` (once its file
    left the review's changes, `on the whole file, which isn't among the review's changes now`;
    its file deleted, `on the whole file, which is deleted now`; on the old side of a file new
    now, `on the whole file as it was, which is new now`), in place of what follows.
  - The excerpt (`dv_core::review::excerpt`), one row per line: `>` on a commented line or a
    space, the line's number on the review's new side now (right-aligned to the widest; blank
    on a removed line), a space, `+` / `-` / a space, ` |`, then a space and the line, its
    whitespace at the end left out (an empty line, or one of only whitespace, ends at `|`; the
    JSON `excerpt` is these same rows). Every number printed beside code is a line as it is now; old numbers
    appear only as words in the header. The window is `ANCHOR_CONTEXT` (3) lines of the
    thread's side before and after its lines, plus every row of the other side between them;
    an old-side thread's `>` rows are its old lines (a removed one unnumbered). A run of more
    than 8 other-side rows keeps its first and last 3 with `⋮ N more removed lines` (`added`
    for an old-side thread) between. A commented span of more than 24 lines of the thread's
    side keeps its first and last 10 of them, with the other side's rows among those kept, and
    hides the rows from just after the 10th to just before the last 10th: `⋮ N lines` between
    (N of the thread's side), or `⋮ N lines, M removed` (`added` for an old-side thread) when
    the hidden rows hold M of the other side too; so a 20-line range with removed rows among
    them is shown whole. A line is cut at 240 characters, ending in `…`. A scope
    row leads: the nearest line of the thread's side above the window that starts with an
    ASCII letter, `_` or `$` (git's default hunk-header rule, language-blind), followed by a
    lone `⋮` when lines lie between; it is left out when such a line already stands in the
    window above the commented lines.
  - When the code changed since the comment (`current_code` isn't `code`: edited, or found by a
    fuzzy match), `as commented in round N:` follows, with the lines as written as `> | line`,
    unnumbered; over 24 of them, the first and last 10 with `  ⋮ N lines` between, the span
    rule above.
  - An outdated thread has no excerpt: only `as commented in round N (not found in the file
    now):`, then `code_context.before` as `  | line`, the code as `> | line` (bounded by the
    same span rule), and `code_context.after`, unnumbered: lines to search for. With a trail,
    where that code went follows: `code changed since round N; these lines replaced it
    (path:40-45):` and the excerpt around those lines, their rows marked `>`; or `code changed
    since round N; it was removed: it stood before line K of path` (`at the end of path`). These
    rows, too, end without whitespace.
  - Who wrote what: `<kind> <name>, round <n>:` above the comment, `<kind> <name>:` above each
    reply, `<state> note:` above the last state change's note, all at column 0; every body is
    indented 4 spaces, each line's whitespace at its end left out and its blank lines left
    empty, so nothing anyone wrote reaches column 0,
    where it could pass for a header or a code row.
- A closed round, where `review wait` and `review show` print it: `round N closed: <verdict>`,
  then its summary in full, indented 4 the same way (`review show` prints earlier rounds on one
  line each, `round N closed: <verdict>: <the summary's first line>`).

### 10.4 WSL routing
Inside a distro, `dv` is the native Linux binary and runs every command itself; it never starts
`wsl.exe`. On Windows, a command that reads a repository (`status`, `review …`, `comment …`; not
`setup`, `doctor` nor `backend`) whose repository is a WSL
location (§10.1: `--wsl`, `--repo \\wsl.localhost\…` or `\\wsl$\…`, or a working directory in
a distro) runs inside the distro instead (`dv_cli::reexec`): one crossing for the command, no
per-git-command spawns (D6), and the app's `Connections` play no part.
- **The line:** `wsl.exe -d <distro> --cd / --exec [/usr/bin/env HOME=<DV_WSL_HOME>] /bin/sh -c
  <run script> sh <x86_64 hash|-> <aarch64 hash|-> <folder> <args…>` (`dv_backend::wsl::RUN`,
  §5.2's launch with its choice of build): it runs `~/.local/share/dv/bin/dv-<hash>`, this
  build exactly, never whatever `~/.local/bin/dv` is, in `<folder>`: the working directory's
  path when that is in the distro, else the repository's (`/` when the folder isn't there:
  `wsl.exe`'s own `--cd` prints an error and runs in `/` anyway, so the script changes folder
  itself). The exit code is the command's, passed through. When stdout and stderr are both
  terminals the child shares the caller's console (no console flags), so the distro's dv sees
  a terminal. Otherwise (a pipe, a file, an agent's shell) dv pipes the child's stdout and
  stderr and copies them into its own: `wsl.exe` relays the two streams with a write position
  each, so a caller sending both to one file (`> out 2>&1`, Claude Code's shell on Windows)
  would get them written over each other. stderr is copied as it arrives, stdout whole once the
  child has exited (no command streams its stdout; `review wait` prints once, at its end). It
  joins dv's kill-on-close job
  (§5.1), so a `dv` that is killed (a tool's timeout, a `taskkill` without `/T`) takes its
  `wsl.exe` with it, and with that the distro's `dv review wait`; Ctrl-C reaches it through the
  console as before. The distro is named as given (`wsl.exe` takes names without case): no
  `wsl.exe --list` runs first.
- **The arguments** are the command line as parsed (without the app's own options, which a
  build without the app lacks), the repository first as `--repo <its path in the distro>` and the
  `--repo` and `--wsl` given dropped, a `--file` as one word, `--file=<path>` (so a path that
  starts with `-` stays the option's value there), a path spelled through the distro's share
  (`\\wsl.localhost\Ubuntu\…`) as its path there. A `--body-file` or `--note-file` is read on
  Windows (stdin for `-`, else the file relative to the working directory, so a Windows path
  works; a file must be UTF-8, checked here so the error names it) and handed to the child's
  stdin as `-`, which also lets the second run after an install lose nothing; otherwise the
  child's stdin is empty.
- **Who it acts as** crosses too: `CLAUDECODE`, `DV_AUTHOR`, `DV_MODEL` and `DV_REVIEW`, those set, join
  `WSLENV` as `NAME/u` after whatever else it lists, in its order. An entry it already has for
  one of them, in any case or with any flags, gives way to that `NAME/u` (`/w` wouldn't pass the
  value in, `/p` would translate it as a path, `dv_author` would export another name), so each
  crosses verbatim under its own name.
- **Missing pieces:** the run script exits 120 when this build isn't installed there; dv then
  installs it (§5.2's install, the distro's architecture from the install check; "dv:
  installing dv in Ubuntu…" on stderr) and runs the command once more. No `wsl.exe`, no such
  distro, no Linux build beside `dv.exe` (or in `DV_LINUX_DIR`), none for the distro's
  architecture, a `DV_WSL_HOME` that isn't a POSIX path, an install that fails, and `wsl.exe`
  failing to start the distro (an exit code no Linux program has) are exit 4
  (`backend_unavailable`, a JSON error under `--json`) saying what is missing. The Linux build
  and `DV_WSL_HOME` are checked before any `wsl.exe` starts; only once `wsl.exe` couldn't start
  the distro does `wsl.exe --list` (which boots nothing) word why: no such distro, or WSL not
  installed. What `wsl.exe` itself printed then (UTF-16LE, on its stdout) is the error's
  detail ("wsl.exe said: …"), never dv's stdout, which under `--json` is the error document
  alone; on a console it shows there as `wsl.exe` printed it. An install whose
  `~/.local/bin/dv` link couldn't be made is a warning on stderr, and the command runs.
- `DV_WSL_HOME` is the sandbox seam of §5.2 here too, and the automation host's `cli` children
  get a `DV_LINUX_DIR` without a Linux build unless the app is sandboxed itself, so no test or
  scenario runs a command in a real distro home (§14.2).

### 10.5 The agent skill
The `dv-review` skill is `crates/dv_cli/skill/SKILL.md`, embedded in the binary
(`dv_cli::skill::SKILL_MD`), frontmatter `name: dv-review` and a description that triggers on
"address review comments", "dv review" and "the reviewer left comments". It teaches the loop
`dv status --json` (keep `current.id`; no current review and none clearly the one asked about:
tell the user, never guess or start one) → read the round's summary (`current.rounds`' last
`body`, `round.body` in the wait's output: a reviewer may ask for more there than in the
threads) → `dv comment list --status open`, read as text (each thread's block, §10.3: the
diff around its code now, every number a line as it is now, what each row's marks mean), and
`--status addressed` for the reviewer's replies on threads already addressed → for each
thread, `dv comment show <id>` again before the fix only when the agent has edited that file
since it listed it (its numbers go stale then), acting on the numbered rows (`anchor.current`
/ `current_code` / `excerpt` in `--json`) and never on a header's old numbers → fix → `dv comment reply <id> --body-file -` (a quoted heredoc
in a POSIX shell, a PowerShell here-string piped in, both shown in the loop) → `dv comment address
<id> --note '…'` → once every open thread is answered and the summary acted on, `dv review done
<id> --round <n> --note-file -` (a Markdown list piped in; the id, because a review approved
meanwhile is no longer in context, and only named does `done` hear it is `finished`; `<n>` the
round the agent worked on, `round.n` from the wait or the last of `current.rounds`; on
`unseen_round`, read the round it names and run `done` again with its number) → `dv review wait <id> --timeout 600 --json` → again on
`changes_requested` (also with `threads.open` 0), stop on `approved` (committing only if the
user's instructions say so) or `closed`. A thread whose `author.kind` is `agent` is a question an
agent asked the reviewer, not a request: left open while it waits for their reply, acted on once
they replied (`review wait`'s text sets such questions apart from the threads to address; its
`threads.open` counts them). It says what `target.kind` means for where the agent's fixes show: a `working` review
is the files on disk (work in its `worktree`), a `branch` one too in whichever worktree has the
branch checked out (its tip when none has), a `staged` one what is staged (fixes show only once staged: ask the user before `git
add`), and a `commit` or `range` one fixed history (tell the user rather than hand back). Its
rules: threads read as dv prints them, `--json` when parsing (`status`, `review show`,
`review wait`, whose ids and round numbers the agent keeps), `--author <agent>` on every write, `<agent>` being the agent's own
name, which every example writes as that placeholder (Claude Code is recognised by `CLAUDECODE`
anyway), and `--model '<model>'` beside it, the model the agent runs on by the name its own
instructions give it (§2.5), left out rather than guessed when nothing tells it, never `resolve` or `reopen`, no `dv review new` unless the
user asks (handing work over, below, is the user asking), text through `--body-file -` / `--note-file -` and a short note inline only in single
quotes (double quotes run backticks in a POSIX shell, and a backtick is PowerShell's escape), a
disagreement is a reply with reasons and still an address with a note saying so;
and what each exit code means (1 `finished`: stop; 1 `unseen_round`: read the round, then `done
--round`; 2 with candidates: pick with `--review`, with none: tell the user; 4: dv couldn't
reach the WSL distro the repository is in, so tell the user what the message says is missing).
Its **Handing work over** section (§19.2) is the start of the loop when the user asks the agent
to put its work up for review in dv (or the project's instructions say to): `dv review new
--json` (keep `review.id`), annotations with `dv comment annotate` and what makes a good one,
`dv review done <id> --note-file -` with the overview, then `dv review wait`; the description
triggers on "review in dv", "hand it over for review" too.
Every
command line in it works as written: a test parses each fenced command and runs the loop
against a fixture repo (`crates/dv_cli/src/tests/skill.rs`).

`dv setup agents [--yes]` installs it, idempotently, repairing a copy that drifted, for each agent
set up in the home directory (`%USERPROFILE%` / `$HOME`): Claude Code when `~/.claude` exists,
at `~/.claude/skills/dv-review/SKILL.md`; Codex when its home (`$CODEX_HOME`, else `~/.codex`)
exists, at `~/.agents/skills/dv-review/SKILL.md`, the user-level skills folder Codex's
documentation names (checked 2026-10-09; `$CODEX_HOME` holds Codex's own state); Cursor when
`~/.cursor` exists, at the same `~/.agents/skills/dv-review/SKILL.md`, which Cursor reads too
(its documentation, checked 2026-10-17): one file the two share (`dv_cli::checks::skills`). Each file is
written beside itself and renamed into place. Without `--yes` a terminal is shown the plan and
asked; anything else (a pipe, `--json`) is shown the plan and refused with exit 2 ("pass
`--yes`"); a "no" is exit 1 (`cancelled`); nothing to change needs no answer. `--json` reports
`{agent, path, action: installed | updated | unchanged | skipped, reason}` per agent. A hidden
`--home <dir>` stands in for the home directory, so tests never write the real one. Inside a WSL
distro the skill is the distro dv's to install (`dv setup agents` run there, §12.2), and the
app's setup screen and its out-of-date notice cover each distro too.

---

## 11. UI architecture (`dv_ui`)

The sibling app's shape: per-view directories, a services layer of entities installed as a gpui `Global`,
typed events, and `is_settled()` on every service for automation.

**Services** (`dv_ui::services`): `Locations` (one `Arc<dyn Backend>` per repo, opened off the UI
thread and cached by the location asked for and its worktree root; a route can be registered for
a path, an environment overlay and optionally a loopback connection, which the automation host
does for its seeded repos; it holds the app's `Connections` (§5.1) and opens a WSL location
through its distro's, a `RemoteBackend`, following each connection's state so the status bar
repaints as it changes, and telling its subscribers when one goes down (reconnecting, stopped,
failed) or comes back (`LocationsEvent::ConnectionChanged`). An explicit open (`open`) is the
connection's explicit start, booting a stopped distro and starting a failed or stopped
connection once more, and waiting out a reconnect; the listing's `reach` only ever starts one
automatically, and reports a distro that isn't running, a connection reconnecting and one that
failed instead (§5.2's connecting rule). A cached local backend is reused while its worktree is
still on disk; a cached remote one is kept, its connection started as the open allows, also
while that connection is down, a repo gone inside the distro surfacing as the next call's
error. Which connection a location goes through (`key_for`, asked on the UI thread) is known
without the file system: a distro's, else what the last open of it found, its route matched by
canonical paths in that open's background job. A distro is named as WSL spells it
(`canonical`): `wsl.exe` takes names without case, so `\\wsl$\ubuntu\…` and
`\\wsl.localhost\Ubuntu\…` are one repo through one connection, which is named from `wsl.exe
--list` — an explicit open of a distro dv hasn't seen lists WSL's distros first, off the UI
thread — so every location it hands back is spelled WSL's way), `Reviews` (the known repos and, per repo, its
reviews (`ReviewStore::list`, an unreadable log a row of its own), its uncommitted changes and its
local branches with their stack bases (§2.2, for the branch picker),
computed in the background when a repo is added, when the switcher opens, and when a
connection a row goes through goes down or comes back, whether or not the switcher is open (one
that comes back clears what its rows said about it at once), through `Locations::reach`: a
stopped distro's repo a row of its own whose **Start** connects to the distro, a failed
connection's whose **Retry** connects again, a reconnecting one's keeping its last listing
(§5.2); it opens or
resumes a target's review as the reviewer, whose name is the repo's `user.name`, or loads a review
by id (the switcher's rows for reviews of other targets; a remote repo that can't be reached
fails as "couldn't open Ubuntu:/home/kyle/api", the connection's error and diagnostics its
detail), either way bound to the review's home
worktree (§2.2: its backend, opened through `Locations`, which the session resolves, watches and
snapshots, while the repo it was opened from is the one marked used, the row the switcher
refreshes and the one the source picker offers); a row records why a repo can't be opened
at all and the worktree root its reviews name; a known repo is a worktree root, its distro
spelled WSL's way, and one added before it could be resolved (a subfolder added while its
connection was down, so its row could say why) or under another spelling of its distro becomes
that root, row and `known_repos.json` re-keyed (or dropped when the root is known already), the
first time a listing or an open resolves it; "Remove from list" (`remove_repo`, DESIGN §4.1) forgets
one, its row, any refresh of it and its entry in `known_repos.json` going, nothing in the repo
touched, and never the open review's, whose targets the source picker lists; repos compare with the distro folded
(`Location::same_place`), so a spelling WSL couldn't confirm never makes a second row; so the switcher shows each target beside its open
review by the rule `open_review` resumes by (`review::ops::is_review_of`); it watches nothing
itself), `ReviewUi` (what the reviewer chose per review: split or unified), `GitHub` (gh calls, the
PR mirrors' refresh and the picker's lists, §9), `Settings` (material, mode, panel width, and the language servers' `lsp`, which the
reviewer writes by hand and which is read again on **Check again** and when the window comes
back to the front, §18.5), `LanguageServers` (every worktree's language servers, shared by the
reviews open on it, §18.5). The **system seam**
(open URL, reveal, folder picker, clipboard writes — recorded instead of performed under automation)
is its own global, `dv_ui::system`, because automation swaps the whole thing for a recorder.

**Data files** (the data dir, §13.3): `settings.json` (`material`, `mode`, `panel_width`, and
`lsp` as the reviewer writes it, §18.4; every write reads the file again first and keeps its
`lsp` and the keys dv doesn't know as they are on disk, writing only the keys dv owns),
`known_repos.json` (`{"v": 1, "repos": [{"location", "added_at", "opened_at"}]}`, ms; deliberately
not the first implementation's `repos.json`, which sits in the same default dir with another shape)
and `review_ui.json` (`{"v": 1, "reviews": {"<review id>": {"view_mode": "split" | "unified"}}}`).
Each is read leniently (unknown keys kept and written back), written atomically in the background,
and never written over when it exists but couldn't be read (`dv_ui::services::persist`).

**Views** (each its own entity and directory): `shell` (title bar, status bar, layout, window
material), `switcher` (repos + reviews, `Ctrl O`), `palette` (`Ctrl K`: reviews, PRs, commands),
`file_finder` (`Ctrl P`: the files of the review's new side, §18.1), `file_list`,
`comments_list`, `diff_pane` (the stream, and the file view in a one-file scope), `thread` +
`composer`, `intel` (code intelligence: the hover card, the `references` panel tab, §18.6),
`submit` (sheet), `onboarding`, `settings`, `banner` (errors and backend state, inline, never
over content). `agents` is how every one of them shows an agent (§2.5): its model's name, and
the model family's mark read from that name.

**Review session:** `ReviewSession` entity (`dv_ui::session`) = one review, its target fixed for
life: the repo and its store, the folded review, the comparison (§2.3: the interdiff since a
round, or all changes) and the `DiffSpec` it resolves to, the file list and each
file's state (unloaded, loading, loaded with its diff and, once they land, its highlights, or
failed), and the watch. The file list loads first; diffs load on demand (the stream asks for the
files around its viewport, at most 4 in flight, then every file a thread shows in, wherever it
is, so threads are placed against their files' current text, §8; "Load anyway" reloads one
without caps). A live
target (`working`, `staged`, a branch) watches the worktree when its new side is the working tree,
the git state and the store (§5.3); others the store only. Worktree and git batches are coalesced
again for 150 ms and refresh the spec and the list, keeping every loaded diff whose entry didn't
change and isn't in the batch (a working tree side's object id becoming known or unknown, which
`git add` alone does, is no change); the branch labels are read again only for a git batch, a
rescan or a moved spec; an append to the review's own log reloads the fold. The watch starts with
the session, and once it is registered whatever was read before it (the list, loaded diffs, the
fold) is read again, since a change made before then is never reported; the same after every
restart. A file that loads again keeps showing the diff it had until the new one lands, so its
cards and an open composer stay put; a switch of comparison lists everything again that way, the
diffs of the other comparison shown until theirs land (an old side listed under the other
comparison is another text, so no thread is placed and no comment anchored against it
meanwhile). A draft comment survives a switch: one whose file the comparison doesn't list, or on
an old side listed under the other comparison, keeps its text and lines, undrawn, until all
changes show it again (`C` switches there and gives it the keyboard); only a file that left the
review drops it. Stopping a watch joins its thread, so the session drops
handles on a thread of their own, never the UI's. A watch that can't cover the worktree leaves
a notice and the session refreshes on window activation instead. Writes go through `review::ops`
off the UI thread and reload the fold; a reply, Resolve or Reopen on a thread that shows moved
records its relocated anchor (§8; a thread whose file was renamed records the name it has now); a failed viewed toggle leaves a banner saying why. The shell holds the open
session; switching reviews swaps sessions (a small LRU of 3 may keep the last ones warm). Every open
goes through `Shell::open` — the switcher's rows, the source picker, automation's `open_review` —
in the background: the main area shows that it is opening, a failure becomes its banner (with
Retry), and resuming the review already open keeps its session; a pick whose connection is the
trouble waits for it (`shell::pending`, §5.2), never replacing by itself an open sheet or a
composer's draft. A review whose backend is remote
(a WSL distro's) follows its connection (`session::connection`): while it reconnects, a call that
fails with `BackendError::Disconnected` raises no banner and loads wait, the status bar saying
"reconnecting…"; the reconnect's synthetic rescan reads the list, the diffs and the fold again,
and a watch that didn't survive starts again. Once the connection has failed for good the main
area shows the backend-down banner (DESIGN §4.4), whose Retry is `Connection::retry` off the UI
thread, and the review's own refresh banner stays down. Paths above the `Backend` trait are never
read as this machine's for a WSL repo: worktree roots compare as the distro spells them
(`review::same_worktree` takes the repo's `Location`), and "Reveal in folder" goes through the
distro's share (`Location::file_path`). The shell hands the session to the
panel's `file_list` and to the `diff_pane`; the file list highlights the stream's current file and
asks the stream to show a file (`DiffPane::reveal_file` / `current_file`, the seam the stream
implements; the current file is the cursor's while it is on screen, else the one at the top).
The keyboard rests on the main area, which always renders with the diff pane's focus handle and
the `DiffPane` key context, so the shell's keys and the stream's both work there (DESIGN §5).
Overlays (the switcher, the source picker, the "Finish round" sheet) take the keyboard while open
and give it back where it was (the main area when that place is gone, such as a filter the panel
took away with it); a menu opens over the sheet rather than in its place, so its unsaved summary
survives, and should the keyboard ever rest on an element no longer drawn, the shell's
focus-lost listener hands it to the main area. Keys come from one table (`dv_ui::keymap`, actions
in `dv_ui::actions`). Viewed toggles flip the state last asked for until the fold reads it back,
one write per file at a time, so quick presses never ask for the same state twice. A load
generation counter on the session is the only stale-completion guard needed, because the target
never mutates: a list result applies only if no newer refresh started, and each file load carries a
token a refresh replaces.

Comment bodies render through `dv_ui::markdown` (gpui-component's `TextView` over the sanitiser of
§15: no image or raw HTML survives to be fetched; links open through the system seam).

**Threads, the composer and finishing a round.** The diff pane places the review's threads as
rows of its stream where `ReviewSession::placement` puts them — `review::place_thread` (§8) over
the review's files (in an interdiff, the target's own list, listed beside it) and the diffs
shown, on the UI thread; since relocation can scan a whole side, each result is kept until
something it read changes (its anchor, the comparison, its file's entry, the diff read), so a
refresh that keeps a file's diff keeps its threads' placements, and a side's fuzzy table is
shared by its threads; a file shown without text to place against (too large to show) has the
review's own diff of it read in the background for the threads anchored in it, kept while the
review's entry for the file stands as read; a closed round's history (§8) is read the same way
for the threads that need it, `session::history` — (`diff_pane::rows`: an insert after the last
line of a thread's range on its side, those lines kept shown rather than folded into a gap, an
outdated card with a trail after the line its code went to, or an outdated card under the
file's header; a thread whose file left the list in the stream's
"Not in the diff" section, a block of its own before the first file, `diff_pane::section`) and
draws them with `dv_ui::thread`'s card. In an
interdiff, old-side threads and threads whose file isn't in it are "not in the comparison":
not drawn, and listed with an "all changes" tag (DESIGN §4.1), placed against the review's own
diff of their file, read in the background, for where they stand in all changes; it also holds the one composer (a new thread under the selection, or a reply inside its
card), the focused thread (`]`/`[`, `R`, `X`) and which resolved threads are open. Posting,
replying, resolving and reopening are the session's `review::ops` writes; the pane counts them
until their follow-up lands, for `settled`. `comments_list` (the panel, under the files) lists
the waiting threads, addressed ones first, each group in stream order, and builds "Copy as prompt" (`thread::prompt_markdown`)
for the clipboard seam; a row's right-click menu resolves, reopens, replies (through the pane)
or copies that one thread. `Ctrl Enter` opens `submit::FinishSheet`, an overlay the shell holds
like the switcher, whose Submit is `ops::finish_round` (§2.3); while that runs the sheet can't be
closed or submitted again, so its outcome (or failure) always shows in it. A review whose status is
final is read-only — the shell's banner offers "Start a new review", which opens the target again (§2.4).

Keyboard map and states: DESIGN §5.

---

## 12. Onboarding, setup and doctor

### 12.1 Shape
- One list of **checks** per environment (`dv_cli::checks`): this machine (Windows, macOS,
  Linux; inside a WSL distro, that distro) and, on Windows, each WSL distro. Each check has an
  id, a title, a state (`ok`, `missing`, `out_of_date`, `failed`, `skipped`) with a one-line
  detail, and a fix: the button dv applies (`apply`, idempotent: Install, Update, Add to PATH)
  and the command that does the same by hand (`dv setup agents`, `gh auth login`); some have
  only the command. Detecting is read-only and fast. The same list backs the first-run screen,
  Settings › Setup, `dv setup` and `dv doctor` (§10.1).
- **The first-run screen** (DESIGN §4.3) shows while the data dir has no `onboarding.json`
  (never the first implementation's `setup.json`, which `%APPDATA%\dv` holds): one screen of
  every environment's checks, **Check again**, **Skip for now** and **Continue `Enter`**, both
  of which write the marker (`{v, finished_at, how}`). It stands in for the columns and the
  status bar; what `dv open` asked for opens behind it as ever, there the moment it is left. An
  automation run shows it only with `DV_FIRST_RUN=1` (every scenario starts on a fresh data
  dir).
- **Setup** stays reachable after the first run: Settings (`Ctrl ,`, the title bar's `⋯`) has
  one page, Setup, the same cards with **Check again**; opening it checks everything again.
  The backend-down banner's **Open setup** opens it too.
- **The out-of-date notice:** a `dv-review` skill installed by an older dv, or edited since,
  shows as out of date with **Update**; when any environment has one, the status bar says so
  once ("dv-review skill out of date in Ubuntu · Update", with a dismiss), since an agent working
  from an old skill misuses the CLI. A skill *missing* for an installed agent is the first-run
  screen's and Setup's to offer, not a nag, except in a distro (below). The notice's Update
  applies every skill fix of that environment. At startup only this machine's skills are
  checked (no git, `gh` or shell runs: the notice needs nothing else; the first run checks
  everything); the distros are checked when the setup screen asks, and each once its connection
  is connected (a WSL repository opened there), never by booting one. Checks run on threads of
  their own, each environment apart, a late one dropped once a newer one of the same
  environment started. Fixes that write one file run once (Codex's and Cursor's skill is one
  file; a distro's `dv setup agents` installs every agent's), and a fix's failure shows under
  its row until the row reads ok.
- **Never blocks local use**, and never blocks a review: opening a WSL repository in a distro
  whose skill is missing or out of date for an installed agent (the distro "isn't set up": dv
  itself it installs on connect, §5.2) checks that distro once its connection is up and shows
  the notice there ("dv-review skill not installed in Ubuntu · Install"), Setup having the rest.

### 12.2 Checks
1. **Environments:** Windows; WSL distros and their running state from `wsl.exe --list` (no
   distro is booted to list it), only when this dv has a Linux build to check them against
   (else this machine's list has a skipped `wsl` row and no `wsl.exe` starts). The distros
   container tools make for their VMs (`docker-desktop*`, `rancher-desktop*`, `podman-machine*`,
   `checks::distro::VM_DISTROS`) aren't the user's and aren't listed. A stopped distro is listed
   as stopped with **Start**, which boots it (`wsl.exe … /bin/sh -c 'exit 0'`, the user's
   explicit start, §5.2) and checks it. A distro listed running is asked afresh
   (`wsl::running_now`) right before anything runs in it, so one stopped since the listing isn't
   booted to check it; one stopped in the instant between that and the probe would still boot,
   a window this leaves.
2. **dv in the distro** (`dv`): this app's Linux build installed and intact and
   `~/.local/bin/dv` running it, or, for a development build, the installed copy's packaged
   build (§5.2's link rule; the detail says which build the link runs) (`dv_backend::install::
   probe`); Install is §5.2's install, with its progress. Until it is there, the distro's other checks aren't listed (a note says so).
   When its dv can't run them, a failed `checks` row says why (so `dv doctor` exits 1).
3. **CLI on PATH** (`path`): **Windows:** the `dv.exe` a terminal runs, found along the
   machine's `Path` (`HKLM\…\Session Manager\Environment`, read only) then the user's (HKCU
   `Environment`): this executable's folder first is ok; absent is missing (Add to PATH);
   another folder's `dv.exe` ahead of it (an older install, the first implementation's
   `dist\dv.exe`) fails, naming that `dv.exe` (the installed copy's, Velopack's `current`
   folder with `Update.exe` above it, reads "the installed dv comes first": what a development
   build run from `target\debug` says once dv is installed, which is right). Applying writes the user `Path`: this folder
   before the first entry holding another `dv.exe` (any later entry of its own taken out), else
   at the end, then `WM_SETTINGCHANGE`; a `dv.exe` in the machine's `Path` stays ahead, which
   only an administrator changes, so that check stays failed, naming the folder, with no button,
   and `dv setup path` says so. `--remove` takes out exactly this folder's entries, leaving no
   empty one. Entries match by their expansion (`%VAR%`), case, quotes and a trailing `\`
   aside. **Linux, each distro and macOS:** `~/.local/bin/dv` linking to this dv (for a development
   build, the installed copy's packaged build will do, which it leaves alone: §5.2's link
   rule) and `~/.local/bin` on
   the user's shell's `PATH`, read from their interactive login shell as the language servers'
   resolver reads it (`dv_lsp::resolve::shell::read_here`, §18.4); else a marked line
   (`checks::path::RC_MARK`) in the file that shell reads as an interactive login shell:
   `~/.zshrc` (zsh), the first of `~/.bash_profile`, `~/.bash_login`, `~/.profile` (bash),
   `~/.config/fish/conf.d/dv.fish` (fish), else `~/.profile`, the shell being the one that
   printed the `PATH`, else `$SHELL`. After adding the line the shell is asked again (still
   lacking it is an error); a line there that the shell doesn't pick up fails the check. On
   **macOS** this route needs no administrator, where the usual `/usr/local/bin` would, and zsh
   (its shell) reads `~/.zshrc` after `/etc/zprofile`'s `path_helper`, so `~/.local/bin` stays
   first. `dv setup path` applies
   it, `--remove` takes it back; on Windows Velopack's install and uninstall hooks call them
   for the installed copy's `current` folder (D9). Velopack runs no hooks on macOS: there the
   first-run screen's **Add to PATH** (or `dv setup path`) links `~/.local/bin/dv` to the
   bundle's `dv`, a path every update keeps.
4. **Agent skills** (`skill-claude-code`, `skill-codex`, `skill-cursor`): per agent set up in
   the environment's home (§10.5), installed, missing, out of date (differs from this build's
   `SKILL.md`) or skipped (agent not installed); Install / Update is `dv setup agents`.
   Optional Claude Code `Stop` hook (§3 D) later.
5. **git identity** (`git`): `user.name` and `user.email` as git resolves them outside a
   repository (`git config --global --includes`, then `--system`, which is skipped when the
   checks run with `GIT_CONFIG_NOSYSTEM`, as a test's and an automation run's do); the fix is the
   `git config --global …` command to run.
6. **GitHub** (`gh`): `gh` there (`DV_GH`) and `gh auth status` ok (with
   `GH_PROMPT_DISABLED=1`), naming the account; a distro's own `gh` is a separate login. When gh
   says nobody is signed in (or to sign in again) the check is missing with `gh auth login` to
   run; any other failure (offline, a timeout) is failed with gh's own first line. dv never runs
   `gh auth login` itself.
7. **Later:** default editor for "Open in editor" (VS Code / Cursor / Zed, with WSL remote URIs),
   `dv://` URL handler and Explorer/Finder "Review in dv" entry come with polish (phase 9). The update
   feed is no check: `settings.json`'s `update.feed` (D9).

**A distro's checks run as that distro's Linux dv** (`dv_cli::checks::distro`), so they read and
write the distro user's own home, shell, `~/.claude`, `~/.agents`, `$CODEX_HOME` and `gh`:
detecting runs this app's build there as `dv doctor --json` and applying as `dv setup agents
--yes --json` / `dv setup path --yes --json`, each through `wsl.exe` on §10.4's run line (`RUN`,
the build by its hash). Not through the distro's backend connection: a backend's processes
belong to an open worktree (`Backend::spawn_stdio`, §18.3), and setup runs before any
repository there is open. Every one of them runs with `DV_WSL_HOME` as `$HOME` when it is set
(the sandbox, §5.2), with `Seams::distro_env` (an automation run's and the real-WSL test's
`DV_GH` naming a program that isn't there, so nothing in the distro asks GitHub).
`DV_ONLY_DISTROS` (comma-separated; set and empty, none) names the only distros the checks
check, start and fix: an automation run reaches no distro but `DV_TEST_WSL_DISTRO`'s, and
its `cli` children get the same restriction.

**Seams** (`dv_cli::checks::Seams`, CLAUDE.md): the home directory, the `Path` values
(`DV_USER_PATH_FILE` names a text file that stands in for the user's registry value and
`DV_MACHINE_PATH_FILE` one for the machine's, empty without it; `dv_cli`'s own tests refuse the
registry outright), the variables `%VAR%` expands from, this executable, `gh` (`DV_GH`), the
environment of the git and `gh` the checks run, the Linux builds, the WSL sandbox and the
distros reached (`DV_ONLY_DISTROS`). The CLI reads them from its environment; the app under
`--automation` points the home at `<data_dir>/home` (git's global configuration there,
`GIT_CONFIG_NOSYSTEM` set so the system's isn't read), the user `Path` at
`<data_dir>/user-path.txt` and `gh` at `DV_GH` or nothing, and its `cli` children get the same
(§14.2).

### 12.3 Failure surface
Every failed check renders as one row: what failed, the exact error, a primary fix action (or
the command to copy). A fix that fails says why under its row ("Didn't work: …"); a Start that
fails says why under its distro's name. The repo banner for a dead WSL backend offers **Open
setup**.

---

## 13. Platforms and packaging

### 13.1 Binaries
- **Windows:** `dv.exe` (console subsystem: CLI, and launches the GUI detached) + `dv-gui.exe`
  (windows subsystem). The split exists only because a Windows exe is one or the other. Both
  are thin `main`s over the `dv` crate's library (`src/main.rs`, `src/bin/dv-gui.rs`, `dv::main`),
  so they share everything; `dv-gui` needs the `gui` feature. `dv.exe` is a console program in
  every build, so a headless command prints and returns its exit code in a terminal. `dv` and
  `dv open` start the GUI detached (`crates/dv/src/launch.rs`): in a packaged build (an
  installed copy) `dv-gui.exe` when it is beside `dv.exe`, else — a development build always,
  so it never starts a `dv-gui.exe` an older build left in `target` — the same binary again
  with the hidden `--no-detach`, `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` on Windows (a
  session of its own elsewhere, `setsid`, so closing the terminal sends it no hangup; on Linux
  inside an AppImage the AppImage again, `$APPIMAGE`, since the mount the running copy came
  from goes when it exits, and only while this executable lies in that AppImage's `$APPDIR`
  (a terminal in another AppImage inherits its variables): `dv_cli::this_executable`; none of
  the caller's descriptors past the standard three either), stdio closed, while the first returns 0 at once; `--automation`
  and `--no-detach` run the GUI in the process. The detached line names what it opens
  absolutely: a path, `--repo` and `--wsl` become `open <absolute path>` (`--review <id>`
  after it), a bare `dv` in a repository `open <its absolute path>`, the app's own options
  kept, so the GUI opens the same thing whatever folder it starts in. `cargo run -p dv` runs
  `dv` (`default-run`). `dv-gui.exe` always runs the GUI in its own process (the Start menu's
  shortcut and Velopack start it, with no console to flash); handed a headless command or a
  line that doesn't parse it says so in a message box and exits 2, the commands being
  `dv.exe`'s, and `--help` / `--version` show their text in an information box and exit 0. Both carry dv's
  icon (DESIGN §1.7) as resource 1, which gpui loads as the window's icon, and a version
  resource (`crates/dv/build.rs`, `embed-resource`, Windows only).
  On macOS an app's Dock and app-switcher icon comes from its bundle, which only the packaged app
  has, so a development build run outside one (`cargo run`, `target/release/dv`) gives AppKit the
  bundle's icon itself at startup (`dock_icon.rs`: `crates/dv/resources/macos/dv.icns` through
  `-[NSApplication setApplicationIconImage:]`), leaving a bundled app's alone.
  A Windows child inherits every inheritable handle of its parent, the caller's stdout pipe
  among them, so the first makes its own standard handles non-inheritable before it starts the
  second: a caller capturing the output (`$(dv open .)`, an agent's shell tool) sees its end at
  once, not when the window closes. The detached GUI has no console, so a fatal startup error is
  a message box (`fatal.rs`): one before the app starts (the data dir) and one inside it (the
  window can't open, the fonts don't load) alike. A panic in the app (outside automation) is
  appended to `crash.log` in the data dir (dv's version, the thread, the message and place, a
  backtrace) before the process ends: a panic inside a window's callback can't unwind and
  aborts, which on screen is the window vanishing (`crash.rs`).
- **macOS:** `dv.app` with two binaries in `Contents/MacOS`, both thin `main`s as on Windows:
  `dv-gui`, the bundle's executable, which the Dock, Finder and Velopack start and which runs
  the app in its own process (a bundle whose executable detached and exited at once would
  leave the Dock with a process that ended), and `dv`, the console program; the CLI is a
  symlink to `dv` (`dv setup path`'s `~/.local/bin/dv`, §12.2; run from the app, whose
  executable is `dv-gui`, it links the `dv` beside it, `dv_cli::this_executable`). A packaged
  `dv` and `dv open` detach to `dv-gui` beside it, as on Windows, `dv` found through its links
  (run as `~/.local/bin/dv`, it is still the bundle's: an app started outside the bundle would
  find no install and no updates); a development build runs itself again. `dv-gui` refuses the
  headless commands as `dv-gui.exe` does, its message on stderr (no message box off Windows),
  and drops the `-psn_…` argument macOS has passed an app Finder starts. Built on the Mac itself (no cross-build from Windows): gpui's
  platform there takes `font-kit` (without it Core Text draws no text) and `runtime_shaders`
  (the Metal shaders compiled when the window opens, so a build needs neither Xcode nor its
  separately downloaded Metal toolchain; the Command Line Tools do), both in
  `crates/dv/Cargo.toml`. Detaching is as on Linux (its own session, stdio closed). The app
  has a menu bar (`dv_ui::app_menu`: dv › About dv, Settings…, Services, Hide,
  Quit; Edit; Window › Minimize) and the Mac's own keys (`⌘Q`, `⌘H`, `⌥⌘H`, `⌘M`); its title
  bar keeps the native traffic lights in a transparent titlebar (DESIGN §2).
- **Linux:** one `dv` binary (GUI + CLI + backend), plus the static musl build used inside WSL.
  The app is X11 and Wayland both (gpui takes Wayland when `WAYLAND_DISPLAY` is set, else
  X11), with client-side decorations: dv's title bar moves the window and holds its caption
  buttons, gpui-component's `Root` draws the frame and its resize edges (DESIGN §2); where the
  platform keeps the decorations (X11 without a compositor) the window manager's own frame
  shows. It opens local repositories only: WSL is Windows', and the app's WSL access answers
  no distros and starts none there (`WslAccess::System` off Windows; inside WSL `wsl.exe`
  would otherwise answer through interop). Its data dir is `$XDG_CONFIG_HOME/dv` (§13.3);
  links, Reveal in folder and the folder picker go through gpui (`xdg-open` and its kin, then
  the XDG desktop portal; the portal's file chooser), the clipboard is gpui's (X11 selections,
  Wayland's data device).

**Shapes.** The `dv` crate's `gui` feature brings the app (`dv_ui`, gpui, gpui's platform with
its Linux `x11` and `wayland` features); `automation` implies it. Three shapes, each linted by `script/check`
step 2:
- **default** (`gui` + `automation`): development, the tests and the e2e runner;
- **the shipped app** (`-p dv --no-default-features --features gui`): Windows, macOS, a Linux
  desktop;
- **headless** (`-p dv --no-default-features`): the CLI and `dv backend --stdio`, no gpui at all
  (step 3 checks its tree), which is the static musl build installed into WSL distros. It has
  none of the app's options, and `dv` and `dv open` exit 2 there (§10.1).

The unit tests in `dv` that need gpui are gated on `gui`; `crates/dv/tests/*` run in every shape.

### 13.2 Builds and gates
Every gate runs locally through one script, `script/check` (bash; also used by agents), before a
change is committed; CI (§13.4) runs the same script on GitHub's runners once it is pushed:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`, then `-p dv` again in the other two
   shapes (§13.1): `--no-default-features --features gui` (the shipped app) and
   `--no-default-features` (headless)
3. the gpui-boundary check (`cargo tree -e normal --target all`, §4): the headless crates with
   every feature, and `dv` without its default features
4. `cargo test --workspace`
5. a plain `cargo build -p dv -p mock_gh -p mock_lsp` (step 4's test build links the binaries
   with gpui's test support), then the e2e scenarios (`cargo run -p dv_e2e --bin e2e -- tests/e2e`),
   background mode; `--fast` skips this step, and `--e2e-only` runs it alone (CI's second job
   beside `--fast`, §13.4). `--foreground` also runs `tests/e2e/foreground/`,
   the scenarios that must take the focus (screen captures of the window materials, §14.2)
6. `--linux`: inside WSL Ubuntu with `CARGO_TARGET_DIR` on ext4, `cargo test -p dv_core -p
   dv_backend -p dv_cli -p dv_lsp -p dv_test_support -p mock_gh -p mock_lsp`, `cargo test -p dv
   --no-default-features` (the backend loopback tests among them) and `cargo clippy -p dv_backend
   -p dv_cli -p dv_lsp -p mock_lsp -p dv --no-default-features --all-targets -- -D warnings`; then `script/build-linux`; then, on
   Windows, the real WSL (`crates/dv_backend/tests/wsl.rs`, `dv setup wsl`'s check and the CLI
   run inside the distro, in-process and as the built `dv`, §14.2) with `DV_TEST_WSL_DISTRO=Ubuntu`
   and `DV_WSL_HOME` a fresh folder under the distro's `/tmp`, `--test-threads=1`. Run inside a
   distro (Linux first: interop puts `wsl.exe` on its `PATH` too), the tests, the lint and the
   build run right there and the real-WSL step, which needs Windows, is skipped with a message;
   `script/build-linux` likewise builds where it runs on Linux and hops through `wsl.exe` only
   from Windows
7. `--linux-gui`: the Linux app (§13.1) inside WSL Ubuntu, in a target dir of its own on ext4
   (`~/.cache/dv-target-gui`: gpui with X11 and Wayland, apart from step 6's headless one):
   `cargo clippy -p dv -p dv_ui -p dv_e2e --all-targets -- -D warnings` and the shipped
   shape's (`-p dv --no-default-features --features gui`), `cargo test -p dv -p dv_ui -p
   dv_e2e` with no display in its environment (gpui's test platform), a plain `cargo
   build -p dv -p mock_gh -p mock_lsp`, then every e2e scenario, which the runner starts on an
   **Xvfb display of its own** (§14.2), never WSLg's; run on a Linux host, the same right there.
   The distro needs the X11, Wayland and fontconfig development packages (`libxkbcommon-x11-dev
   libfontconfig-dev libwayland-dev libx11-xcb-dev`), `xvfb`, and for the portal
   `xdg-desktop-portal` with a backend (`xdg-desktop-portal-gtk`), all installed by the owner

**The Linux build** WSL distros run (§5.2) is `script/build-linux [--aarch64]` (bash, like
`script/check`): inside WSL Ubuntu, `rustup target add x86_64-unknown-linux-musl`, then `cargo
build -p dv --no-default-features --release --target x86_64-unknown-linux-musl` with the target
dir on ext4 and `CARGO_PROFILE_RELEASE_STRIP=symbols` (the release profile itself stays as the
Windows build has it); the result, a static ELF of about 5 MB that `--version` runs, is copied
beside the Windows builds as `target/debug/dv-linux-x86_64` and `target/release/dv-linux-x86_64`
(`--debug-only`: the first alone). `--out <dir>` copies it to that folder alone and `--profile
<name>` builds with another Cargo profile (stripped the same way): packaging's own copy, which
never touches `target/release`. `DV_CHANNEL` and `DV_VERSION` in its environment reach the build.
`--aarch64` adds `aarch64-unknown-linux-musl` linked with `rust-lld` (`dv-linux-aarch64`; it links
here, checked 2026-10-10; when it can't, the script says so and the x86_64 build stands). No
dependency of the headless shape needs a C toolchain beyond what the musl target ships. **The
Linux builds ship beside `dv.exe`, not embedded in it**: an embedded build would force a WSL
build into every Windows build. The app looks in `DV_LINUX_DIR` when set, else its own folder;
packaging puts them beside the installed `dv.exe` (below).

**A Windows release** is `script/package [--publish] [--version <semver>] [--aarch64]` (bash;
Windows only; never run by `script/check`). By default it is a **trial**: everything below, its
installer and feed in the scratch folder `target/package/try`, which no installed copy reads.
**`--publish` is the owner's alone** (no agent passes it): it writes the feed the packaged app
names, `target/package/releases`, so the owner's installed copy updates to it. It builds nothing
until the version is settled, then:
1. **the version** (`dv_package`, a never-shipped helper crate): the workspace's with a prerelease
   of the UTC time, `0.1.0-local.20261018093012`, or `--version`'s; it must be newer than every
   release already in the folder it writes (`releases.*.json` and the packages' names: the trial
   folder, `--out`'s, or the published feed with `--publish`), since Velopack updates only to a newer version and `vpk
   pack` refuses another; later local releases are newer, and the workspace's own `0.1.0`
   once released is newer than all of them. So an explicit `--version` of the workspace's own
   (`--version 0.1.0`, or anything above `0.1.0-local.*`) published to the feed blocks every
   later default local version there: each must then be named with a higher `--version`;
2. **the app** in the shipped shape (`-p dv --no-default-features --features gui`) with the
   `dist` profile (`[profile.dist]`, release's settings: its own `target/dist`, so packaging
   never writes `target/release`, the owner's own build; its limited debug info stays there as
   `.pdb` files, which `vpk pack` leaves out), with `DV_CHANNEL=release`, `DV_VERSION=<version>`
   and `DV_UPDATE_FEED` baked in (`dv_backend::build`, D9), the feed always the published one,
   `target\package\releases`, whether or not this run publishes; `target/dist/dv.exe --version`
   must say that version;
3. **the Linux build** the same way, `script/build-linux --profile dist --out
   target/package/linux` (with `--aarch64` when asked);
4. **the stage**, `target/package/stage`: `dv.exe`, `dv-gui.exe`, `dv-linux-x86_64` (and
   `dv-linux-aarch64`);
5. **`vpk pack`** (found on `PATH`, else `%USERPROFILE%\.dotnet\tools\vpk.exe`; missing, exit 2
   with `winget install Microsoft.DotNet.SDK.8` and `dotnet tool install -g vpk`): pack id `dv`,
   title `dv`, `--mainExe dv-gui.exe`, dv's icon, `--runtime win-x64`, the default channel
   `win`, `--shortcuts StartMenuRoot` (no desktop shortcut), `--noPortable`,
   `--skipVeloAppCheck` (vpk can't read a Rust binary's startup code), into the trial folder or
   the feed: `dv-win-Setup.exe`, `dv-<version>-full.nupkg`, from the second release there on
   `dv-<version>-delta.nupkg`, `releases.win.json` and the older `RELEASES`.
It never runs the installer, `Update.exe` or a built dv but `dv.exe --version`, and stops when
the checkout's drive would have under 30 GB free (not on a GitHub Actions runner). The release
workflow (§13.4) runs it with three more: `--feed <url>` bakes that feed in instead of the local
one, `--linux-dir <dir>` takes the Linux builds from its Linux job instead of building them, and
`--out <dir>` writes the release there (never the local feed, which only `--publish` writes);
`--linux-app` run on Linux builds and packs right there.

**A Linux AppImage** is `script/package --linux-app`, a trial only (no Linux feed exists, so
`--publish` with it is refused): the version as above; the Linux app in the shipped shape
built inside WSL Ubuntu with the `dist` profile in the Linux GUI target dir
(`~/.cache/dv-target-gui`, `CARGO_PROFILE_DIST_STRIP=symbols`), its `--version` checked
there, staged alone in `target/package/linux-app`; then `vpk "[linux]" pack` on Windows —
vpk's directive for packing another OS's app, no .NET needed in the distro — with pack id
`dv`, `--mainExe dv`, `--runtime linux-x64`, the icon `crates/dv/resources/linux/dv.png`
(`crates/dv/tests/app_icon.rs` keeps it the SVG at 256 px) and the category `Development`,
into `target/package/try`: `dv.AppImage`, `dv-<version>-linux-full.nupkg`,
`releases.linux.json`. The AppImage runs on Ubuntu 24.04 without FUSE 2 installed, and it
starts the app there (on an Xvfb display: `dv` detaching through `$APPIMAGE`, the first-run
screen up). The Linux app has no Velopack hooks or updates yet (the
`velopack` crate is a Windows and macOS dependency, D9): it updates by replacing the AppImage.

**The macOS gate** is `script/check-mac [--sync-only] [-- <script/check arguments>]` (bash), run
on Windows: the owner's Mac (`$DV_MAC_HOST` over SSH, key auth; Apple silicon; its
checkout `~/code/dv`; `DV_MAC_HOST` / `DV_MAC_REPO` name others) is made this working tree —
moved to this `HEAD` (sent as a `git bundle` when it hasn't the commit, never through GitHub),
its tracked files reset and untracked ones removed (ignored ones such as `target/` stay), this
tree's uncommitted changes applied (`git diff --binary`) and its untracked files copied (tar) —
then `script/check` runs there in a login shell and its exit code comes back. On the Mac
`script/check` is the same steps 1–5: fmt, clippy in the three shapes, the boundary, the tests
and every scenario (a header's `platforms` skips Windows-only ones), the windows shown on the
Mac's own screen, unfocused, behind nothing (§14.2, "macOS"); `--linux` and `--linux-gui` don't apply there.
The Mac is the owner's machine: nothing is installed on it but the pinned toolchain through
rustup, nothing runs with `sudo`, and its real data dir and agent homes are never touched (the
same rules as on Windows, CLAUDE.md). The macOS-only code is otherwise built only there. A
known gpui behaviour: on every launch gpui's macOS platform writes
`NSAutoFillHeuristicControllerEnabled = false` into the process's user defaults when the key
is unset (a workaround for AppKit's autofill heuristics), so running dv, a test or a scenario
on the Mac leaves that one key in the defaults domain of the executable's name
(`~/Library/Preferences/dv.plist` for a bare `dv`); dv writes no defaults itself.

**A macOS release** is `script/package` run on a Mac (the release workflow's macOS job, §13.4):
`vpk pack` for macOS runs only on a Mac with the .NET SDK 8 and `vpk` installed, which the
owner's Mac doesn't have (from Windows, `vpk [osx]` offers only `bundle`, per `vpk` 1.2.161's
help), so a macOS release is made on GitHub's runner alone. The version as above; the app in
the shipped shape with the `dist` profile and `DV_CHANNEL`, `DV_VERSION`, `DV_UPDATE_FEED`
baked in as on Windows, `target/dist/dv --version` and `dv-gui --version` checked; `dv` and
`dv-gui` staged in `target/package/mac-app`; then `vpk pack` with pack id `dv`, title `dv`,
`--mainExe dv-gui` (§13.1, macOS), the icon `crates/dv/resources/macos/dv.icns` (the SVG on
macOS's icon grid, 32 to 1024 px, kept so by `crates/dv/tests/app_icon.rs`), `--bundleId
io.github.kylekz.dv`, the Mac's runtime (`osx-arm64` on Apple silicon), the default channel
`osx` and `--noPortable`, into the trial folder or `--out`: `dv-osx-Setup.pkg`,
`dv-<version>-osx-full.nupkg` (from the second release on its delta), `releases.osx.json`,
`assets.osx.json`. The bundle is sealed with an ad-hoc signature (`--signAppIdentity -`), with
no certificate and no notarization (D10). `--publish`, `--linux-app`, `--linux-dir` and
`--aarch64` are refused there (no local macOS feed exists).


### 13.3 Install layout
Windows per-user `%LOCALAPPDATA%\dv\` (Velopack), settings in `%APPDATA%\dv\`; macOS
`dv.app` in `/Applications` or `~/Applications` (the `.pkg` asks which; its `Contents/MacOS`
holds `dv-gui`, `dv` and Velopack's `UpdateMac`, the same folder after every update, downloaded
packages in `~/Library/Caches/velopack/dv`), `~/Library/Application Support/dv/`; Linux
AppImage or tarball, `$XDG_CONFIG_HOME/dv/` (`~/.config/dv/` when it is unset). From an AppImage, `dv setup path`
links `~/.local/bin/dv` to the AppImage itself (`$APPIMAGE`, while this executable lies in its
`$APPDIR`), not to the executable inside its mount, which goes when the process that mounted
it exits. Every data path honours `--data-dir` / `DV_DATA_DIR` (tests, §14).

On Windows Velopack puts the app in `%LOCALAPPDATA%\dv\current\` (`dv.exe`, `dv-gui.exe`, the
Linux builds; the same folder after every update), `Update.exe` beside `current`, downloaded
packages in `packages\`. The install hook puts `current` on the user `Path`, ahead of an older
`dv.exe` there, and the uninstall hook takes it off (D9, §12.2); the Start menu's "dv" starts
`dv-gui.exe`. The installed copy's updates come from its feed (D9: GitHub Releases for a
released copy, the packaging checkout's `target\package\releases` for a local one, or
`settings.json`'s `update.feed`). In a WSL distro its Linux build
owns `~/.local/bin/dv` (§5.2's link rule).

### 13.4 CI and releases (GitHub Actions)
The repository is public, so GitHub's hosted runners cost nothing. Two workflows:

- **`.github/workflows/ci.yml`**, on every push to main, every pull request, and for a release
  tag whose commit has no passing run on main (`release.yml` calls it): `script/check` on
  Windows (`windows-2025`), `script/check --fast` on macOS (`macos-15`, Apple silicon: its
  runner's 1024×768 screen is too small for the scenarios' 1440×900 windows, which
  `script/check-mac` runs on the owner's Mac), `script/check --linux` on Ubuntu 24.04 (the
  gates, the e2e scenarios on the runner's own Xvfb with Mesa's software Vulkan, the headless
  crates and the static musl build; the real-WSL step needs Windows with WSL and is skipped).
  On Windows and Linux each runs as two jobs side by side, `--fast` (with `--linux` on Linux)
  and `--e2e-only`, which together are every step (the scenarios take about as long as the
  tests, so the two halves run at once). Beside them, the workflows themselves (actionlint, and zizmor's security audits, pedantic; locally
  `script/check-workflows`), and the dependencies (cargo-deny, `deny.toml`: every crate from
  crates.io with a license on its list, no yanked crate, no open RustSec vulnerability; one that
  doesn't apply is ignored there with why). A failure keeps the e2e screenshots and logs as the
  run's artifact for a week. The caches (Cargo's registry and the target dirs) are saved only
  by a push to main. A push or pull request that changes only documentation (`docs/`, the
  Markdown at the root, `tests/e2e/README.md`) runs nothing, since no gate reads it.
- **`.github/workflows/release.yml`**, on pushing a tag `v*`:
  1. **the tag** (`script/release-check`): an annotated `v<semver>` (a prerelease such as
     `v0.2.0-rc.1` too) whose version without the prerelease is the tagged commit's workspace
     version, on a commit on main; the tag's message is the release notes;
  2. **CI** on that commit, alongside the builds: the push to main that brought the commit ran
     ci.yml on it, and when that run passes (the release waits for it) it stands for this one;
     with no such run, or one that doesn't pass (failed, cancelled, superseded in the queue),
     the release runs ci.yml in full itself, without caches;
  3. **the builds**, with no cache: the static musl `dv-linux-x86_64` (`script/build-linux
     --profile dist`, a job of its own) and the AppImage (`script/package --linux-app`, `vpk
     pack` on the runner), and once the musl build is done the Windows release (`vpk download
     github` fetches the latest release's full package, from which `vpk pack` makes the delta
     and against which `dv_package` checks the version newer, then `script/package --version
     <v> --feed https://github.com/<repo> --linux-dir … --out …`), and the macOS release on `macos-15` the same way (its channel
     `osx`, `script/package --version <v> --feed … --out …`);
  4. **publish**, in the `release` environment, which waits for the owner's approval: the
     files' `SHA256SUMS`, their build provenance (`actions/attest-build-provenance`), then the
     GitHub release, made as a draft with every file and then published, marked latest unless
     it is a prerelease. Its files: `dv-win-Setup.exe`, `dv-<v>-full.nupkg` (and from the second
     release on its delta), `releases.win.json`, `assets.win.json` and `RELEASES` (the feed),
     `dv-osx-Setup.pkg`, `dv-<v>-osx-full.nupkg` (and its delta), `releases.osx.json` and
     `assets.osx.json` (the macOS feed), `dv-linux-x86_64.AppImage`, `SHA256SUMS`.

**Making a release** (the owner's): bump `[workspace.package] version` in `Cargo.toml` (and
`Cargo.lock`, which `cargo build` updates) in a commit on main and push it; then `git tag -a
v<version> -m "<notes>"` and `git push origin v<version>`; once the run's builds are green,
approve its `release` deployment. A prerelease tag (`v0.2.0-rc.1`) runs the same way and never
reaches the feed.

**What keeps a bad release out.** The workflows follow GitHub's hardening guidance, which
zizmor checks: every token read-only but the publish job's, no secrets, checkouts that keep no
credentials, every action pinned to a commit (Dependabot proposes bumps once a version is two
weeks old, `.github/dependabot.yml`), no `pull_request_target`, outside values reaching scripts
only through environment variables, release builds without caches and with `--locked`; the
publish job, the only one that can write, runs no code from the repository. A tag runs its own
commit's copy of `release.yml`, so `script/release-check` and the approval catch a mistaken tag,
not a hostile one: what keeps those out is who can push a tag. The repository's settings (the
owner's, applied once): a ruleset that lets only the owner create `v*` tags and nobody else
update or delete one; a ruleset that keeps main from being force-pushed or deleted; the
`release` environment, which admits only `v*` tags and waits for the owner; immutable releases
(a published release's files and tag can't change); Actions limited to SHA-pinned actions, and
workflows from outside contributors' pull requests waiting for approval. Nothing automated
pushes a tag or approves a deployment.

---

## 14. Automation and testing

### 14.1 Principles
Hermetic by default: every test and scenario uses its own data dir, its own fixture repos, `mock_gh`,
and a local backend. Nothing touches `%APPDATA%\dv` or GitHub unless an owner-only live switch is
set. Nothing touches a real distro but the real-WSL tests, which `DV_TEST_WSL_DISTRO` turns on and
which work only in a scratch home under the distro's `/tmp` (`DV_WSL_HOME`, §14.2), and the
owner's live check (§14.3).

### 14.2 Pieces
- **`dv_test_support::FixtureRepo`** — builds real git repos in temp dirs from a short script: files,
  commits, branches, CRLF + `autocrlf=true`, `.gitattributes`, renames, binaries, a 50k-line file,
  untracked files, worktrees. `FixtureRepo::persistent(path)` builds one at a given path and keeps
  it (its `HOME` and side directories in `<path>.fixture/`), for the automation host's seeded repos,
  which outlive the call that built them; `FixtureRepo::reopen(path)` finds one again (a later
  process, after a scenario's `restart`).
- **`mock_gh`** — reads a scenario file (`argv` pattern → stdout/stderr/exit), serves responses
  recorded from `kylekz/difftest` (scrubbed), and appends every call + stdin to a log tests assert on
  ("exactly these 5 comments were posted with `REQUEST_CHANGES`"). Error scenarios: 422 line-not-in-diff,
  auth failure, rate limit.
- **Backend conformance** — one suite (`crates/dv_backend/tests/conformance/`) runs the same cases
  against `LocalBackend` and against `RemoteBackend` over a server on in-memory pipes
  (`Launcher::InProcess`): repo info, git (failures, timeouts, ok codes, stdin, an index file),
  worktree reads (missing, links, binary, a 5 MB file), `probe_path` outside the worktree (a file
with and without its bytes, only looked at, read past what was asked, a directory, a link, nothing, and the paths it refuses), the store (concurrent appends, atomic
  writes, the index copy), invalid paths, every error's `Display` and `detail()`, watches
  (worktree, store, git state; dropping the handle, the sink gone when `drop` returns and a sink
  in progress finishing first; from inside the sink too), and processes (§18.3), also against a
  server that is a process of its own: output, stderr lines and the exit in order, a megabyte
  through stdin and back, the working directory and environment, refusals, a kill, a drop (from
  inside the sink too) and an exit each ending what the process started; then a server killing
  its processes when its input ends (also one a write is blocked on, and one a `spawn` racing
  that end starts), a process printing faster than the client reads waiting for it while other
  answers get through, a running process holding the server against the idle close, and a lost
  connection ending its processes with an exit. The process is `dv_test_child`, a helper bin of
  `dv_backend` that is never shipped (no shell, no tool of the OS's).
- **Backend loopback** — `crates/dv/tests/backend_loopback.rs` starts the built `dv backend
  --stdio` as a child (`Launcher::Command`, no WSL), in every shape of `dv`, so the protocol,
  reconnect and watch paths run on Windows and inside WSL with no distro setup: the handshake with
  and without a build (and a wrong one refused), a repository's git, store and watch, a kill
  mid-call (`Disconnected` at once, the reconnect, the synthetic rescan, calls working again),
  three blocked respawns ending `Failed` and `retry()` bringing it all back, and the server's end
  when the last connection goes. Each test checks every server it met has exited (no child
  leaks). `dv_backend`'s own unit tests cover the same lifecycle against in-process and fake
  servers with a shortened backoff, and the connecting rule with a stand-in host check: a death
  on a stopped host launching nothing and ending `Stopped` (on a running one, the respawn), a
  first call on a stopped host likewise, an explicit start booting it, the idle close and the
  automatic start after it (the idle period counted from the last watch's end, and a fresh
  count of respawn attempts after it), a listing's host check taken from the cache, an
  explicit start during a reconnect waiting for its outcome, calls failing at once while a
  reconnect installs, and a watch the new server takes back differently (degraded, refused)
  ending for its consumer; a child tied to the kill-on-close job is in it.
- **WSL without a distro** — `dv_backend`'s unit tests decode `wsl.exe`'s UTF-16LE output, read
  the launch markers and build the command lines everywhere, and on Linux run the launch,
  install, prune and check scripts themselves with `/bin/sh` in a scratch home
  (`wsl::Shell::local`), installing and starting a stand-in build; the connection's reinstall
  and `Installing` progress run against a fake launcher.
- **Real WSL** — `crates/dv_backend/tests/wsl.rs` (Windows; only when `DV_TEST_WSL_DISTRO` names
  a distro, `--test-threads=1`) runs every script through the real `wsl.exe` in a home of each
  test's own under `DV_WSL_HOME`, a fresh folder under the distro's `/tmp` (a value elsewhere
  fails the test rather than touch a real home), removed at its end: the distro listed as
  running, scripts and arguments arriving intact, an install streamed and checked (a wrong copy
  refused), a connection installing then shaking hands (`build_ok: true`) and the next one not
  installing again, a repository made under `/tmp` opening as `Location::Wsl` with its git,
  store and a watch seeing an edit made inside the distro, a build changed in the distro
  installed again, the prune, a `~/.local/bin` dv can't write (the install done with a
  warning, the server started), and a distro that isn't there: an automatic start launching
  nothing (`Stopped`), an explicit one `Unavailable` with `wsl.exe`'s words. `dv setup wsl`'s own check (`crates/dv_cli/src/tests/setup_wsl.rs`) runs the same way, as
  do a distro's setup checks read and applied through its own dv (§12.2:
  `crates/dv_cli/src/tests/doctor.rs`, dv installed, a skill missing, installed, edited and
  updated, `PATH`),
  and so does the CLI run inside the distro (§10.4): in-process with captured streams
  (`crates/dv_cli/src/tests/reexec.rs`), and as the built `dv` from Windows
  (`crates/dv/tests/wsl_cli.rs`, where cargo builds and names it): a first command installing
  this build in the scratch home, then `--wsl`, the share's spelling and a working directory
  inside the distro naming the repository, an exit code passed through (`review wait --timeout
  1` → 3, an error's 1 with its JSON), `--body-file -` over a pipe, `CLAUDECODE`, `DV_AUTHOR` and
  `DV_REVIEW` crossing through `WSLENV` (`DV_MODEL` with them, `dv_cli`'s `reexec` tests), a `dv` killed during `review wait` taking the distro's
  `dv` with it (the kill-on-close job), stdout and stderr sent to one file coming out whole, a
  `~/.local/bin` dv can't write leaving the crossing working, and a distro that isn't there as
  exit 4 with its error document alone on stdout. Without a
  distro, `dv_cli`'s tests cover which commands cross, the arguments, the folder and `WSLENV`,
  the exit-4 paths that start no `wsl.exe`, and on Linux the run itself through `/bin/sh` with a
  stand-in build (installed, then run). `script/check --linux` runs all of these after
  `script/build-linux`.

- **Remote reviews in the GUI** — `dv_ui`'s tests open repos through a loopback connection over
  an in-process server (`Launcher::InProcess`, a route registered with `Locations`) and kill it,
  block its respawns and retry it: the session riding out a lost connection and catching up
  through the rescan, the status bar's states and the backend-down banner (also for a host found
  stopped, through a launcher's stand-in host check, `Launcher::with_host_check`), the
  switcher's reconnecting, Retry and Start rows, and the listing that never starts a stopped
  distro (WSL's stand-in, `WslAccess::Scripted`), even after a death. The e2e scenario
  `backend_loopback` does the same against this `dv`'s own `backend --stdio` as a child of the
  app (`seed_repo` with `"backend": "loopback"`, `backend_kill`, `backend_block`,
  `backend_stop`).
- **`gpui_automation` + the `e2e` runner** — toolbox-rs's shared crates
  (github.com/kylekz/toolbox-rs, pinned by tag in the root `Cargo.toml`; its `docs/automation.md`
  documents the protocol, the scenario format and the runner): `gpui_automation`, the in-app
  driver, whose `enabled` feature `dv_ui`'s and `dv`'s `automation` features turn on (without it
  the crate is the no-op stand-ins the views call, which `dv_ui::automation` re-exports), and
  `gpui_automation_runner`, the runner as a library, which `dv_e2e` wraps with dv's profile and
  extensions. A change to either goes to toolbox-rs first, then a new tag here. JSONL scenarios in `tests/e2e/`, named
  targets (`.automation_target("file_list/src/routes/users.ts")`), frame-synced replies,
  `wait_for`/`assert` JSON-pointer checks, background runs that never take focus, per-scenario output
  dir with numbered screenshots and `failure.png`. dv adds custom commands `set_setting` (window
  material and mode), `seed_repo` / `repo_edit` (fixture repos at `<data_dir>/repos/<name>`, built
  and edited with `FixtureRepo` steps, a `write` step's `repeat` making a large file in one line,
  a `worktree` step adding a linked worktree; their environment overlays are registered with
  `Locations` for the work tree and each of its linked worktrees, wherever they are,
  and found again with `FixtureRepo::reopen` after a restart, so scenarios never depend on the
  owner's git configuration), `open` (adds a repo and opens the switcher on it, as `dv open`
  does), `open_review`, `review_op` (an agent's reply, address or "done" straight through
  `review::ops`, without the CLI's rules), and `cli` / `cli_spawn` / `cli_join`, which play the
  agent through the real CLI: the same `dv` executable started as a child process with a
  headless command (never one that opens a window), off the UI thread, in the seeded repo's
  fixture environment (also from one of its linked worktrees) with the app's data dir, a home of its own under it (`HOME`,
  `USERPROFILE`, `CODEX_HOME` its `.codex`: `dv setup agents` never reaches the owner's; the
  app's own setup checks use the same home, §12.2), the user `Path`'s stand-in
  (`DV_USER_PATH_FILE` = `<data_dir>/user-path.txt`: `dv setup path` never writes the
  registry), `DV_ONLY_DISTROS` (the sandbox's distro, else none: `dv doctor` and `dv setup`
  reach no other), and without the
  inherited `CLAUDECODE`, `DV_AUTHOR`, `DV_MODEL` and `DV_REVIEW` (a scenario sets them on purpose); `cli`
  runs one to its end and replies `{exit, stdout, stderr, json}`, `cli_spawn` starts one that
  blocks (`dv review wait`) and `cli_join` collects it, and a child still running when the app
  quits is killed. `seed_repo` with `"backend": "loopback"` reaches its repo through a loopback
  `dv backend --stdio` the scenario can kill (`backend_kill`), stop from starting
  (`backend_block`) and stop the host of, as `wsl --shutdown` stops a distro (`backend_stop`);
  `wsl_distros` tells WSL's stand-in which distros there are and which run; `state.backends`
  lists the connections; `home_file` writes into the scratch home (a fake agent's folder, an
  older dv's skill) and `state.setup` is the setup screen's (§12).
  `workflow_a_agent` runs §3 workflow A end to end that way. The host lists these commands in
  the handshake, so the runner rejects a misspelt one before a scenario's first step. The
  runner gives every launch `DV_DATA_DIR` = the scenario's data dir and no inherited `DV_`
  variable (no `DV_WSL_HOME`: only a header's `env` names one, so dv reaches WSL through its
  stand-in; no `DV_LSP_<ID>`). `dv_e2e`'s fixture wires `mock_gh` and `mock_lsp`: every launch
  gets `DV_GH` = `mock_gh` and `GIT_CEILING_DIRECTORIES` = the scenario's output dir, a scenario
  header's `gh` names the `mock_gh` scenario file (`DV_MOCK_GH`) and its `lsp` the `mock_lsp`
  scenario for a language server (`DV_LSP_<ID>`), and `{"gh_log": {}}` and `{"lsp_log": {}}`
  lines assert on their logs (tests/e2e/README.md). On Linux the runner starts
  an Xvfb display for the run (`Xvfb :<n> -screen 0 2560x1600x24`, the first free `n` from 90,
  ended with the run: `SIGTERM`, then `SIGKILL` after 5 s, and with the runner however it ends,
  `PR_SET_PDEATHSIG`; a lock a dead server left is no obstacle) and every app on it under X11
  alone (`DISPLAY=:<n>`, no `WAYLAND_DISPLAY`), so no window reaches the desktop of the person at the machine (WSLg's,
  under WSL) and `screenshot` is an X11 `GetImage` of a display nothing covers; there the
  window keeps the platform's frame (no compositor) and dv's caption buttons don't show.
  `--host-display` keeps the runner's own display instead: an explicit exploratory look on a
  real compositor, never a gate. A header's `platforms` keeps a scenario to the systems its
  chrome belongs to (`setup` Windows', `setup_linux` Linux's).
- **Screenshots of the window material:** the default capture (`PrintWindow`) includes Mica but
  not Acrylic's blur-behind, and a background window is inactive. A `screenshot` with `"source":
  "screen"` copies the window from the desktop instead; it only works in front, so the scenarios
  that use it live in `tests/e2e/foreground/` and run on demand (`script/check --foreground`),
  never by default (tests/e2e/README.md, "Screenshots and window materials").
- **macOS runs** (`script/check-mac`): the runner holds the display awake (`caffeinate -d -u`
  for the run: macOS stops drawing a window nobody can see, a sleeping display's included); a
  background window opens in front but unfocused (macOS stops drawing a fully covered one), the
  app an accessory (no Dock icon); screenshots are Core Graphics images of the window
  (`CGWindowListCreateImage`, Retina 2x, sRGB), and `"source": "screen"` works there in the
  background too (the window and what is under it as the screen shows them).
  `shell_materials_macos` covers its one material, `setup_macos` its Setup (§12). A line of a
  scenario can be limited to some platforms too (`"platforms"`), for a value that differs by
  platform, such as the title bar's layout around the traffic lights.
- **View tests** with gpui's `TestAppContext`/`VisualTestContext` next to each view (`tests.rs`).
- **CLI tests** run the commands in-process (`dv_cli::main_with` with a `Context` over captured
  streams, an explicit environment and the fixture's git environment) against fixture repos and
  `mock_gh`, from the agent's point of view; a few binary tests run the built `dv`
  (`CARGO_BIN_EXE_dv`) for dispatch, exit codes and real pipes. Neither ever starts the GUI, and
  every one removes `CLAUDECODE`, `DV_AUTHOR`, `DV_MODEL` and `DV_REVIEW` (which Claude Code's environment
  may carry) unless it sets them on purpose.

### 14.3 Owner-only live checks
`DV_LIVE_WSL=1` and `DV_LIVE_GH=1` (§9.11: open, reply, resolve, Viewed and submit a review on
`kylekz/difftest`, `crates/dv_cli/tests/live_gh.rs`). No agent, script or CI run sets these.

The live WSL check is `crates/dv_backend/tests/live_wsl.rs`, `#[ignore]` and inert without
`DV_LIVE_WSL=1`: after `script/build-linux`, `DV_LIVE_WSL=1 cargo test -p dv_backend --test
live_wsl -- --ignored --test-threads=1` installs this build into the real home of
`DV_LIVE_WSL_DISTRO` (default `Ubuntu`) through the app's own connection (replacing the first
implementation's `~/.local/bin/dv` with the link, pruning other builds of `~/.local/share/dv/bin`,
leaving its other folders), checks `~/.local/bin/dv --version` there, opens a scratch repository
under `/tmp` (removed at the end) with a watch round trip, runs the distro's `dv` on it, and
then the Windows `dv` (`target/debug/dv.exe`, so `cargo build -p dv` first) for it, which runs
the command inside the distro (§10.4).

---

## 15. What carries over from the first implementation

Reviewed 2026-10-04 (live automation runs + four code reviews). Keep (port, then tidy):
- `dv_core` review model shapes (flat review/comment/reply, schema `v`, forward-compatible optional
  fields) → re-expressed as events (§7).
- `anchor_spec` mapping (source × side → which blob an anchor belongs to).
- Intraline word diff (`crates/core/src/diff/intraline.rs`).
- Persistent `git cat-file --batch` reader (`crates/core/src/git/batch.rs`), `:(literal)` pathspecs,
  `LC_ALL=C` for parsed git output.
- PR prep: merge-base semantics and verify-after-fetch (`crates/cli/src/pr.rs`).
- Submit: validate-everything-before-posting, the blocked-with-violations result, the do-not-resubmit
  guidance on writeback failure (`crates/cli/src/submit.rs`).
- Markdown body sanitiser (no remote fetches from comment bodies) (`crates/app/src/markdown.rs`).
- The SKILL.md workflow and its `dv review wait` loop (the wait itself is state-based now,
  §10.1).
- The four themes' colour values (DESIGN §1).

Do not carry over: the `Workspace`/`AppShell` god objects, the epoch/generation machinery, the
optional-host WSL routing and its fallbacks, the review lock, the hand-rolled arg parsing, the LSP
client and its node/vtsls provisioning (§18 is a new design), conflict badges, the PR diff cache and workspace LRU, comment archaeology.

Known bugs in the first implementation that v1 must not repeat (each gets a test): CRLF diffs (§6),
visited-only totals (§6.1), Histogram-based PR line validation (§9.3), one bad review file failing
`list` (§7), unpaginated GitHub threads (§9.2), hard-coded `origin` (D7), WSL route frozen at repo
open (§5.2), automation writing the real data dir (§14.1), raw git errors drawn over the sidebar
(DESIGN §4).

---

## 16. Phases

Each phase ends with its e2e scenarios green and a screenshot review against DESIGN.md.

| Phase | Scope | Done when |
|---|---|---|
| 0 | Workspace on `gpui-pre`, crates per §4, `script/check` (§13.2) and `.github/workflows/` removed, `gpui_automation` + `e2e` ported, `FixtureRepo`, `mock_gh` skeleton, window with Mica/Acrylic/Opaque | an empty Glass shell screenshots in all three materials, light and dark |
| 1 | `dv_core`: targets, git diff pipeline (§6 steps 1–3), event store (§7), `LocalBackend` | unit + fixture tests incl. CRLF/attributes/renames/binary |
| 2 | Review screen: switcher, file list (viewed, totals), diff stream (split/unified, gaps, highlight), threads + composer, comments list | Workflow A steps 1–3 by keyboard, e2e |
| 3 | CLI (§10) + skill + rounds/interdiff + relocation (§8) | Workflow A end to end with a scripted "agent" in an e2e scenario |
| 4 | `dv backend`, `RemoteBackend`, WSL install + routing + watch (§5), CLI re-exec (§10.4) | loopback tests pass on Windows and in WSL, and the real WSL ones (the app's connection, `dv setup wsl`, the CLI run inside the distro) in a sandboxed home (`script/check --linux`); the owner's live WSL check (§14.3) is ready for the owner to run |
| 5 | Code intelligence (§18): text selection and `Ctrl F`, the file view and its history, language servers where the repo lives (`spawn_stdio`, `dv_lsp`, `mock_lsp`), TypeScript 7, then Tailwind CSS and CSS: hover, go to definition, find references | a scenario against `mock_lsp` hovers, goes to a definition in the stream and in a file view, steps back and lists references; the real TypeScript check passes on a fixture project; the owner reviews a WSL TypeScript branch with it |
| 6 | Onboarding, setup and doctor (§12): the first-run screen and Settings › Setup over one list of checks per environment (Windows and each distro): dv in the distro, the CLI on PATH, the agent skills (Claude Code, Codex, Cursor) and the out-of-date notice, git identity, `gh`; `dv setup` and `dv doctor` | first run on a fresh data dir → onboarding → review, on Windows with a WSL distro (sandboxed home); an out-of-date skill in the distro is noticed and updated from Settings › Setup |
| 7 | Packaging + updates on Windows with Velopack (D9), local feed: `Setup.exe`, `dv-gui.exe`, dv's icon on the executables, the window and the shortcuts, the install hooks running `dv setup path` (§12), the Linux builds beside the installed `dv.exe` (§13.2); a development build never replaces the installed copy's `dv` in a distro | install from `Setup.exe` → PATH set, icon in the taskbar and Start → a WSL repo opens from the installed copy → a second local release updates it |
| 8 | **Parked** by the owner (dv reviews agents' work before it reaches GitHub; phase 9 goes first). GitHub (§9, in the order of §9.12): which repository and running `gh`, opening a PR into a draft that follows its head, the PR mirror, GitHub's threads shown with reply and resolve, the PR picker and strip, Start round n+1 with its interdiff, submitting through a pending review with validation against GitHub's hunks and the file-comment fallback, Viewed sync both ways, the CLI (`--pr`, `submit`, `refresh`; agents read only) | Workflow C end to end against `mock_gh` and the fixture GitHub; the owner's live check (§9.11) |
| 9 | Polish: settings, palette, light themes, empty states, performance pass on a 2,000-file PR (the Linux app, its gate `script/check --linux-gui` and its trial AppImage are done, §13.1, §13.2; so is Settings › Appearance, the mode and the material, DESIGN §4.3, the `Ctrl K` palette over commands, reviews and files, DESIGN §4.1, removing a repo from the switcher, DESIGN §4.1, and the Claude and Dracula themes, DESIGN §1.3; PRs join the palette's reviews with phase 8) | DESIGN review of every screen; perf budget (§17) met |
| 10 | GitHub Actions (§13.4): CI on Windows, Linux and macOS with the workflows' and the dependencies' checks; the release workflow publishing the Windows and macOS releases (`Setup.exe`, a `.pkg` without a certificate, D10) and the AppImage to GitHub Releases after the owner's approval; the update feed on GitHub Releases, read by Windows and macOS installs | a prerelease tag runs end to end: CI green on all three, the release published after approval, its provenance verified |
| 11 | Annotations (§19, in the order of §19.5; built before phase 8): `comment annotate`, `comment edit` / `delete`, the `annotation` state and a reviewer's reply opening it, the skill's handing-over section, the annotation card, `A`, the counts | the `agent_annotations` scenario green, its screenshots reviewed against DESIGN §4.1 |

---

## 17. Risks and open questions

- **Budgets:** time-to-first-diff < 300 ms on a warm local repo, < 800 ms for a WSL repo with the
  backend connected; scrolling a 20k-line file at 120 fps; opening a 2,000-file PR's file list < 2 s.
  Measured 2026-10-29 on Windows (the `dist` profile, a background e2e run): the first diff of an
  eight-file working review 250–280 ms after `Enter`; 2,001 changed working-tree files listed in
  1.46 s (`git diff --raw --numstat` over them is 1.1 s of it); the stream and a 20k-line file view
  scroll at ~225 fps. Each git process costs ~40 ms to start on Windows, so the open asks
  independent questions at once (the labels beside the list, the tracked and untracked files,
  the review's log beside who the reviewer is) and a resumed review never asks who the reviewer
  is. `RUST_LOG=dv_core=debug` logs every git command's duration.
- **`git diff -U<all>` cost on huge files** — bounded by the size cap (§6.2); measure in Phase 1.
- **Working-tree snapshots** write objects into the user's object store (§2.3). They are unreachable
  except via `refs/dv/**` and removed with the review; `git gc` handles the rest. Acceptable; document
  in the README.
- **GitHub API may gain unchanged-line comments** — when it does, §9.3's fallback becomes the normal
  path; keep validation behind one function.
- **WSL on arm64** — build `aarch64-unknown-linux-musl` from day one; test is owner-only.
- **A second GUI on the same data dir** is allowed for now: `dv open` always starts a new app.
  Handing the request to a running one (single instance) comes later; until then two apps share
  `settings.json` and `known_repos.json`, each written atomically, the last write winning.
- **macOS is verified on one Mac**, the owner's (§13.2: `script/check-mac`); keep platform code
  behind small modules so a gap there stays contained.
- Window material (D8) is available: `gpui-pre` 0.3.11 has `WindowBackgroundAppearance::{Blurred,
  MicaBackdrop, MicaAltBackdrop}` (`gpui-pre-windows/src/window.rs`), checked 2026-10-04.

---

## 18. Code intelligence

Goal 8, D11, phase 5. Reviewing a change means reading the code around it: what a changed call
returns, where a renamed function is still used, what a type holds. dv answers those questions in
the review itself, with the project's own language servers, so the reviewer never opens an editor
to check one.

### 18.1 What the reviewer gets

- **Text.** The stream and the file view (below) select text with the mouse: a drag, a
  double-click for a word, a triple-click for a line. `Ctrl C` copies it through
  `dv_ui::system::write_clipboard`. A click in a line's code puts the cursor on that line (as a
  click on its number does) and a **caret** at the character, which `F12`, `Shift F12` and `Ctrl
  I` act on. A drag from a line's `+` still selects lines for a comment (DESIGN §4.1); a drag in
  the code selects text. A selection is one file's (in split view one column's) and is kept by
  file, side, line number on that side and byte offset, never by row, so scrolling and rows being
  rebuilt keep it, until its file's diff changes to other lines (the agent's edit), which clears
  it and the caret. The pointer, the selection, the caret and find's marks map through the line
  as it was shaped and painted, so wide characters, emoji and combining marks sit where their
  glyphs are. Copying takes the source text, the lines a gap inside the selection hides
  included; switching split / unified or the comparison clears the selection, whose ends name
  one column or both. Every text field's own copy and cut (the find field, the composer, the
  filters) go through `write_clipboard` too. A press in code takes the keyboard from the
  composer, so `Esc` there never discards a composer holding a draft: only `Esc` in the
  composer itself does (DESIGN §5); an empty one still closes.
- **Find.** `Ctrl F` opens a find bar over the main area. In a file view it searches that file. In
  the stream it searches what the review shows, both sides, the text inside gaps included (loaded
  on demand); a match inside a gap opens the gap around it. Case-insensitive unless the query has
  a capital; `Enter` / `Shift Enter` go to the next / previous match; the bar counts them ("3 of
  17", "3 of 17+" while files still load). The stream's search loads every listed file through
  the session (`ReviewSession::load_for_search`, after the viewport's files and the threads'),
  matches each as it lands and keeps its matches while the diff they came from is shown; files
  without text to search are counted ("2 files not searched"). It runs on the UI thread: 200
  files of 1,000 lines take about 130 ms in a debug build (`search` module's ignored timing
  test), and a landing file costs only its own search. A file loaded only for the search isn't
  highlighted until the stream shows it.
- **The file view.** Any file of the repo, whole, at the review's new side (or its old side, for
  a file the review deletes), drawn like the stream's unified rows with one line number (the
  side's): highlighted, the review's changed lines marked with the diff's colours and its threads
  inline. Its text is the stream's diff of the file cut to that side when the stream lists the
  file, else the file read at the new side through the diff pipeline itself
  (`dv_core::diff::side_file`: the side diffed against the empty tree, one file, the pinned flags,
  every line given back as a context line), so autocrlf, `eol`, filters and encodings, binary and
  the size caps (`DiffLimits`) are what git decides for that side and a line here is byte for
  byte the same line as in the stream (anchors compare text, §8). Any line takes a comment (its
  `+`, `C`, `Shift ↑`/`↓`), anchored on that text; an anchor on a line the review didn't change,
  or in a file it doesn't change at all, is an ordinary anchor (§8 "Threads on files the review
  doesn't change"). It opens from a file header's menu (**View file**), from `F` on the current
  file (both at the cursor's line when it is in the file), from `Ctrl P` (every file of the new
  side, filtered as the file list filters), from a "Not in the diff" card's **View file**, and
  from go to definition and references. It is the stream's pane in a one-file scope
  (`dv_ui::diff_pane::file_view`), so the cursor, selection, text, copy and find work as in the
  stream; `S`, `V`, `I` and the gap keys do nothing there. Its side, status and whether the review
  changes it come from the review's own list (all changes) whichever comparison shows: a file the
  review deletes shows its old side (read at the review's base in an interdiff, every line
  removed); its lines are marked as the stream marks them (in an interdiff, changes since the
  round; a file the interdiff doesn't list is read at the side, unmarked, "unchanged since round
  n"). A file read at its side that is over the size caps shows the too-large notice with "Load
  anyway". A draft in its composer is never dropped: opening another file, closing the file
  view, the history and a panel row that shows the stream leave it be, the draft taking the
  keyboard instead; only `Esc` in it discards it. One file view shows at a time, over the
  stream, which keeps its place; `Esc` (once nothing else in the file view takes it) or its close
  button returns to the stream, and so does a panel row that shows something in the stream. The
  stream and the file view share one **history** of positions (`dv_ui::history`, a view, a file,
  a side and a line): opening the file view and leaving it are jumps, each pushing the position it
  leaves and dropping the positions ahead; moves within a view (the keys, `J`/`K`, find's `Enter`,
  scrolling) aren't. `Alt ←` / `Alt →` (and the mouse's back and forward buttons) step through it,
  restoring the view, the file and the line (the stream opens the gap that hides it). It keeps
  100 positions each way and is cleared when another review opens.
- **The file finder** (`Ctrl P`): the new side's paths (`dv_core::diff::side_paths`: a tree's
  blobs and links, the index's entries, or the working tree's tracked files still on disk plus the
  untracked ones git doesn't ignore), read once per review open and round, joined with the
  review's own files (a file the agent added since, a deleted one); a file passes when every word
  is in its path, ignoring case; by the last word typed, a name that is that word (or is it but
  for its extension: "listing-feed" finds `listing-feed.tsx` before `listing-feed-filters.tsx`)
  comes first, then a name starting with it, then a name holding every word, then a match
  through the directories only; the review's changed files first among equals, the rest by
  path; the words' matches show in each row. Filtering runs in memory on each keystroke (20,000
  paths take a few milliseconds). Not over the "Finish round" sheet.
- **Hover.** The pointer resting 350 ms on any character of code but whitespace and quotes, or
  `Ctrl I` at the caret's identifier, asks the file's servers, at the pointer's own character
  (so every piece of a class name in a string asks Tailwind CSS: `bg-red-500`'s `500`,
  `md:hover:text-sm`, `[--x:1px]`). Until an answer comes the token is the identifier there
  (alphanumerics, `_` and `$`, numbers included), else the run of characters around it that
  aren't whitespace or quotes (`dv_ui::intel::hover_token_at`); the card then hangs under the
  range the servers answered for (the union of their ranges on that line that hold the asked
  character: Tailwind's is the whole class), and the pointer anywhere in it keeps the card
  without asking again. Go to definition, references and `Ctrl I` keep the identifier rule (a
  number is no symbol). The card hangs under that range (above it when it doesn't fit below,
  always within the main area, DESIGN §4.1) and shows the
  signature as highlighted code and the docs as Markdown (`dv_ui::markdown`, sanitised; a link to
  a file of the repository opens it as go to definition does, `#L12` naming its line; any other
  link goes through `system::open_url`). It stays while the pointer is over the token or the
  card, closing 300 ms after the pointer left both (and at once on a scroll, a press in the code
  or `Esc`), so its text can be selected (copied through the clipboard seam), and offers **Go to
  definition** and **Find references**. Nothing shows while the server isn't there, or the
  answer is empty for the working tree's text; when every server answered with nothing for
  another side's text (a removed line's symbol, which they read against the working tree), a
  one-line card says so ("No information: read against the working tree"). The hover asked for
  before is cancelled when the pointer rests on another token; one in the pane the file view
  hides (or replaces) goes as the view changes.
- **Go to definition.** `Ctrl`+click, or `F12` at the caret. Where a location shows
  (`dv_ui::shell::intel`):
  - in the file it was asked from, answered against that side's text (an old side, a round's
    snapshot, the index: the text dv opened, §18.2): that side's line, in the pane it was asked
    from (the stream, for a file it lists);
  - in a review whose new side is the working tree (`working`, a branch checked out in its home
    worktree, whose new side resolves to the working tree, §2.2): a line of a file the review
    lists within three lines (the stream's context) of a changed line scrolls the stream there,
    opening the gap around it (a listed file whose diff hasn't loaded loads first and is decided
    then); any other location in the worktree opens the file view there, a file git ignores
    included (a library's `.d.ts` under `node_modules` reads as an untracked file would,
    `dv_core::diff::side_file`);
  - in any other review (`staged`, a commit, a branch not checked out): the file view of the
    file as the working tree holds it now, the text the server read ("as it stands now", §18.2;
    the file view's working-tree mode, labelled "working tree", read-only: no comments, no
    threads), ignored files included. In that file view the pane shows the working tree, so a
    server reads its text from the disk: hover, a definition in the same file, its references
    and a hover card's file links all stay on the working tree, never labelled "read against
    the working tree";
  - outside the worktree, a file: the file view of it, read-only, by its absolute path (read
    through `Backend::probe_path`, up to its 4 MiB, links on the way followed; over the
    display caps the too-large notice with **Load anyway**, past 4 MiB with none), highlighted, labelled "Outside the repository ·
    read-only" with the path and line; hover and go to definition work in it, asked of the
    spec's first running server (no directory of the worktree routes it), which reads it from
    the disk;
  - anything else (a URI that isn't a file): it can't be shown, and the main area's quiet
    notice says where it is ("Defined outside the repository: untitled:Untitled-1:10"), until
    **Dismiss**, `Esc`, or another place.

  The cursor and the caret go to the symbol, and it is a jump the history keeps; asked with the
  mouse (`Ctrl`+click, the hover card's buttons), the cursor and the caret move to the clicked
  symbol first, so `Alt ←` comes back to it. An answer that lands after the reviewer moved on
  (the view, the file or the line isn't where it was asked) is dropped; so is a place that
  waits for its listed file's diff (§ above) once another place is asked for, the history steps
  or the file view opens or closes, or when that diff fails to load, and a definition's that
  fires after the reviewer moved on. Several locations list in the References tab
  ("Definitions"); none does nothing. A references row and a hover card's file link go the
  same way.
- **Find references.** `Shift F12` at the caret, or the hover card's button. The panel's
  **References** tab lists them grouped by file, the files the review changes first, then the
  rest, each group in path order; the count and the rows (their line numbers) show as soon as
  the server answers, each row's line text landing as its file is read (eight files at a time,
  in the background) as the server read it: from the working tree, or for the asking file, when
  the server read the text dv opened for another side (an old side, the index), from that text,
  so its rows are that side's lines and a click goes to that side. Once every file is read the
  rows' lines alone are highlighted, each on its own (nothing one opens colours another), and
  the symbol is marked; a row whose symbol starts past
  its room gives way at its start ("…{formatQuantity(rep.count)}") so the symbol shows, and a
  path that doesn't fit drops whole leading segments ("…/feedback/feedback-badge.tsx"). The
  list is virtualised, so 700 places in 100 files show at once (their lines read and
  highlighted in about 0.1 s locally in a debug build: `references` module's ignored timing
  test). While a list is open the panel heads its column with two tabs, **Review** (the file
  list and the comments) and **References**; closing the list takes them away. Rows on lines
  the review changed (an added line of its new side with that text; for rows read on the old
  side, a removed line of it) carry the changed marker, and **In this review** keeps only
  those: "who else calls the function this change rewrote, and did the agent update them?" In
  an interdiff the marker means changed since the round (the comparison shown). Which rows are
  marked is worked out when a file's text or its diff lands, not per frame. A row goes there as
  go to definition does. The review's diffs of the files the rows are in load for it
  (`ReviewSession::load_for_references`).
- **Not included:** diagnostics (the agent's own checks report errors), completion, rename,
  code actions and formatting. dv never edits (§2.4).

### 18.2 Which text a server reads

A language server reads the project from disk, the working tree, and takes any open document's
text from its client instead. Mapped onto a review's sides:

- **The working tree.** A `working` review's new side, and a `branch` review's whose branch is
  checked out in its home worktree (§2.2), the usual case, are the files on disk. dv opens the
  document as it stands, and every answer is exact.
- **Any other text** (an old side, an interdiff's snapshot column, a `staged` review's index, a
  branch that isn't checked out, a file deleted since): dv opens that side's text as the document
  at the file's path (`didOpen`, which overrides the disk for that file only), so hover and
  definition read the text under the pointer. Other files are still read from the working tree,
  so a location in another file opens that file as it stands now. A hover card or references list
  answered this way says "read against the working tree"; one asked in a file view that shows
  the working tree as it stands (or a file outside the repository) reads the disk's text and
  doesn't. The texts a review's requests opened are kept for the last 32 files asked about.
- One version of a path is open at a time. Hovering the other side of the same file closes the
  document and opens that side's text.
- A document that isn't the working tree's is closed once its request answers, so every other
  request (another file's definition, a references search) reads that file from the working
  tree again. One opened from the working tree's text stays open, and is closed when the review's
  watch reports its path changed (an agent's edit; a server prefers an open document to the
  disk and ignores `didChangeWatchedFiles` for it), so the next request opens it afresh; a
  rescan (anything may have changed) closes every open document on every server.

### 18.3 Where servers run

A server runs where its repo lives, beside the repo's backend. `LocalBackend` spawns it on this
machine. For a WSL repo, the distro's `dv backend --stdio` spawns it inside the distro, so it
reads the distro's files natively; no file crosses 9P.

The `Backend` trait (§5.1) gains one generic capability rather than an LSP one
(`dv_core::backend::process`):

```rust
fn spawn_stdio(&self, spec: &ProcessSpec, sink: ProcessSink) -> Result<ProcessHandle, BackendError>;
// ProcessSpec { program, args, cwd, env }: the executable (an absolute path a resolver found,
// §18.4), its arguments, a cwd relative to the worktree root ("" is the root), env changes.
// ProcessHandle: pid(), write(bytes), kill(); dropping it kills.
// ProcessSink: ProcessEvent::Stdout(chunk) | Stderr(line) | Exit(Option<i32>), on a thread of
// the process's own, in order, the exit last.
```

- **What runs.** Nothing runs through a shell: `toolbox_process::Command::new(program).args(args)`
  (toolbox-rs's crate, `spawn_streaming`), the executable
  one a resolver found and its arguments the language's spec's. A program that isn't an absolute
  path, NUL in an argument or the environment, and on Windows a batch file (`.bat`, `.cmd`, which
  only `cmd.exe` runs) are refused before anything starts (`BackendError::InvalidProcess`); the
  working directory is validated like every worktree path and must be a directory; a program
  that can't start is `BackendError::StartProcess`. Its stdio is piped, and on Windows it gets no
  console window.
- **What the sink hears.** Stdout in chunks as the pipe gives them (a message can span chunks);
  stderr a line at a time, decoded leniently and cut at 8 KiB (diagnostics, which the caller
  keeps a few of); then the exit code: `None` for a signal, which is what a kill gives on Unix,
  while a kill on Windows ends the process's job with exit code 1, so it reads `Some(1)` there.
  The exit comes after both pipes end, or 1 s after the process exited when something it
  started still holds one: something that escaped its tree (it made itself a session or a job
  of its own), whose pipe's reader thread then stays blocked until it closes the pipe or exits.
  Two reader threads feed the delivery thread through a queue of 256 chunks (64 KiB each) or
  lines, so a sink that falls behind makes the process wait on its pipes rather than dv buffer
  without end. A stderr line longer than 1 MiB arrives as several lines, each cut at 8 KiB.
  Dropping the handle stops the delivery: once `drop` returns the sink is never called again
  and has been dropped (from inside the sink, after it returns), as for a watch (§5.3). A write
  to a process that has ended fails as `io` "writing to <program> failed: the process has
  ended", locally and through the wire alike.
- **Its tree.** A process takes everything it starts with it: on Unix it leads a process group of
  its own (`process_group(0)`), on Windows it is alone in a job object of its own with the
  kill-on-close limit, which also kills it when dv ends however it ends.
  A kill, a dropped handle, and the process's own exit each kill the group or the job: its exit
  is seen before it is reaped (`waitid` with `WNOWAIT`), so the group's id is still its own then;
  a wait that fails (the process reaped elsewhere) kills nothing. So a language server's
  children never outlive it. On Windows the process is created suspended and resumed only
  once it is in its job, so not even a child it starts at once escapes the job.
- **On the wire** (`PROTO = 2`, §5.1): `spawn {repo, spec}` → `{proc, pid}` (the pid where it
  runs: inside the distro), `proc_write {proc, bytes}`, `proc_kill {proc}`, and `{"proc": n,
  "out": <base64>}`, `{"proc": n, "err": <line>}`, `{"proc": n, "exit": <code or null>}`
  interleaved like watch batches, in order per process, none before the `spawn` answer. A write
  is one call, answered once the server has written the bytes, so writes from one thread arrive
  in order. The server forgets a process at its exit. When its stdin ends it stops taking
  processes, waits for every `spawn` still starting one (which kills what it started instead of
  keeping it), and kills every process it runs, also one a `proc_write` is blocked on (the kill
  ends that write), so a language server never outlives the client. A running process counts as
  in use for the idle close (§5.1).
- **What is bounded.** In the server, a process's lines waiting for the writer thread count
  against 1 MiB of its own, given back as each line is written: with that spent the process's
  sink waits, its 64-entry queue fills, and the process waits on its own pipe, so a process
  printing faster than the client reads costs the server a bounded amount of memory and every
  other answer waits behind at most that much of its output. The server writes to the client's
  pipe, so this holds when the client stops reading. The client reads the server's output as
  it comes and queues each remote process's events for its sink without a bound: a sink must
  keep up (hand events to a channel), as a watch's must.
- **A lost connection.** A process belongs to the server that started it: when the connection
  is lost (§5.2) each remote process's sink hears `Exit(None)` and its writes fail, and nothing
  starts it again by itself; its owner starts another once the connection is back (§18.5). A
  server killed outright (`SIGKILL` in the distro) can't kill its groups; a language server
  reading its stdin then sees it end.

### 18.4 Languages

A new crate, **`dv_lsp`** (never gpui), holds:

- **The client.** Content-Length framing; requests with ids and cancellation (`$/cancelRequest`
  when a hover is abandoned); answers to the server's own requests (`workspace/configuration`
  from the spec's settings, `client/registerCapability` recorded, `window/workDoneProgress/create`
  turned into the status bar's progress). It uses `lsp-types` 0.97, already in the tree through
  gpui-component. Positions are UTF-8 (`general.positionEncodings`); the client converts from
  UTF-16 for a server that offers only that, reading the files an answer names eight at a time
  (an open document's text, else the file through the backend). It keeps at most 64 documents
  open, closing the one asked about least recently past that (the server reads it from the
  disk again).

  `dv_lsp::LspClient::start` spawns the server through the review's backend, says `initialize`
  and `initialized` (and `workspace/didChangeConfiguration` with the spec's settings), and
  returns once the server is ready, within 30 s; it blocks, so the app calls it off the UI
  thread. `processId` is `null`: the server may run in a distro, where dv's process id means
  nothing and a server that watches its parent would end at once. The server's stdout is read on
  its process's own thread (§18.3), where its own requests are answered at once:
  `workspace/configuration` from the spec's settings by dotted section, `client/registerCapability`
  recorded (its `workspace/didChangeWatchedFiles` globs kept, with `**`, `{a,b}` and relative
  patterns, case-insensitive on Windows), `client/unregisterCapability`,
  `window/workDoneProgress/create` and `workspace/workspaceFolders` (the root), anything else
  `MethodNotFound`; `$/progress` keeps each token's title, message and percentage until its
  `end`. Everything it sends goes through one writer thread, so no caller and not the process's
  own thread ever waits on the server's stdin. A request (`hover`, `definition`, `references`,
  each about an open document) returns a `Pending` whose `wait` blocks for the answer until its
  timeout (§18.5), cancelling it then; dropping it unanswered sends `$/cancelRequest`, and an
  answer that arrives after is dropped. Answers come back as worktree paths (a `file:` URI
  inside the worktree; anything else, a library outside it or a server's bundled file, as the URI
  and its path if it has one), UTF-8 ranges (a definition's link as its target's selection, the
  symbol's name) and hover parts (Markdown, code in a language, plain text). Converting a UTF-16
  server's columns reads each file's lines once: the open document's text, else the file through
  the backend; a file outside the worktree keeps the server's columns. `open_document` keeps one
  version of a path open (§18.2): the same text again does nothing, other text closes the old
  version first. The client keeps the server's last 50 stderr lines and reports its status, its
  progress and each stderr line as events. `stop` says `shutdown` and `exit` and kills the server
  if it hasn't ended within 2 s of each, and returns how it ended; dropping the client kills it at
  once, and ends it there and then (every request still waited on, on any thread, fails
  `Exited` at once, the status event sent), since the process's own exit may never reach the
  sink after the drop. The writer thread is detached, never joined: a write blocked on a backend
  that stopped answering never holds up whoever drops the client. A server that ends by itself
  is `Exited(code)`, a crash its owner restarts (§18.5); one that ends after `stop` asked, or
  whose client was dropped, is `Stopped(code)`, with nothing to restart (TypeScript's `tsc`
  exits 1 even after a proper `shutdown` and `exit`, so the code can't tell them apart).
  A message whose params are `null` leaves `params` out: `tsc` refuses `shutdown` with `"params":
  null` (`InvalidParams`) and ignores an `exit` that has any, so with them a stop never ends it
  cleanly (checked 2026-10-17 against `tsc` 7.0.2). The framing reads a header part line by
  line (a `\r` before each `\n` dropped) and finds `Content-Length` in any case wherever it is in
  the part, so a stray print just before a header loses nothing; a part without a length is
  dropped up to the next `Content-Length`, a body over 64 MiB is skipped, exactly its length,
  with one error. A definition's or references' entry whose URI doesn't parse (an unencoded
  space or backslash) is dropped and logged, never the whole answer; the URI is read as text
  and decoded leniently, so an unencoded space still names its file. Lines are counted as LSP
  counts them (`\r\n`, `\n` or a lone `\r`), each file's line index built once per answer.
- **The languages.** A language server is data, `LanguageServerSpec`: its id and name, the
  documents it serves (language ids and extensions), when it applies to a project (root markers,
  a dependency), how its executable is found (resolvers, in order), its arguments, initialization
  options and settings. Each spec is one module in `dv_lsp::languages`, registered in one list
  (`languages::ALL`); a spec applies to a file it serves when one of its root markers is in the
  file's directory or above it, up to the worktree root (`typescript`'s: `tsconfig.json`,
  `jsconfig.json`, `package.json`; a spec without markers applies in every directory), and
  when it passes the spec's worktree check if it has one (`tailwindcss`'s: the worktree uses
  Tailwind), asked once per worktree and remembered until the resolver forgets.
  Adding a language is one module and its tests. Several servers can serve one file (a `.tsx`:
  TypeScript and Tailwind CSS; a stylesheet: Tailwind CSS and CSS). A hover asks every one of
  them at once, and the card opens with the first answer that says anything and takes each
  other one as it lands, stacked in the list's order (`languages::ALL`: `typescript`,
  `tailwindcss`, `css`) whatever order they landed in; a server whose answer is empty or fails
  adds nothing. Definition and references skip the servers whose capabilities don't offer them
  (`definitionProvider`, `referencesProvider`; hover likewise `hoverProvider`), ask the rest at
  once, and take the first answer with anything in the list's order: an earlier server that
  hasn't answered yet holds a later one's answer back until it answers or the request's one
  deadline passes (then the first useful answer in order). With nothing useful, an empty answer
  over an error.

**Resolvers** run in order, each a function of the repo's files read through the backend, so the
answer is the same wherever the repo lives:

1. **Project.** The project's own copy, at the version its lockfile pins. For TypeScript this is
   Node's resolution of `typescript` from the file's package directory, following symlinks (the
   npm, pnpm and bun layouts). TypeScript 7 or later gives its native executable, from
   `@typescript/typescript-<platform>-<arch>`'s `lib/`, run as `tsc --lsp --stdio`. No Node is
   needed. Checked 2026-10-13 on the owner's bun monorepo, with TypeScript 7.0.2 in WSL:
   initialize took 0.09 s, the first hover 0.9 s (the project loading), definition was immediate,
   references took 0.05 s across the workspace, and the server offers UTF-8 positions.
   `dv_lsp::resolve::node` resolves like Node through `Backend::probe_path` (§5.1), so it reads
   the backend's machine read-only, outside the worktree too: from the file's directory up to
   the filesystem's root (a linked worktree inside the main checkout, an agent's usual one, has
   no `node_modules` of its own and uses the checkout's), and through links wherever they lead
   (pnpm's global store, `npm link`). A path's real location is found a component at a time,
   `..` included, at most 40 links deep, each path probed once per resolution. When nothing is
   found the reason says where it looked ("no node_modules/typescript in /a/src, /a or /"); a
   link dv can't read says so ("… leads to /x, which dv can't read: …"), never "isn't
   installed". The native package is resolved from `typescript`'s real
   directory, which in the pnpm and bun layouts finds the platform package's link beside it
   (`node_modules/.bun/typescript@7.0.2/node_modules/@typescript/typescript-linux-x64`, checked
   2026-10-17 on the same monorepo, where every workspace package's `node_modules/typescript`
   links to the one copy). Its platform and CPU are the backend's (Node's names: `win32`,
   `linux`, `darwin`; `x64`, `arm64`), the server's `hello` for a distro and this machine's for a
   local repo: its native CPU, asked of the OS (`IsWow64Process2`, Rosetta's
   `sysctl.proc_translated`, `uname`), then dv's own build's when dv runs emulated (an x64 dv on
   ARM64 Windows tries `-arm64`, then `-x64`); its executable is `lib/tsc`, `lib/tsc.exe` on
   Windows. A file whose package
   resolves TypeScript 6 or older is missing ("needs typescript-language-server"); one whose
   TypeScript lacks its platform package is missing with that said. The server serves the
   worktree root, so files that resolve the same executable share one server.
2. **Settings.** A command configured for that server (`settings.json`, `lsp.<id>.command`): an
   argument list, never a shell string. Its program is an absolute path where the repo lives,
   or a bare name looked up on `PATH` (3) and run as found there (missing, saying so, when
   `PATH` lacks it); for a JavaScript server a program ending in `.js`, `.mjs` or `.cjs` runs
   with the runtime (below). Absolute means as the repository's machine spells paths: POSIX in a
   distro, Windows locally (`lsp.runtime` likewise). `dv_lsp::LspSettings` reads the `lsp` object leniently, and the app
   passes it in.
3. **PATH.** The executable on `PATH`, the first folder that has it (`dv_lsp::resolve::host`):
   on Windows only `<name>.exe`, never a batch file, which `spawn_stdio` refuses. Locally the
   `PATH` is this process's. In a distro the backend runs under `wsl.exe --exec` without the
   user's profile, and the profile is where bun, nvm and the like add their tools; a login
   shell isn't enough either, since bun's installer and nvm edit `.zshrc` / `.bashrc`, which
   only an interactive shell reads (checked 2026-10-17 on the owner's Ubuntu: `$SHELL` is
   `/usr/bin/zsh`, `zsh -lc` has no `~/.bun/bin`, `zsh -lic` has, in 0.4 s). So the app reads
   the user's shell once per connection (`dv_lsp::resolve::shell`, through `spawn_stdio`): it
   runs `/usr/bin/env -0` (plain `env` where `-0` isn't known) for the backend's `SHELL`, `HOME`
   and `PATH` (the shell from the user's `/etc/passwd` line when `SHELL` is unset), then
   `<shell> -l -i -c '<print>'` through `/bin/sh` with stdin from `/dev/null`, and takes `PATH`
   (and `BUN_INSTALL`) from between two markers made for the call (printed with `printf` and
   `printenv`, which every shell runs alike, the markers in parts around a nonce the environment
   carries, so a shell echoing its commands can't fake one), since an interactive shell's
   startup files print banners and terminal titles; the shell is killed as soon as the end
   marker is in, else after 4 s; failing that `-i -c` (csh and tcsh take `-l` only alone), then
   `-l -c`, then the backend's own `PATH` (checked 2026-10-17 on the owner's zsh: 0.3 s). Entries under `/mnt/<drive>` (WSL's appended Windows folders, whose
   programs interop would run as Windows executables, `/mnt/d/Software/diff/dist`'s old dv
   among them) and relative ones are left out. The result is kept per connection and server
   process (`hello`'s pid), each with a lock of its own held while it is read (two reviews on
   one distro read it once; a slow distro holds up no other), an earlier server's entry dropped
   when its connection comes back as another and every entry of a connection no worktree uses
   any more; a connection that comes back is read again, and when the `PATH` changed the
   resolver forgets and every directory resolves again. **Every server runs with that `PATH`**
   (locally this process's): the string as given, unfiltered, set as the process's `PATH`
   (`ServerCommand::env`), so the shims a server or its runtime goes through find what they call
   (asdf's `node` is `exec asdf exec "node"`, with `asdf` in `~/.local/bin`; pnpm's shims run
   `node`), which under the backend's own `PATH` exit 127. A loopback
   server on Windows is this machine, its `PATH` this process's; an automated run reads none
   (it finds servers only through `DV_LSP_<ID>`, §18.5).
4. **Bun global.** A JavaScript server's package in bun's global folder (below).
5. **None.** The status bar names the missing server and how to get it (§18.5); so does the
   doctor (§12).

What `PATH`, bun's global folder and the runtime give doesn't depend on the file's directory,
so each is found once per spec (a WSL repository's probes are calls to its distro) and
forgotten with the rest.

**The override.** `DV_LSP_<ID>` (the spec's id upper case, `-` as `_`: `DV_LSP_TYPESCRIPT`)
names an executable, an absolute path where the repo lives, that replaces every resolver, as
`DV_GH` replaces `gh` (§14.2); `DV_LSP_<ID>_ARGS`, a JSON array of strings, replaces the spec's
arguments. A relative program, or arguments that aren't such an array, make the server missing,
saying which (`settings::OverrideError`). It still applies only where the
spec does (its root markers and worktree check). Tests and scenarios point it at `mock_lsp`
(§18.7).

**JavaScript servers** (`dv_lsp::resolve::js`). A server written in JavaScript (a spec's
`runtime` is `Js`, naming its package) resolves to its package's entry script, the file its
`package.json` `bin` names under the spec's command name (a `bin` that is a string is the
package's only one), not to the `bin` shim, whose `#!/usr/bin/env node` line would pick the
runtime. Where the package is found:

- **project:** Node's resolution of the package from the file's directory, as TypeScript's;
- **PATH:** on POSIX the hit's real path (bun's `~/.bun/bin/<bin>` and npm's global `bin` are
  links into the package), then the nearest `package.json` above it with the package's name;
  a shell script that is a package manager's shim (pnpm's global `bin`, npm's `cmd-shim`:
  `exec node "$basedir/…"`) runs its last quoted `$basedir/…` path that isn't node, relative
  to the folder `PATH` has the shim in, traced to its package the same way; a hit outside the
  package that is a script (`#!` naming node or bun) is that script, and anything else (a
  wrapper that picks its own runtime) runs as it is. On Windows npm's shim is `<bin>.cmd`: its
  last quoted `%dp0%\…` (or `%~dp0\…`) path that isn't `node.exe` or another batch file
  (yarn's calls the `.cmd` in its `.bin`, not followed) is the script, traced to its package
  the same way, else the package is found beside the shim in `node_modules`; a `.exe` on `PATH` (bun's Windows shims) is never taken for a JavaScript
  server, whose bun install is found through the global folder;
- **bun global:** `<bun>/install/global/node_modules/<package>`, `<bun>` being `BUN_INSTALL`
  or `~/.bun` (the distro user's home, from the shell read above), where `bun add -g` puts it.

dv runs the script with a runtime of its own choosing: `lsp.runtime` in settings (a program,
an absolute path or a name looked up on `PATH`, or an argument list whose rest goes before the
script); else **bun** on `PATH`, else bun's default install (`<bun>/bin/bun`, `bun.exe` on
Windows); else **node** on `PATH`. A spec whose server breaks under bun takes node only
(`JsServer::node_only`): bun is skipped wherever it comes from, `lsp.runtime` included. The
command is `<runtime> [runtime args] <script> <spec args>`. A server found without a runtime is
missing ("Tailwind CSS runs with bun or node, and neither is on PATH or in ~/.bun/bin"; install
bun or Node.js, or set `lsp.runtime`), and an `lsp.runtime` dv can't use says so
(`resolve::js::RuntimeError`).

A missing JavaScript server says where it looked ("Tailwind CSS's language server isn't
installed (no node_modules/@tailwindcss/language-server in /a or /; not on PATH; not in
~/.bun/install/global/node_modules)") and how to get it on this machine: with bun there,
"Install it with `bun add -g @tailwindcss/language-server` (or `npm install -g …`), or set
`lsp.tailwindcss.command` in settings.json"; with node only, npm's command alone; with neither,
"Install bun (bun.sh) or Node.js, then …". In an automated run the hint is the spec's own,
naming both. A hint's commands and settings keys are in backticks: the server's details show
each as inline code with a copy button (DESIGN §4.1 "Status bar").

dv installs neither language servers nor runtimes. The first implementation's Node and vtsls
provisioning stays behind (§15).

The first languages:

| Spec | Files | Server | Resolvers |
|---|---|---|---|
| `typescript` | `.ts .tsx .mts .cts .js .jsx .mjs .cjs` | TypeScript 7+'s native `tsc --lsp --stdio` | project, settings |
| `tailwindcss` | the server's own defaults: `.html .vue .svelte .astro`, `.css .scss .less`, `.js .jsx .mjs .cjs .ts .tsx .mts .cts`, in a worktree that uses Tailwind (below) | `tailwindcss-language-server --stdio` (`@tailwindcss/language-server`, JavaScript) | project, settings, PATH, bun global |
| `css` | `.css .scss .less`, every directory | `vscode-css-language-server --stdio` (`vscode-langservers-extracted`, JavaScript) | project, settings, PATH, bun global |

**Tailwind CSS** (`languages::tailwindcss`) applies to a worktree that uses it: a stylesheet
that imports it (v4's `@import "tailwindcss"`, also `"tailwindcss/…"` and `url(…)`, or v3's
`@tailwind` directives) or a `tailwind.config.{js,cjs,mjs,ts,cts,mts}` (v3). The check lists the
worktree's `*.css` and `*tailwind.config.*` through git (`ls-files --cached --others
--exclude-standard`), then one `git grep -l -I --untracked -E` over every stylesheet (`*.css`,
`*.scss`, `*.sass`, `*.less`, `*.pcss`, `*.postcss`) for the import, nothing under a
`node_modules`, with no cap. It is remembered until the resolver forgets: a
`tailwind.config.*` changing makes it forget (`affects_resolution`), and while the answer is
no, so does a stylesheet changing (`WorktreeCheck::watches`). The whole worktree gets one
server, rooted at the worktree: the server finds each project's stylesheet and the files it
covers itself, so a monorepo's component package that another package's stylesheet scans is
served too (the owner's monorepo: `apps/web/src/styles/tailwind.css` imports it, checked
2026-10-17). It answers `workspace/configuration`'s `tailwindCSS` section with the defaults the
extension documents (hovers, suggestions and validation on, `class` / `className` / `ngClass` /
`class:list`, a 16 px root font size, the lint levels), with the class functions the common
helpers name (`cn`, `cva`, `clsx`, `cx`, `tw`: shadcn's `cn` and `cva` are in the owner's
project; the extension's own default is none), so their string arguments read as class lists,
and `editor` with a tab size of 2.
**CSS** (`languages::css`) answers `css`, `scss` and `less` with validation and hover
documentation, `provideFormatter` off; it comes last, as its definition found nothing useful in a
Tailwind v4 stylesheet (§18.8).

A project on TypeScript 6 or older needs `typescript-language-server` (JavaScript), found through
settings, `PATH` or bun's global folder. It isn't a spec yet: it serves the same files as
`typescript`, so it needs a rule for which of the two a project gets (by the TypeScript version
the project resolves), which comes when a project needs it.

### 18.5 Lifecycle

- One server per worktree, spec and executable (§18.4: two TypeScript 7 versions in one
  monorepo run two). When a review opens, after its file list shows, dv starts in
  the background the servers whose specs apply to the review's files, so the first hover doesn't
  wait for a project load. A worktree's servers stop 5 minutes after its last review closes.
- A file changed on disk is announced to servers that registered for it
  (`workspace/didChangeWatchedFiles`), from the backend's watch (§5.3): each path a watcher asks
  about is looked at once per batch, whatever servers ask, and each server told in one call, the
  batches one at a time in the watch's order: a
  deletion when it is gone, a creation when it was last seen missing, a change when it was last
  seen there, and a creation and a change for a path never looked at before (it may be new,
  which a server only takes up as a creation). A rescan closes every open document and
  announces those paths (`dv_ui::intel::servers::changes`).
- `settings.json`'s `lsp` is read again on **Check again** and when the window comes back to
  the front; when it changed, every worktree's resolver is made again with it and every
  directory resolved again (a server whose command changed is replaced).
- A server that exits restarts after 1, 2 and 5 s, then shows as failed until **Retry**, like a
  connection (§5.2); one that ran a minute or more before it ended counts afresh
  (`dv_lsp::Restarts`). Only `Exited` restarts; `Stopped` is a stop dv asked for (§18.4).
  Requests in flight fail quietly: a hover just doesn't show. A remote server that ended with its
  connection (§18.3) waits for the connection to come back and starts then, its restart count
  untouched; the app never starts a distro for it.
- What resolves is remembered per directory (`dv_lsp::Resolver`). The review's watch never
  reports `node_modules` (git ignores it), so the app passes each batch's paths to
  `Resolver::files_changed`, which forgets everything when one is a `package.json`, a lockfile
  (`package-lock.json`, `pnpm-lock.yaml`, `yarn.lock`, `bun.lock`, …), `.npmrc`, a
  `tsconfig.json` / `jsconfig.json`, a `tailwind.config.*` or anything under a `node_modules`
  (and with them what `PATH`, bun's global folder, the runtime and the worktree checks gave),
  and resolves afresh before each restart (`Resolver::resolve_fresh`), so an install between
  starts is seen.
- A hover times out after 5 s, a definition after 10 s, references after 30 s. A request whose
  file's server is still being found or starting waits for it within its own timeout.
- **Off the UI thread.** `LspClient::start`, a request's wait, `stop` and dropping a client all
  wait on the server, and for a WSL repository on the connection to its distro, so each runs on a
  background thread: a request opens its document, sends itself and waits there, and the UI drops
  it when it is no longer wanted (`dv_ui::intel::request::InFlight`, which cancels it at once
  through `Pending::canceller`, even while the thread blocks); a client is held in a slot whose
  drop stops it on a thread of its own. A blocked backend never freezes the window.
- **Which servers.** The app-wide `dv_ui::intel::LanguageServers` (a service) keeps each
  worktree's resolver and servers, shared by every review open on it. `(spec, directory)` pairs
  are resolved in the background, once, when the review's file list lands (its own files) and
  when a request asks about a file not seen yet (a file view of another file); a remote
  repository's platform is its server's `hello`, and its `PATH` its user's shell's (§18.4,
  resolver 3), read in the same background step before the first resolution and again when the
  connection comes back (once per server process; a changed `PATH` resolves every directory
  again). The watch's batches (`SessionEvent::
  WorktreeChanged`) reach `Resolver::files_changed`, which forgets the directories' answers when
  an install may have changed them (the running servers go on), and every ready server's
  `didChangeWatchedFiles`, each path a change or, gone, a deletion, worked out in the background.
  A review whose new side isn't the working tree watches no worktree, so its servers hear of no
  change until a restart. When the resolver forgets, every `(spec, directory)` ever asked for
  is resolved again (a missing server stays shown as missing; one whose directory now resolves
  to another executable is replaced, the old one stopped once no directory routes to it), and a
  batch resolved before the forget is dropped (a generation; the resolver itself keeps no answer
  worked out across a forget). Only the paths a server's watchers ask about are looked at
  (whether each is gone) before telling it.
- **Automated runs** (the app under `--automation`, `dv_ui`'s tests) find servers only through
  `DV_LSP_<ID>` (`Resolver::overrides_only`; a scenario's `lsp` header sets it): the project,
  settings, `PATH` and bun global resolvers are off and no distro's shell is read, so no
  automated run starts a real server a fixture's surroundings hold (a `node_modules` above
  `target/`); without the override a TypeScript file shows TypeScript as missing, saying so (a
  spec's worktree check still decides whether it applies: a Tailwind project without
  `DV_LSP_TAILWINDCSS` shows Tailwind CSS as missing, one without Tailwind nothing).
- The status bar names each server and its state: loading (with its progress), ready, failed (a
  click shows the command line, the last stderr lines and **Retry**), or missing (how to get it,
  per machine, §18.4, and **Check again**).
- **A missing server installed meanwhile.** A global install changes nothing in the worktree,
  so nothing the watch reports makes the resolver forget. **Check again** (the missing
  server's details) reads the machine's `PATH` afresh (a distro's shell again), forgets
  everything and resolves every directory again; the same happens on its own, while a server
  is missing, when another review opens on the worktree and when the window comes back to the
  front, at most every 30 s (`LanguageServers::recheck_missing`, `RECHECK_EVERY`). A `.tsx` in a Tailwind project shows both "TypeScript ready" and
  "Tailwind CSS ready"; Tailwind CSS starts with the review like every server, so its scan of
  the project (about 9 s, §18.8) is done, its progress shown, before the first hover asks.

### 18.6 In the app

`dv_ui::intel` holds a `CodeIntel` entity per open review: its worktree's servers (through the
review's backend and `LanguageServers`), the text of each document it opened, requests and their
cancellation, the last hover, the references list and the notice for a place it can't show. The
shell owns it with the session, made before the one it replaces goes (so a review reopened on the
same worktree keeps its servers), hands it to the stream, the file view and the References tab,
and does the navigation its events ask for (`IntelEvent::GoTo`, `ShowReferences`). The views are
`file_view`, the `references` panel tab, the hover card and the find bar. The stream and the file
view map the pointer to a line and a byte offset through each line's painted slice as it was
shaped (`gpui::ShapedLine`) and a map from its bytes to the source line's (tabs expand): the
columns they paint, whatever width a glyph takes. Under automation, `state` shows the open hover's text, its sections (one per server that answered, in order), the byte it asked at and its span, how many times missing servers were looked for again, the file view (`file_view`: path, side,
top line, line count, whether the review changes it and which lines, whether it is loading), the
history (`history`: positions behind and ahead), the file finder (`file_finder`: query, rows,
count, selection), the references rows, the find bar (`find`: query, current match, count, whether every
file was searched, how many couldn't be), the selected text and the caret
(`stream.text_selection`, `stream.caret`) and code intelligence (`intel`: the servers and the
missing ones, the hover, the last definition's count, the references rows, the panel tab, the
notice). Every character of a line in view is a target,
`code/<path>/<old|new>/<line>/<column>` (the column 1-based in characters of the source line),
for hovering, clicking and dragging; they are recorded only while a capture is armed
(`gpui_automation::record_target`), so they cost nothing otherwise.

### 18.7 Testing

- `dv_lsp`'s own tests use an in-process fake server (a `ProcessHandle` over a thread): framing
  (a message split at every byte, several in one chunk, a stray line), cancellation on drop and
  on a timeout, a late answer dropped, an error answer, the encoding conversion (an open document
  and a file read through the backend), one version of a path open, the server's requests, file
  watchers, progress, a server that ends (while starting too), the stop, a server that ignores
  `shutdown`; the restart schedule on its own.
- **`mock_lsp`** (a bin, like `mock_gh`): a scripted server, a JSON scenario of the requests it
  expects and its answers. An override (`DV_LSP_<ID>`, like `DV_GH`) points a spec at it, so e2e
  scenarios cover hover, definition into the stream and into a file view, back, references, a
  server that crashes and restarts, and one that is missing, with no bun, Node or TypeScript on the
  machine. The scenario is `--scenario <path>` or the file `DV_MOCK_LSP` names (other arguments,
  `--lsp --stdio`, are ignored): `capabilities` for `initialize` (UTF-8 and hover, definition and
  references by default), `stderr` lines printed at start, `on_initialized` actions (a
  notification, a request of its own, a delay), and `requests`, each a method, `params` matched
  as a subset (strings with `*`), a `result` or an `error`, a `delay_ms`, `times` and `send`
  actions; an unmatched request answers `null`, and a request cancelled during its delay answers
  `RequestCancelled`. `crash` exits on the n-th request without answering it, at most `times`
  times over the log, so a restarted server works. Every message it reads, each start and each
  crash are appended to the log (`--log` or `DV_MOCK_LSP_LOG`) as JSON lines. `${root}` in an
  answer stands for `initialize`'s `rootUri`, so a scenario names the fixture's files, and
  `${parent}` for the folder holding it (another seeded repository's files: outside this one);
  its `initialize` answer names it with a version (`serverInfo`);
  `initialize_delay_ms` slows `initialize`; `crash.after_requests: 0` dies at start. Like `tsc`
  it refuses `shutdown` with params (even `null`) and ignores an `exit` that has any, so a client
  that sends them fails its tests. Its own tests drive it with `dv_lsp`'s client through
  `LocalBackend::spawn_stdio`. In an e2e scenario the header's `lsp` (`{"typescript":
  "fixtures/lsp/review_intel.json"}`) has the runner point `DV_LSP_<ID>` at the built `mock_lsp`
  playing that file and logging to the scenario's output dir, which `lsp_log` lines read; the
  runner removes every inherited `DV_LSP_*`, so no scenario reaches a real server unless its
  header names one (`tests/e2e/review_intel*.jsonl`).
- **The app's own** (`dv_ui::intel::tests`): an in-process server through the pool's starter
  seam (`LanguageServers::set_starter`, `set_env`) over real fixture repos: servers start once the
  list shows, hover reads the stream's text, a request the UI abandons is cancelled while its
  thread waits, definition into the file view and the stream and outside the repository,
  references with the changed rows, a crash's restarts until failed and Retry, a missing server,
  the linger's stop (a `shutdown`, never a restart), and the keys at the caret; with two fake
  servers (`tests/stacked.rs`), a `.tsx` hover stacking TypeScript's late answer above Tailwind
  CSS's early one in the same card, a server that says nothing leaving the card to the other,
  an earlier server's late definition winning over a later one's early one, a server without
  `definitionProvider` never asked, a request abandoned while one server still answers
  cancelling that one only, a hover's token at the pointer's character and the card's span
  from the servers' ranges, **Check again** and the focus recheck finding a server installed
  meanwhile, and a Tailwind project without its server showing it missing; `tests/nav.rs`:
  another side's references rows read from the text the server read, the review's files first,
  the working tree's file view keeping a definition there, a place waiting for its file's diff
  dropped once the reviewer moved on, `Ctrl`+click's line kept for `Alt ←`, and a rescan
  closing every open document; and (`tests/mod.rs`) a definition outside the repository opening
  its read-only file view, a non-file place's quiet notice going with `Esc`. The references
  list's pure parts (`references/tests.rs`): a row's symbol and highlight runs on character
  boundaries whatever the server's columns, a line giving way at its start so its symbol shows,
  paths dropping whole leading segments, and a timing test reading and highlighting 700 places.
- Resolver tests run on fixture trees with the npm, pnpm and bun layouts (symlinks, `.bun/`), on
  Windows and in WSL, through `LocalBackend` and through `RemoteBackend` over an in-process
  server, with packages above the worktree root, a link into a store outside it, a link dv can't
  read, an emulated CPU's fallback and an install seen after `files_changed`; where the OS won't
  make a symbolic link (Windows without developer mode) a test says so and passes. The
  JavaScript servers' (`resolve/tests/js.rs`): Tailwind CSS from bun's global folder under bun's
  default install, the project's copy first, a `PATH` link traced to its package and a wrapper
  run as it is (POSIX), npm's `.cmd` shim read for its script and the package beside it
  (Windows), `lsp.runtime` as a path, an argument list and a name, a node-only spec, the missing
  server's hint with bun, node or neither, a server without a runtime, Tailwind's worktree check
  (v3's config, v4's stylesheet found by `git grep`, untracked included, a stylesheet change
  heard while the answer is no, through both backends), a native server on `PATH`, pnpm's POSIX
  shim traced to its package and, run for real through `LocalBackend`, an asdf-style `node`
  shim that finds `asdf` only on the shell's `PATH` (exit 127 without it).
- **The shell-`PATH` reader** (`resolve/shell/tests.rs`): the markers among banners, a title
  escape and CRLF line ends; against a scripted backend, the interactive login shell first, a
  failing or hanging one falling back to `-l`, then the backend's `PATH`, then nothing; on Unix fake shells
  run through `LocalBackend` (one that reads its stdin to the end, which only a closed stdin
  ends, and one that hangs until killed). The real-WSL case (`tests/wsl_shell.rs`, run by
  `script/check --linux`) runs the user's shell and fake shells in the sandboxed home through
  the distro's `dv backend --stdio`: the sandbox has none of the user's startup files, so it
  checks the markers and the fallbacks.
- `spawn_stdio` is tested by the conformance suite (§14.2) against `LocalBackend` and over the
  loopback, in-process and as a child process: output, stdin, kill, exit, what a process started
  dying with it, the server's stdin ending, and the idle-close accounting; `backend_loopback`
  runs one through the built `dv backend --stdio`. The real-WSL tests spawn processes in the
  sandboxed home (`/usr/bin/cat`, `ls`, `env`, `sleep`: the distro's own programs).
- **A real TypeScript check**, opt-in: with `DV_TEST_TSC` naming a TypeScript 7 native `tsc`, a
  fixture project gets hover, definition and references from the real server
  (`crates/dv_lsp/tests/real_tsc.rs`). It is read-only and local, so `script/check` runs it
  whenever the variable is set; it is not an owner-only live check. Checked 2026-10-17 in WSL
  Ubuntu against the owner's TypeScript 7.0.2: the whole test took about 0.1 s, ending with a clean
  `stop` (`Stopped`, within the grace period, nothing killed).
- **The real JavaScript servers**, opt-in the same way (`crates/dv_lsp/tests/real_js.rs`): with
  `DV_TEST_JS_RUNTIME` naming bun or node and `DV_TEST_TAILWIND` naming Tailwind's entry script
  (`bin/tailwindcss-language-server`), a `.tsx` class's hover shows its CSS; with `DV_TEST_CSS`
  naming `bin/vscode-css-language-server`, a property's hover documents it. Not yet run against
  the real servers: neither is installed on the owner's machine (2026-10-17), and nothing
  automated installs one.

### 18.8 Order within phase 5

Each step is its own commit with its own scenarios:

1. Text selection and copy, and `Ctrl F` in the stream.
2. The file view, the history, `Ctrl P`, `Ctrl F` in a file view, and comments on any line.
3. `spawn_stdio` in both backends, `dv_lsp`, `mock_lsp` and the `typescript` spec: hover, go to
   definition, references, the status bar.
4. The JavaScript runtime (bun, then node), the `tailwindcss` and `css` specs, and stacked
   hover cards. Both run under bun (checked 2026-10-13 with bun 1.4.2 on the owner's Tailwind v4
   project): Tailwind's hover shows a class's CSS, its first answer after 9 s while it scans the
   project, so it starts with the review (§18.5); the CSS server's hover documents properties, but
   its go to definition found no custom property in a Tailwind v4 stylesheet, even one defined in
   the same file, so `css` comes last and may stay hover-only.

**Budgets:** a hover shows within 150 ms of the delay ending, once its server is ready; a file
view opens within 100 ms for a local repo and 300 ms for a WSL one.

---

## 19. Annotations

An agent that hands its work over can explain it up front: short notes anchored on the lines (or
the file) they are about, saying why the change is the way it is. The reviewer reads the reasons
beside the code instead of spending round 1 asking for them, and asks only about what the
annotations leave open (the owner's idea, 2026-10-22). An annotation is a thread of its own kind,
so anchoring, relocation, placement and the CLI's reading need nothing new; what differs is that
nothing waits on it.

### 19.1 The model
- **A kind of thread.** `thread_added` with `annotation: true` (§7) opens one
  (`Thread::annotation`, which it keeps for good); its anchor is a comment's (§2.5: a line
  range on one side, or the whole file), relocated, placed, followed through renames and round
  history exactly as a thread is (§8). Only an agent writes one: the fold skips an annotation by
  any other author with a warning, and `ops::annotate` refuses the reviewer
  (`OpError::NotAllowed`). `ops::annotate` refuses a PR review itself (`NotAllowed`: PR reviews
  are the reviewer's, §2.4), ahead of §9.10's rule for every agent write, and a finished review
  as every write is refused (§2.4); an agent's first write on a `changes_requested` review moves
  it to `in_progress` as any other does.
- **Its state is `annotation`** (`ThreadState::Annotation`): waiting on no one. Only
  `thread_added` starts a thread there, and nothing moves one to it: a `thread_state` naming
  `annotation` is skipped by the fold and refused by `ops`. It counts in no open number (the
  panel's "Open comments", the switcher's badge, `threads.open` in the CLI, `]`/`[`), blocks
  nothing, never defaults the "Finish round" sheet to Request changes and is in neither its
  "Comments" nor "Copy as prompt". `ThreadCounts` gains `annotation`, the threads in that
  state (the count's key is the state's name, as the other three are). For the CLI's documents
  the new `state` value is growth (§10.3): the existing values keep their meaning.
- **A reviewer's reply asks about it.** A `reply_added` by the reviewer on a thread in state
  `annotation` moves it to `open` in the fold (the reply is the move: no `thread_state` event,
  no note, `last_change` left as it was), so a question is never lost between two events. From
  then on it is an ordinary thread an agent opened, which the skill already treats as the
  agent's question answered by the reviewer: acted on, replied to, addressed (§10.5), its
  `annotation` flag saying where it began. An agent's replies leave it an annotation (adding to
  its explanation). Deleting the reviewer's reply moves nothing back, as a deletion never moves
  a state: the thread stays open, with no question in it, until the reviewer resolves it (the
  agent's wait counts it as one of its own questions still waiting for the reviewer).
- **No state changes.** Address, resolve and reopen on a thread in state `annotation` are
  refused by `ops::set_thread_state` before its author checks (the fold's own
  `Rejection::Annotation`, the CLI's exit 1 `rejected`: "an annotation waits on no one; reply
  to it to ask about it"), so the reviewer and the agent hear the same thing; the fold skips
  such an event.
- **Editing and deleting** are the author's, as for any thread (`ops::edit_body`,
  `ops::delete_item`). The CLI gains them for whoever wrote an item: `dv comment edit <id> (--body |
  --body-file)` and `dv comment delete <id>` (a thread or a reply; `not_allowed` for anyone
  else's, `finished` on a finished review), so an agent can correct or withdraw an annotation
  that turned out wrong.
- **Older builds** ignore the field and fold an annotation as an agent's open thread. That is
  acceptable as it was for file-level anchors (§7): the app, its CLI and the distro's dv are one
  build. The fold's snapshot cache changes version with this (§7), so a cache an older build
  wrote is rebuilt.

### 19.2 The agent's side
- **`dv comment annotate`** (§10.1) is `comment add` with the annotation flag: the review's
  changed files only, the same sides, lines and file-level anchor, the same text rules, the
  same crossing into a distro (§10.4, `--body-file` read on Windows). An agent identity is
  required (exit 2 without one, as for `review done`). It prints what `comment add` prints
  (`--json` `{thread}`, else one line, `annotated <id>  <place>`). The text documents' counts
  (`status`, `review show`, `review wait`) add "n annotations" when there are any.
- **Handing work over** (the skill's section, §10.5), when the user asks the agent to put its
  work up for review in dv or the project's instructions say to, once the code is final:
  annotations are written last, because one whose code the agent then rewrites goes outdated
  and is never drawn (§19.3). As the skill writes it (every write with `--author <agent>`,
  which also makes `review new` open the review as the agent rather than the reviewer, and
  with `--model '<model>'` beside it, §2.5, left out below):
  1. `dv review new --author <agent> --json` (the working tree's review; `--branch` when the
     work is committed on the branch checked out, whose working tree is then clean) opens the
     review, or resumes the target's open one (§2.4); keep `review.id`. A resumed review that
     has rounds (`created` false, `rounds` not empty) is the loop already: the agent works
     through its open threads as §10.5 says, annotating only changes nobody asked for, and
     hands back with `--round`.
  2. `dv comment annotate --review <review-id> --author <agent> --file src/app.ts --lines
     12:14 --body-file -` for each thing worth explaining; `--lines` left out for why a whole
     file changed.
  3. `dv review done <review-id> --author <agent> --note-file -` with the overview: what changed
     and how it fits together, a Markdown list. No `--round` before a round has closed (nothing
     unseen, §2.4).
  4. `dv review wait <review-id> --timeout 600 --json`, then the usual loop.
- **What makes a good annotation** (the skill's words, and they matter more than the code: an
  agent that annotates everything trains the reviewer to skip them):
  - **Decisions, not descriptions.** Why this approach over the obvious one, a trade-off taken,
    something deliberately left out or out of scope, code that looks wrong but isn't, a
    behaviour a caller will notice, what the tests cover and what they don't.
  - **Never** a restatement of the diff ("adds a function that…"), mechanical changes (renames,
    formatting, imports, generated files, lockfiles), or a verdict on its own work.
  - **Few.** At most one per hunk and usually far fewer: a handful in a review of a dozen
    files. One to three sentences each. The overview belongs in `done`'s note, not in an
    annotation.
  - **Later rounds:** a fix for a thread is explained in its reply and its address note; an
    annotation is for a change nobody asked for.
- **Reading.** `comment list` lists annotations with the rest (state `annotation`; `--status
  annotation` keeps only them), `--status open` leaves them out until the reviewer asks about
  one, and `review wait`'s text says nothing to do about them. `dv status`, `review show` and
  `review wait` count them in `threads.annotation`.

### 19.3 The reviewer's side (DESIGN §4.1)
- **The annotation card**: quieter than a thread card (no shadow, no state chip), under the last
  line of its range as a card is, or heading its file for the whole file, in the stream and the
  file view alike. **Ask** (`R`) opens the reply composer in it; posting turns it into a thread
  card, open, which the agent reads once the round is finished, as any thread. Nothing else
  acts on it: reading one needs no acknowledgement, and a reply is always a question.
- **`A`** shows or hides every annotation in the review, the stream and the file view: the
  open review's state (`ReviewSession::annotations_shown`), never persisted, shown whenever a
  review opens. An annotation the reviewer is writing a question in keeps its card, hidden,
  outdated or not, until that composer closes (`ReviewSession::shows_card`). It is a render filter only: placement, the
  files loaded for threads and the comments list are the same either way.
- **In an interdiff** since round n, only annotations written since (their first anchor's round
  after n) are drawn: one from an earlier round explained code the reviewer has read already.
  This is the interdiff's drawing rule (§8's rule after placement, `session::placement`'s
  `draws`), so such an annotation's comments-list row carries the "all changes" tag and a click
  on it switches to all changes, as for a thread the interdiff doesn't draw. All changes draws
  every one.
- **An outdated annotation** isn't drawn in the stream (no card heading its file, none in the
  "Not in the diff" section): it explained code that is gone. Nor is one whose file left the
  review, even where `place_unlisted` finds its lines (§8): it explained a change the review no
  longer holds. The file view doesn't draw them either. The comments list keeps both, marked
  outdated or "Unchanged file"; a click on such a row opens its whole text in the row (a
  disclosure, nothing scrolls), and it has no menu. Once asked about, an annotation is a thread
  and shows as any outdated thread does.
- **Every place that reads a thread's state** decides about the new one; the compiler flags a
  `match` but not a comparison, so phase 11 goes through each: the "Finish round" sheet's
  "Comments" (`submit::round_comments`, which keeps every unresolved thread) and its default
  verdict; the comments list's waiting and resolved counts, its order and its row menu (an
  annotation's offers **Ask** alone; no Resolve, no Copy as prompt); the stream's
  `is_thread_open`, `]`/`[` and `X` (nothing on an annotation); the switcher's badge; the CLI's
  `--status` filters, counts and `review wait`'s next steps; the status bar.
- The comments list, the status bar and the handoff banner count annotations apart from the
  threads (DESIGN §4.1, §4.4).

### 19.4 Testing
- **`dv_core`:** the fold (an annotation's state; one by the reviewer skipped; the reviewer's
  reply opening it, an agent's leaving it; `thread_state` on it, and one naming `annotation`,
  skipped; edits and deletes; the counts; the cache version) and `ops` (`annotate`'s refusals:
  the reviewer, a PR review, a finished review; `set_thread_state`'s `Annotation` ahead of
  `NotAllowed`).
- **`dv_cli`:** `annotate`'s errors are `comment add`'s (one code path, tested once through
  each), the identity rule, `--status annotation`, the `annotation` flag and the counts in
  every document and text, the crossing of `--body-file` (§10.4's tests); `comment edit` and
  `comment delete` (the author's, anyone else's `not_allowed`, a finished review's
  `finished`). The skill test runs the handing-over section's commands as written against a
  fixture repo, a fresh review and a resumed one with rounds.
- **`dv_ui`:** the card in the stream and the file view, `A`, the interdiff, outdated and
  unlisted rules, the comments list's group, rows and menu, the "Finish round" sheet leaving
  annotations out, Ask turning a card into a thread.
- **e2e** `tests/e2e/agent_annotations.jsonl`: the agent (`cli` with `CLAUDECODE`) opens the
  working review, annotates a range and a whole file and hands over; the reviewer opens it to
  the handoff banner and the cards (screenshots, light and dark), hides and shows them with
  `A`, asks about one; the agent's `comment list --status open` shows the question, it replies
  and addresses, the reviewer resolves. Screenshot review against DESIGN §4.1.

### 19.5 Order
1. `dv_core`: the field, the state, the fold's rules, `ops::annotate`, the counts.
2. `dv_cli`: `comment annotate`, `comment edit` and `delete`, `--status annotation`, the DTOs
   and text, the skill's section.
3. `dv_ui`: the card, Ask, `A`, the interdiff, outdated and unlisted rules, every state reader
   of §19.3's list, the status bar and the banner.
4. The e2e scenario, the screenshot review, a review of the whole change.
