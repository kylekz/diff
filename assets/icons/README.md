# Icons

Single-colour SVGs that `dv_ui` paints as masks tinted with the text colour (`dv_ui::icons`,
DESIGN §1.6). Every file here is listed in `crates/dv_ui/src/icons.rs`, which embeds it.

| File | Source |
|---|---|
| `folder.svg`, `folder-open.svg`, `search.svg`, `chevron-down.svg`, `chevron-right.svg`, `chevrons-up-down.svg`, `check.svg`, `arrow-left-right.svg`, `arrow-up.svg`, `arrow-down.svg`, `square-terminal.svg`, `list-filter.svg`, `triangle-alert.svg`, `git-branch.svg`, `git-commit-horizontal.svg`, `message-square.svg`, `file.svg`, `file-diff.svg`, `columns-2.svg`, `rows-2.svg`, `ellipsis.svg`, `copy.svg`, `binary.svg`, `package.svg`, `circle-check.svg`, `circle-minus.svg`, `circle.svg`, `circle-x.svg`, `monitor.svg`, `bot.svg`, `x.svg`, `message-square-plus.svg`, `play.svg`, `plus.svg`, `rotate-cw.svg` | [Lucide](https://lucide.dev), ISC licence (`LICENSE-LUCIDE`); `search`, the chevrons, `check`, the arrows, `git-branch`, `git-commit-horizontal`, `message-square`, `file`, `copy`, `package`, `x`, `message-square-plus`, `play` and `plus` derive from Feather, MIT, same file |
| `window-minimize.svg`, `window-maximize.svg`, `window-restore.svg`, `window-close.svg` | Windows caption glyphs on a 10 grid at a 1px stroke |
| `dv-mark.svg` | dv's mark (design canvas, `GlassReview` title bar) |
| `model-claude.svg`, `model-openai.svg`, `model-gemini.svg`, `model-kimi.svg`, `model-deepseek.svg`, `model-zai.svg`, `model-qwen.svg` | The model families' marks (DESIGN §1.6), from [lobe-icons](https://github.com/lobehub/lobe-icons) (`@lobehub/icons-static-svg` 1.102.0, its one-colour `claude`, `openai`, `gemini`, `kimi`, `deepseek`, `zai` and `qwen`; MIT, `LICENSE-LOBE-ICONS`), their paths unchanged in a plain 24-grid `<svg>`. The marks themselves are their makers' trademarks, used only to name whose model wrote something |

The Lucide icons keep Lucide's 24 grid with the stroke scaled to match the design canvas, which
draws its icons on a 16 grid at 1.5–1.6 (DESIGN §1.6): `2.25` for 1.5 and `2.4` for 1.6, so they
come out at the same weight at 14–16px. The small glyphs the canvas draws heavier keep its weight
the same way: `check` at `3.4` (the viewed checkbox's 2.4), `message-square` at `3` (the
comment badge's 2), `circle-check` at `2.7` (the resolved chip's 1.8) and `circle-minus`, `circle` and `circle-x` at
`2.55` (the verdict glyph's 1.7); `plus` is drawn at `3` so it holds up at 12px on the accent.
