---
name: dv-review
description: Respond to code-review feedback left in dv, the local diff viewer. Use when asked to address review comments, check what a reviewer wants, reply to or resolve review threads, or wait for new review activity in a repo reviewed with dv.
---

# Responding to dv reviews

dv is a local diff viewer / PR reviewer. A human reviews a diff of this
repo in the dv GUI and leaves line-anchored comment threads; those threads
are stored in the repo itself (under `.git/dv/reviews/`) and are fully
readable and writable through the `dv` CLI — no GUI needed. Your job in
this skill: find what the reviewer asked for, fix it, reply, resolve, and
optionally wait for their next pass.

## Detecting a dv review

A repo has dv reviews if `.git/dv/reviews/` exists (any `r-*.json` inside
is one review). Or just run the canonical query below — an empty result on
a repo with no store is not an error.

`dv` is on PATH in WSL/Linux if installed; on Windows the same commands
work via `dv.exe`. For a repo inside WSL when running from Windows, add
`--wsl <distro>:<posix-path>`; every command below also accepts
`--repo <path>` (default: current directory).

## The canonical query: what does the reviewer want?

```
dv comment list --status open --json
```

Returns `{"comments": [{"review_id", "comment": {...}}, ...]}` — every
open thread across every review in the repo. Each comment has:

- `path`, `side` (`"old"`/`"new"`), `start_line`..`end_line` (1-based,
  inclusive): where it anchors. `"new"` side lines are the *proposed*
  file; `"old"` side lines are the pre-change file.
- `body` (markdown), `author`, `replies` (each `{body, author}`).
- `id` (`c-...`): use this for reply/resolve.
- `blob_sha`: anchor fingerprint. If the file has changed since the
  comment was made, line numbers may have drifted — read the surrounding
  code rather than trusting the exact line blindly.

`--status open` is the filter you almost always want: resolved threads
are done. Add `--review <r-id>` to scope to one review.

## Etiquette: reply, then resolve

For each open comment:

1. Read the anchored code and the full thread (including replies).
2. Make the fix the reviewer asked for (or decide, with reasons, that no
   change is right).
3. **Reply first, then resolve** — a bare resolve with no reply reads as
   dismissive and hides what you did:

```
dv comment reply <c-id> --body "Fixed: <what you changed and where>" --json
dv comment resolve <c-id> --json
```

If you disagree with the feedback, reply with your reasoning and leave
the thread OPEN for the human to decide. Never resolve a thread you
didn't address. Use `dv comment unresolve <c-id>` if you resolved
something prematurely.

## Waiting for the reviewer (no polling)

After addressing everything, you can block until the reviewer (or anyone)
touches the review store again:

```
dv review wait --json --timeout 600
```

Exit `0` = something changed, with a report on stdout:
`{"wait": {"outcome": "changed", "changes": [{"review_id", "kind":
"created"|"deleted"|"updated", "comments_added": [...], "status_changed":
[{"id", "from", "to"}], "comments_updated": [...], "state_changed":
...}]}}`. Exit `3` = timeout (`{"wait":{"outcome":"timeout"}}`). Then
re-run the canonical query and respond to whatever is new. A watch loop
is: address open comments → `dv review wait` → repeat.

Optionally scope it: `dv review wait <r-id> --json`.

## Other commands you may need

- `dv review list --json` — every review in the repo (`{"reviews":
  [...]}`), newest first; each has `id`, `state` (`"draft"` or submitted),
  `source` (working tree / staged / range / commit), and its comments.
- `dv review show <r-id> --json` — one review with full threads.
- `dv comment add --file <path> --lines N|N:M [--side old|new] --body
  <text> --json` — leave a comment of your own (e.g. a question for the
  reviewer). Targets the newest draft review unless `--review <r-id>` is
  given.

## Contract notes

- Always pass `--json` when parsing output; the JSON key shapes above are
  stable. Errors print to stderr and, in `--json` mode, also
  `{"error": "..."}` on stdout.
- Exit codes: `0` success, `1` operation error (unknown id, no repo),
  `2` usage error, `3` timeout (`review wait` only).
- Comment ids (`c-...`), review ids (`r-...`) are globally unique within
  a repo; `reply`/`resolve`/`unresolve` find the comment across all
  reviews — no `--review` needed.
