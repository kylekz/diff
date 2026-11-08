# End-to-end scenarios

Each `*.jsonl` file here is a scenario: a list of automation commands run
against a freshly started `dv`, with assertions on the app's `state` JSON
(docs/PLAN.md §14). The driver, the runner and the scenario format are
toolbox-rs's shared automation crates (github.com/kylekz/toolbox-rs, pinned by
tag in the root `Cargo.toml`): **its `docs/automation.md` documents the
protocol and every builtin command, the scenario format and expectations, the
runner and its options, background runs, screenshots and the Linux Xvfb
display.** This file covers what is dv's own: its runner extensions
(`crates/dv_e2e`, "dv's runner extensions"), its scenarios, its commands and
its `state`.

```bash
cargo build -p dv -p mock_gh -p mock_lsp
cargo run -p dv_e2e --bin e2e -- tests/e2e
cargo run -p dv_e2e --bin e2e -- --filter shell tests/e2e/*.jsonl
cargo run -p dv_e2e --bin e2e -- tests/e2e/foreground
cargo run -p dv_e2e --bin e2e -- tests/e2e/docs
```

`tests/e2e/foreground/` holds the scenarios that take the foreground (their
header says `"foreground": true`): the runner doesn't look into
subdirectories, so `tests/e2e` and `script/check` leave them out, and they run
on demand (the fourth command) or with `script/check --foreground`. Keep hands
off the machine while they run; see "Screenshots and window materials".
`tests/e2e/docs/` holds the scenarios that make the docs' images
(`readme_screenshot`): their bounds are one platform's fonts and chrome, so
they run on demand only (`tests/e2e/docs`), never in `script/check`.

Run that `cargo build` right before a run (`script/check` step 5 does). A
build that compiles `dv`'s bin alongside test targets with unified features
(`cargo build --workspace --all-targets`, or `cargo test`, since `crates/dv`
has integration tests) relinks `target/debug/dv.exe` with gpui's `test-support`,
which includes leak detection: that binary panics at exit when an entity
outlives the App, so a scenario can exit 101 after `quit` against it and pass
against a plain one.

dv opens every window through `dv_ui::automation::open_window` and renders
`pointer_guard()` in its shell, so background runs (toolbox-rs
docs/automation.md, "Background runs") never take the focus and a real cursor
crossing the window can't bend a scripted drag; nothing in dv activates,
resizes or maximizes a window while `dv_ui::automation::is_background()`
(CLAUDE.md, "Windows and the OS go through seams"). dv's automation host lists
its commands in the `ready` handshake, so a misspelt one fails before a
scenario's first step.

### Screenshots and window materials

A `screenshot` takes its pixels from one of two sources (`"source"`):

- **`window`** (the default): the window's own composed frame,
  `PrintWindow(PW_RENDERFULLCONTENT)` on Windows. It works behind other
  windows, so background runs use it. It includes DWM's Mica backdrop (a
  foreground capture matched the screen pixel for pixel) but **not** the
  blur-behind gpui uses for Acrylic (`WindowBackgroundAppearance::Blurred`):
  translucent surfaces over Acrylic come out composited over black, so a dark
  Acrylic panel captures nearly black and a light one muddy grey. Opaque is
  exact.
- **`screen`**: the window's client rect copied from the desktop as shown,
  blur included (Windows only). It needs the window in front, so it is
  refused in background mode, and fails rather than copying another window's
  pixels when the window isn't the foreground window, is covered at any of 9
  sample points, or runs off the screen.

What a background run shows is therefore the window **inactive**: Mica as its
flat inactive fill (light or dark following dv's mode), which is DWM's own
inactive Mica, as it would be on screen (DESIGN §3: the OS manages the
inactive look), and Acrylic over black, which is not how it looks anywhere. Judge layout, type and Opaque from
background screenshots; judge Mica's active tint and Acrylic only from
`shell_materials_foreground`'s screen captures, taken with the window active
(Acrylic's blur samples whatever is behind the window at that moment, so
those captures vary from run to run). The trade-off of a foreground scenario
is the person's machine: it takes the focus for a few seconds, keystrokes
typed meanwhile reach dv, and a click elsewhere fails its screen captures.
That is why it lives in `tests/e2e/foreground/` and runs only on demand.

On **macOS** both sources are Core Graphics images (`CGWindowListCreateImage`,
at the display's resolution: 2x on Retina, so a 1440x900 window captures as
2880x1800; converted to sRGB; the shadow left out). `window` is the window's
own image, covered or not. `screen` is the window's frame as the screen
composes the window and what is under it (windows above it left out); since a
background window is in front on macOS, it works in background runs there.
macOS's only material is Opaque (PLAN D8), so the two should look the same;
`shell_materials_macos` takes both for a look side by side (nothing compares
the files). They differ at the rounded corners (black in the window's image,
the desktop in the screen's), and a dark window's own image can come out a
few pixels wider than its frame (seen as 2884 against 2880: the outline macOS
draws around a dark window). Both need the Screen Recording permission for whatever runs the
app (granted to SSH sessions on the owner's Mac).

### Linux

On Linux the runner starts an **Xvfb display of its own** for the whole run
(`Xvfb :<n> -screen 0 2560x1600x24 -nolisten tcp`, the first `n` from 90 no
live server holds: a `/tmp/.X<n>-lock` naming a dead process is no obstacle;
its output in `<out>/xvfb.log`) and ends it with the run (`SIGTERM`, on which
it removes its lock, then `SIGKILL` after 5 s). It also dies with the runner,
however the runner ends (`PR_SET_PDEATHSIG`). Every app starts there under X11 alone: `DISPLAY=:<n>` and no
`WAYLAND_DISPLAY`, so inside WSL no window reaches WSLg, whose windows open on
the Windows desktop. `screenshot`'s `window` source is an X11 `GetImage` of the
window (`capture_x11.rs`): exact, since nothing covers it on that display; at
X11's scale 1 a 1440x900 window captures as 1440x900 (`GPUI_X11_SCALE_FACTOR`
in a header's `env` renders at another scale). Xvfb has no compositor, so the
window keeps the platform's decorations (none drawn: there is no window
manager either) and dv's caption buttons don't show; the `screen` source is
Windows' and macOS's. Without Xvfb installed the runner says so and stops.

`--host-display` keeps the display the runner was started with instead
(WSLg's, under WSL: Wayland, with X11 through `env -u WAYLAND_DISPLAY`): for an
explicit exploratory look at dv on a real compositor, a short scenario under
`target/tmp/` that quits, never a gate. Under Wayland `screenshot` fails (the
X11 capture needs an X11 window); `bounds`/`query` still answer.

Every scenario but `setup`, `shell_caption_buttons` and `shell_materials`
(Windows' chrome: `platforms: ["windows"]`), `docs/readme_screenshot` (the
README's image, its frame Windows'), `setup_macos` and
`shell_materials_macos` (`["macos"]`) runs on Linux; `setup_linux` is
`setup`'s Linux sibling. `script/check --linux-gui` runs them all in WSL
Ubuntu, from the Linux GUI target dir (`~/.cache/dv-target-gui`), whose
`e2e/` then holds the screenshots.

### dv's scenarios

| Scenario | Covers |
|---|---|
| `shell_layout` | the empty shell's regions (title bar, panel, main area, status bar and their parts; the status bar says "No repo" and nothing else) at DESIGN §1.5/§2 sizes at 1440x900, at the 1100x700 minimum after a `resize`, and a smaller `--window-size` raised to the minimum |
| `shell_caption_buttons` | Windows only: the caption buttons' bounds |
| `shell_materials` | Windows only: every material × mode through `set_setting`, the resolved values, a screenshot of each (`NN-shell-<material>-<mode>.png`, window source), and the last choice surviving a restart. Needs a machine that can show all three materials (Windows 11 22H2+, transparency effects on); its first assertion says so when it can't |
| `foreground/shell_materials_foreground` | Windows only, foreground, on demand: the same six combinations as screen captures (`NN-shell-<material>-<mode>-screen.png`) |
| `setup_macos` | macOS only: `setup`'s macOS sibling (as `setup_linux` is its Linux one): the first-run screen over this Mac's checks alone (`macOS`, no WSL) in a scratch home with `mock_gh` signed in; a fake Claude Code folder, Check again, Install; Add to PATH linking `~/.local/bin/dv` to this dv and putting `~/.local/bin` on the shell's `PATH` through `~/.zshrc`, all in the scratch home; the git command through the clipboard seam; Continue and a review opening; after a `restart` the status bar's notice for a skill edited since, and its Update; screenshots of the first run, set up (dark and light) and the notice |
| `shell_materials_macos` | macOS only: Opaque the only material available and the default; an Acrylic or Mica setting kept but resolved to Opaque; the empty shell dark and light, each as the window's own image and as the screen shows it (`NN-shell-<mode>.png`, `NN-shell-<mode>-screen.png`, to look at side by side); the last choice surviving a restart |
| `review_chrome` | the screen around the stream (DESIGN §4.1): `open` on a seeded repo (the board's `api-server`) opens the switcher on its uncommitted changes; typing filters, `↑`/`↓` wrap, `Enter` opens the working review; the file list (rows, current file, viewed checkboxes through the store, the filter and `Esc`), `Ctrl B`, the panel's resize and double-click reset, the source picker, the status bar, a WSL repo's switcher row (through WSL's stand-in, which refuses to start anything) and the main area's banner ("couldn't open Ubuntu:/home/kyle/web", Details with why and the connection's diagnostics, Copy diagnostics through the clipboard seam, Dismiss), the title bar at 1100x700 (and with a long repository name), a repo whose `.git` went away while open (its switcher row says why and offers nothing to open), and after a `restart` the WSL repo, its distro stopped (`wsl_distros`), listed as "Ubuntu isn't running" with **Start Ubuntu**, whose click connects and, the stand-in booting nothing, leaves the row offering Start again with why it failed as its `error`; then, the distro running (`wsl_distros`), the failed connection's row offering **Retry**, whose click connects again; screenshots of the switcher, the review and the banner (and its Details) in dark Mica, dark Opaque and light, and of the stopped distro's row, its failed start and the Retry row |
| `review_session` | the review plumbing without its screens: a seeded repo is `open`ed and listed with its uncommitted changes, its working review opens (named in full at once) and resumes, follows the agent's edits and commit live, and sees the agent's `review_op` through the watch; a WSL location is listed as unavailable (WSL's stand-in refuses to start anything); repos, review and seeded fixture survive a `restart` |
| `setup` | Windows only. Setup (PLAN §12, DESIGN §4.3) on a fresh data dir with `DV_FIRST_RUN=1` and `mock_gh` signed in (`tests/fixtures/gh/auth_status.json`): the first-run screen over this machine's checks; a fake Claude Code folder (`home_file`), Check again, its skill's Install; Add to PATH writing the user `Path`'s stand-in; the git command copied through the clipboard seam; WSL's stand-in naming Ubuntu (listed, not reached), Debian (stopped; Start refused: the run reaches no distro but its sandbox's, why under its name) and `docker-desktop` and `podman-machine-default` (container tools' VMs, not listed); Continue (`Enter`) writing the marker and a review opening; Settings (`Ctrl ,`) showing a skill edited since as out of date and Update fixing it, `Esc` closing it; after a `restart`, no first run and the status bar's notice "dv-review skill out of date on Windows", whose Update fixes it; screenshots of the first-run screen, with the distros, Settings and the notice |
| `setup_linux` | Linux only, `setup`'s sibling: the first-run screen over this machine's checks alone (no distros on Linux; the header empties `WSL_DISTRO_NAME`, so a run inside WSL names the machine `Linux` as a desktop would), in the scratch home with `mock_gh` signed in; a fake Claude Code folder, Check again, Install; Add to PATH linking `~/.local/bin/dv` to this dv and putting `~/.local/bin` on the shell's `PATH` through its startup file, the shell then asked again with the scratch home; the git command copied through the clipboard seam; Continue and a review opening; after a `restart` the notice "dv-review skill out of date on Linux" and its Update; screenshots of the first-run screen (dark, and set up in dark and light) and the notice |
| `switcher_branches` | the branch picker (DESIGN §4.1 "Compare branches…", PLAN §2.2): a stacked repo (`main → feat-a → feat-b`, and `side` off `main`) offers **Compare branches…** after its targets; it lists the branches newest first, each against its stack base (`feat-b` vs `feat-a`, the others vs `main`), a click on `feat-b` lists its bases (`feat-a` "stack base", `main` "default branch", then `side`), `Esc` steps back with the selection on `feat-b`, and `Enter` twice opens `feat-b` vs `feat-a` with only `b.txt`; the source picker's **Compare branches…** opens the picker on the repo, the branch's open review beside it, and `Esc` closes it; screenshots of the branches and the bases in dark, the stacked review, and the branches in light |
| `switcher_remove_repo` | Remove from list (DESIGN §4.1): with two seeded repos and `api`'s review open, `web`'s `⋯` (`switcher/repo/web/more`) opens its header menu and **Remove from list** (`switcher/menu/remove`) drops it from the switcher and `repos` (`known_repos.json`), the switcher staying open; a right-click on `api`'s header opens its menu with the item disabled ("Its review is open", `menu.removable` `false`), `Esc` closes the menu and leaves the switcher open, and `Ctrl Shift Backspace` on `api` removes nothing; screenshots of both menus |
| `update` | dv's own update (PLAN D9, DESIGN §4.1): nothing shows in an automation run (it checks no feed); `update_ready` through the seam shows "dv <version> is ready" and Restart to update at the status bar's right beside an open review; with two other dv processes running from the install (the stand-in's `others`) its click opens the confirmation (`update.confirm`, `status/update/confirm`) and Not now keeps everything; a restart the stand-in fails reads "Update failed · details" (`update.failure`, its tooltip) with Retry; with an unsent comment (`J`, `C`, typed) the confirmation names it and Restart anyway records the restart (`update.restart_requested`) while dv keeps running, the draft intact; screenshots of the item in dark and light, both confirmations and the failure |
| `backend_loopback` | a repo reached through the loopback `dv backend --stdio` (this `dv`'s own server, a child of the app), as a WSL repo is through its distro's (PLAN §5.2): `seed_repo` with `"backend": "loopback"`, its review opening and a comment posting through the connection (`state.backends`), the remote watch following the agent's edit, a `backend_kill` that reconnects by itself (the status bar's "loopback · reconnecting…", no banner) and a file written while it was down listed after the synthetic rescan, then `backend_block` and a kill that runs out of respawns (attempts 2 and 3: a death soon after a reconnect continues its count): `failed`, "loopback · disconnected" and the backend-down banner (Details, Copy diagnostics); meanwhile the open switcher's row reads "reconnecting…" keeping its rows, then offers **Retry** once the connection failed; unblocked, the banner's Retry connects again and the watch follows the agent once more; then `backend_stop` (the host stopped, as `wsl --shutdown` stops a distro): the respawn starts nothing, `stopped`, "loopback · stopped", the banner's **Start loopback** and the switcher's Start row, and the banner's Start boots it again; stopped once more, the switcher's Start row started with the start held (`backend_hold`): the row stays, inert (`starting`), with the selection, and `Enter` again does nothing, until the repo lists and its first row takes the selection; a review picked from the switcher while the connection reconnects opens once it is back, with no banner; one picked while the reconnect then fails waits as `shell.pending` (the backend banner the only banner, `shell.banner` `null`); with a round summary typed in the sheet meanwhile it waits on after the switcher's Retry row brings the connection back (the review under the sheet stays, the summary kept), and opens by itself once the sheet is cancelled, no banner left; and after a `restart` with `app_args: ["open", <repo>]` the repo is reached through the loopback again, where a review picked with no review open and its server killed at once (under the open) opens once the connection is back (generation 2, no banner); screenshots of the review over the loopback, while reconnecting, the switcher reconnecting and offering Retry, the banner and status bar in dark Mica, dark Opaque and light, the review reconnected, the stopped banner and switcher row, the switcher while its Start runs, and the pick waiting under the backend banner and then for the sheet |
| `review_screen` | the diff stream (DESIGN §4.1, §5): the board's `api-server` with a CRLF README under `core.autocrlf`, a binary, a file too large to show, a rename, an untracked file, a deletion and a long file with gaps, opened from the switcher by keyboard; `J`/`K`, `N`/`P`, the cursor (`↑`/`↓`, `←`/`→`), a `Shift` selection and `Esc`, `S` to unified and back, gap rows and `E`/`Shift E`, `PageDown` with the pinned header, the agent staging everything under the reader (the gaps stay open, the place stays), the sideways scroll, line-number clicks and shift-clicks, the file menu's Copy path, `V` collapsing a viewed file and its chevron, "Load anyway"; screenshots of each state in dark Mica, dark Opaque and light |
| `review_workflow_a` | PLAN §3 workflow A steps 1–3 by keyboard alone (no clicks): `open` shows the seeded `api-server` with its seven uncommitted changes, `Enter` opens the working review, `V` marks a file viewed (the event read back from the store), `J` and `Shift ↓` select lines, `C` opens the composer (letters type, they don't move), `Ctrl Enter` posts, `Ctrl Enter` opens the Finish round sheet on Request changes (`↑`/`↓`, `Tab` into the summary and back, `Ctrl O` opening the switcher over it without losing the summary), `Ctrl Enter` finishes round 1 (snapshot, `changes_requested`, round 2, nothing viewed, the empty interdiff since round 1 with the thread in all changes only; `I` shows all changes); then the agent's `review_op` address and reply show live (and an addressed card at 1100 wide keeps Resolve and Reopen whole), `]` focuses the thread, `R` replies, `X` resolves and reopens, and Approve leaves the review read-only (`C`, `Ctrl Enter`, `X` do nothing; the finished banner); screenshots of the composer, the sheet and the thread states (open, addressed, reply, resolved) in dark Mica, dark Opaque and light |
| `open_startup` | `dv open` as the app starts (PLAN §10.1), through `restart`'s `app_args`: `open <relative path>` adds the repo and opens the switcher on its uncommitted changes (`Enter` opens the working review), `open <path> --review <id>` opens that review without the switcher, a review that isn't there and a path that isn't a repository show the main area's banner, and a launch without `open` opens nothing; screenshots of the switcher, the review and the banner |
| `review_live` | a review open while the agent works (`repo_edit`): the agent saving the file the reviewer is commenting on (the composer stays and every keystroke lands), an edit updates a file's totals and rows, a new file joins the list, a thread whose line the agent edits is relocated (a fuzzy match at the same line, so `placement` `unchanged`, not `moved`), one whose function the agent rewrites turns outdated (the comments list flags it with its original line; its card heads its file), and a commit of everything leaves the working tree clean with the thread still listed as outdated and drawn in the stream's "Not in the diff" section in place of the empty state; screenshots of the outdated card in dark Mica, dark Opaque and light |
| `review_placement` | threads placed where the CLI places them (PLAN §8, §11): the CLI comments on a file far below the viewport and on a file renamed without an edit, the agent inserts lines above the first; opened, the stream at its top, the far file has loaded anyway and the thread shows `moved` at its new line, the renamed file's thread `unchanged` (its diff a "No content change" notice), the comments list and `dv comment list` agreeing; the agent inserts more lines while the reviewer reads elsewhere and the thread follows; after the CLI requests changes, the empty interdiff's rows name where the threads stand in all changes (`not_in_comparison`, `lines`, `outdated: false`); a screenshot of those rows |
| `review_not_in_diff` | threads whose file left the review (PLAN §8, DESIGN §4.1): the CLI comments on an untracked file the agent then deletes, on a file the agent reverts and on one that stays; opened, the two head the stream in its "Not in the diff" section (`stream.not_in_diff`) as full cards and come first in the comments list; a row's right-click menu (`comments_list.menu`) copies its thread alone and resolves it, another's Reply opens the reply composer in its card in the section and its card's Resolve resolves it, the agent's CLI reply shows in the card; after Request changes the empty interdiff keeps the section, the empty state a row under its cards (`stream.not_in_diff.empty`) and `R` replying in a card, and once the agent edits again the interdiff still shows it. Screenshots of the section, the row menu, the answered cards, the empty interdiff (dark Mica) and the interdiff (light) |
| `review_file_comment` | comments on a whole file (PLAN §8, DESIGN §4.1, §5): the file header's `⋯` menu (Comment on file, Copy path, Reveal in folder) opens the composer heading the file (`session.composer.file_level`, `stream.files[].heading`); the posted card heads it (`placement` `file`, `file_level`), the comments list names it `path (file)`; the agent reads it through the CLI (`comment show`, JSON and text) as a comment on the whole file and replies, the card shows the reply and its Resolve resolves it; `Shift C` on the current file opens the same composer there, and once the agent deletes that file its card keeps heading it, outdated, as the CLI says; screenshots of the menu, the composer, the card, its reply and the deleted file's card in dark Mica, the last in light too |
| `review_rounds` | rounds and relocation (PLAN §2.3, §8; DESIGN §4.1, §4.4): no toggle before a round closes (`I` does nothing); four threads (clicked line numbers, `C`), Request changes closes round 1 and the stream shows the empty interdiff since round 1 (the panel's toggle, "Show all changes", every thread tagged "all changes"); the agent (`repo_edit`) moves a commented block, inserts lines above another, rewrites a third file and leaves the fourth: the interdiff lists the three, the moved threads show at their new lines (`moved`, `lines` and `original_lines`, cards under their own lines), the rewritten one is outdated, its card under the line that replaced its code (`trail`, "code changed since round 1"), the fourth `not_in_comparison`; the agent's address, reply and `done` with a note: the ready banner (Show note), the comments list addressed first; `I` to all changes and the panel's "Since round 1" back; a click on a comments row shows its card; `X` records the moved anchor; Approve finishes on all changes without a toggle; screenshots of the empty interdiff, the interdiff with its toggle and the ready banner (the outdated card at its trail), the moved card in dark Mica, dark Opaque and light, the open note, all changes and the approved review |
| `review_round_history` | a thread read against its round's snapshot (PLAN §8 "Round history", DESIGN §4.1): the CLI comments on lines the agent wrote in `format.ts` and on its untracked hook `use-can.ts`; Request changes closes round 1; the agent (`repo_edit`) rewrites the commented lines and renames the hook (`use-has-permission.ts`, untracked, which git can't pair with its first name): the first thread is outdated with its trail (`replaced`, round 1), its card drawn inline under the line that replaced it with "code changed since round 1", the comments list naming that line; the second follows the rename (`unchanged`, `file` the new name), nothing in the "Not in the diff" section; the agent's `dv comment list --json` reports the same (`trail`, the new path); a reply on the outdated one records no anchor; `I` shows the same in all changes; screenshots of both cards in dark Mica and all changes in light |
| `review_select` | text in the stream (PLAN §18.1, DESIGN §4.1 "Text in code"): a `drag` between two characters' targets (`code/<path>/<side>/<line>/<column>`) selects across lines on the new side (`stream.text_selection` with its `from`, `to` and text, the caret and the cursor at its end), `Ctrl C` puts the source text, tabs and all, on the clipboard seam (`system.requests`), a double-click takes a word and a triple-click a line, `Esc` clears it, a click puts the caret before the character clicked, a keyboard move takes the caret away and `Ctrl C` with nothing selected copies nothing, a drag on the old side keeps to it, and unified view takes the removed and the added line in order (switching views clears it); then a line of CJK, one of emoji and one with combining accents: the selection, a word, the caret and the find boxes fall on the glyphs as drawn (two cells for a wide character, none for a combining mark); screenshots of the drag, the word, the line, the caret and the unified selection in dark Mica, the drag and the caret in light, and the wide-character selection and find in dark |
| `review_find` | `Ctrl F` in the stream (PLAN §18.1, DESIGN §4.1 "Find"): a review of nine files, the last below the viewport and not loaded (`session.files[].state`), a binary one and a viewed one; typing searches every file, both sides, a context line once (`find`: three matches, `complete`, one file `skipped`), the far file loads for it, and the first match, inside a collapsed gap, opens the gap around it (`stream.files[].gaps`) with the cursor on its line; `Enter` goes on into the far file (the caret at the match), wrapping, `Shift Enter` and the bar's ↑ go back, the keyboard staying in the field; a capital makes it case-sensitive, a match in the viewed file opens it, "No results" for a query that matches nothing, `Esc` closes the bar and gives the keyboard back to the stream, `Ctrl F` again keeps the query and the bar's × closes it; screenshots of the opened gap, the far file, the viewed file opened, no results (dark Mica) and the bar in light |
| `review_intel` | code intelligence against `mock_lsp` (PLAN §18.1, §18.5, DESIGN §4.1; `fixtures/lsp/review_intel.json`): the TypeScript server starts once the file list shows (`intel.servers`, the status bar's `statusbar/lsp/typescript`); the pointer resting on `helper` opens the hover card (`intel.hover`: its text, `hover/definition`, `hover/references`), moving onto a line number closes it, `Esc` closes it; moving straight onto another identifier shows that one's (the first card's grace closes only the card); a hover the server is slow to answer waits with no card (`intel.hover.open` false) once its request has reached the server (`lsp_log` with `wait_ms`) and is cancelled once the pointer leaves it (`$/cancelRequest`); `F12` with the pointer over the card goes to the definition and hovers go on working after it; `Ctrl`+click on it opens the file view of the unchanged `src/b.ts` at its definition, the caret on the symbol (`history` one back), `Alt ←` returns; `F12` at `total`, defined near the change, scrolls the stream there; `Shift F12` lists the references in the References tab by file in path order, the changed row marked, **In this review** keeping it alone; a row opens its place; the tabs switch and closing the list takes them away; the status bar's details open and close; the server read the stream's text (`lsp_log`); screenshots of the hover card (dark and light), the references with and without In this review, the file view after a definition and the server's details |
| `review_intel_crash` | a language server that crashes (PLAN §18.5; `fixtures/lsp/review_intel_crash.json`): `mock_lsp` exits on the first hover, which shows nothing; the server restarts (`starts` 2, ready again) and the next hover's card opens; `lsp_log` has the crash; a screenshot of the card after the restart |
| `review_intel_missing` | a TypeScript project and no `lsp` header: under automation servers come only from `DV_LSP_<ID>` (PLAN §18.5, "Automated runs"), so none starts, `intel.missing` says why ("DV_LSP_TYPESCRIPT isn't set, …") and how to get it, a hover shows nothing, and the status bar's "TypeScript missing" opens its details; a screenshot of them |
| `review_intel_stacked` | several servers on one file and hovers in class strings (PLAN §18.1, §18.4, DESIGN §4.1; `fixtures/lsp/review_intel_stacked_ts.json` and `_tw.json`, the header's `lsp` naming both): a `.tsx` in a Tailwind v4 project starts TypeScript and Tailwind CSS with the review; the pointer on the `500` of `bg-red-500` asks at that character, TypeScript says nothing in a string and Tailwind's card spans the class (`intel.hover.offset`, `span`), so moving onto `red` keeps it unasked; on `className` both answer (Tailwind 300 ms later) and the card stacks them in the registry's order (`intel.hover.sections`, `hover/section/<id>`); on `title` only TypeScript; on `rounded-lg` Tailwind answers at once and leaving it cancels TypeScript's slow request (`$/cancelRequest` in `lsp_log`); screenshots of the class card, the stacked card (dark, light) and the single one |
| `review_intel_tailwind_missing` | a Tailwind v4 project whose header names only TypeScript: TypeScript starts, `intel.missing` has Tailwind CSS ("DV_LSP_TAILWINDCSS isn't set, …"; the hint naming `bun add -g @tailwindcss/language-server` and npm's), a hover shows TypeScript's answer alone, and the status bar's "Tailwind CSS missing" opens its details, whose **Check again** looks again (`intel.rechecks` 1, still missing); a screenshot of them |
| `review_intel_sides` | which text a server reads and where its answers go (PLAN §18.1, §18.2; `fixtures/lsp/review_intel_sides.json`): a hover on the old side opens that side's text at the path (`lsp_log`: its `didOpen`, then a `didClose` once answered) and says "Read against the working tree"; `Ctrl`+click on the old side lands on the old side's line in the same file; a staged review's definition in another file opens the file view at the working tree (`file_view.worktree`, every line unmarked, `C` opening no composer), `Alt ←` back; screenshots of the old side's card and the working tree's file view |
| `review_intel_places` | where code intelligence's places go and what they read (PLAN §18.1, §18.2, DESIGN §4.1): references asked on a changed line's old side list that side's text, marked changed (`intel.references.rows[].side` "old") and a click goes to the old line; 700 references (mock_lsp's answer) show their count as soon as the server answers (`wait_for` on `intel.references.count`), the rows' text once read, the review's file first; a hover at the window's bottom hangs above its token, inside the main area (`bounds` of `hover`); `Ctrl`+click into a library's declarations in another repository beside this one (`${parent}` in the fixture) opens a read-only file view of it (`file_view.outside`), where hover works and `C` does nothing; `Esc` in the code after a press to quote keeps a composer's draft, `C` gives it the keyboard and `Esc` in the composer discards it; screenshots of each |
| `review_file_view` | the file view (PLAN §18.1, DESIGN §4.1 "File view"): the header menu's View file (`stream/menu/view_file`) opens the stream's file whole at the cursor's line (`file_view`: its new side's 60 lines, `changed_lines` the review's), the cursor moving in it; `Esc` returns to the stream at the line it left, `F` opens it again, `Alt ←` / `Alt →` step between the views (`history`), a draft survives `Ctrl P`, `Alt ←` and the close button (the composer keeps the keyboard) until `Esc` discards it, `Ctrl F` searches that file only (from the cursor on), `Esc` closes the bar first, then its close button the view; screenshots of the file view and its find bar in dark Mica, the file view in light |
| `review_palette` | `Ctrl K` (DESIGN §4.1 "Palette"): a click on the title bar's command field opens the palette (`palette`), its field focused and its first command selected; typing filters by every word, `Enter` runs Next file in the main area (`file_list.current_file` moves), a typed name lists the review's file and a click on its row opens it in the file view, a repository's targets list by name, and `Esc` and `Ctrl K` close it; screenshots of the palette empty, filtered to a file and in light mode |
| `review_themes` | the themes (DESIGN §1.3): Settings › Appearance's Theme row switches to Claude, which is opaque over the Mica setting (its Mica and Acrylic disabled, the note keeping Mica for Aura), then light; Dracula asked for light shows dark, the mode row saying so; back to Aura the light mode and Mica return; screenshots of a review with an annotation in Aura Dark, Claude Light, Claude Dark and Dracula, and of Settings in Claude Dark, Claude Light and Dracula |
| `review_file_finder` | `Ctrl P` and comments on any line (PLAN §18.1, §8): the file finder (`file_finder`) lists every file of the working tree, the changed one first, filters (a name match before a directory match), moves and opens an unchanged file in the file view; `C` on a line of it posts a thread placed there (`session.threads[].in_diff` `false`, `placement` `unchanged`), its card inline; back in the stream it heads the "Not in the diff" section with its "Unchanged file" chip and View file (`thread/<id>/view_file`), which opens the file view at its line; `dv comment list --json` (the automation host's `cli`) says `unchanged` and `in_diff: false` with `anchor.current`, `--oneline` `(not in the diff)`; the agent then edits the file, which joins the review, and the thread follows its code, `moved`; screenshots of the finder, the file view with the card and the section |
| `workflow_a_agent` | PLAN §3 workflow A end to end, the agent played by the real CLI (`cli`, `cli_spawn`, `cli_join`: this `dv.exe` as a child process) and the reviewer by keyboard alone: before any review the agent's `dv status` has none, and `cli` refuses `open`; `Enter` opens the working review from the switcher and `dv status` names it; the agent's `dv review wait` blocks (still running after the reviewer's comment) while `V`, `J J`, `Shift ↓`, `C` and `Ctrl Enter` comment and `Ctrl Enter`, `Tab`, the summary and `Ctrl Enter` request changes; the wait wakes with `changes_requested`, the round's summary and one open thread; `dv comment list --status open` gives the thread and its lines (the comments list's location agrees); the agent's fix inserts lines above the thread (`repo_edit`) and `dv comment show` finds it moved; `dv comment reply --body-file -` over stdin (the review goes `in_progress`), `dv comment address --note`, `dv review done --note`; the GUI shows the ready banner, the interdiff since round 1 with one file and the thread addressed at its new lines (`moved`, its reply's recorded anchor); a second wait (by id) blocks while `]`, `X` and `Ctrl Enter`, `Tab`, a summary, `Ctrl Enter` approve, then wakes with `approved`; `dv comment list` (`$DV_REVIEW`), `dv review list --all` and `dv status` see the finished review, and a bare `dv review wait` is exit 2 `no_review`; a wait left blocked on a staged review (`dv review new --staged`) outlives `cli_join`'s wait and is killed as the app quits; screenshots of the comment, both sheets, the ready interdiff, the moved card and the resolved one in dark Mica, the approved review in light |
| `review_worktree` | reviews homed in a linked worktree (PLAN §2.2, §2.4): the agent, in a linked worktree on `feat` (`wt/`, ignored by the main work tree), starts its working review and comments through the CLI, and `dv comment list` run from the main work tree places the thread there; the reviewer opens it from the main work tree's switcher row (a click on its review row) and it lists the linked worktree's one file with the thread `unchanged` at its line, labelled `working tree vs feat`, the main work tree the only known repo; finishing round 1 from that session snapshots the linked worktree (the interdiff since it is empty); `dv open <main> --review <id>` opens the same; a branch review of `feat` started from the main work tree means the same from both worktrees in the CLI (and `review show` without an id is ambiguous in the linked worktree, which both reviews belong to, and finds none in the main one) and in the GUI (`open_review` from the main work tree); with the linked worktree removed, `dv comment list` is exit 1 `target` and the GUI's open is the main area's banner; screenshots of the review from the main row, after `dv open`, and the banner |
| `docs/readme_screenshot` | Windows only (the frame's bounds are Windows' fonts and chrome), on demand: the README's hero image, `docs/images/dv-review.png`, copied by hand from its `NN-readme.png` (the run never writes into the checkout). Dark, Opaque, 1600x1000: a made-up shop's `src/components/ProductCard.tsx` (`fixtures/lsp/readme_screenshot_ts.json` and `_tw.json`) in split view, highlighted and opened as `typescriptreact` (`lsp_log`), TypeScript and Tailwind CSS ready, nothing missing; the reviewer (the repo's `user.name`, Sam Rivera) comments on the badge's condition through the CLI and Claude Code replies through it (`"CLAUDECODE": "1"`), the thread inline with its reply; the hover card on `percentOff` in the changed code (`intel.hover`, Go to definition and Find references) hangs under its token and ends above the thread, which ends above the status bar (`bounds`); one screenshot |

### dv's `state`

`state` is the object dv's automation host (`crates/dv/src/automation/`,
the `dv` binary's `automation` feature) returns, plus the library's `window`
and `windows` keys (toolbox-rs docs/automation.md, "Protocol"). Its keys are listed here as they land;
assert on them rather than on screenshots, which are for style.

| Key | Contents |
|---|---|
| `app` | `"dv"` |
| `data_dir` | the data dir this run uses (absolute) |
| `settings.palette`, `settings.material`, `settings.mode` | the settings as set: `"aura"` / `"claude"` / `"dracula"` (DESIGN §1.3), `"mica"` / `"acrylic"` / `"opaque"`, and `"system"` / `"dark"` / `"light"` |
| `settings.resolved_palette`, `settings.resolved_material`, `settings.resolved_mode` | what is on screen: the palette, the material the platform and the palette can show (an unsupported one, or any but Opaque with Claude or Dracula, resolves to `"opaque"`, PLAN D8) and `"dark"` / `"light"` (`system` follows the OS; Dracula is always `"dark"`) |
| `settings.available_materials` | the materials this machine can show now, e.g. `["mica", "acrylic", "opaque"]`; `["opaque"]` on Windows 10, with transparency effects off, on Linux and on macOS |
| `settings.path` | the settings file, `<data_dir>/settings.json` |
| `settings.save_error` | why the last save failed, or `null` |
| `shell.panel_visible` | whether the left panel shows |
| `shell.panel_width` | the panel's width in logical pixels (`settings.json`'s `panel_width`; 280 until resized, 220–420) |
| `shell.empty_state` | the main area's empty state (DESIGN §4.4): `"no_review"`, `"working_tree_clean"` (a `working` review with no changes), `"no_changes"` (another target with none), `"no_changes_since"` (an empty interdiff, with Show all changes), or `null`; `null` while the stream's "Not in the diff" section holds threads, which then says it as its last row (`stream.not_in_diff.empty`) |
| `shell.opening` | a review is being opened (the switcher, the source picker or `open_review`) |
| `shell.pending` | the pick that waits for its connection (PLAN §5.2), or `null`: `{repo, target, review, connection}` (`target` the stored shape, or `review` the id asked for; `connection` its name). A review picked whose open failed because its connection was lost under it again or is down (failed, stopped) runs again by itself once that connection is back, unless the open review holds unsaved text (`session.sheet` open, or a composer with a draft): it then waits on until the sheet closes or the draft is posted or cancelled; meanwhile the main area raises nothing for it when the open review goes through the same connection (its backend banner and status bar say what is wrong), else `shell.banner` says "couldn't open …", whose Retry runs it at once. A newer pick or its own banner's Dismiss drops it; another banner's Dismiss leaves it |
| `shell.banner` | the main area's banner, `null` or `{message, detail}`: a review that didn't open (its Retry runs the open again), or a folder or `open` path (the `open` command, `dv open` as the app starts) that isn't a repository |
| `shell.backend_banner` | the backend-down banner (`banner/backend`, DESIGN §4.4), `null` or `{message, detail, action}`: once the open review's connection to its `dv backend --stdio` has failed for good, "Lost the connection to dv in Ubuntu" (the connection's name: the distro, `loopback`), the error and the connection's diagnostics as its detail, `action` `"Retry"` (`banner/backend/retry`); once it found its host stopped, "Ubuntu stopped", `action` `"Start Ubuntu"` (`banner/backend/start`); never while it only reconnects |
| `shell.status` | what the status bar says: `{health, location, review, finish}`; `health` is the dot's `"idle"`, `"live"`, `"degraded"` (refreshes when focused) or `"failing"`; `location` e.g. `"Local · watching for changes"`, `"No repo"`, `"Ubuntu · can't be opened"`; a remote repo is named by its connection, whose state comes first: `"loopback · connecting…"`, `"Ubuntu · installing dv (3.1 / 7.4 MB)…"` (`idle`), `"… · reconnecting…"` (`degraded`), `"… · stopped"` and `"… · disconnected"` (`failing`), while a connection closed for idleness reads as connected (the review's own state); `review` e.g. `"Open · round 1 · no comments"` (an interdiff adds `" · since round 1"`) or `null`; `finish` whether `Ctrl Enter finish review` shows |
| `shell.window_material` | the material last applied to the window (it follows `resolved_material` before the reply to the command that changed it) |
| `system.requests` | what the app asked the OS to do, oldest first, recorded instead of performed: `{"kind": "open_url", "url"}`, `{"kind": "reveal", "path"}`, `{"kind": "pick_folder", "prompt"}`, `{"kind": "clipboard", "text"}` (nothing reaches the real clipboard) |
| `system.answers_left` | folder-picker answers still queued |
| `repos` | the known repositories (`<data_dir>/known_repos.json`), most recently used first: `{path, name, location, loading, error, stopped, retry, reconnecting, starting, working, reviews}`. `location` is the stored shape (`{"local": "D:\\…"}`, `{"wsl": {"distro", "path"}}`); `error` is `null` or `{message, detail}` (a WSL location's says why it can't be opened: `"Ubuntu isn't running"`, or its connection's failure, kept when the distro still isn't running after a failed start); `stopped` is the distro (or connection) of a repo whose host isn't running, whatever its connection did before (its row offers **Start**), else `null`; `retry` whether its connection failed with its host running (its row offers **Retry**); `reconnecting` whether its connection is reconnecting (the row keeps `working` and `reviews` from its last listing); `starting` whether a Start or Retry is connecting; `working` is `{files, additions, deletions}` of the uncommitted changes, `null` until computed (on `open`, and when the switcher opens) or when it can't be; `reviews` newest first, each `{id, target, target_label, status, round, threads: {open, addressed, resolved, annotation}}`, or `{id, error}` for a log that doesn't load |
| `switcher` | the switcher (`Ctrl O`): `{open, mode, query, selected, items, menu}`; closed it is `{open: false, mode: null, query: null, selected: null, items: [], menu: null}`. `mode` is `"repos"`, or the branch picker's `"branches"` (a repo's branches) and `"bases"` (the bases the picked branch can be compared with, DESIGN §4.1). `menu` is a repo header's open menu (a right-click on `switcher/repo/<repo>`, or a click on its `⋯`, `switcher/repo/<repo>/more`; the repositories' list only), `{repo, removable}` (`repo` its path; `removable` `false` for the open review's repo, whose **Remove from list**, `switcher/menu/remove`, is disabled), else `null`. `items` are its pickable rows in order, each `{key, kind, target, label, detail, starting, repo, review}`: `key` names the row (`<repo>/working`, `<repo>/staged`, `<repo>/branch`, `<repo>/review/<id>`, `<repo>/start`, `<repo>/retry`, `<repo>/compare`, `open_folder`; in the branch picker `<repo>/pick/<branch>` and `<repo>/base/<branch>`; `<repo>` is the repo's name, its full path when two known repos share a name), `kind` is `"target"`, `"review"`, `"start"` (**Start Ubuntu**, a stopped distro's one row), `"retry"` (**Retry**, a failed connection's one row), `"open_folder"`, `"compare"` (**Compare branches…**), `"branch"` or `"base"`, `starting` whether it is a Start or Retry whose start runs (drawn inert; picking it does nothing), `target` the target or base row's `"working"`/`"staged"`/`"branch"` (else `null`), `detail` a branch row's `"vs <base>"` or a base row's `"stack base"` / `"default branch"` (else `null`), `repo` its path, `review` its open review `{id, status, round, open_threads}` or `null`; `selected` is a `key` |
| `source_picker` | the source picker (the title bar's `⇄ ▾`): `{open, selected, items: [{kind, label, review}], compare}`, `selected` a `kind` or `"compare"`, `compare` whether it offers **Compare branches…** (`source_picker/compare`) after the items |
| `file_list` | the panel's file list: `{query, shown, current_file}`; `shown` the paths the filter leaves, in list order; `current_file` the stream's current file (the highlighted row) |
| `session` | the open review, or `null`: `review_id`, `repo` (the root of the review's home worktree, where its target resolves: PLAN §2.2; another worktree's than the row it was opened from for a working review made in a linked worktree, or a branch review of a branch checked out in one), `target` (stored shape: `"working"`, `"staged"`, `{"branch": {…}}`, …), `target_label` (`"working tree vs main"`), `status`, `round` (the one in progress; for a finished review the one that finished it, as the status bar says), `read_only` (approved, closed or submitted), `loading` (the file list is loading), `error` (`null` or `{message, detail}`), `viewed_error` (why the last viewed toggle failed, shown as `banner/viewed`; `null` once one lands or it is dismissed), `watch_notice` (why the worktree isn't watched, or `null`), `generation` (the load generation), `warnings` (problems in the log), `viewed` / `total` (files viewed this round / listed), `current_file` (the stream's current file: the cursor's while the cursor is on screen, else the file at the top), `view_mode` (`"split"` or `"unified"`, remembered per review in `<data_dir>/review_ui.json`), `pane` (`"stream"` or `"file_view"`: the pane `cursor`, `selection`, `composer` and `focused_thread` describe, the one the main area shows), `cursor` (`null` or `{path, side, line}`: `side` `"old"`/`"new"`, `line` its number on that side), `selection` (`null` or `{path, side, start_line, end_line}`, one file and one side) |
| `session.files` | sorted by path: `{path, old_path, status, additions, deletions, binary, viewed, open_threads, state, kind, highlighted}`; `status` is `added`, `modified`, `deleted`, `renamed`, `copied`, `type_changed`, `untracked` or `unmerged`; `state` is `unloaded`, `loading`, `loaded` or `failed` (diffs load when the stream asks for them); `kind` is the loaded diff's `text`, `binary`, `too_large`, `submodule` or `unmerged`, else `null`; `highlighted` once syntax kinds landed |
| `session.threads` | live threads in the order they were added: `{id, path, side ("old"/"new"), start_line, end_line, file_level, state, annotation, card, note, author: {kind, name}, body, replies: [{id, author, body}], outdated, placement, file, lines, trail, original_lines, round}`; `state` `"open"`, `"addressed"`, `"resolved"` or `"annotation"` (an agent's annotation nobody asked about, PLAN §19), `annotation` whether an agent opened it as one (asked about since or not), `card` whether its card is drawn where it is placed (`false` for an annotation hidden with `A`, outdated, or on a file the review no longer changes, PLAN §19.3); `path`, `side`, `start_line`, `end_line` and `round` are its anchor as recorded (a reply or state change on a moved thread records where it moved, PLAN §8); `placement` is where the stream shows it (`ReviewSession::placement`, `dv_core::review::place_thread`): `"unchanged"`, `"moved"` (showing at other lines than it was written on, relocated now or standing where a recorded move put it: `review::shows_moved`; `"edited"` (standing where it was written, its text changed there: `review::shows_edited`, as the CLI says), `"pending"` (its file's text, or its round's history, isn't there yet: every file a thread shows in loads whatever the viewport, and a history is read at once, so this is brief), `"outdated"` (relocation can't find it, or its file left the diff), `"file"` (a comment on the whole file, `file_level` `true` with lines 0, standing in its file: `lines` `null`) or `"not_in_comparison"` (an interdiff that doesn't show it: an old-side thread, or its file isn't in it); `lines` `{start, end}` where it shows now (for a thread not in the comparison, where it stands in all changes, read from the review's own diff of its file), or `null`; `in_diff` `false` for a thread whose file isn't among the review's changes (placed in that file at the new side — `placement` `unchanged`, `moved`, `edited` or, a comment on the whole file, `file`, `lines` where it stands there —, `pending` while that file's first read hasn't landed, or outdated), which the "Not in the diff" section shows; `file` the listed file the stream draws it in (after a rename its round's history followed, the new name; `null` when its file left the diff, when the interdiff shown doesn't draw it, or while it is `pending` with no listed file at its path — a thread placed before keeps its placement, file included, while only its round's history is read); `trail` where an outdated thread's code went since its round (PLAN §8), `{round, went ("replaced"/"removed"), start, end}` (for `replaced`, the lines that replaced its code; for `removed`, the line it now follows, or 1 when it stood at the top, where its card heads the file), else `null`; `original_lines` the lines it was opened on; `outdated` `true`/`false` (for a thread not in the comparison, as in all changes), `null` while pending |
| `session.composer` | the open composer, or `null`: `{path, side, start_line, end_line, file_level, reply_to, text, posting, error}`; `file_level` a new comment on the whole file (lines 0, heading its file); `reply_to` the thread a reply answers (`null` for a new thread), `text` as typed, `posting` while a post is on its way, `error` `null` or `{message, detail}` (why the last post failed) |
| `session.focused_thread` | the keyboard's thread (`]`/`[`, a click on a card or a comments row, a new post), or `null` |
| `session.sheet` | the "Finish round" sheet, or `null`: `{verdict, can_submit, submitting, tab, summary, error}`; `verdict` `"approve"` or `"request_changes"`, `tab` `"write"` or `"preview"`, `summary` as typed |
| `session.rounds` | closed rounds: `{n, verdict, old_oid, new_oid, body}`; `new_oid` the snapshot at `refs/dv/reviews/<id>/<n>` |
| `session.comparison` | what the files and diffs shown are (PLAN §2.3): `{"kind": "since_round", "round": n}` (the interdiff since round n) or `{"kind": "all"}`; `session.comparison_asked` is the one asked for (`I`, the panel's toggle), which `comparison` becomes once its listing lands |
| `session.comparison_available` | whether the review offers the toggle: a live target (`working`, `staged`, a branch), not finished, with a closed round |
| `session.comparison_error` | `null`, or `{message, detail}` when the interdiff asked for fell back to all changes because its round's snapshot is missing (the main area's `banner/comparison`, Dismiss) |
| `session.annotations_shown` | whether the stream and the file view draw the agent's annotations (`A`, PLAN §19.3; `true` whenever a review opens) |
| `session.ready_note` | the agent's handoff while the status is `ready` (the main area's `banner/ready`), or `null`: `{author, round, note, annotations, open}`; `author` the agent's `{kind, name}` (`null` for an event that doesn't say), `round` the round handed back (the one in progress), `note` the `status` event's note (`dv review done --note`) or `null`, `annotations` how many annotations nobody asked about the agent wrote in that round (the banner's "· 5 annotations"), `open` whether Show note has opened it |
| `comments_list` | the panel's comments, or `null` with no review open: `{show_resolved, show_annotations, expanded, rows: [{id, state, outdated, not_in_comparison, pending, location, file_level, summary, snippet, unlisted, drawable}]}`: the addressed threads, then the open ones, each in stream order (threads the stream doesn't show last), then the agent's annotations while `show_annotations` (`comments_list/annotations`), then the resolved ones while `show_resolved`; `unlisted` it stands in a file the review doesn't change, `drawable` the stream can draw its card (`false` for an annotation that is outdated or on a file the review no longer changes: a click on its row opens its text under it, `comments_list/<id>/body`, and `expanded` names it); its menu holds `comments_list/menu/ask` alone; `not_in_comparison` the "all changes" tag (the interdiff doesn't show it; a click on the row switches to all changes), `pending` its file's text hasn't landed yet (`location` is then where it was written), `location` is `path:31` or `path:12–14` where it stands now (for a thread not in the comparison, in all changes; else where it was anchored), with " (old)" after an old-side thread's, `path (file)` (`path (file, old)`) for a comment on the whole file (`file_level`), `summary` the comment's first line as plain text (its Markdown dropped), `snippet` an outdated thread's first anchored line as it was (else `null`); threads in the stream's "Not in the diff" section come first in each group, in the order they were added; `menu` the thread whose row's right-click menu is open (its items `comments_list/menu/resolve`, `/reopen`, `/reply`, `/copy`), else `null` |
| `stream` | the diff stream, or `null` with no review open: `top_file` (the file at the top of the viewport), `pinned_file` (the file whose header is pinned at the top, `null` while a header sits there itself), `h_offset` (the sideways scroll in logical pixels, one for every row), `focused` (the main area has the keyboard, so the stream's keys work), `files` (in the session's order: `{path, collapsed, rows, gaps, notice, heading}`; `collapsed` a viewed file shown as its header alone; `rows` the file's rows in the stream, header and footer included; `gaps` how many lines each gap row hides, top to bottom; `notice` what shows instead of lines: `loading`, `failed`, `binary`, `too_large`, `submodule`, `unmerged` or `no_content_change`, else `null`; `heading` the cards heading its rows, in order: thread ids — its comments on the whole file, then its outdated threads — with `"composer"` between them while a comment on the whole file is being written), `not_in_diff` (the "Not in the diff" section heading the stream, `null` while no thread needs it: `{collapsed, threads, empty}`, `threads` the ids of the threads whose file left the review, in the order they were added (none before a file list has landed: they are `pending` meanwhile), `empty` the empty state its last row says while no file is listed (`shell.empty_state`'s keys; `null` otherwise; its Show all changes is `stream/not_in_diff/empty/show_all`); its header is `stream/not_in_diff/header` with `/chevron`; it is never `top_file` or `pinned_file`, and isn't among `files`) |
| `stream.text_pane` | `"stream"` or `"file_view"`: the pane `stream.text_selection` and `stream.caret` describe (the one the main area shows); every other `stream` field is the stream's own |
| `stream.text_selection` | text selected with the mouse (DESIGN §4.1 "Text in code"), or `null`: `{path, side, from, to, text}`; `side` the split view's column (`"old"`/`"new"`), `null` in unified view, where each end is on its line's own side; `from` the first selected character and `to` the place after the last, each `{side, line, column}` (`line` its number on that side, `column` 1-based in characters of the source line, a tab one character); `text` what `Ctrl C` copies (the source text, lines joined with `\n`) |
| `stream.caret` | the caret (a click in code, a find match's start), or `null`: `{path, side, line, column}` as above, the column the character it stands before; keyboard line moves clear it |
| `find` | the shown pane's find bar, or `null` with no review open: `{pane, open, query, current, count, complete, skipped, focused}`; `pane` `"stream"` or `"file_view"`, the pane whose bar it is; `query` the text searched for (the field's, once typing paused), `current` the current match's place among them from 1 (`null` with none), `count` the matches found (0 while closed), `complete` every listed file searched (`false` while some still load for it: the bar reads "3 of 17+"), `skipped` files without text to search (binary, too large, submodule, unmerged, failed: "2 files not searched"), `focused` the field has the keyboard |
| `file_view` | the file view while it shows (PLAN §18.1), else `null`: `{path, side, top_line, line_count, changed, changed_lines, loading, notice, worktree, outside}` (`worktree` the file read as the working tree holds it now, for go to definition in a review whose new side isn't it; `outside` a file outside the repository, `path` then absolute on the backend's machine, read-only); `side` `"new"` (`"old"` for a file the review deletes), `top_line` the first line in view below its header, `line_count` its side's lines, `changed` the review changes it (its own list, all changes, whichever comparison shows), `changed_lines` the numbers of its lines marked changed as the stream marks them (in an interdiff, since the round; the first 200), `loading` its text isn't there yet, `notice` what shows instead of lines (`failed` — the side holds no such file, or the read failed —, `binary`, `too_large`, `no_content_change` for an empty file), else `null`. While it shows, `session.cursor`, `session.selection`, `session.composer`, `session.focused_thread`, `stream.text_selection`, `stream.caret` and `find` are its own (the shown pane's), `stream.focused` whether it has the keyboard |
| `palette` | the palette (`Ctrl K`): `{open, query, rows, selected, focused}`; closed it is `{open: false, query: null, rows: [], selected: null}`. `rows` the first 40 in order, each `{key, section, label}`: `section` `"commands"`, `"reviews"` or `"files"`; `key` `command/<action>` (`command/diff_pane::NextFile`), `review/<the switcher's row key>` (`review/web/staged`) or `file/<path>`; `label` the row's words (`"web · Staged changes"`); `selected` a key |
| `file_finder` | the file finder (`Ctrl P`): `{open, query, rows, count, selected, complete, focused}`; closed it is `{open: false, query: null, rows: [], count: 0, selected: null}`. `rows` the first 20 paths shown, in order (the review's changed files first among equal matches), `count` how many pass the filter, `selected` a path, `complete` the new side's paths have been read (the review's own files list meanwhile), `focused` its field has the keyboard |
| `history` | `{back, forward}`: how many positions `Alt ←` / `Alt →` can step to (jumps between the stream and the file view push one; go to definition and a references row push one too) |
| `intel` | the open review's code intelligence (PLAN §18.5, §18.6), or `null` with no review open: `{servers, missing, hover, definitions, references, panel_tab, notice}`. `servers` its worktree's language servers in the order they were found: `[{id, name, state, error, root, version, command, source, starts, progress, stderr}]`, `root` the directory it serves as the backend's machine spells it, `version` its `serverInfo` version (`null` when it gave none, or before it is ready), `state` `"starting"`, `"ready"`, `"restarting"`, `"waiting"` (for its connection) or `"failed"` (`error` why), `source` where its command came from (`DV_LSP_TYPESCRIPT`, "the project's own (7.0.2)", `settings.json`), `starts` how many times it started, `progress` its work in progress `[{title, message, percentage}]`, `stderr` its last lines; `missing` the specs the review's files need that no server runs for, `[{id, reason, hint}]`; `hover` the card, `{open: true, pane, path, side, line, symbol, offset, span, text, sections, against_worktree, note}` (`note` what a card without sections says, "No information: read against the working tree", else `null`) (`offset` the byte of the line it asked at, 0-based; `span` `[start, end)`, the bytes it is about: the servers' range, else the token) (`pane` `"stream"` or `"file_view"`, `text` every section's parts joined by blank lines, `sections` one per server that answered with anything, in the registry's order: `[{server, text}]`, `server` the spec id), or while one is asked for `{open: false, pane, path, side, line, symbol}`, else `null`; `definitions` how many places the last go to definition found (`null` before one answered); `references` the References tab's list, or `null`: `{kind, symbol, loading, reading, error, count, in_review, against_worktree, files, rows: [{path, in_repo, side, line, column, text, changed}]}`, `rows` those shown (with `in_review` only the changed ones), the review's files first, `count` every place found (known as soon as the server answered: `loading` false), `reading` the rows' lines still being read or highlighted (`text` `null` until its file is read), `side` the side a row was read on when the server read the asking file's other side (`"old"`, `"new"`), else `null`; `panel_tab` `"review"` or `"references"`; `notice` the main area's quiet notice for a place that can't be shown (a URI that isn't a file), or `null`; `rechecks` how many times missing servers were looked for again (**Check again**, a review opening, the window coming back) |
| `cli` | `{spawned: [{name, args, pid, running}]}`: the `cli_spawn` children not yet collected by a `cli_join`, oldest first; `args` as given, `running` whether the process is still running (one that has exited stays listed until it is joined) |
| `backends` | the app's connections to `dv backend --stdio` servers (PLAN §5.1), WSL distros first, then loopbacks: `[{name, state, attempt, generation, error}]`; `state` is `"idle"` (made, nothing started, or its server closed after a minute unused), `"connecting"`, `"installing"`, `"connected"`, `"reconnecting"`, `"stopped"` (an automatic start found its host stopped and started nothing) or `"failed"`; `attempt` the reconnect attempt (from 1) while reconnecting, else `null`; `generation` how many servers it has started, failed starts included; `error` why it failed, else `null` |
| `setup` | the setup screen (PLAN §12, DESIGN §4.3): `{open, first_run, settings_open, checking, notice, environments}`. `first_run` whether the first-run screen shows (a run shows it only with `DV_FIRST_RUN=1` in the header's `env`, until Continue or Skip writes `<data_dir>/onboarding.json`); `settings_open` whether Settings is; `open` either; `checking` a check, fix or start is running; `notice` the status bar's `{environment, text, action}` ("dv-review skill out of date on Windows", `"Update"`) or `null`; `environments` this machine first, then the distros WSL lists (the stand-in's, from `wsl_distros`, are listed and never reached): `[{name, kind, running, note, checking, starting, start_error, checks: [{id, title, state, detail, fix: {action, command} | null, applying, error}]}]`, `kind` `"windows"`, `"macos"`, `"linux"` or `"wsl"`, `state` `"ok"`, `"missing"`, `"out_of_date"`, `"failed"` or `"skipped"`, `error` why its last fix failed. At startup (but on the first run) only this machine's skills are checked, so `environments/0/checks` lists those three until something asks for every check (Check again, Settings opening). The checks run against `<data_dir>/home` (git's global configuration there; `GIT_CONFIG_NOSYSTEM` set, so the system's isn't read), `<data_dir>/user-path.txt` for the user `Path` (no machine `Path` unless `DV_MACHINE_PATH_FILE` names one) and `DV_GH` (`mock_gh`); a running distro is checked only in a run sandboxed by the header's `DV_WSL_HOME`, and then only `DV_TEST_WSL_DISTRO`'s (Start and fixes likewise); targets `setup/check_again`, `setup/<environment>/start`, `setup/<environment>/<check>/fix` and `…/copy`, `setup/skip`, `setup/continue`, `settings`, `settings/close`, `titlebar/settings`, `status/skill_notice/update` and `…/dismiss` |
| `update` | dv's own update (PLAN D9): `{ready, restart_requested, confirm, failure, scripted}`. `ready` the version downloaded and ready to restart into, else `null` (an automation run never checks a feed: only `update_ready` makes one ready); `restart_requested` a restart went ahead (Restart to update with nothing to lose, or the confirmation's Restart anyway); `confirm` what the confirmation says will close while it shows (`["2 other dv windows or commands running from this install", "your unsent comment in this window", "the Finish round sheet in this window"]`, those that apply), else `null`; `failure` why the last restart failed, until another is tried; `scripted` the seam's stand-in records the restart instead of applying it, as every automation run's does. Targets: `status/update` (the item), `status/update/restart` (Restart to update, or Retry after a failure), `status/update/failed` (the failure, its tooltip the error), `status/update/confirm` (the confirmation), `status/update/not_now`, `status/update/restart_anyway` |

`settle` waits for the settings and the known repos to be written, for every backend open,
repo row refresh and review open (the shell's included) and any "Open folder…" to finish, and,
with a review open, for its file list (and
any refresh the watch scheduled), its fold reloads, its writes and the files asked for (every listed file while the find bar searches, and
their highlighting) to land, for typing in the find field to have been searched, for a post or a thread's Resolve/Reopen and a round being finished
(the sheet's Submit) to be through, and for the stream to have worked out what its viewport shows after the last frame, as well as for a frame. A change the watch reports (an agent's
edit or append, the CLI's included) arrives later than any `settle` can know about, so scenarios
`wait_for` it. `cli` and `cli_spawn` children never hold up `settle` or `wait_ready`.

dv's own commands:

| Command | Arguments | Result |
|---|---|---|
| `set_setting` | `key`: `"palette"`, `"material"` or `"mode"`; `value`: one of that key's values above | `{settings}` (as in `state`); the change goes through the settings service as the UI's would: it is saved to `<data_dir>/settings.json` and the window has repainted in the new theme and material before the reply |
| `seed_repo` | `name` (a plain file name); `steps` (below); `backend` (`"local"`, the default, or `"loopback"`) | `{path, commits}`: a fixture repo built at `<data_dir>/repos/<name>` (`FixtureRepo::persistent`; its home in `<name>.fixture/`), `steps` run on it, and its environment registered so dv opens it, and each of its linked worktrees, with the fixture's git configuration, never the machine's (`core.autocrlf=true` on the dev box). A repo seeded before a `restart` is found again at startup. With `"backend": "loopback"` dv reaches it, and its linked worktrees, through the connection `loopback`: this `dv`'s own `backend --stdio`, a child of the app started when the repo first opens (the fixture's environment sent in `open`), as a WSL repo is reached through its distro's server; a `<name>.loopback` file beside it says so after a `restart`, the repo `dv open` names as the app starts (`restart`'s `app_args`) included |
| `repo_edit` | `name` (a seeded repo); `steps` | `{path, commits}`: more steps on it, as the agent editing and committing |
| `open` | `path` (a local path anywhere inside a work tree, or `\\wsl.localhost\<distro>\<path>`) | `{path, location}`: what `dv open <path>` does as the app starts (`Shell::open_repo`): its worktree root becomes a known repo, its row is computed and the switcher opens with its first row (`<repo>/working`) selected, so `Enter` opens the working review; a WSL location connects to its distro (in a run without `DV_WSL_HOME`, WSL's stand-in, which refuses), and one whose connection fails is known as given, its row saying why (the selection then stays on the open review's row); any other path that doesn't open is an error, and the main area's banner (`shell.banner`) |
| `open_review` | `path`; `target` (default `"working"`; a stored target such as `"staged"` or `{"branch": {"branch": "feat", "base": null}}`) | `{review_id, created}`: opens or resumes that target's review as the reviewer (the repo's `user.name`) and shows it, through the shell as the switcher would (a failure also becomes `shell.banner`); `settle` then waits for its file list |
| `review_op` | `op`: `"reply"` (`thread`, `body`), `"address"` (`thread`, optional `note`) or `"done"` (status `ready` with an optional `note`, as `dv review done --note`: the ready banner, `session.ready_note`; refused, as the CLI's is, when the reviewer finished a round the agent hasn't seen, PLAN §2.4); `author` (the agent's name, `"agent"` by default) | the op's result (`{comment}`, `{changed}`): an agent's change to the open review straight through `dv_core::review::ops`, without the CLI's own rules (an agent's first write moving `changes_requested` to `in_progress`, relocated anchors, PLAN §10.1); the review sees it through its watch, so `wait_for` the change. Scenarios that play the agent the way an agent works run the real CLI instead (`cli`) |
| `cli` | `args` (`dv`'s arguments, a list of strings); `cwd` (default the app's working directory, the data dir; a relative one resolves against it); `stdin` (text written to the child's stdin, then closed; without it stdin is empty); `env` (`{"NAME": "value"}` sets, `{"NAME": null}` removes); `timeout_ms` (default 20000, at most 120000) | `{exit, stdout, stderr, json}`: runs this same `dv` executable as a child process with a headless command (PLAN §10), off the UI thread, to its end: `exit` its exit code (`null` if a signal ended it), `stdout` / `stderr` as text, `json` stdout parsed when it parses (a `--json` document), else `null`. A line that would open a window (no command, `open`) is refused before anything starts; `--help`, `--version` and lines clap refuses run (the child reports them). The child's environment is the app's (the runner's hermetic one, so `DV_GH` is `mock_gh`), then `HOME` and `USERPROFILE` = `<data dir>/home` and `CODEX_HOME` = its `.codex` (so `dv setup agents` without `--home` writes there, never the owner's real home; the app's own setup checks use that home too), then the fixture overlay of the seeded repo `cwd` is in, or of the seeded repo whose linked worktree it is (its git configuration and its own `HOME`, as the app's backend uses), `DV_DATA_DIR` = the app's data dir, `DV_USER_PATH_FILE` = `<data dir>/user-path.txt` (the user `Path`'s stand-in, the app's own: `dv setup path` never writes the registry), `CLAUDECODE`, `DV_AUTHOR` and `DV_REVIEW` removed (Claude Code exports `CLAUDECODE`, which the CLI takes for the agent `claude-code`), `DV_LINUX_DIR` = a folder without a Linux build unless the app runs with a `DV_WSL_HOME` of its own (so a command for a WSL repository is exit 4 rather than run inside a real distro, PLAN §10.4), `DV_ONLY_DISTROS` = the only distro its setup checks reach (the sandbox's `DV_TEST_WSL_DISTRO`, else none), then `env` (which can't change `DV_DATA_DIR`, `DV_GH`, `DV_MOCK_GH`, `DV_MOCK_GH_LOG`, `DV_LINUX_DIR`, `DV_WSL_HOME`, `DV_ONLY_DISTROS` or `DV_USER_PATH_FILE`); so `"env": {"CLAUDECODE": "1"}` makes it Claude Code and `"env": {"DV_REVIEW": "${review}"}` names its review. On Windows it starts without a console window. Still running at `timeout_ms`: killed, and the command fails |
| `cli_spawn` | `name` (letters, digits, `-`, `_`, `.`; not one spawned and not yet joined); `args`, `cwd`, `stdin`, `env` as for `cli` | `{name, pid}` once the child has started: for a command that blocks (`dv review wait`) while the scenario plays the reviewer. It is listed in `state.cli` until joined, never holds up `settle`, and is killed if it is still running when the app quits (a `quit`, a `restart`) |
| `cli_join` | `name`; `timeout_ms` (default 20000, at most 120000; the runner adds it to the line's reply timeout) | `cli`'s reply, once the child has exited: then it leaves `state.cli`. Still running at `timeout_ms`: the command fails and the child keeps running (a later `cli_join` waits again). An unknown or already joined name fails at once, naming the ones there are |

| `backend_kill` | `name` (a loopback connection's, or a distro's) | `{name, state}`: kills that connection's server as a crash would (`Connection::kill`): calls in flight fail with `Disconnected` and it reconnects after 1 s, 2 s and 5 s (PLAN §5.2); `state` is the connection's right after, `"reconnecting"`. Wait for what follows on `backends` |
| `backend_block` | `name` (a loopback connection this host made: `loopback`); `blocked` (bool) | `{name, blocked}`: while `blocked`, the loopback's server fails to start, so the respawns after a kill run out and the connection is `failed` (`shell.backend_banner`); unblocked, the banner's Retry (`banner/backend/retry`) connects again |
| `backend_hold` | `name` (a loopback connection this host made: `loopback`); `held` (bool) | `{name, held}`: while `held`, a start of the loopback's server waits (up to a minute), as a distro takes seconds to boot, so a scenario sees a start in progress (the switcher's inert Start row, `starting`); released, the start goes on (and fails if `backend_block` says so) |
| `backend_stop` | `name` (a loopback connection this host made: `loopback`) | `{name, state}`: stops the loopback's host as `wsl --shutdown` stops a distro, then kills its server: the host reads as stopped to the connection's host check, so the respawn starts nothing and the connection is `stopped` ("loopback · stopped", the banner's **Start loopback**, the switcher's Start row); any start that launches a server (the banner's Start, an open) boots the host again |
| `wsl_distros` | `distros`: `[{"name": "Ubuntu", "running": false}, …]` | `{distros}`: tells WSL's stand-in which distros there are from now on, and which run (its connections' host check reads it: a failed connection whose distro runs lists with Retry, one whose distro doesn't with Start) (below); an error in a run that reaches the machine's WSL (`DV_WSL_HOME` naming a scratch home) |
| `home_file` | `path` (relative, inside the home), `contents` | `{path}`: writes `contents` at `path` in the run's scratch home, `<data_dir>/home` (the home of the app's setup checks and of the `cli` children), its folders made: a fake agent's folder (`.claude/settings.json`), a skill an older dv installed, a `.gitconfig`. The setup checks see it once they run again (`setup/check_again`, Settings opening, a restart) |
| `update_ready` | `version`, `others` (default 0), `fail` (default none) | `state.update`: an update of dv downloaded and ready (PLAN D9), through the seam an installed copy's updates report to (`dv_ui::services::Updates`), with a stand-in for the install: `others` other dv processes running from it, which Restart to update asks about first, and `fail` the error its restart reports; otherwise it records Restart to update instead of applying it. Again with other arguments, the stand-in changes |

**WSL in automation runs.** A run reaches the machine's WSL only with `DV_WSL_HOME` naming a
scratch home (an absolute POSIX path other than `/`, PLAN §5.2; a scenario header's `env` can
set it, and dv checks it: blank or anything else is no sandbox, never the real home). Without it,
which is every scenario here, dv reaches WSL through a stand-in (`WslAccess::Scripted`): it lists
the distros `wsl_distros` names (none until then) and refuses to start dv in any of them, so no
scenario ever starts, installs into or boots a real distro. A WSL repo's row and banner then say
"Ubuntu can't be opened: the connection to dv failed: couldn't start dv: an automation run starts
dv in a WSL distro only with DV_WSL_HOME set (…)" (the row as its `error`, offering Start while the
distro isn't running, Retry once it is), and `backends` lists the failed connection.
The connection states themselves are scripted through the loopback (`backend_loopback`). The
agent's CLI (`cli`, `cli_spawn`) keeps out of WSL the same way: without `DV_WSL_HOME` its
`DV_LINUX_DIR` names a folder without a Linux build, so a command for a WSL repository is exit
4 (`backend_unavailable`) before any `wsl.exe` starts.

Steps for `seed_repo` and `repo_edit`, each one object, run in order (a failing step fails the
command with the fixture's message):

| Step | Does |
|---|---|
| `{"write": p, "text": t}`, `{"write": p, "bytes": [n, …]}` | writes a file, creating directories; `"repeat": n` (1 to 1,000,000) writes the text or bytes `n` times over, a large file in one step (`{"write": "seed.sql", "text": "INSERT …;\n", "repeat": 25000}`) |
| `{"write_crlf": p, "text": t}` | writes a file with CRLF line endings |
| `{"remove": p}` | deletes a file or a directory |
| `{"rename": [a, b]}` | moves a file |
| `{"commit": message}` | `git add -A` and commits, with the fixture's identity and dates |
| `{"branch": b}` / `{"checkout": rev}` | creates a branch at `HEAD` / checks out a branch or revision |
| `{"config": [key, value]}` | sets a key in the repo's `.git/config` (`["core.autocrlf", "true"]`) |
| `{"attributes": text}` | replaces the root `.gitattributes` |
| `{"worktree": [name, branch]}` | adds a linked worktree on a new branch at `HEAD`, at `<data_dir>/repos/<repo>.fixture/worktrees/<name>` (`FixtureRepo::add_worktree`) |
| `{"git": [args…]}` | any other git command (`["add", "a.txt"]`, `["commit", "-q", "-m", "x"]`) |

A seeded repo's linked worktrees, wherever they are (a `worktree` step, or `git worktree add`
through a `git` step), get its environment too: dv's backends opened on one, and `cli` children
started in one, run git with the fixture's configuration.

Named targets, at the default 1440x900: `titlebar` (0,0 1440x40), with
`titlebar/mark`, `titlebar/switcher`, `titlebar/source` (with a review open),
`titlebar/command` (300 wide, centred), `titlebar/min`, `titlebar/max`,
`titlebar/close` (46 wide and 40 high less the title bar's one-device-pixel
bottom hairline, Windows only); `shell/panel` (0,40 280x832) with
`shell/panel/header` and `shell/panel/resize` (the 8-wide handle on its right
edge); `shell/main` (280,40 1160x832) with `shell/main/empty`,
`shell/main/loading` and `stream`; `shell/scrim` under the top overlay (over the sheet too while a menu is open over it); `statusbar` (0,872 1440x28) with `statusbar/location`,
`statusbar/review` and `statusbar/finish` (the last two with a review open).
The switcher: `switcher` (560 wide, hung under its button), `switcher/filter`,
`switcher/repo/<repo>`, `switcher/<key>` per row (`state.switcher.items`, `switcher/<repo>/start` included), and
`switcher/empty`; the source picker: `source_picker` and
`source_picker/<working|staged|branch>`. The file list: `file_list/filter`,
`file_list/progress`, `file_list/<path>` (a 28-high row), `file_list/<path>/viewed`
(its checkbox), `file_list/loading` and `file_list/note`. Banners:
`banner/main` (a review that didn't open), `banner/session` (a failed
refresh), `banner/viewed` (a viewed toggle that failed) and
`banner/comparison` (an interdiff whose round snapshot is missing) and
`banner/backend` (the review's connection failed, DESIGN §4.4), each with
`/<action>` (`retry`, `dismiss`), `/details` (when there is a detail) and
`/copy`; `banner/finished` over a finished review, with
`banner/finished/new_review`; `banner/ready` (the agent's handoff) with
`banner/ready/note` (Show note / Hide note); `shell/main/empty/show_all`
("Show all changes" on an empty interdiff). The panel's comparison toggle:
`file_list/comparison` with `/since` and `/all`, while the review offers it. The comments: `comments_list`, `comments_list/copy`
("Copy as prompt"), `comments_list/resolved` (show / hide the resolved threads) and
`comments_list/<thread id>` (a row). Thread cards: `thread/<id>` with
`/resolve`, `/reopen` (as the state offers them), `/toggle` (a resolved thread's
chevron) and `/reply` (the reply field); the composer: `composer` with
`/input`, `/cancel` and `/post`. The sheet: `sheet`, `sheet/summary`,
`sheet/<write|preview>`, `sheet/verdict/<approve|request_changes>`,
`sheet/cancel`, `sheet/close`, `sheet/submit` and `sheet/error`. The stream: `stream/file/<path>/header` (68 high with its
side labels, 44 without: collapsed, or a notice instead of lines) and its parts
`stream/file/<path>/<chevron|split|unified|viewed|prev|next|menu>`; the pinned
copy `stream/pinned` with the same parts (`stream/pinned/viewed`, …);
`stream/menu` and `stream/menu/<view_file|comment|copy_path|reveal>` while the file menu is open;
`stream/gap/<path>/<n>` (the file's n-th gap row from the top, 28 high) with
`/expand` (shown when it hides more than 20 lines) and `/expand_all`;
`stream/line/<path>/<old|new>/<n>` (a line number in view: a click puts the
cursor on the line, a shift-click selects to it); `stream/comment/<path>/<old|new>/<n>`
(the line's comment `+`, there whenever the line is in view and takes a comment, drawn
only under the pointer: a click opens the composer on the line, a `drag` from it to
another line's opens it on the lines between); `stream/notice/<path>` (with
`/load_anyway` for a file too large, and a failed load's banner parts `/retry`,
`/details` and `/copy`); `stream/footer/<path>` and its `/next`; `code/<path>/<old|new>/<line>/<column>`, each character of a line's code in view (`column` 1-based in characters of the source line, a tab one character; recorded through `gpui_automation::record_target`, so `query{"prefix": "code/"}` lists only the characters drawn): a press puts the caret before it, a `drag` from one to another selects between them, `count` 2 or 3 a word or the line. The find bar (`Ctrl F`): `find`, `find/input` (its field), `find/count`, `find/skipped`, `find/prev`, `find/next` and `find/close`. The file menu's `stream/menu/view_file`; a "Not in the diff" card found in a file the review doesn't change, `thread/<id>/view_file`. The file view: `file_view` (the pane), `file_view/header` (pinned `file_view/pinned`), `file_view/close`, `file_view/line/<n>` (a line number: a click puts the cursor there), `file_view/comment/<n>` (its `+`), `file_view/code/<n>/<column>` (a character, as `code/…`), `file_view/notice` (with `/load_anyway` for a file over the size caps), and the find bar's and the cards' own. The file finder: `file_finder`, `file_finder/input`, `file_finder/row/<path>` (a click opens it), `file_finder/empty`. The palette: `titlebar/command` (a click opens it), `palette`, `palette/input`, `palette/row/<key>` (a row by its `state.palette` key: a click runs or opens it), `palette/empty`. Code intelligence (PLAN §18.1): `hover` (the card) with `hover/section/<id>` (each server's answer, by spec id), `hover/definition` and `hover/references`; hovering is a `mouse_move` onto a `code/…` target and a `settle` (the 350 ms rest is part of it), `Ctrl`+click (`⌘`+click on macOS) a `click` with `"modifiers": "secondary"`; the panel's tabs `panel/tab/review` and `panel/tab/references` (while a list is open); `references` (the tab's body), `references/close`, `references/in_review`, `references/row/<path>/<line>` (the first place on that line); the status bar's `statusbar/lsp/<id>` (a server, or a missing one), its details `statusbar/lsp/details`, `statusbar/lsp/<id>/retry` on a failed one, `statusbar/lsp/<id>/check_again` and `statusbar/lsp/<id>/copy/<n>` (the n-th command of its hint, from 1) on a missing one; `notice/intel` (a place that can't be shown) with `/dismiss`.

## dv's runner extensions

`crates/dv_e2e` is dv's `e2e` bin: toolbox-rs's `gpui_automation_runner` with
dv's profile (the app `<target>/debug/dv`, every variable `DV_`, the data dir
`DV_DATA_DIR`, the default window `1440x900`, platforms `windows`, `linux` and
`macos`) and its fixture (`src/fixture.rs`). Its own options, besides the
runner's:

```
  --mock-gh <exe>         default <target>/debug/mock_gh (.exe on Windows)
  --mock-lsp <exe>        default <target>/debug/mock_lsp
```

Every launch is hermetic whatever the runner's own environment holds (PLAN
§14.1). The runner itself removes every inherited `DV_` variable (an owner's
`DV_WSL_HOME`, `DV_LSP_<ID>`, `DV_MOCK_*`, …) and sets `DV_DATA_DIR` to the
scenario's data dir, so even a launch without `--data-dir` never reaches the
owner's `%APPDATA%\dv`, which holds the first implementation's files. dv's
fixture adds:

- `DV_GH` is always `mock_gh` (PLAN D7), so no scenario can reach GitHub
  through the real `gh`, whether or not it scripts any answers;
- `DV_MOCK_GH` is the header's `gh` file as an absolute path, and is removed
  without one (then `mock_gh` has no entries and every call fails with its
  exit code 64);
- `DV_MOCK_GH_LOG` is always `<out>/<scenario>/gh_calls.jsonl`, where
  `mock_gh` appends one line per call; every launch of the scenario shares it;
- `GIT_CEILING_DIRECTORIES` is `<out>/<scenario>`: the data dir (the app's
  working directory) sits inside this checkout's `target/`, and a repository
  lookup from there must never find dv's own checkout;
- for each id in the header's `lsp`, `DV_LSP_<ID>` (upper case, `-` as `_`) is
  `mock_lsp` and `DV_LSP_<ID>_ARGS` is `["--scenario", <file>, "--log",
  <out>/<scenario>/lsp_messages.jsonl]`;
- `DV_WSL_HOME` reaches the app only from a header's `env`, so dv reaches WSL
  through its stand-in (`dv_ui::services::WslAccess`) unless a scenario names a
  scratch home on purpose.

A header's `env` can't set `DV_DATA_DIR`, `DV_GH`, `DV_MOCK_GH`,
`DV_MOCK_GH_LOG`, `GIT_CEILING_DIRECTORIES`, `E2E_OUT_DIR` or `E2E_DATA_DIR`.
On Windows every child sits in the runner's kill-on-close job object, which the
`mock_gh` processes and dv's `cli` children inherit. An app killed after a
failure can't kill its `cli_spawn` children itself, so a blocked `dv review
wait` lives on until the run ends or its own `--timeout` runs out.

Besides the runner's outputs, `<out>/<scenario>/` holds `gh_calls.jsonl`
(`mock_gh`'s call log, once anything has called `gh`), `gh_calls.jsonl.lock`
(empty; `mock_gh` locks it while it counts and logs a call) and
`lsp_messages.jsonl` (every `mock_lsp`'s log).

### Header keys

```json
{"scenario": {"gh": "../fixtures/gh/pr_482.json", "lsp": {"typescript": "fixtures/lsp/review_intel.json"}}}
```

- **`gh`** names a `mock_gh` scenario file, relative to the scenario file
  ("`mock_gh` scenario files", below). It must exist, and so must `mock_gh`.
- **`lsp`** maps a language server's id (lower case) to a `mock_lsp` scenario
  file (PLAN §18.7, `crates/mock_lsp/src/scenario.rs`), relative to the
  scenario file, so dv runs `mock_lsp` in place of that server (several ids,
  several servers, all logging to the one file: `review_intel_stacked`'s
  `typescript` and `tailwindcss`). Under `--automation` dv finds servers only
  through `DV_LSP_<ID>` (PLAN §18.5, "Automated runs"), so a scenario without
  `lsp` (or an `env` naming a server, as an exploratory run against a real
  distro does) shows TypeScript as missing ("DV_LSP_TYPESCRIPT isn't set, …")
  wherever a TypeScript file is, and starts no server.

A header's `platforms` keeps a scenario to the systems its chrome belongs to;
keep a sibling scenario for the other platforms rather than dropping their
coverage (`setup`, `setup_linux`, `setup_macos`). A line's `platforms` is for a
value that differs by platform (the title bar's layout around macOS's traffic
lights): one line per platform, each tight, rather than one loose range.

### Actions

```json
{"gh_log": {}, "expect_result": [{"pointer": "", "len": 1}, {"pointer": "/0/argv/0", "equals": "api"}, {"pointer": "/0/stdin", "contains": "REQUEST_CHANGES"}]}
{"lsp_log": {"wait_ms": 3000}, "expect_result": {"pointer": "", "contains": {"received": {"method": "$/cancelRequest"}}}}
```

- **`gh_log`** reads `mock_gh`'s call log (`DV_MOCK_GH_LOG`) and makes its
  calls, in the order they were made, the result for `expect_result` and
  `save`: an array of `{"argv": [...], "stdin": "..." | null, "matched": <entry
  index> | null}`, `[]` when nothing has called `gh` yet. It works with or
  without a `gh` header (without one, every call is unmatched), and after a
  `quit` too, since the log outlives the app. Its options object must be
  empty. `mock_gh` logs a call before it exits, so once the app has a call's
  result (after the command that made it, or a `settle`) a `gh_log` line sees
  it.
- **`lsp_log`** reads what every `mock_lsp` the scenario started logged
  (`${dv_mock_lsp_log}`): an array of its lines in order, `{"pid", "started":
  [args]}` per start, `{"pid", "received": <message>}` per message it read
  (`"crashed": <code>` on the one it crashed on), `[]` before any. It may
  follow a `quit`. Its one option, `wait_ms`, reads the log again until the
  line's `expect_result` holds, for that long at most (then the line fails on
  the last read): for a message the app sends off its UI thread, which no
  `state` shows, such as a request that has reached the server.

dv takes `open <path>` and `open <path> --review <id>` in a `restart`'s
`app_args`, as `dv open` (PLAN §10.1); a launch without `open` opens nothing.
`ready` comes with the first frame, before anything dv loads in the
background: a restart's own `expect` sees only what startup restores
synchronously, so check the rest on a `settle` line after it.

### Variables

Besides the runner's `${data_dir}`, `${out_dir}` and `${workspace}`:
`${dv_gh}` (the `mock_gh` the app runs as `gh`), `${dv_mock_gh_log}` (its call
log), `${dv_mock_lsp}` (the `mock_lsp` a header's `lsp` runs),
`${dv_mock_lsp_log}` (its log) and, with a `gh` header, `${dv_mock_gh}` (the
absolute path of its file), e.g. `"path": "${workspace}/tests/fixtures/repos/crlf"`.

### `mock_gh` scenario files

A header's `gh` file tells `mock_gh` (`crates/mock_gh`, PLAN §14.2, the
authority on the format) how to answer:

```json
{"calls": [
  {"argv": ["pr", "view", "482", "--json", "*"], "stdout": {"number": 482}, "times": 1},
  {"argv": ["api", "**"], "stdin_contains": "\"event\":\"REQUEST_CHANGES\"", "stdout": {"id": 7}},
  {"argv": ["api", "**"], "stderr": "gh: HTTP 422\n", "exit": 1}
]}
```

`argv` patterns are exact strings, `"*"` for any one argument and `"**"` for
all the rest (possibly none; it must be last). The first entry that matches
(argv, and `stdin_contains` when given) and has calls left (`times`,
unlimited by default) answers with its `stdout` (a string as-is, or any other
JSON value as compact JSON plus a newline, as `gh --json` prints), `stderr`
and `exit` (0 by default). A call no entry answers writes `mock_gh: no
scenario entry for argv [...] in <scenario>` to stderr and exits 64. Unknown
fields are errors. Keep these files in `tests/fixtures/gh/` (`"gh":
"../fixtures/gh/pr_482.json"` from a scenario here), where other tests can
share them.

How `mock_gh` behaves around that:

- **stdin** is read only when an argument asks gh to read it, as the real gh
  does: an argument `-` (`--input -`), or one ending in `=-` or `@-`
  (`--body-file=-`, `-F body=@-`). Otherwise it is left alone (logged as
  `null`), so a call whose stdin is an inherited pipe never drains or waits
  on it, and `stdin_contains` can't match.
- **`times`** counts the calls each entry answered in the call log, across
  every `mock_gh` process and app restart that shares it. A call takes an
  exclusive lock on `<log>.lock` while it counts and appends, so concurrent
  calls never hand out an entry's last answer twice. Without
  `DV_MOCK_GH_LOG` nothing is counted.
- **The log line** (`{"argv", "stdin", "matched"}`, one `write` on an
  append-mode handle) is written before anything is printed, so whoever has
  the call's output can already read it.
- **Exit codes** of its own: 64 for an unmatched call, 65 when the scenario
  file can't be read or is invalid (the call is still logged), 74 when the
  log can't be written (nothing is answered).

## Exploratory runs

Looking at the app by hand, or trying a change that no scenario covers yet,
keeps to the same rules as a scenario: a data dir under `target/`, `mock_gh`
in place of `gh`, windows in the background, and a run that ends with `quit`.
A plain `dv` launch without `--automation` is the owner's own run, never an
exploratory one: it uses the real data dir (`%APPDATA%\dv`) and the real
`gh`.

**A scratch scenario** is the simplest way. Keep it out of `tests/e2e`,
where every `*.jsonl` is a test, for example in `target/tmp/explore.jsonl`:

```json
{"scenario": {"window_size": "1440x900"}}
{"cmd": "wait_ready", "timeout_ms": 20000}
{"cmd": "settle"}
{"cmd": "screenshot", "name": "shell"}
{"cmd": "state"}
```

```bash
cargo build -p dv -p mock_gh -p mock_lsp
cargo run -p dv_e2e --bin e2e -- --out target/tmp/explore-e2e --keep-data target/tmp/explore.jsonl
```

Everything in "dv's runner extensions" applies: the hermetic environment,
background windows, the closing `quit`, and killing whatever is left.

**A real WSL repo** needs the sandbox seam: after `script/build-linux`, the
scratch scenario's header sets `"env": {"DV_WSL_HOME": "/tmp/dv-explore-home"}`
(a fresh folder under the distro's `/tmp`, never the distro user's home), so
dv reaches the machine's WSL instead of its stand-in, installs this build there
and starts it from there; the scenario opens a scratch repo made under the
distro's `/tmp` (`open_review` on `\\wsl.localhost\Ubuntu\tmp\…`), and both
folders are removed inside the distro afterwards (CLAUDE.md, "WSL"). An edit
made inside the distro meanwhile (`wsl.exe -d Ubuntu --exec …`) shows through
the remote watch. The replies,
`state` included, are in `target/tmp/explore-e2e/explore/transcript.log`
(`-v` prints them as well), next to the screenshots and, with `--keep-data`,
the data dir. Giving the run its own `--out` keeps it clear of a full run's
`target/e2e`.

**Attached over a named pipe**, to send commands one at a time to a running
app. This needs PowerShell 7.4 or later for `Start-Process -Environment`.
Run it as one script, because the process id lives in its variables:

```powershell
cargo build -p dv -p mock_gh -p mock_lsp
$dir = (New-Item -ItemType Directory -Force 'target/tmp/explore').FullName
$app = $null
try {
    # dv on a scratch data dir, with mock_gh as gh (no scenario file: every gh call fails, exit 64).
    $app = Start-Process target/debug/dv.exe -NoNewWindow -PassThru -RedirectStandardError "$dir/app.log" `
        -ArgumentList "--automation-pipe dv-dev --automation-background --data-dir $dir/data" `
        -Environment @{ DV_DATA_DIR = "$dir/data"; DV_GH = (Resolve-Path target/debug/mock_gh.exe).Path
                        DV_MOCK_GH_LOG = "$dir/gh_calls.jsonl" }
    $pipe = [System.IO.Pipes.NamedPipeClientStream]::new('.', 'dv-dev', 'InOut'); $pipe.Connect(60000)
    $reader = [System.IO.StreamReader]::new($pipe); $writer = [System.IO.StreamWriter]::new($pipe)
    $writer.AutoFlush = $true
    $reader.ReadLine()                                           # {"event":"ready",...}
    $writer.WriteLine('{"id": 1, "cmd": "state"}'); $reader.ReadLine()
    # ...more commands (toolbox-rs docs/automation.md, "Protocol")...
    $writer.WriteLine('{"id": 2, "cmd": "quit"}'); $reader.ReadLine()
    $pipe.Dispose(); $app.WaitForExit(15000) | Out-Null
} finally {
    if ($app) { Stop-Process -Id $app.Id -ErrorAction Ignore }
}
```

Paths in commands resolve against the app's working directory, which is the
repo root here. The app refuses to start if another process already serves
the pipe name, so give concurrent sessions different names.
