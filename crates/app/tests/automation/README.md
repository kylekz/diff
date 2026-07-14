# --automation regression scripts

Hermetic `dv --automation` command scripts (docs/phase-7-performance.md
deliverable 0) that exercise real keyboard-activation paths for the three
shell-level overlays (PR picker, theme picker, settings panel) — the exact
class of focus/key-context regression that let the PR-picker Enter binding
silently stop dispatching (docs/phase-7-performance.md § "Known
regression").

These are **not** `cargo test`s: `--automation` drives a real GPUI window,
which needs an actual desktop/compositor, so they're run by hand (or by an
agent) against the built binary, the same way CLAUDE.md's own automation
examples work. There's nothing in these files that requires network access
or a GitHub PR — every assertion here is local/offline.

**Side effects**: `dv` persists settings/recent-repo state globally at
`<data_dir>/dv/` (`settings.json`, `recent.json`, `review_index.json`), not
per-repo. `theme-picker-enter-applies-theme.jsonl` deliberately changes the
applied theme (ending on `"Dracula"` — see that script's own section for
why it applies twice) and that change is saved for real — if you run it
against a real dev machine rather than a throwaway profile, restore
`settings.json`'s `"theme"` afterward if you care which one was active.

## Running

From the repo root, after `cargo build -p dv`:

```bash
# bash — PowerShell > redirects write UTF-16, avoid it here
./target/debug/dv.exe <repo-path> --automation < crates/app/tests/automation/<script>.jsonl > out.jsonl
```

`<repo-path>` just needs to be *some* local git repo — these scripts don't
touch its content, only the shell-level overlays and the sidebar's default
workspace focus. The `dv` repo itself works fine (self-hosting):

```bash
./target/debug/dv.exe . --automation < crates/app/tests/automation/theme-picker-enter-applies-theme.jsonl > out.jsonl
```

Then inspect `out.jsonl` — each line is `{"ok":true,"data":{...},"id":N}`
in request order — against the assertions listed below each script. A
`state` command's payload is at `data.shell` (and `data.shell.workspace`
for the active review); the `focus` field is the Phase-7 D0 addition
(`AppShell::focus_label`): `"workspace"` | `"shell"` | `"none"`.

## Scripts

### `theme-picker-enter-applies-theme.jsonl`
Baseline for the "shell captures focus" pattern (`on_open_theme_picker`
targets `AppShell`'s own handle, matching its `"AppShell && ThemePickerOpen"`
key context) — asserts Enter both applies the highlighted theme *and*
routes through the real keystroke-dispatch path.

The registry order is `["Aura Dark", "Dracula", "Claude Dark", "Claude
Light"]` (`themes::names()`) and `ThemePickerNext`/`Prev` (`up`/`down`)
**do not wrap** (`shell.rs`'s `on_theme_picker_next` clamps at `len - 1`,
`on_theme_picker_prev` saturates at `0`). A naive single `down` + `enter`
is therefore non-deterministic across repeated runs: since this script
itself persists whatever it picks (see "Side effects" above), running it
several times in a row walks the selection toward the last entry, and once
the persisted theme is already `"Claude Light"` a single `down` is a
no-op and Enter re-applies the *same* theme — a false failure on correct
code (P2, caught in review).

That determinism alone isn't enough for the *assertion* to be trustworthy,
though (P3, caught in a later review pass): a single apply-then-check-an-
absolute-value assertion (e.g. "after `up`x3 + `down` + `enter`, `theme ==
"Dracula""") can false-green a real "Enter closes the picker but never
calls `choose_theme`'s apply/persist step" regression once `"Dracula"`
happens to already be the persisted theme from a prior run of this very
script (see "Side effects" above) — the picker still lands on index 1
deterministically, Enter still closes it, and `theme` is still
`"Dracula"`, just because it never changed. The script instead applies
*twice*, to two different, individually-deterministic targets, and
asserts the two applied results differ from each other rather than
checking either one against an absolute value:

- Cycle 1 (ids 5-9): `up` x3 saturates the selection at index 0
  (`"Aura Dark"`) regardless of where it started; `enter` applies it.
- Cycle 2 (ids 10-14): reopening re-seeds the picker's selection from
  whatever is now persisted (`on_open_theme_picker`'s `themes::names()
  .position(...)` — shell.rs:1482-1484); if cycle 1's apply landed for
  real, that's index 0 again, so a single `down` lands deterministically
  on index 1 (`"Dracula"`); `enter` applies it.

Under working code this always produces two distinct absolute theme names
(`"Aura Dark"` then `"Dracula"`) no matter what theme the script started
from. Under the close-without-apply regression, `theme` never actually
moves off whatever it started at, so both cycles report the *same* value
— the two-value comparison catches that where a single absolute-value
assertion would not.

- id 4 (after `ctrl-shift-t`): `shell.theme_picker_open == true`,
  `shell.focus == "shell"`.
- id 9 (after `up` x3, `enter`): `shell.theme_picker_open == false`.
  Record `shell.theme` as `theme_A`.
- id 11 (after the second `ctrl-shift-t`): `shell.theme_picker_open ==
  true`, `shell.focus == "shell"` (confirms the second open dispatches
  too, not just the first).
- id 14 (after `down`, `enter`): `shell.theme_picker_open == false`, and
  `shell.theme` (`theme_B`) `!= theme_A` — the change-detecting assertion:
  Enter actually applied a *different* highlighted theme each cycle, not a
  no-op that happens to match whatever was already persisted.

### `settings-escape-closes.jsonl`
Same shell-focus-capture pattern, for the settings panel. Doc deviation
(see `phase7-plan.json` § `doc_deviations` #1): the settings panel has no
keyboard-activatable *toggle* (mouse-first UI) — its only binding is
Escape, so this asserts open/close instead of a value change. Still
exercises the identical focus/key-context class the PR-picker regression
lives in.

- id 3 (after `ctrl-,`): `shell.settings_open == true`,
  `shell.focus == "shell"`.
- id 5 (after `escape`): `shell.settings_open == false`.

### `pr-picker-focus-guard.jsonl`
**The direct regression guard**, covering the original Enter regression, a
keyboard-trap regression the first fix attempt could have reintroduced
(review finding P1), and a false-decline regression the P1 fix itself
introduced (review finding P3) — all fixed across the same slice.

This script does not rely on a bare `ctrl-g` to catch the regression, and
deliberately opens the picker by *clicking* the "PRs · ctrl-g" hint button
instead (ids 9/11 below) — a bare `ctrl-g` right after `wait_ready` would
pass whether or not the original bug is present, since `on_open_review`
already parks focus on the workspace's own handle for unrelated reasons,
so that keystroke's own dispatch precondition (`"Workspace && ..."`)
already guarantees `focus == "workspace"` regardless of whether
`on_open_pr_picker` itself moves focus. That would be a false-negative
regression test — see the finding P2 discussion below for the full
argument.

The original bug (docs/phase-7-performance.md § "Known regression") was
that `on_open_pr_picker` never moved focus itself, so any call site that
opened the picker *without* focus already resting on the workspace left it
stuck wherever it was. The trigger the doc identifies is the mouse "PRs ·
ctrl-g" hint button (`Workspace::render`, the button labeled `"PRs ·
ctrl-g"`), which calls `on_open_pr_picker` as a plain method, bypassing
the `browse` key-context gate that a `ctrl-g` keystroke would require.

**Ids 3-8 — the keyboard-trap guard (finding P1).** Open the theme picker
first (a real, common prior interaction that parks focus on `AppShell`'s
handle), then click the PR-picker hint button directly while it's open.
Fixing the focus capture alone, without more, would have opened the PR
picker on top of the still-open theme picker — and since closing the PR
picker afterward re-focuses the workspace, that would strand the theme
picker keyboard-trapped behind it (this workspace's own `escape` bindings
outrank the shell's while focus rests here — no way out without a mouse
click). `on_open_pr_picker` therefore declines outright while a shell
overlay is genuinely open, so id 5's click here is a no-op and the theme
picker stays open, escapable, and untouched — ids 7-8 confirm `escape`
still reaches it normally afterward.

**Ids 9-12 — the actual Enter-regression discriminator (findings P2 +
P3).** The first fix attempt gated the id-5 decline on
`shell_focus_handle.is_focused()` as a stand-in for "an overlay is open".
That proxy is wrong: ordinary sidebar chrome — the "REVIEWS" label,
`render_sidebar_header`'s group-header row, the padding around the
grouping/filter controls — carries no `on_click`/`track_focus` of its
own, so a click anywhere on it bubbles all the way to `AppShell`'s root
`track_focus` and leaves the shell handle focused with **no overlay
open at all**. Id 9 clicks exactly that ("REVIEWS" label, `32,96`) to
reach that precondition; id 10 confirms it (`focus == "shell"`,
`theme_picker_open == false`, `pr_picker_open == false` — genuinely no
overlay). Id 11 then clicks the PR-picker hint button from that state:
the fixed guard (`AppShell::overlay_open`, queried directly rather than
inferred from focus) does *not* decline here, since no overlay is open —
so the picker opens, and id 12 is the assertion that actually
discriminates the D0 `window.focus` fix: `pr_picker_open == true` AND
`focus == "workspace"`. This is the scenario a bare `ctrl-g` right after
the theme-picker/escape sequence (ids 3-8) could **not** catch, because
`close_theme_picker` (shell.rs) already restores focus to the workspace
on `escape`, making a follow-up `ctrl-g`'s own dispatch precondition
(`"Workspace && ..."`) guarantee `focus == "workspace"` regardless of
whether `on_open_pr_picker` itself moves focus — which is exactly why ids
9-11 click the hint button instead of pressing `ctrl-g`. Ids 13-14 then
exercise the real keystroke-dispatch path for `PrPickerChoose`
(`enter`, no-op here — no loaded PR list without network, this script is
intentionally offline), and ids 15-16 clean up with `escape`.

Verified live against pre-P1-fix, pre-P3-fix, and the final build (see
the slice's report):
- Pre-D0-fix (focus never captured by `on_open_pr_picker`): id 12 shows
  `pr_picker_open == true` but `focus == "shell"` (the click opened the
  picker — nothing declined it — but never moved focus onto the
  workspace, so `enter` at id 13 would be swallowed by whatever binding
  applies at the shell context instead of reaching `PrPickerChoose`).
  Confirmed by disabling the `window.focus(&self.focus_handle, cx)` call
  in `on_open_pr_picker` and rerunning this exact script.
- Pre-P1-fix (focus captured, but no overlay decline guard at all): id 6
  shows `pr_picker_open == true` AND `theme_picker_open == true` (both
  open at once) AND `focus == "workspace"`; id 7's `escape` then closes
  the PR picker but leaves `theme_picker_open == true` with focus back on
  the workspace — the theme picker is now stuck; a *second* `escape` hits
  this workspace's own `ClearSelection` instead of `ThemePickerClose` and
  the theme picker never closes for the rest of the script.
- Pre-P3-fix (decline guard present, but keyed on
  `shell_focus_handle.is_focused()` instead of genuine overlay state): id
  12 would show `pr_picker_open == false` — the click at id 11 is
  wrongly declined even though `theme_picker_open == false` at id 10 (no
  overlay is actually open), a silent dead first click on the hint
  button.
- Final (fixed) build: id 6 shows `pr_picker_open == false` (declined —
  a real overlay is open) and `theme_picker_open == true` unchanged; id 8
  shows the theme picker closed by a single `escape`, `focus ==
  "workspace"`; id 10 confirms the no-overlay precondition; id 12 shows
  `pr_picker_open == true` AND `focus == "workspace"` (opens, and focus
  lands correctly, discriminating both the D0 and P3 fixes at once).

The click coordinates (logical px, at the default `1440x920` automation
window size — if the window has been resized, re-locate with a screenshot
first per CLAUDE.md § Visual verification):
- `1201, 51` — the "PRs · ctrl-g" header button.
- `32, 96` — the "REVIEWS" sidebar-header label (`shell.rs`'s
  unconditional label above the grouping/filter row); any point on that
  row's padding works equally, since none of it is focusable.

- id 2 (baseline): `shell.focus == "workspace"`.
- id 4 (after `ctrl-shift-t`): `shell.theme_picker_open == true`,
  `shell.focus == "shell"`.
- id 6 (after clicking the PR-picker hint button while the theme picker is
  open): **the keyboard-trap guard** — `shell.workspace.pr_picker_open ==
  false` (declined to stack) AND `shell.theme_picker_open == true`
  (untouched) AND `shell.focus == "shell"` (untouched).
- id 8 (after `escape`): `shell.theme_picker_open == false`,
  `shell.focus == "workspace"` (the theme picker is still reachable and
  closes normally — proves no trap).
- id 10 (after clicking inert sidebar chrome): **the no-overlay
  precondition** — `shell.focus == "shell"` AND
  `shell.theme_picker_open == false` AND
  `shell.workspace.pr_picker_open == false`.
- id 12 (after clicking the hint button from that precondition): **the
  actual Enter-regression discriminator** —
  `shell.workspace.pr_picker_open == true` AND `shell.focus ==
  "workspace"`. Fails (`focus == "shell"`) on pre-D0-fix code; fails
  (`pr_picker_open == false`, wrongly declined) on pre-P3-fix code.
- id 14 (after `enter`): `shell.workspace.pr_picker_open` stays `true`
  (no-op — there's no loaded PR list to choose from without network, this
  script is intentionally offline; see docs/phase-7-performance.md
  deliverable 0's live/PR-open acceptance pass for the networked
  follow-up), `shell.focus` stays `"workspace"` (proves `enter` reached
  `PrPickerChoose` via the real keystroke-dispatch path rather than being
  swallowed or misrouted).
- id 16 (after `escape`, cleanup): `shell.workspace.pr_picker_open ==
  false`.

No PR-opens-on-Enter assertion here by design (needs a real repo with a
real `gh`-authenticated PR list) — see the plan's verification item (d)
for that live, non-CI counterpart.
