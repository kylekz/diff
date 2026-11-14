# dv: design spec

The chosen direction is **Glass**: dv's Aura colours on the operating system's window material.
Pixel reference: row 2 of the design canvas, (a private canvas) —
artboards `GlassReview`, `GlassSubmit`, `GlassOnboarding` (each has `material` and `mode` tweaks).
Behaviour lives in [PLAN.md](PLAN.md); this file is tokens, layout and interaction. Where a board and
this file disagree, this file wins (§6 lists the known differences).

---

## 1. Tokens

Colours are the board's `tokens()` values. Every surface token has three values — **Acrylic / Mica /
Opaque** — because translucent surfaces need more tint over Mica (which samples only the wallpaper)
and none over Opaque. Solid tokens are the same in all three.

### 1.1 Dark (default; derived from Aura Dark)

| Token | Acrylic | Mica | Opaque | Use |
|---|---|---|---|---|
| `chrome` | `rgba(26,24,36,.56)` | `rgba(28,26,37,.82)` | `#110f18` | title bar, status bar, PR strip |
| `panel` | `rgba(20,19,29,.44)` | `rgba(24,22,32,.76)` | `#13121a` | file list / comments column |
| `code` | `rgba(21,20,27,.94)` | `rgba(21,20,27,.975)` | `#15141b` | diff surface (near-opaque, legibility) |
| `codeHead` | `rgba(28,26,37,.95)` | `rgba(27,26,35,.98)` | `#1b1a23` | file header row in the diff |
| `layer` | `rgba(255,255,255,.045)` | `rgba(255,255,255,.04)` | `#1a1922` | cards on material |
| `layerStrong` | `rgba(32,30,44,.82)` | `rgba(32,30,43,.90)` | `#1e1c28` | selected/active cards |
| `thread` | `rgba(31,29,41,.98)` | `rgba(31,29,41,.99)` | `#1d1b26` | inline thread card |
| `overlay` | `rgba(31,29,42,.965)` | `rgba(31,29,42,.975)` | `#1d1b27` | sheets, palette, menus |
| `ctrl` | `rgba(255,255,255,.06)` | `rgba(255,255,255,.055)` | `#1f1d29` | button fill |
| `inputBg` | `rgba(0,0,0,.24)` | `rgba(0,0,0,.22)` | `#0f0e15` | text inputs |

Solid: `fg #edecee`, `fg2 #c6c4d8`, `fg3 #9d9ab3` (lowest text tier; ≥4.5:1 on every surface's
Opaque value; on Acrylic and Mica it also depends on the desktop behind the window, §6),
`accent #a277ff` (fill) with `accentFg #15141b`, `accentText #c4a9ff` (accent as text),
`green #61ffca`, `red #ff6767`, `yellow #ffca85`, `blue #82e2ff`; soft fills at 12–18% of each
(`accentSoft`, `greenSoft`, `redSoft`, `yellowSoft`, `blueSoft`); `line rgba(255,255,255,.08)`,
`line2 rgba(255,255,255,.14)`, `ctrlLine rgba(255,255,255,.11)`, `overlayLine rgba(255,255,255,.13)`,
`scrim rgba(6,5,10,.56)`, `sel rgba(162,119,255,.22)` (rows: the cursor's line, a line
selection); `textSel rgba(162,119,255,.42)` (text selected in code); `findMatch
rgba(255,202,133,.22)` and `findCurrent rgba(255,202,133,.32)` (find matches in code, the
current one outlined in `yellow`, the others in `findLine rgba(255,202,133,.62)`, so a match on
an intraline word's fill still reads as a match); `popover` = `overlay`'s Opaque value on every
material (popovers with no scrim under them, §3). A segmented control's track is `segTrack`
(`rgba(255,255,255,.055)` dark, `rgba(28,24,56,.06)` light), its chosen segment on `segOn`
(`rgba(255,255,255,.12)` dark, white light): in light the track is a tint, so a white chosen
segment stands out from it.

Diff: `addBg rgba(97,255,202,.085)`, `addGut .15`, `addWord .30`; `delBg rgba(255,103,103,.09)`,
`delGut .16`, `delWord .34`; `hunk rgba(130,226,255,.06)`. Added and removed rows differ in lightness
as well as hue.

Syntax: `kw #a277ff`, `fn #ffca85`, `str #61ffca`, `num #61ffca`, `type #a277ff`, `cst #82e2ff`,
`prop #f694ff`, `punc`/`plain #edecee`, `comment` = `fg3` (`#9d9ab3`). Highlighting produces these
token kinds, never colours (PLAN §6 step 4); each paints in its token, the code gpui-component
highlights itself (a fenced block in Markdown: a hover card's signature, a comment's code) too.

### 1.2 Light (new; "Aura Light")

Same token names. Key values: `fg #1d1b26`, `fg2 #46434f`, `fg3 #5e5a6f`; `accent #6d43e0`,
`accentFg #ffffff`, `accentText #5a32cc`; `green #0d7553`, `red #bf2537`, `yellow #875600`,
`blue #0b6687`; `chrome` Acrylic `rgba(247,246,251,.58)` / Mica `rgba(239,238,244,.85)` / Opaque
`#efeef4`; `code` `rgba(255,255,255,.93)` / `rgba(254,254,255,.975)` / `#fdfdfe`; `scrim
rgba(36,32,58,.30)`; `sel rgba(109,67,224,.13)`, `textSel rgba(109,67,224,.28)`, `findMatch
rgba(196,128,0,.18)`, `findCurrent rgba(196,128,0,.30)`, `findLine rgba(135,86,0,.58)`; diff `addBg rgba(18,170,115,.10)`, `delBg rgba(225,45,70,.075)`. Syntax: `kw
#6236d6`, `fn #8a4f00`, `str #0d7553`, `cst #0b6687`, `prop #9a2694`, `punc #3d3a48`, `plain #1d1b26`,
`comment` = `fg3` (`#5e5a6f`).
Full set: the board's `tokens()`.

### 1.3 Other themes
Dracula, Claude Dark and Claude Light remain as **opaque-only** themes (their colours from
`themes/*.json`, selections in Claude's blue rather than its orange accent, which over an added
line reads as a removed one; contrast-corrected: `fg2` at 7:1 and `fg3`, the hues, `accentText` and the syntax
colours at 4.5:1 on every surface). Glass materials apply to the Aura pair; a
non-Aura theme forces `Opaque`. Syntax colours, diff colours and surfaces are one token set per theme;
views never hard-code a colour. The setting is the theme's family: **Aura** (Aura Dark and Aura
Light, by the mode), **Claude** (Claude Dark and Claude Light, by the mode) and **Dracula** (dark
only, whatever the mode). Each family's surfaces are its JSON's: `chrome` its title bar, `panel` its
sidebar, `code` its editor background, `overlay` its popover (a lift of `code` where the popover is
the sidebar's colour), the cards lifted from `code` toward `fg` (dark) or white (light).

### 1.4 Typography
- **UI:** Geist (bundled, OFL), fallback Segoe UI Variable / system-ui. 13px/18px body; 12px
  secondary; 11px keycaps and badges; 600 weight for titles and the primary button, 500 for controls,
  400 for text. Only 400/500/600 are loaded; the boards' 700 wordmark is drawn at 600.
- **Code:** JetBrains Mono (bundled), 12px on 20px rows; line numbers `fg3`, tabular.
- Headings: sheet title 15px/600; onboarding title 20px/600.

### 1.5 Shape and space
Spacing scale 4 / 6 / 8 / 12 / 16 / 24. Radii: 4 (keycaps, inline code), 5–6 (buttons, chips,
inputs), 7 (segmented control), 10 (thread cards), 12 (sheets). Heights: controls 28, small controls
24/22, title bar 40, status bar 28, file rows 28, code rows 20. Shadows: cards `0 6px 20px
rgba(0,0,0,.28)`; overlays `0 28px 80px rgba(0,0,0,.55), 0 2px 10px rgba(0,0,0,.35)` plus a 1px
inner top highlight `rgba(255,255,255,.06)`. Light themes: cards `0 4px 14px rgba(30,22,70,.08)`,
overlays `0 20px 56px rgba(30,22,70,.14), 0 2px 10px rgba(30,22,70,.10)` (softer, so a popover
over code with no scrim under it, the find bar or the hover card, doesn't sit in a grey haze).

### 1.6 Icons
Inline stroke icons at 14–16px, 1.5–1.6 stroke, `currentColor`, from one set (Lucide-compatible
geometry, bundled as SVG paths). No emoji anywhere.

**Model marks.** An agent that names its model (PLAN §2.5) shows with its model family's mark:
Claude, OpenAI (GPT), Gemini, Kimi, DeepSeek, Z.ai (GLM) and Qwen, each one colour like every
icon (the makers' logos as filled shapes, from lobe-icons; `assets/icons/README.md`). The family
is read from the model's name (`dv_ui::agents`: "Claude Fable 5.2", "gpt-6.1-sol", "Opus 5.5",
"o4-mini"), else, for an agent that names none, from its own (`claude-code`, `codex`,
`gemini-cli`); a name that tells no family keeps the bot glyph. A mark is drawn in its family's
colour, the same in every palette, a lighter step in dark where the brand's own is too deep
for dark surfaces: Claude `#d97757` in both; Gemini `#3478f6` light, `#8ab4f8` dark; DeepSeek
`#4d6bfe`, `#7c93ff`; Qwen `#615ced`, `#9a96f8`; OpenAI, Kimi and Z.ai, whose marks are black
or white, in `fg`. As an avatar it sits on a soft fill of that colour at 14%.

### 1.7 App icon
dv's icon (the executables, the window and the taskbar, the Start menu's shortcut, the installer)
is a split diff on an accent tile: a 224/256 square with radius 52 filled with the accent
(`#b694ff` at the top to `#8c5cf5` at the bottom), on it the title bar's mark — two panes — as
two code surfaces (`#15141b`, radius 18), each with three lines: `fg3` above and below, the old
side's middle line `red`, the new side's `green`. It reads at 16 px (the tile and the two coloured
lines) and on light and dark taskbars alike. The source is `crates/dv/resources/windows/dv.svg`;
`dv.ico` holds it at 16, 20, 24, 32, 40, 48, 64 (bitmaps) and 256 (PNG), and the Linux app's
`crates/dv/resources/linux/dv.png` (its AppImage's icon) at 256, both written and checked by
`crates/dv/tests/app_icon.rs`.

---

## 2. Layout

```
┌ title bar (chrome, 40) ─────────────────────────────────────────────────────────────────────┐
│ dv  [repo · review ▾  Ctrl O] [source ▾]   [ Go to file, review or command   Ctrl K ]  PRs Ctrl G  ⋯  – □ × │
├ PR strip (chrome, PR reviews only) ─────────────────────────────────────────────────────────┤
├ panel 280 ─────────────┬ diff stream (code) ────────────────────────────────────────────────┤
│ Changes  4/7 viewed    │ ▸ file header (codeHead, pinned): status · path · ±  Split|Unified ✓ │
│ [ Filter files       ] │   hunk header                                                       │
│ ☑ M src/routes/…  2 ◯  │   old │ new  rows …                                                 │
│ …                      │   ┌ thread card ┐                                                   │
│ Open comments  3       │   …                                                                 │
│  ◯ users.ts:31 …       │   next-file footer: "Next: …   Mark viewed and go next  V J"        │
├ status bar (chrome, 28) ──────────────────────────────────────────────────────────────────┤
│ Ubuntu · watching for changes   Draft review · 3 open · 1 resolved     Ctrl Enter finish review │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
```

- Two columns only. Repos and reviews live behind the title-bar switcher (`Ctrl O`) and the palette;
  they never take a permanent column. The panel is resizable 220–420 (its width is remembered, in
  `settings.json`) and collapsible (`Ctrl B`).
- Split view gets ≈575px per side at 1440px; long lines clip with a fade and scroll horizontally
  together (`Shift`+wheel). Unified is offered and remembered per review.
- The diff is one stream over all files (PLAN §6.7); the panel's file rows scroll the stream.
- Below 1100px window width the panel auto-collapses into an overlay.
- **Linux** (PLAN §13.1): dv draws the window's decorations where the platform lets it (Wayland,
  X11 with a compositor). The title bar is the window's: a drag on it moves the window, a
  double-click maximizes or restores it, a right-click opens the window manager's menu, and at
  its right sit the caption buttons the window manager supports (minimize, maximize/restore,
  close always), drawn as Windows' are: 46 wide, the bar's height, `control_hover` on hover,
  Close red. Around the window gpui-component's frame: a 1px border and a 20px soft shadow
  that are also its resize edges, gone on the sides it is tiled or maximized against. Where
  the platform keeps the decorations (X11 without a compositor) the window manager draws its
  own frame and buttons, and dv's bar shows none. The material is always Opaque (§3).

---

## 3. Materials

| Surface | Acrylic / Mica / Vibrancy | Opaque |
|---|---|---|
| Window background | OS material (`Blurred` / `MicaBackdrop`; macOS's `NSVisualEffectView`) | `chrome` solid |
| Title bar, status bar, PR strip | `chrome` translucent | solid |
| Panel | `panel` translucent | solid |
| Diff surface, thread cards | near-opaque (`code` ≥ 94%, `thread` ≥ 98%) | solid |
| Sheets, palette, switcher, file finder | `overlay` ≥ 96.5% over `scrim`; no blur | solid over `scrim` |
| Popovers with no scrim (hover card, menus, find bar, a server's details) | `popover`: solid, so the code under them never shows through | solid |

GPUI blurs only what is behind the window, so in-app overlays never "frost" app content (PLAN D8).
Vibrancy (macOS, PLAN D8) takes Acrylic's surface values: both blur what is behind the window,
and Vibrancy has no tuned set of its own yet, which is why Opaque stays macOS's default.
Opaque is chosen automatically on Windows 10, with system transparency off, on Linux, on macOS
with Reduce transparency on, and for non-Aura themes; it must look finished in its own right, not
like a degraded mode. Window
inactive: material falls back to its inactive appearance (OS-managed); dv dims nothing itself.

---

## 4. Screens

### 4.1 Review (`GlassReview`)
- **Title bar:** dv mark; switcher button "`api-server` · working tree vs main" (the repo's name at
  600, the target label with its "vs" in `fg3`) with the location chip (`Ubuntu`, WSL repos only) and
  its `Ctrl O` keycap; source picker (`⇄ ▾`); centred command field (`Ctrl K`; a click opens the
  palette, its outline `line2` on hover); `PRs Ctrl G`;
  overflow (`⋯`, a 28-square ghost button with the ellipsis in `fg2`: Settings, `Ctrl ,`, §4.3);
  caption buttons (Windows; Linux while dv draws the decorations, §2) / traffic lights (macOS,
  left: the native ones in a transparent titlebar, centred on the bar at 12 from its left edge,
  dv's mark 84 in, after the lights' 60 and the spacing scale's 12; nothing at the right). The
  field is centred on the
  window unless the left group needs more than the half that leaves it: then the field moves
  right as far as the group needs (12 between them), never closer than 12 to the `⋯` and the
  caption buttons. The left group never runs under the field: once the field can't move further the
  target label truncates at its end, and once it has no room left the repo's name does. With no
  review open the
  button reads "No review open"; while one opens, "`repo` · opening…", then the full label at once
  (the labels are read as the review opens, not after its files are listed).
- **Switcher** (`Ctrl O`, or a click on its button): an overlay 560 wide hung 8px under the button,
  over the scrim (which covers everything below the title bar; a click on it closes the switcher),
  radius 12. A filter field (46 high, search glyph) has the keyboard: typing filters (every word must
  appear in the repo's name, path or the row's label), `↑`/`↓` move the selection (wrapping), `Enter`
  opens, `Esc` closes. Per known repo (one per worktree root, however its distro is spelled: a
  subfolder added before it could be reached becomes its root's row once a listing reaches it,
  PLAN §11), most recently used first: a header — folder glyph, name at 600,
  the location chip, the path in `fg3` (truncated at its start; a WSL repo's path inside its
  distro, `/home/kyle/api`, the chip naming the distro) — over one line: "uncommitted changes ·
  7 files `+n −m`", "Working tree clean", a skeleton bar while it is computed, or why it can't be
  opened (a triangle in `yellow` for an unreachable repo: "Ubuntu isn't running", "Ubuntu · the
  connection to dv failed: …", "Ubuntu · reconnecting…"; in `red` for a read error). A WSL repo
  whose distro isn't running, whatever its connection did before, offers one row, **Start
  Ubuntu** (play glyph): it connects, booting the distro (PLAN §5.2: listing never boots one),
  the line reading "Starting Ubuntu…" in `fg3` meanwhile and the row staying, dimmed (`fg3`,
  no hover) and inert, with the selection — `Enter` or a click on it again does nothing, and
  the selection never drops to the footer — then lists the repo as usual, its first row taking
  the selection; a start that fails leaves the line saying why ("Ubuntu · the connection to dv
  failed: …" once the distro runs, else "Ubuntu isn't running" again), the row's Start or Retry
  keeping the selection. A repo whose connection failed, its distro running, offers one row,
  **Retry** (rotate glyph), which connects again the same way ("Ubuntu · connecting…" in `fg3`
  meanwhile, the row inert as Start's). One whose connection is reconnecting reads "Ubuntu ·
  reconnecting…" and keeps the rows its last listing had, and picking one of them opens it once
  the connection is back. A repo's rows follow its connection going down or coming back
  whether or not the switcher is open, so it never opens on a stale Start or "reconnecting…". Then its rows (30 high, indented): **Uncommitted changes**, **Staged
  changes** and, with a branch other than the default checked out, **Branch `feat` vs `main`**, each
  beside its open review (status chip, "round n", open-comment badge; "New review" on the selected row
  without one), then the repo's other open reviews by their target label, then, when it has a branch
  other than the default one, **Compare branches…** (arrows glyph). Picking a row opens or
  resumes that review (PLAN §2.4). A repo that can't be opened offers no rows. A header's menu
  (a right-click on it, or its `⋯`, a 22-square ghost button at the end of its first line shown
  under the pointer and while its menu is open; the menu panel at the pointer, as the comments
  row menu's) holds **Remove from list** (x glyph): the repo leaves the switcher and
  `known_repos.json`, nothing in it changing (its reviews show again once it is opened again), and
  the selection moves to the next repo's first row, else the previous one's, else **Open
  folder…**. For the open review's repo the item is disabled, "Its review is open" in `fg3` under
  it. `Ctrl Shift Backspace` removes the selected row's repo the same way, from the filter field
  too (which deletes text with Backspace's other forms), and does nothing on the open review's;
  the footer's hints start with it ("remove") while the selected row's repo can be removed. `Esc`
  closes an open menu before the switcher. The branch picker's header has
  no menu, and `Ctrl Shift Backspace` removes nothing there.
  A row beside an open review (a target's, another target's review, the branch picker's rows)
  has a menu of its own, opened the same two ways (its `⋯` at the row's end, after the
  badge; the row takes the selection): **Close review** (x glyph) closes that review for good
  (PLAN §2.4: `closed` is final, as `dv review close`), whichever it is — the open review
  too, which the window then shows closed (§4.4). A target's row stays, without a review
  ("New review" when selected); a row that stood for the review alone goes, the selection
  moving to the row after it, else the one before. `Ctrl Shift W` asks the same of the
  selected row, from the filter field too: the row reads "Close this review?" (at 500) in
  place of its badge and the footer's hints become `Enter` "close review" and `Esc` "keep";
  `Enter` closes it, `Esc` keeps it (the switcher staying open), and so does moving the
  selection, typing in the filter or picking a row. The key alone never closes a review,
  since nothing reopens one. While the selected row has a review the footer's hints start
  with it ("close review"). A close that fails (the connection lost under it) leaves the
  review as it was and says why in the footer in place of the hints, a triangle in `red`,
  until the selection moves.
  **Compare branches…** turns the switcher into the branch picker for that repo, in the same panel:
  the field cleared, its placeholder "Pick a branch to review", the repo's header over one row per
  local branch but the default one (branch glyph, the most recently committed first): the name, then
  "vs feat-a" in `fg3`, the base it is offered against (its stack base, PLAN §2.2, else the default
  branch), and that pair's open review as on the switcher's rows. `Enter` on one lists its bases,
  the placeholder "Compare feat-b with…": the stack base first ("stack base" in `fg3`), then the
  default branch ("default branch"), then every other local branch; `Enter` on one opens or resumes
  the `branch` review of the pair ("New review" on the selected row without one), against the
  default branch the target with no base, the switcher's own **Branch** row's. Typing filters
  either step by name; `Esc` steps back (bases → branches → the repos, the selection back on the row
  picked) and closes from the branches when the picker was opened from the source picker. The
  footer's hints read "next" for `Enter` on the branches and "back" for `Esc` where it steps back. With
  nothing to list it reads "No branches to compare" ("No branches match “…”" for a filter). The footer holds **Open
  folder…** (the system's folder picker; the folder's repo is added and selected) and the keys.
  Opened over the "Finish round" sheet (§4.2), it and the source picker hang above the sheet, which
  the scrim then covers too; closing them returns to the sheet as it was. Status
  chips: `open`/`draft` accent, `ready` yellow, `changes_requested` red, `in_progress` ("Agent
  working": the agent's first reply, address or comment after changes were requested, PLAN §2.4)
  blue, `approved`/`submitted` green, `closed` grey; always worded.
- **Source picker** (`⇄ ▾`): a 340-wide menu under its button with the open review's repo's targets
  (the switcher's three rows) and their open reviews; the current target is checked; then, when the
  repo has a branch to compare, **Compare branches…**, which opens the switcher as the branch picker
  on that repo (above). `↑`/`↓`/`Enter`/`Esc`; picking one opens or resumes its review.
- **Panel:** "Changes", the file count and the review's `+n −m`; under it, while the review offers
  it (a live target — `working`, `staged`, a branch — not finished, with a closed round; or a
  draft PR review whose PR has an earlier submitted review, reading **Since your last review**;
  PLAN §2.3, §9.5), a full-width segmented control **Since round n | All changes** with its `I` keycap (the
  header's Split | Unified style; each segment as wide as its label, sharing the room left; in
  a panel narrower than 270 the second reads **All**, so the first keeps its round; the choice
  shows at once, the list follows when it lands); a thin
  progress track with "`viewed` of `total` viewed"; filter input (`Enter` shows the first file left, `Esc` clears it; both hand the
  keyboard back to the diff); file rows (28 high, radius 6) — viewed checkbox (16, accent filled with
  a check when on; dimmed and inert on a finished review), status letter (M modified, A added, U
  untracked, D deleted, R renamed, C copied, T type changed, `!` unmerged; colour + letter, never
  colour alone), path with dimmed directory (truncated at its start; a viewed file's name dims to
  `fg3`), open-comment badge, `+n −m`. The current file's row is `sel`, its name at 600; a click on a
  row shows that file in the diff, a click on the checkbox toggles viewed. Then, under a hairline,
  "Open comments" with the number of threads still waiting (open, and addressed but not yet
  resolved) and "Copy as prompt" — one row per thread, the addressed ones first (the reviewer's
  turn, PLAN §3 workflow A step 5), then the open ones, each group in stream order (the "Not in
  the diff" section's threads first, in the order they were added, then by file; an accent
  ring dot for `open`, a blue dot and "addressed" for `addressed`, `path:line` in mono where the
  thread stands now (an old-side thread's ends in "(old)", as the CLI prints it, so its lines
  aren't taken for the file's lines now; an outdated thread with a trail, PLAN §8, where its
  code went, `path:40–45`, still flagged "outdated"; a comment on the whole file `path (file)`,
  "(file)" in `fg3` where `:line` would be, under its file's name now; while its file's text hasn't landed — briefly, since
  every file a thread shows in loads at once — the line it was written on, in `fg3`), the first
  line of the
  comment as plain text — its Markdown dropped, a fenced block's first line of code, as in every
  one-line summary of a comment; an outdated thread says "outdated" in `yellow` and shows its original first line in
  mono; a thread the interdiff doesn't show carries a quiet "all changes" tag in `fg3`, and its
  `path:line` and "outdated" are where it stands in all changes), then
  "n annotations · show" (PLAN §19), which lists the agent's annotations under it — a 6-square
  `blue` dot, `path:line` as for a thread (an outdated one flagged "outdated" in `yellow`, one an
  interdiff doesn't draw carrying the "all changes" tag), the first line — and "n resolved ·
  show", which lists the resolved ones too; with none waiting, "No open comments" above them. A
  click on an annotation's row shows the annotations again when `A` hid them, then focuses it
  as a thread's row does; one the stream never draws (outdated, or its file left the review)
  opens its whole text under the row instead, nothing scrolling. An annotation's row menu holds
  **Ask** alone (none for one the stream doesn't draw). A click on a row shows and focuses the thread in the diff — an "all changes" row
  switches the stream to all changes first; the focused thread's row is `sel`. A right click on
  a row opens its menu at the pointer, in the file menu's style (the `overlay` panel, 200 wide,
  items a control high with their glyphs; the row keeps its hover tint meanwhile): the card's
  actions for the thread's state — **Resolve** (open), **Resolve** and **Reopen** (addressed),
  **Reopen** (resolved) — then **Reply** (the thread shown and focused, its reply composer open
  in its card, as `R`) and, under a hairline, **Copy as prompt** (that thread alone, in the
  words "Copy as prompt" uses, headed for its state: "Open review comment", "Addressed review
  comment" or "Resolved review comment on …"). On a finished review the writes are there,
  dimmed and inert, as on the card. A click outside the menu or on an item, `Esc`, an overlay
  opening (the switcher, the source picker, the sheet), the panel hiding, or its row leaving
  the list closes it. "Copy as prompt" puts the `open` threads on the clipboard as Markdown (`path:line` where each stands now, as its row,
  "(where it was written; dv hasn't placed it yet)" beside one not placed yet, the thread id,
  the anchored code, the comment and its replies), for pasting to an agent; in a
  panel narrower than 270 it is its copy glyph alone, with the words in a tooltip, so the header
  never runs past the panel. The list grows to 240 high, then scrolls. The panel's right edge is a resize handle (a 2px accent line while hovered; a double click restores 280).
- **Not in the diff:** threads whose file left the review's list (PLAN §8: the agent deleted
  the file, reverted it, or renamed one git can't pair with its first name; or the reviewer
  commented on a file the review never changed, from the file view) head the stream, in
  a section before the first file: a 44-high `codeHead` bar like a file header's — chevron, a
  file-diff glyph in `fg3`, "Not in the diff" at 600, the count badge, and "comments on files
  the review doesn't change now" ("comment" for one) in `fg3` (truncating) — then one full thread card per
  thread, in the order they were added, each as an outdated card heading a file looks (the
  "Outdated" chip, its original lines in the `recess` block, Resolve / Reopen, the reply field)
  with its anchor naming the path it was written on (`src/hooks/use-can.ts · new · line 12`,
  truncating at its start). A thread waiting for its file's read (briefly, as the review opens)
  shows neutrally: no chip, the lines it was written on. A thread found in its file as the
  review's new side holds it (`Placement::Unlisted`, or a comment on its whole) wears an
  "Unchanged file" chip (a file glyph, `fg2` on `layer`) in
  place of "Outdated", its anchor the lines it stands at now, and **View file** (a small button
  before Resolve) opens the file view at those lines. Resolved ones collapse as anywhere. The section shows whichever
  comparison is shown (in an interdiff too, its threads never having a file there), collapses
  to its bar with its chevron, its bar pins at the top while its cards scroll as a file's
  header does, and is absent while no thread needs it. It isn't a file: while it is at the top
  no file is current, and `J`/`K`, the headers' arrows and the footer pass over it, while
  `]`/`[`, `R`, `X` and the comments list reach its cards like any other. With no file listed
  (the agent reverted everything, or an empty interdiff) it keeps the stream rather than the
  main area's empty state (§4.4), and ends with that state as a quiet 48-high row under its
  cards (shown collapsed too): a file-diff glyph in `fg3`, the heading at 500 in `fg2`, its line
  in `fg3`, and for an empty interdiff **Show all changes** with its `I` keycap. Before the
  first file list lands, or while it fails, no thread has left the review: they wait, as
  threads whose file's text hasn't landed (PLAN §8), and the section doesn't show.
- **Diff:** one stream over every file (PLAN §6.7). Each file starts with its header: a 44-high
  `codeHead` bar — chevron, path (directory in `fg3`, name at 600; a renamed file shows
  `old → new`), status pill, totals, a quiet "Loading…" once its diff takes longer than 300 ms,
  Split|Unified segmented control with its `S` keycap, "Viewed" checkbox with `V`, previous / next
  file, and `⋯` (a menu in the comments row menu's style: **View file** with the file glyph (the
  file view, at the cursor's line when the cursor is in the file), **Comment on file** with the
  comment-plus glyph, dimmed and inert where the file takes none — a finished review, an
  interdiff's deleted file —, then under a hairline Copy path and Reveal in folder) — over
  24-high side labels (`main` /
  `working tree`, one per column in split view, `main → working tree` in unified; in an
  interdiff `round 1` / `working tree` and `round 1 → working tree`, the round by a commit glyph
  since its column is the round's snapshot) when the file shows lines. The header of the file being read stays pinned at the top while its rows scroll
  under it, and the next file's header pushes it up. **Viewed files collapse to their header bar**
  (as on GitHub), the chevron pointing right; the chevron opens one again (and collapses any).
  Rows per §1.1: 20 high, a 44-wide line number (`fg3`) and a 16-wide `+`/`−` marker per side;
  split view pairs the i-th removed line of a change with the i-th added one and fills a missing
  side with `recess`; unified shows both numbers then the marker. Code is JetBrains Mono 12 with
  syntax colours, intraline words on `addWord`/`delWord`, tabs as four spaces and no ligatures
  (`=>` stays two characters). Long lines clip with a fade at the right edge and scroll sideways
  together, one offset for every row and both sides (`Shift`+wheel, a trackpad); scrolled, the
  left edge fades too. Gaps are one 28-high row on `hunk`: the next hunk's `@@ -a,b +c,d @@` in
  `blue`, "··· 27 unchanged lines", "Expand 20 · Expand all" (a click reveals 20 lines next to
  the hunk below, or above for the end of the file); the board's separate hunk header and
  "Expand context below" bar are this one row. Files without lines show one notice row instead:
  binary (sizes), too large ("Load anyway"), submodule (old → new commit, `dirty`), unresolved
  conflict, no content change (mode change, rename without edits, empty file), skeleton rows
  while loading, and a failed load's banner (§4.4: one line, Details, Retry, Copy diagnostics).
  Each open file ends with the next-file footer. The **cursor** marks its line number in
  `accentText` at 600 with a 2px accent bar, and its line on `sel`; a selection is `sel` over the
  selected lines of one side. **Commenting with the mouse** is GitHub's gesture: the pointer over a
  line (a unified row, a split row's half) shows a `+` over its marker column, an 18 square on
  `accent` with a 12px plus in `accentFg` (radius `small`); a click opens the composer on that
  line, and pressing it and dragging over other lines of the same file and side selects them as
  the drag goes, the composer opening on them when the button comes up (anywhere). A line takes
  no `+` where `C` takes no comment: a finished review, an interdiff's old column. Inline thread
  cards go between rows, and the lines a card sits
  on stay shown (a thread relocation moved can stand on lines no change is near, which would
  otherwise fold into a gap).
- **Text in code** (PLAN §18.1): a press in a line's code (the pointer an I-beam there) puts the
  cursor on its line, as a click on its number does, and the **caret** at the character: before
  the one under the pointer, after it from 60% of its width on. Characters are where their
  glyphs are drawn: a wide one (CJK, an emoji) takes two cells, a combining mark goes with the
  letter before it (the caret never falls between them), a tab four cells. The caret is a 2px
  `accent` bar 16 high, painted while the stream has the keyboard; the keyboard's line moves
  (`↑`/`↓`, `J`/`K`, `N`/`P`, pages, `E`), `S` and `I` take it away. Dragging with the button
  down selects characters, the caret and the cursor following the pointer; a double-click
  selects the word under it (a run of identifier characters — alphanumerics and `_` —, of
  whitespace, or of any other character) and a drag after it extends by whole words, a
  triple-click the whole line and a drag after it by whole lines; a shift-click extends from
  the selection's start (else the caret). A selection is one file's: in split view one column's
  (the one the press was in), in unified view the rows in order, removed and added lines alike;
  dragging past the file's first or last row clamps there, onto a card or a gap row takes the
  end of the line before it, and within 24 of the stream's top (below the header's 44px bar) or
  bottom edge, or past it, scrolls the stream, faster the further, until it can't move. Selected
  text paints on `textSel` behind the characters, a line the selection goes on past one column
  further; it stays through scrolling either way and gaps opening, and goes with a press
  anywhere else in the stream, `Esc`, switching split / unified or the comparison (`I`), and
  its file's diff changing to other lines (the agent's edit; the caret too). A drag the window
  stops being active during ends there. `Ctrl C` copies it as the source text (real tabs, lines
  joined with `\n`, the lines a gap inside it hides included); with no text selected, a line
  selection copies its lines; with neither, nothing. A drag from a line's `+` still selects
  lines for a comment, never text.
- **Find** (`Ctrl F`, PLAN §18.1): a bar over the stream's top right, 8 below the file header's
  44px bar (the pinned header's controls stay in reach) and 16 in from the right edge, 400 wide:
  the `popover` surface with the `overlayLine` outline and the overlay shadow (as menus), radius
  `card` (10), 6 padding.
  One line holds the field (a control high, `inputBg`, `ctrlLine` outline, the search glyph, "Find
  in review"), the count in `fg2` at 12 ("3 of 17"; "3 of 17+" while files still load;
  "Searching…" before a first match while they do, "No results" in `fg3`, nothing for an empty
  query), then 24-square ghost buttons ↑ (previous, `Shift Enter`), ↓ (next, `Enter`) and ×
  (close, `Esc`) with their keys in tooltips; under it, when some file couldn't be searched,
  "2 files not searched" in `fg3`. `Ctrl F` opens it with the field focused (the selected text
  in it when that is one line), and again focuses the field with its text selected. Typing
  searches once it pauses (120 ms): every listed file in stream order, both sides, the text
  inside gaps and in viewed files included, through what the stream shows (an interdiff's
  diffs in an interdiff); a file not loaded yet loads for it (after the viewport's), and a file
  without text to search (binary, too large to show, a submodule, unmerged, a failed load) is
  counted as not searched; one whose content didn't change shows a notice, not lines, and isn't
  searched. Plain text, case-insensitive unless the query has a capital (smart case). A context
  line matches once, on its new side (in split view its new column, so the count is the same
  in either view); rows top to bottom and the old column before the new. The search goes to the first match at or after the cursor's
  row (else the current file's top) in that order; while later files are still loading it waits
  for a match after the cursor rather than going back behind it, and wraps to the first match
  once every file has been searched. Going to a match opens its file if it is viewed, opens the
  gap that hides it around it (3 lines either side), scrolls it to the middle of the stream
  unless it is in view below the pinned header and the bar, and puts the cursor on its line and
  the caret at its start; the keyboard stays in the field. `Enter` before typing has paused
  searches the new query and goes to its first match. A file loaded only for the search isn't
  highlighted until the stream shows it. While the bar is open every match in view
  paints on `findMatch` with a 1px `findLine` outline and the current one on `findCurrent` with
  a 1px `yellow` outline, radius 2: on an intraline word's `addWord`/`delWord` fill a match
  still reads as one by its outline. `Esc` (in the field or the stream) or × closes it: the matches go and the keyboard returns
  to the stream, the cursor on the current match's line. A press on the bar leaves the stream's
  selection alone.
- **File view** (PLAN §18.1): one file whole over the main area in place of the stream, which
  keeps its place under it. Its header is a 44-high `codeHead` bar, pinned at the top while the
  lines scroll: a file glyph in `fg3`, the path (mono 13, directory in `fg3` at 500, name at
  600, truncating at its start), a chip with the side's label ("working tree", "main"; a file
  glyph, `fg2` on `layer`), the review's status pill (as a file header's: "Modified", "Added",
  "Deleted" — a deleted file shows its old side) or, for a file the review doesn't change,
  "Unchanged in this review" in `fg3`, then at the right a ghost **Close** with an × glyph and its
  `Esc` keycap (tooltip "Back to the review (Esc)"). No side labels, no gaps, no footer. Its rows
  are the stream's unified rows with one 44-wide line number (the side's) and the marker: every
  line of the side, highlighted, the review's added lines on `addBg` with their `+` and
  intraline words as in the stream; the cursor, a line selection, the comment `+`, text
  selection, the caret and find matches look as in the stream. Thread cards sit under their last
  line, past the line number; comments on the whole file and outdated cards head the lines.
  While the text loads, skeleton rows; a file the side doesn't hold or a read that failed shows
  the one-line banner (§4.4), binary and too-large files their notices, an empty file
  "Empty file · nothing to show". `Ctrl F` opens the find bar over it ("Find in file"), searching
  that file only. A file read at its side over the size caps shows "Too large to show" with
  **Load anyway** (a small button), as the stream's notice does. Its side, status pill and side
  label are the review's own (all changes, `main` / `working tree`) whichever comparison shows;
  in an interdiff, after the pill, "changes since round n" in `fg3` (its marked lines those the
  stream marks) or "unchanged since round n" (read at the side, unmarked), and a file deleted in
  all changes shows its old side, every line on `delBg`. A composer holding a draft keeps it:
  another file opened (`Ctrl P`, View file, `F`), the close button, `Alt ←` / `Alt →` and a panel
  row that shows the stream do nothing but give the draft the keyboard; `Esc` in the composer
  discards it.
- **File view at the working tree** (PLAN §18.1 "Go to definition"): in a review whose new
  side isn't the working tree, a definition or reference elsewhere opens the file view of the
  file as the working tree holds it now. Its header's side chip reads "working tree" and, in
  place of the status pill, "As it stands now, not this review's side · read-only" in `fg3`; its
  lines carry no changed marks, no threads and no comment `+` (`C` does nothing). A definition,
  a hover card's link or a reference into the same file from there stays in this view.
- **File view outside the repository** (PLAN §18.1 "Go to definition"): a definition in a file
  outside the repository (a library's declarations) opens it read-only: the header shows its
  absolute path and the line asked for (`/home/kyle/…/@types/react/index.d.ts:120`; mono 13,
  cut in its middle by whole segments when it doesn't fit, its first segment and its file kept),
  no side chip, and "Outside the repository · read-only" in `fg3`; no changed marks, threads or
  comment `+`. Hover and go to definition work in it. Over the display caps (2 MB or 20,000
  lines: a library's 40,000-line declarations) it shows the too-large notice with **Load
  anyway**, as any file read at its side; past the 4 MB dv reads of such a file, the notice
  says so ("… over the 4 MB dv reads of a file outside the repository") without **Load
  anyway**.
- **File finder** (`Ctrl P`, PLAN §18.1): the switcher's look — an `overlay` panel 560 wide,
  radius 12, over the scrim, hung 8 under the title bar's command field and centred on it. A
  46-high filter field with the search glyph ("Go to file") has the keyboard; the list (up to 14
  rows, then it scrolls) holds 30-high rows radius 6: a file glyph (`fg` when selected, else
  `fg3`), the review's status letter in its colour for a file it changes (a blank column
  otherwise), and the path, directory in `fg3`, name at 500, truncating at its start, what the
  typed words match in `accentText` at 600; the selected row on `sel`. A 40-high footer: the count in `fg3` ("2 files") and the keys (`↑` `↓` move, `Enter`
  open, `Esc` close). No match: "No files match “x”" with "Every word must be in the file's path."
  in `fg3`. A click on a row or `Enter` opens it in the file view; `Esc`, a click on the scrim or
  `Ctrl P` again closes it.
- **Palette** (`Ctrl K`, or a click on the title bar's command field; PLAN §11 `palette`): the
  file finder's look and place, its field's placeholder the command field's words ("Go to file,
  review or command"). Its rows sit under section labels (12/600 in `fg3`, the first 22 high, the
  rest 30, bottom-aligned), in this order: **Commands**, the shell's and the stream's actions by
  name (a `chevron-right` glyph, the title, the action's keycap at the right), the stream's only
  while a review is open; **Reviews**, every repository's targets and open reviews as the
  switcher lists them (the switcher's glyph, the repository's name at 500, the label in `fg2`,
  an open review's status chip and "round n" in `fg3`); **Files**, once something is typed with
  a review open, the file finder's first five matches as its rows show them. A command passes
  when every typed word is in its title or the few other words it is known by; reviews filter
  as in the switcher. 30-high rows radius 6, the selected one on `sel`; the first row is
  selected and `↑`/`↓` move across the sections, wrapping. A 40-high footer holds the keys (`↑`
  `↓` move, `Enter` open, `Esc` close). Nothing left: "Nothing matches “x”" with "Every word must
  be in a command, a review or a file's path." in `fg3`. `Enter` or a click runs a command as
  its key would (a stream command once the main area has the keyboard back), opens a review, or
  opens a file in the file view; `Esc`, a click on the scrim or `Ctrl K` again closes it. Over
  the "Finish round" sheet it lists neither the review's commands nor its files, which would act
  under the sheet.
- **Interdiff** (round n+1 of a live target, PLAN §2.3): the stream, the file list and its
  totals are what changed since round n (its snapshot → now); `I` or the panel's toggle switches
  to all changes and back, the reading position staying where the same file and line still
  show. Its new side is the same text as in all changes, so new-side threads show there as in
  all changes; old-side threads, and threads whose file isn't in the interdiff (but is among the
  review's changes: one that left them is in the "Not in the diff" section), aren't drawn in
  the stream (`]`/`[` pass over them) and carry the comments list's "all changes" tag, named
  where they stand in all changes.
- **Thread card:** avatar (reviewer round; agent square with its model family's mark in the
  family's colour on a soft fill of it, §1.6, else the bot glyph in `blue` on `blueSoft`), name
  (an agent that named its model shows by it, "Claude Fable 5.2", wherever dv names the agent:
  here, the state chip, an annotation's header and its Ask, the handoff), agent tag (`agent`;
  the agent's own name, `claude-code`, beside a model's), time,
  anchor (`new · line 31`: where the thread shows now; "`new · lines 34–36 · moved`" while
  those are other lines than the ones it was written on, found there by relocation or recorded
  there by an event — never for code edited where it stands, PLAN §8), rendered
  markdown body, replies, state chip + actions, reply input. States
  and their chips: `open` (accent ring dot), `addressed` (blue "Addressed by agent — Resolve /
  Reopen"), `resolved` (green check, collapsed by default), `outdated` (yellow, shown with its
  original snippet). The card (`thread`, radius 10, `line2` outline, the card shadow; the focused
  thread's outline `accentRing`) sits under the last line of its range on its side: in split
  view in that side's half over the line's tint, 62 in from the half's edge, the other half
  `recess`; in unified view past both line numbers. Its 32-high header holds the state chip
  ("Open", "Addressed by Claude Fable 5.2" — the agent as it shows, its own name when it named no model — "Resolved"), "Outdated" when it is, the anchor in `fg3` ("file" for a comment on the whole file)
  and the reviewer's actions as small buttons (Resolve on an open thread, Resolve and Reopen on
  an addressed one, Reopen and a chevron on a resolved one; in a narrow card the anchor, then the
  chip's agent name, truncate, never the actions); an addressed thread's note follows
  on `blueSoft` with the agent's glyph (its family's mark, else the bot). A resolved thread collapses to that header with its
  comment's first line until its chevron opens it (or while a reply to it is being written). A
  thread whose code moved (lines inserted above it, its block moved, a small edit) follows it to
  its new lines. An outdated thread (relocation can't find its code at all, PLAN §8) has no line
  to follow: its card heads its file, under the header, with the lines it was anchored on in a
  `recess` block. When its round's snapshot tells where that code went (its trail: a new-side
  thread of a closed round, PLAN §8 "Round history"), the card stands there instead, inline
  as any card, under the last line that replaced the code (or, for code removed, under the line
  before where it stood; heading the file only when it stood at the top), with the "Outdated"
  chip, the anchor "code changed since round 1" and its original lines in the `recess` block;
  those lines stay shown as any card's. A thread whose file was renamed since its round, one
  git can't pair (an untracked file renamed), follows it by content into the new file; one whose file left the review heads the stream instead, in the "Not in the
  diff" section above. A **comment on the whole file** (PLAN §8) has no line either: its card
  heads its file, under the header (and its side labels) and above its first row or its notice
  (also while the file loads, since it needs none of its text), with the anchor "file" and no
  `recess` block; a file's comments on the whole file come first, in the order they were
  added, then the composer of a new one, then its outdated cards. One on a file deleted since
  keeps heading it with "Outdated" (its new side is gone); one whose file left the review is
  in the "Not in the diff" section, reading `path · file`. In a file shown without lines (too large to show, or "No content change": a
  file only renamed, or only its mode changed), a thread still found in it (PLAN §8: placed
  against the file read in full, or against the unchanged text the diff carries) heads the file
  too, above its notice, with the anchor where it stands and no "Outdated". The
  reply field reads "Reply…" (with `R` on the focused thread) and opens the reply composer in
  its place. On a finished review the actions and the field stay, disabled ("Replies are closed
  on a finished review").
- **Annotation card** (PLAN §19): an agent's explanation of its change, quieter than a thread
  card because nothing waits on it. It stands where a thread card would (under the last line of
  its range on its side, the same insets in split and unified view; one on the whole file heads
  its file before the file's comments on the whole file) and stacks with cards on the same line
  in the order they were added. `code` fill (the diff's own surface, so it reads as part of the
  code rather than over it, whatever the line's tint), `line` outline, radius 10, no shadow, a
  2px `blue` rule down its left edge inside the radius; the focused one's outline `accentRing`.
  A 28-high header: the agent's square avatar at 16 with its mark, the agent's name (its model's, when it named one) at 600 in
  `fg2`, "Annotation" in `fg3`, the anchor in `fg3` (`new · lines 12–14`, "file", "· moved" as
  a thread's), and at the right a small **Ask** (with its `R` keycap while focused). The body:
  Markdown at 13 in `fg2`, in full (the skill keeps an annotation to one to three sentences);
  the agent's own additions follow, each over a hairline under its name and time in `fg3`. The
  file view draws it at its lines as the stream does. **Ask** (or `R`) opens the reply composer in the card, its field reading "Ask
  claude-code about this…" and its footer saying "claude-code reads it with the round"
  in `fg3` in place of "Markdown", which a narrow card would otherwise cut it for (the question goes to the agent when the round is finished); posting turns the card into a thread card,
  `open`, whose first comment keeps a quiet "annotation" tag beside the agent tag. Hidden with
  `A`, annotations aren't drawn at all (the status bar says they are hidden). In an interdiff
  only the annotations written since its round are drawn; an outdated one, or one whose file
  left the review, isn't drawn anywhere in the stream (never in the "Not in the diff" section),
  the comments list keeping its row. On a finished review **Ask** stays,
  disabled.
- **Composer:** a card under the selected lines (or the cursor's line), outlined in
  `accentRing`: "Comment on new · lines 12–14" with the comment glyph, a multi-line Markdown
  field (3 rows, growing to 12; `Enter` is a newline), why the last post failed in `red`, and a
  footer — "Markdown", Cancel with `Esc`, the primary "Comment" with `Ctrl Enter`. As a reply it
  is the field and the footer alone, inside the card ("Reply"). It takes the keyboard while
  open, also scrolled out of view (typing brings it back); posting or cancelling gives it back to
  the diff, posting focuses the thread. **Comment on file** (the header's `⋯`, or `Shift C` on the
  current file) opens it heading the file instead, across its width, "Comment on file", on the
  file's new side (a deleted file's old side). Hidden by its file collapsing, it keeps its text and
  gives the keyboard back to the diff (`C` opens the file and takes it up again). A
  comparison that can't hold it — an interdiff that doesn't list its file, or (it being on the
  old side) whose old column is the round's snapshot — leaves it undrawn with its text and
  lines, until all changes show it again; `C` then switches to all changes and takes it up. Its
  file leaving the review (the agent reverted it) drops it.
- **Hover card** (PLAN §18.1 "Hover"): the pointer resting 350 ms on code (the stream or the
  file view; any character but whitespace and quotes), or `Ctrl I` at the caret, hangs a card 2
  below what it is about, the servers' range (a whole class name, as this frame draws it) or
  else the token, starting at its left edge; where it doesn't fit below, it hangs 2 above the
  token's top instead (never over the token), and it stays 8 inside the main area (the pane's
  rows: never over the title bar, the status bar or the panel), on the roomier side when it
  fits neither way: the `popover` surface
  with its `overlayLine` outline and the overlay shadow, radius `card` (10), 280 to 560 wide. Its
  body (12/8 padding, scrolling past 360 high) is the server's answer as Markdown, its fenced
  signature highlighted as code, inline code on `icodeBg`, links in `accentText` (a file link opens
  the file there, a web link the browser); the text can be selected. When several servers serve
  the file (a `.tsx`: TypeScript and Tailwind CSS) their answers stack in the registry's order
  (PLAN §18.4), the card opening with the first that says anything and growing as the others
  land: a hairline (`line`, 8 above and below) between two answers and, while more than one is
  shown, each headed by its server's name ("TypeScript", "Tailwind CSS") in `fg3` at 12 (the
  secondary text), 4 above its answer; one answer alone has no heading. Under a hairline, two small
  buttons — **Go to definition** `F12` and **Find references** `Shift F12` with their keycaps — and,
  when the server read a side other than the working tree, "Read against the working tree" in
  `fg3` at the right. It stays while the pointer is over the token or the card and closes 300 ms
  after it left both (closing only the card: resting on the next token still asks about it),
  at once on a scroll, a press in the code, `Esc` (first in the stream's chain) or its file's
  diff changing. A hover asked for and not answered yet is cancelled as soon as the pointer
  leaves its token. Text selected in the card takes the keyboard; when the card goes, the
  keyboard returns where it was before the press in it. Nothing shows without a server, or for
  an empty answer about the working tree's text; when every server answered with nothing about
  another side's text (a removed line's symbol), a one-line card, 12/8 padding, says "No
  information: read against the working tree" in `fg3` at 12, no buttons.
- **References tab** (PLAN §18.1 "Find references"): while a references list (or several
  definitions) is open, the panel heads its column with a segmented pair 8 below its top, 16 in
  from its sides (the `ctrl` fill and `ctrlLine` outline of the Split | Unified control, radius 7,
  3 padding; each tab half the width, 26 high, 13/500, the chosen one on `segOn` in `fg`, the other
  `fg2`): **Review** (the file list and the comments, as without the tabs) and **References n**
  (n once it answered). `Shift F12` and a go to definition with several places choose References.
  Its body: a header — "References" (or "Definitions") at 600, the symbol in mono `accentText`,
  the count in `fg3`, a 22-square × at the right ("Close the references") — then **In this review**
  (the panel's checkbox and its label in `fg2` at 12) and, when it is so, "Read against the working
  tree" in `fg3`, over a hairline. Then each file, the review's files first, then the rest, each
  group in path order: a 26-high row with the file glyph, its path (directory `fg3`, name `fg` at
  600; when it doesn't fit, whole leading segments give way to "…/", never a cut inside a name;
  "outside the repository" in `fg3` after a path outside it) and its row count in `fg3`; under
  it a 24-high row per place: a 3-wide changed marker (`green`, on rows the review changed — in an
  interdiff, changed since the round —, else empty), the line number in mono 11 `fg3` right-aligned in 36, then the line's text in mono
  11.5 without its indentation, highlighted, the symbol on `findCurrent` at 600 — when the
  symbol starts past what the row has room for, the line's start gives way to "…" so the symbol
  shows with a little of what precedes it; `controlHover` under the pointer. The count and the
  rows' numbers show as soon as the server answers, each row's text as its file is read, the
  colours once all are; the list is virtualised (only the rows in view are drawn). While the
  server is asked, four skeleton bars; none, "No references found" (or with **In this
  review**, "None on lines this review changed") in `fg3`. A click on a row goes there as go to
  definition does (a row read on another side, to that side).
- **Next-file footer:** "Next: `src/routes/users.test.ts`" + "Mark viewed and go next `V` `J`"
  ("Go to next file `J`" once viewed; the last file says "Last file" with "Mark viewed `V`").
- **Status bar:** a dot and the location with the backend's state — green "Local · watching for
  changes", yellow "· refreshes when focused" (the watch can't follow the worktree), red "· refresh
  failed" or "· can't be opened", `fg3` "No repo" / "· opening…". A remote repo is named by its
  connection (the distro: "Ubuntu · watching for changes"), whose state comes first: `fg3`
  "Ubuntu · installing dv (3.1 / 7.4 MB)…" (also while its review opens) and "· connecting…",
  yellow "· reconnecting…", red "· stopped" (the distro was shut down, which dv leaves so until
  asked) and "· disconnected" (each with the banner, §4.4); a connection closed for idleness
  (PLAN §5.1) is up, so the review's own state shows, "· refreshes when focused" when its watch
  couldn't start; then, after a `|`, the review's
  status, round and thread counts ("Changes requested · round 2 · 3 open · 1 resolved", "no comments"
  when there are none; then the agent's annotations, "· 5 annotations", "· 5 annotations
  hidden" while `A` hides them; an interdiff adds "· since round 1"); at the right, each language
  server of the review's worktree (PLAN §18.5) — a 7 dot and its name and state in `fg2`, 22 high,
  `controlHover` under the pointer: green "TypeScript ready", yellow "TypeScript loading 40%" (its
  progress), "TypeScript restarting…" and "TypeScript waiting for the connection", `fg3` "TypeScript starting…", red
  "failed", and `fg3` "TypeScript missing" for a server the project lacks — then `Ctrl Enter
  finish review` while the review can still be finished. When an update of dv is downloaded
  (PLAN D9) the right end starts with the ↑ glyph and "dv 0.1.0-local.20261018120000 is ready"
  in `accentText` and a small **Restart to update** button (22 high), which quits dv, installs
  the update and starts it again on the open review; it shows until then, before the
  out-of-date notice (§4.3). When installing would close something (other dv processes running
  from the install, this window's unsent comment or open Finish round sheet, PLAN D9) it asks
  first, over the status bar's right end as a server's details do (a menu panel 420 wide, 16
  padding): "Restart into dv <version>?" (title), "Installing the update now closes:" in `fg2`,
  a `fg3` bullet per thing ("2 other dv windows or commands running from this install", "your
  unsent comment in this window", "the Finish round sheet in this window"), and right-aligned
  small buttons **Not now** and **Restart anyway**. A restart that fails turns the item into
  the warning triangle and "Update failed · details" in `red`, its tooltip "dv <version> wasn't
  installed: <why>", and **Retry**. A click on a server opens its details
  over the status bar's right end (a menu panel 420 wide, 16 padding, the `popover` surface): the
  dot, the name at 600, its version in `fg3` when the server gives one (`initialize`'s
  `serverInfo`) and its state, **Retry** (a small button) on a failed one with why it failed in
  `red`, then "Command" and the command line in mono on `icodeBg`, "From DV_LSP_TYPESCRIPT" (or
  "the project's own (7.0.2)", "settings.json") in `fg3`, "Serves" and the directory it serves
  (its absolute path) in mono, and "Last output" with its last stderr lines in mono; a missing
  one's say "TypeScript isn't available", why (where it looked in `fg3` under it), "How to get
  it" — its prose in `fg2`, each command and settings key as inline code with a 20-square copy
  button (the copy glyph, through the clipboard seam) — and a small **Check again** button (the
  ↻ glyph) 6 below. A click on the item again, or `Esc`, closes them; `Esc` in the composer
  closes them first, the draft staying.
- **Code notice** (PLAN §18.1 "Go to definition"): a place that can't be shown (a URI that
  isn't a file; a file outside the repository opens in the file view instead) heads the main
  area as a quiet one-line notice, not an error: on `layer` with a `line` outline, radius 6, 12
  in from the main area's edges, a package glyph in `fg3`, "Defined outside the repository:
  `<uri>:<line>`" in `fg2`, why in `fg3` at 12, and **Dismiss**; `Esc` (once nothing else in the
  main area takes it) or going to another place takes it away.

### 4.2 Finish review / Submit (`GlassSubmit`)
- Sheet, 980 wide, centred, over `scrim`. Header: "Finish review", `PR #482`, the checks (the
  strip's counts, §4.5), close.
- Left: **Summary** with Write/Preview tabs (markdown), then **Verdict** as three radio cards —
  Comment (`1`) / Approve (`2`) / Request changes (`3`) (local reviews show Approve / Request
  changes only). On the viewer's own PR, Approve and Request changes are dimmed with "You can't
  approve your own pull request" (PLAN §9.8); Request changes needs a summary, Comment a summary
  or a comment, said under the verdicts while missing.
- Right: **Comments to post** — each with an include checkbox, `path:line`, body excerpt; draft
  replies to GitHub threads as "Reply to @login on `path:line`" (PLAN §9.6). A comment outside
  GitHub's hunks shows a yellow row: "Unchanged line outside the diff — GitHub's API can't anchor
  it here", explanation, **Post as file comment** / **Keep local**, with Undo after choosing; on a
  file the PR doesn't touch, "Not in this PR" and **Keep local** alone; on a file GitHub shows
  without lines (binary, renamed only, too large), **Post as file comment** / **Keep local**. A
  pending review of the viewer's on GitHub that dv didn't start heads the list as a yellow row:
  "You have a pending review on GitHub (3 comments). Submit or discard it there first", **Open on
  GitHub**; Submit stays disabled until a refresh no longer finds it.
- Footer: "Posting as `kylekz` via gh", blocker text when something needs fixing, Cancel, **Submit
  review `Ctrl Enter`** (disabled until every included comment is postable or decided).
- **Posting** (PLAN §9.8): the primary reads "Posting 12 of 40…" and the sheet stays, as the
  local sheet's "Finishing…" does. A comment GitHub refuses gets a red row with GitHub's message;
  anything else (signed out, rate limited, offline) shows in the footer in `red`; both offer
  **Retry** (continues where it stopped) and **Discard pending review** (a confirmation naming
  how many comments it deletes from GitHub). "The PR moved" closes the sheet on a refresh, the
  strip saying what changed.
- Keys: `Tab` moves between the summary, the verdicts and the comments; in the comments `↑`/`↓`,
  `Space` (include), `F` (post as file comment), `L` (keep local), `U` (undo the choice).
- Local reviews use the same sheet titled "Finish round" with no GitHub rows: "Finish round n"
  over "`repo` · `working tree vs main` · 7 files `+n −m`" (the review's changes, which the round
  finishes, also while the stream shows an interdiff); the Summary (a 150-high field in
  mono 12.5, "Markdown · optional", `Tab` to move between it and the verdicts); the Verdict as
  two cards, **Approve** (`1`, "The work is done; the review closes") and **Request changes**
  (`2`, "The agent has more to do; the round goes back to it"), Request changes chosen while a
  thread is open, else Approve; on the right "Comments" — those opened this round and earlier
  ones not yet resolved, never an annotation nobody asked about (PLAN §19.1) (with their round: the one a thread was opened in, which a move recorded
  in a later round, PLAN §8, doesn't change), each with its state glyph, `path:line` where it
  shows now (as the comments list names it, `fg3` while not placed yet) and first line; the footer "Finishing round n as `name`", why the last attempt failed in `red`, Cancel
  and the primary button named by the verdict with `Ctrl Enter`. The verdicts have the keyboard
  when it opens (`↑`/`↓`, `1`/`2`); `Esc` closes it. While the round is being finished the
  primary reads "Finishing…" and the sheet stays: `Esc`, Cancel (dimmed) and the close button do
  nothing until the round lands, when it closes, or fails, when it shows why. It holds unsaved
  text, so `Ctrl O` and the source picker open over it, not in its place. It hangs 92 below the
  title bar in a 900-high window, less in a shorter one (24 at 700).

### 4.3 Onboarding (`GlassOnboarding`) and Settings
One screen of grouped rows rather than the board's numbered steps: the checks (PLAN §12) are
independent of each other (only a distro's own checks wait for dv to be installed there, which
its card says), so a wizard's order would only add clicks, and the same cards serve Settings ›
Setup unchanged.
- **First run:** shown in place of the panel, the main area and the status bar (the title bar
  stays) while the data dir has no `onboarding.json`, on the `panel` surface over the window
  material. A centred column 860 wide, 30 from the top: "Set up dv" (the onboarding title, 20/600)
  over "dv runs where your code lives. Each environment is set up once, here." in `fg2`, **Check
  again** (a ghost button with the rotate glyph, "Checking…" while checks run) on the right; then
  one card per environment; then, over a `line` rule, "Change any of this later in Settings." in
  `fg2`, **Skip for now** and **Continue `Enter`**, both of which end the first run. It has the
  keyboard: `Enter` continues.
- **An environment's card** (`layer`, `line` outline, the card radius 10): a 30×30 tile on
  `blueSoft` with its glyph in `blue` (a monitor for this machine, the terminal for a distro), its
  name at 600 and under it, 12px in `fg2`, a dot (`green` running, a `fg3` ring stopped) and "This
  PC", "WSL · running" or "WSL · stopped" ("· checking…" in `fg3` while it is checked); a stopped
  distro has **Start** (a small button; "Starting…"). A note in `fg2` says why its checks are
  missing or short ("Ubuntu isn't running: start it to check it", "the other checks run once dv
  is installed in Ubuntu"); a failed Start says why in `red`. Then its checks as rows in a
  `line`-outlined box (radius 8), 1px `line` between rows.
- **A check's row:** the state's glyph at 15 (ok: circle-check in `green`; missing: a `yellow`
  ring; out of date: the warning triangle in `yellow`; failed: circle-x in `red`; skipped:
  circle-minus in `fg3`), the title at 500 in a 230 column, the detail in 12px `fg2` (`red` when
  failed; the sentence first, a path last), whose longest path gives way first, by whole
  segments in its middle (`D:\Software\…\SKILL.md`), so the sentence's start and the path's end
  stay readable, then the fix: a small button when dv applies it (Install, Update, Add to
  PATH; its progress in `fg3` while it runs, "Installing 3.1 / 7.4 MB…"), else the command as
  inline code (at most 280 wide, cut with an ellipsis) and a copy button that goes through the
  clipboard seam. A fix that failed says "Didn't work: …" under the row in `red`, until the row
  reads ok.
- **Settings** (`Ctrl ,`, the title bar's `⋯`): a sheet over the scrim like the "Finish round"
  sheet (`overlay`, radius 12, up to 900 wide, hung as §4.2's), its header "Settings" with a
  close button. Its body scrolls through two pages, each a 13/600 title over its cards:
  **Appearance**, one card with a row per setting (its name in 600, what it does in `fg2`
  secondary, its control on the right, a header-style segmented control): **Theme** Aura |
  Claude | Dracula (§1.3), **Mode** System | Dark | Light and **Material** Mica | Acrylic |
  Opaque (on macOS Vibrancy | Opaque), each applied as it is clicked. With Dracula the mode row's `yellow` line says
  "Dracula has no light palette: dv shows it dark, keeping the mode for the other themes". With
  Claude or Dracula the translucent materials are disabled segments and the material row's line
  says "Mica and Acrylic are Aura's: Claude is opaque" ("Vibrancy is Aura's" on macOS; Dracula
  likewise), keeping the setting for Aura. A setting made on another platform (Mica on a Mac)
  shows Opaque, the line saying so ("Mica is Windows 11's: dv shows Opaque here, keeping it for
  Windows 11"). A
  material the platform can't show is a disabled segment in `fg3`, and a `yellow` line under
  the row says why ("Mica and Acrylic need Windows 11 with Transparency effects on…"; "Vibrancy needs Reduce
  transparency off…" on macOS; on Linux, that dv is Opaque there) and, when the setting is one of them, that dv shows
  Opaque meanwhile, keeping the setting. Then **Setup**, the cards above with Check again. Opening it checks everything again; `Esc`, the close button or a click on the scrim
  closes it, the keyboard back where it was.
- **The out-of-date notice** sits at the status bar's right: the warning triangle and
  "dv-review skill out of date in Ubuntu" ("on Windows" for this machine; "not installed in
  Ubuntu" for a distro reached without it) in `yellow`, **Update** (or Install) and a dismiss
  (×). Once dismissed it doesn't come back this session.

### 4.4 States every view handles
- **Loading:** skeleton rows in the panel and diff (no spinners over content) for up to 300 ms, then a
  quiet progress line in the file header. While a review opens or lists its files, the main area
  shows the file header's bar with "Opening the review…" / "Listing changes…" over skeleton bars; the
  panel shows skeleton rows, then "Listing changes…". That is the first listing only: a refresh
  keeps the list, the stream or the empty state on screen until it lands, and a file whose diff
  loads again keeps showing the one it had (with the header's "Loading…" after 300 ms).
- **Errors:** an inline banner at the top of the affected region — icon, one-line message, the
  command that failed in a disclosure ("Details"), a fix action, "Copy diagnostics". Never drawn over
  other regions, never raw stderr as the headline (the first implementation painted git's stderr across
  the sidebar). The banner is `redSoft` with a `red` outline at 35%, radius 6, 12 in from the
  region's edges. A review that didn't open shows one at the top of the main area (Retry, Dismiss;
  the review open before stays open); one in a WSL repo whose connection is the trouble reads
  "couldn't open Ubuntu:/home/kyle/api", the connection's error and diagnostics (its launcher,
  state history, the server's last output) in Details and Copy diagnostics. A review picked
  whose connection is the trouble is never lost and never reported twice (PLAN §5.2): it waits
  for the connection as the pending pick, and opens by itself once the connection is back (its
  banner going, menus and the keyboard left where they are), but never in place of unsaved
  text: while the "Finish round" sheet is open or a composer holds a draft, it waits on until
  the sheet closes or the draft is posted or cancelled; meanwhile the main area raises nothing
  while the open review goes through the same connection (its status bar and the backend-down
  banner below say it), and otherwise shows the "couldn't open …" banner, whose Retry runs the
  pick at once (the reviewer's own pick, as from the switcher) and whose Dismiss drops it; the
  Dismiss of any other banner there leaves the pick waiting. A
  refresh that failed shows one there too (Retry), and so
  does a folder (or a `dv open` path) that isn't a repository (Dismiss). A review whose worktree
  is gone (PLAN §2.2: a working review of a linked worktree removed since) says so in its
  headline, "review r-… can't be opened: its worktree is gone (removed, moved or deleted)", the
  worktree's path in Details.
  A round's snapshot missing from the repository (its ref deleted by hand and pruned) is a banner
  there too, "Showing all changes: round 1's snapshot (…) is missing from the repository"
  (Dismiss), over the all-changes stream it falls back to.
- **Handoff:** while the review is `ready` the agent's handoff heads the main area: an info
  banner on `blueSoft` over `code` made opaque (on Mica and Acrylic the material doesn't show
  through it, so its note's fade ends in exactly its colour), with a `blue` outline at 35%,
  radius 6, 12 in from the region's edges —
  the agent's glyph (its model family's mark in the family's colour, §1.6, else the bot in
  `blue`), "`Claude Fable 5.2` marked round 2 ready" (the agent as it shows: its model, else
  its own name, at 600; a long
  name truncates, "marked round n ready" and the disclosure never do), "· 5 annotations" in
  `fg2` when the review holds annotations written in that round (PLAN §19), the first line of its
  note (`dv review done --note`, PLAN §7) as plain text in `fg2`, and a **Show note** disclosure
  that opens the whole note under it as Markdown (the first line then leaves the header),
  growing to 240 high, then scrolling, so the stream keeps its room; its `md` inset at the foot
  stays below the scrolled text, so a clipped line stops short of the outline, and a 28-high
  fade into the banner's colour sits there while more of the note lies below. It goes once the
  reviewer finishes the round. Each handoff (the next round's, or the agent's `dv review done`
  again with another note) is a new banner, its note closed.
- **Backend down (WSL):** while the connection to the distro's dv reconnects (PLAN §5.2) the status
  bar says "Ubuntu · reconnecting…" in `yellow` and nothing else changes: the stream, the list and
  the threads stay, no banner is raised for the calls that fail meanwhile, and the reconnect reads
  everything again. Once it has failed for good, the status bar says "Ubuntu · disconnected" in
  `red` and the error banner heads the main area: "Lost the connection to dv in Ubuntu", the
  error and the connection's diagnostics (its launcher, state history and the server's last
  output) in Details, **Retry** (connects again; the banner goes while it tries and comes back if
  it fails) and Copy diagnostics; the review's own "refresh failed" banner stays down meanwhile.
  When the distro was shut down (`wsl --shutdown`, which dv never undoes by itself, PLAN §5.2),
  the status bar says "Ubuntu · stopped" in `red` and the banner reads "Ubuntu stopped", why and
  the diagnostics in Details, with **Start Ubuntu** (boots it and connects; the review then reads
  everything again) and Copy diagnostics. A banner that comes back after its action opens with
  its Details closed. **Open setup** beside the action opens Settings › Setup (§4.3).
- **Empty:** no changes ("Working tree clean — nothing to review" for a `working` review, "No changes
  — nothing to review" for other targets), an empty interdiff ("No changes since round 1",
  "Nothing changed after the round was handed over.", and **Show all changes** with its `I`
  keycap), no reviews ("Pick a repo to start a review"), no open comments. While threads whose
  files left the review are in the "Not in the diff" section (§4.1), no changes and an empty
  interdiff show the stream with that section instead, the same words (and Show all changes) a
  row under its cards. The switcher with no
  known repo says "No repositories yet"; a filter that matches nothing says so.
- **Read-only (submitted PR review):** banner "Submitted to GitHub · view on GitHub", composer and
  thread actions disabled, not hidden (a GitHub thread keeps Resolve / Unresolve and Open on
  GitHub: GitHub's state, not the review's, PLAN §9.6). A finished local review says "Approved · this review is
  finished" (on `greenSoft` with a `green` outline at 35%; "Closed · …" in `fg2` on `layer`) with
  **Start a new review**, which opens a fresh review of the same target; `C`, `R`, `X` and
  `Ctrl Enter` do nothing on it.

### 4.5 Pull requests (PLAN §9)
- **PR picker** (`Ctrl G`, the title bar's **PRs**): an overlay like the switcher (same width,
  surface and rows), for the repository in view. A field, "PR number, #n, owner/repo#n or URL",
  then the repository's open PRs, those requesting your review first with a "Review requested"
  chip (accent), each row: `#482` in `fg3` mono, the title, the author's login in `fg3`, a
  `draft` chip when it is one, and "2h" for when it was updated. Typing filters by number and
  title; a reference the field parses opens directly on `Enter`. While the list loads it shows
  the session's last list, else a shimmer row; a failure shows the setup check's words ("Sign in
  to GitHub: `gh auth login`") in place of the list, the field still working.
- **PR strip** (§2: under the title bar, `chrome`, 34 high, PR reviews only): `#482` mono `fg3`,
  the title `fg1` 13 semibold (truncated), the author, `base ← head` in mono 12 (`owner:head` for
  a fork), the PR's state chip (Open accent, Draft grey, Merged violet, Closed red), the checks as
  small counts (`✓ 12` green, `✗ 1` red, `● 2` yellow; a click opens the PR's checks page), then
  on the right the round ("Round 2 · since your last review") and what moved in `fg2` ("2 new
  commits since this draft's head", "Read 3 min ago"; in yellow "The PR moved: 2 new commits"
  while a pending review holds the draft, and "Couldn't refresh · details" when the mirror is
  stale), and the buttons **Refresh**
  (`Ctrl R`, spinning while it runs), **Start round 3** (accent, only when the latest review is
  submitted and the PR has newer commits) and **Open on GitHub** (an icon button).
- **GitHub's threads** use the thread card (§4.1) with: GitHub's mark as the avatar (round, `fg2`
  on `surface2`), the login as the name, an "on GitHub" tag, the time, and no state chip but
  **Resolved** (green) or **Outdated** (yellow) when they apply; a resolved GitHub thread is
  collapsed like a resolved local one. The actions row: **Reply** (`R`; on a submitted review
  "Start round n+1 to reply" instead), **Resolve** / **Unresolve** (`X`, only where GitHub allows
  the viewer), **Open on GitHub**. A draft reply sits at the
  bottom of the card, framed like the composer, headed "Your reply · posts with your review",
  with Edit and Delete; while it is posted to the pending review it reads "Posted to your pending
  review" and can't be edited (PLAN §9.8). Comments of a pending review on GitHub that dv didn't
  post show with a "Pending on GitHub" tag; dv's own posted drafts show once, as the drafts.
- **Not in the diff** (§4.1) lists GitHub threads that don't relocate under "Outdated on GitHub",
  each with the code GitHub shows for it (its diff hunk, mono, the commented lines marked).
- **Comments list:** GitHub's threads in their own group, "On GitHub", after the review's own;
  the status bar counts them apart ("2 open · 5 on GitHub").
- **Switcher:** a repository's PR reviews in a "Pull requests" section, `#482 Title` with the
  status chip (`draft` accent, `submitted` green); the title bar names a PR review the same way.
- **After submitting:** the banner "Submitted to GitHub · View on GitHub" (§4.4) and, when the
  PR has newer commits, **Start round n+1** beside it.

---

## 5. Keyboard map

| Key | Action |
|---|---|
| `J` / `K` | next / previous file: its header at the top, the cursor on its first change (the "Not in the diff" section isn't a file: `J` from it goes to the first file) |
| `N` / `P` | next / previous block of changed lines, on into the next files |
| `]` / `[` | focus the next / previous thread that isn't resolved (open or addressed), scrolling to it, on into the next files (in an interdiff, the threads it shows); annotations are passed over |
| `A` | show / hide the agent's annotations in the stream and the file view (PLAN §19.3; shown whenever a review opens) |
| `I` | switch between the interdiff since the last round and all changes (the panel's toggle; while the review offers it, PLAN §2.3) |
| `V` | toggle viewed on the current file (`V` then `J`: mark viewed and go next); each press flips the state the last one asked for, however fast they come |
| `C` | comment on the selected lines, else the cursor's line (in the file view, any line of the file, changed or not), else (no cursor yet) the first line in view, where the cursor goes too (click a line number to put the cursor there, shift-click to select to it; a line's `+` does it all with the mouse, §4.1); a composer holding a draft takes the keyboard instead, wherever the cursor is. In an interdiff it comments on the new side only: on the old column (the round's snapshot, which no anchor names) it does nothing |
| `Shift C` | comment on the whole of the current file: the composer heads the file (the header menu's Comment on file); a composer holding a draft takes the keyboard instead, as for `C`. Nothing on a finished review, or in an interdiff on a deleted file (its old column is the round's snapshot) |
| `R` | reply to the focused thread (the reply composer opens in its card; on a GitHub thread, a draft reply that posts with your review; on an annotation, Ask: posting opens it as a thread) |
| `X` | resolve the focused thread, or reopen it when it is resolved (a GitHub thread: resolve / unresolve on GitHub, where GitHub allows the viewer); nothing on an annotation |
| `↑` / `↓` | move the cursor a line (the stream scrolls to keep it in view; it crosses into the next file) |
| `Shift ↑` / `Shift ↓` | extend a selection from the cursor, within its file and side; a gap in the way opens a step towards the cursor rather than being selected unseen |
| `←` / `→` | in split view, put the cursor on the old / new side (the new side until one is chosen, or a line number clicked). A file without lines on that side (a deletion, an addition) moves it to the other side while it is in that file only: the next file that has lines on the chosen side puts it back |
| `PageUp` / `PageDown` | scroll a page, the cursor keeping its place on screen |
| `E` / `Shift E` | expand the gap nearest the cursor by one step towards it / fully |
| `S` | toggle split / unified (remembered per review) |
| `Ctrl Enter` | finish review / submit: the "Finish round" sheet, while no text field has the keyboard (in the composer: post comment) |
| `Ctrl K` | the palette: commands, reviews and, as you type, the review's files (§4.1; again, or `Esc`, closes it) |
| `Ctrl O` | switch repo / review (the switcher; again, or `Esc`, closes it) |
| `Ctrl Shift Backspace` | in the switcher: remove the selected row's repo from the list (not the open review's, §4.1) |
| `Ctrl Shift W` | in the switcher: ask to close the selected row's review; `Enter` then closes it for good, `Esc` keeps it (§4.1) |
| `Ctrl G` | the PR picker (§4.5) |
| `Ctrl R` | on a PR review: refresh it from GitHub (fetch, threads, checks, Viewed; PLAN §9.4) |
| `Ctrl B` | toggle panel |
| `Ctrl ,` | Settings: Appearance and Setup (§4.3); again, or `Esc`, closes it |
| `Enter` | on the first-run screen: Continue |
| `Esc` | close the top overlay, else a comments row's menu, else the language server's details, else the hover card, else the file menu, else close an empty composer (one holding a draft is passed over: only `Esc` in the composer discards a draft, so a press in the code to select text to quote, `Ctrl C` and `Esc` never lose it), else close the find bar, else clear the selection (the lines and the text both), else the code notice (a place that can't be shown), else, in the file view, close it (back to the stream where it was); in the find field it closes the bar first, and in the composer it discards the draft once a comments row's menu or a language server's details over it have closed (the field has the keyboard, so the stream's chain doesn't run) |
| `F` | the current file whole in the file view, at the cursor's line when the cursor is in it (a line on the other side only, a removed one, maps to the next line of the side shown) (the header menu's View file); nothing in the file view itself; with a draft in the file view's composer, the draft takes the keyboard instead |
| `Ctrl P` | the file finder: every file of the review's new side (again, or `Esc`, closes it) |
| `Alt ←` / `Alt →` | back / forward through the history of jumps between the stream and the file view, restoring the view, the file and the line; the mouse's back and forward buttons too. A step that would replace or close a file view holding a draft gives the draft the keyboard instead, the history left as it was |
| `Ctrl C` | copy the selected text, else the selected lines (§4.1 "Text in code"); in a text field (the find field, the composer, the filters, the summary) its selection — that and `Ctrl X` going through the clipboard seam (`dv_ui::system`) like every copy |
| `Ctrl I` | the hover card for the symbol at the caret (§4.1 "Hover card") |
| `F12` / `Ctrl`+click | go to the definition of the symbol at the caret / under the pointer (one place: the stream or the file view there, a jump the history keeps; several: the References tab) |
| `Shift F12` | find the references of the symbol at the caret: the panel's References tab |
| `Ctrl F` | the find bar, its field focused (the selected text in it when that is one line); again, the field focused with its text selected (§4.1 "Find") |
| `Enter` / `Shift Enter`, `Esc` | in the find bar: the next / previous match, wrapping; close it (the keyboard back to the stream) |
| `↑` / `↓`, `Enter` | in the switcher, the source picker, the file finder and the palette: move (wrapping), open |
| `Enter` / `Esc` | in the file filter: show the first file left / clear the filter (both return to the diff) |
| `Ctrl Enter` / `Esc` | in the composer: post / discard (both return to the diff; `Enter` is a newline) |
| `↑` / `↓`, `1` / `2`, `Tab` | in the "Finish round" sheet: choose the verdict (Approve, Request changes) / move between the verdicts and the summary; `Ctrl Enter` finishes the round, `Esc` closes |
| `1` / `2` / `3`, `Tab`, `Space`, `F`, `L`, `U` | in the PR "Finish review" sheet: the verdict (Comment, Approve, Request changes) / move between the summary, the verdicts and the comments / in the comments: include, post as file comment, keep local, undo (§4.2); `Ctrl Enter` submits, `Esc` closes |

macOS uses `Cmd` for every `Ctrl` above (`⌘`+click for `Ctrl`+click; gpui's `secondary`
modifier, which every row of `dv_ui::keymap` names). Single-letter keys are inactive while a text
field has focus. Keycaps render with `kbd` style (11px mono, `line2` border), in the platform's
spelling: words on Windows and Linux (`Ctrl O`, `Shift ↓`, `Alt ←`, `Ctrl Enter`), macOS's menu
symbols in its order `⌃⌥⇧⌘` against the key there (`⌘O`, `⇧↓`, `⌥←`, `⌘↩`).

**macOS** also has its menu bar (`dv_ui::app_menu`) and the keys every Mac app has:

| Key | Action |
|---|---|
| `⌘Q` | quit dv (dv › Quit dv) |
| `⌘H` / `⌥⌘H` | hide dv / hide the other apps (dv › Hide dv, Hide Others; Show All) |
| `⌘M` | minimize the window (Window › Minimize) |
| `⌘,` | Settings (dv › Settings…, the row above) |

dv › About dv shows its version. The Edit menu (Undo, Redo, Cut, Copy, Paste, Select All) acts
on the focused text field, with the keys the fields already have (`⌘Z`, `⇧⌘Z`, `⌘X`, `⌘C`,
`⌘V`, `⌘A`); its Cut and Copy are dv's, through the clipboard seam.

In the file view the stream's keys act on its one file; `S`, `V`, `I` and the gap keys do
nothing there.

The stream's keys work wherever the main area has the keyboard, which it does whenever no overlay
or text field does (a click in the stream gives it back). The **current file** — the one the panel
highlights, `V` marks and `J`/`K` step from — is the cursor's file while the cursor is on screen,
else the file at the top of the viewport, whose header is the pinned one.

---

## 6. Known differences between the boards and this spec

- Boards say "Resolved by agent"; the model is `addressed` by the agent, `resolved` only by the
  reviewer (PLAN §2.5). Build per this file.
- `GlassOnboarding` says "Installing dv-host 0.3.0"; the backend is the Linux `dv` itself (PLAN D6):
  "Installing dv 0.3.0 in Ubuntu". It shows numbered steps with a tile row of environments and an
  agents × environments checkbox grid; this file groups every check under its environment's card
  instead (§4.3), one row per agent's skill.
- Boards show one file at a time; the diff is a continuous stream (PLAN §6.7) with the same file
  header pinned. The stream adds a chevron to the header (viewed files collapse, §4.1), shows the
  side labels only over files that show lines, and makes the board's hunk header and its "Expand
  context below" bar one gap row that heads the hunk after it.
- `GlassReview` splits the title bar's label across two buttons (`api-server` / `⇄ working tree vs
  main`), groups the panel's files under directory headings, and fills the status bar's left with key
  hints. This file keeps the repo and target together on the switcher button with the source picker
  as `⇄ ▾`, shows each file's full path with its directory dimmed, and gives the status bar the
  location, the review and `Ctrl Enter finish review` (§4.1).
- The board draws the inline thread card at radius 8 and has no composer, no outdated card and
  no local sheet. This file uses the card radius (10, §1.5), adds the composer and the outdated
  card heading its file (§4.1), titles the panel's section "Open comments" rather than the
  board's capitals, and gives the local "Finish round" sheet a "Comments" column without the
  include checkboxes and validation rows `GlassSubmit` shows for a PR (§4.2).
- The boards show a first round only. This file adds what later rounds bring (§4.1, §4.4): the
  panel's Since round n | All changes toggle, the `round n` side label, "· moved" on a card, the
  comments list's "all changes" tag, the empty interdiff and the agent's ready banner.
- Board copy (file names, comment text, PR #482) is illustrative fixture content; the e2e fixtures
  (PLAN §14) reproduce it so screenshots compare 1:1.
- The boards' wallpaper is light pastel, which flatters the translucent surfaces. On Windows,
  gpui's Acrylic (`Blurred`) is an untinted blur of the desktop, so the token's alpha is the only
  tint, while Mica gets the OS's own tint as well. Over a contrasting desktop, Acrylic's `panel`
  and `chrome` text therefore drops below 4.5:1 (light Acrylic over a dark desktop: `fg3` on
  `panel` ≈2.5:1, `fg2` on `chrome` ≈4.4:1); dark Acrylic over a light desktop fares worse. Mica,
  the default, and Opaque hold. **Open:** keeping `fg3` at 4.5:1 over any desktop needs a
  `panel`/`chrome` alpha of about .85, which removes most of the material; the alternative is to
  accept it for the opt-in material. Until decided, the values stay the board's.
