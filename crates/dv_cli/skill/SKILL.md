---
name: dv-review
description: Address code review comments left in dv, the local diff reviewer, then hand the work back and wait for the verdict; or put your finished work up for review in dv with annotations that explain it. Use when asked to address review comments, to act on a dv review, when the reviewer left comments on your changes, to wait for the reviewer in a repository reviewed with dv, or to hand your work over for review in dv.
---

# Answering a dv review

dv is a desktop app in which a person, the reviewer, reads your changes as a diff and leaves
comment threads on their lines. The threads live in the repository (under `.git/dv/`); you read
and answer them with the `dv` command line, never by editing those files.

A review goes in rounds. The reviewer finishes a round by requesting changes, in comment threads
and in a summary for the round; you fix the code, answer every open thread and hand the review
back with `dv review done`; the reviewer then resolves threads, requests more changes or
approves.

## Rules

- Read threads as dv prints them (`dv comment list`, `dv comment show`, without `--json`): that
  text is written for you. Pass `--json` to `dv status`, `dv review show` and `dv review wait`,
  whose ids and round numbers you keep: one JSON document on stdout, `"schema": 1`.
- Pass `--author <agent>` on every command that writes, `<agent>` being your own name:
  `claude-code` for Claude Code, `codex` for Codex. Claude Code is recognised without it; say it
  anyway. The commands below write `<agent>` where your name goes.
- Pass `--model '<model>'` beside it, `<model>` being the model you run on, by the name your
  own instructions give it: `'Claude Fable 5.2'`, `'GPT 6.1 Sol'`. dv shows it beside what you
  write, with that model family's mark. Write the name as you were told it, never a guess:
  when nothing tells you which model you are, leave `--model` out.
- A number in front of a line of a thread's code is that line's number now. Act on those, never
  on the numbers in a thread's header ("moved from", "was", or the number of a thread marked
  `(old)`, which counts the file as it was before the change), nor on numbers you read before
  your last edit to that file: the file has moved on. In `--json` the same rows are `excerpt`:
  act on `anchor.current`, `current_code` and `excerpt`, never on `anchor.original`. For a
  thread whose `side` is `old`, `anchor.current` counts the file as it was before the change, not
  the file now: find its place by the numbered rows around its `>` rows in `excerpt`.
- Never run `dv comment resolve` or `dv comment reopen`: they are the reviewer's.
- If you disagree with a comment, reply with your reasons and still address the thread, with a
  note saying that you left the code as it is.
- Send text through `--body-file -` (a note through `--note-file -`), which survives any
  quoting: a quoted heredoc in a POSIX shell, a here-string piped in under PowerShell (steps 3
  and 4 show both). A short one-line note may go inline in single quotes; never in double
  quotes, where a POSIX shell runs whatever is in backticks and PowerShell reads a backtick as
  its escape character.
- Don't start a review (`dv review new`) unless the user asks you to (asking you to hand your
  work over for review in dv is asking: "Handing your work over", below).
- Commit only if the user's instructions say so.

## The loop

1. Find the review and where it stands:

```sh
dv status --json
```

`current` is the review the other commands act on: the open review of the work checked out
here. Keep its `id` for steps 4 and 5, and the `n` of the last of its `rounds` (the round the
reviewer finished) for step 4. When it is `null`, `reviews` lists the repository's open reviews,
each with its `worktree`: if one of them is clearly the review you were asked about, add
`--review <review-id>` to every command (or set `DV_REVIEW` to its id). If none is, or you can't
tell which, stop and tell the user; don't guess, and don't start a review.

`target.kind` says what the review compares, and so where your fixes show up for the reviewer:

- `working`: the files as they are on disk in the review's `worktree`. Your edits show as you
  make them. Work in that worktree.
- `branch`: the branch's commits, plus the files on disk in whichever worktree has that branch
  checked out (the review is `current` only in that worktree; checked out nowhere, it is the
  branch's commits alone). Your edits show there.
- `staged`: what is staged (`git add`ed), not the files on disk. Your edits show only once they
  are staged: ask the user before you run `git add`, and say so when you hand back.
- `commit`, `range`: fixed history. Edits can't change what this review shows: tell the user
  instead of handing back.

2. Read what the reviewer asked for. The summary of the round they finished is the `body` of the
   last of `current.rounds` (`round.body` in step 5's output): it speaks for the whole round
   and may ask for more than the threads do. Then the threads waiting for you:

```sh
dv comment list --status open
```

Each thread prints as a block: its header (`<thread-id>  <state>  <path>:<lines>`, with `(moved
from …)`, `(outdated, was …)` or `(old)` after it), the review's diff around its code now, then
the comment that opened it under its author (`reviewer <name>, round <n>:`), each reply under
its own author, and an `addressed note:` when there is one. A diff row is `<number> <mark> |
<line>`: the number is the line's number now on the review's new side (the file on disk for a
`working` review, what is staged for a `staged` one); mark `+` is a line the change added, `-`
one it removed (no number: it isn't there now), a blank mark an unchanged line; `>` in front
marks the lines the comment is on, and `⋮` lines left out. A thread marked `(old)` is on the
file as it was: place it by the numbered rows around its `>` rows. `as commented in round <n>:`
follows when the code has changed since the comment. When the code can't be found any more
(`outdated`), only that is shown, with the lines that stood around it: judge from it and the
comment, and find the place by those lines. When dv can tell where that code went since the
round the comment was written in, a line after them says so: `code changed since round <n>;
these lines replaced it (<path>:<lines>):`, then the numbered rows around the lines that
replaced it, those lines marked `>`: the comment is about what they replaced, so act on them
(`code changed since round <n>; lines <lines> of <path> replaced it` when dv can't show them);
or `code changed since round <n>; it was removed: it stood before line <k> of <path>` (`at the
end of <path>`), the header then saying `removed before <k>` (`removed at the end`). In
`--json` that is the thread's `trail` (`went`: `replaced` with `start_line`, `end_line`,
`code` and `excerpt`, or `removed` with `before_line`).

A thread on a whole file rather than on lines has the header `<thread-id>  <state>  <path>
(file)` and the line `on the whole file` where code would be: the comment is about the file as
a whole, under the name `<path>` it has now. In `--json` it has `file_level: true`, no
`anchor.current` and no `excerpt`. Once that file is gone from the review or deleted, the header
says `(file, outdated)` and the line says which.

A thread on a file the review doesn't change (the reviewer commented on code the change didn't
touch, or you reverted the file since) has `(not in the diff)` after its place, and its rows are
that file as it is now, every line unchanged; in `--json` it has `in_diff: false`. Act on it as
on any other thread: your edit brings the file into the review.

A thread opened by an agent (`agent <name>, round <n>:`; `author.kind` `agent` in `--json`) is
a question an agent asked the reviewer (`dv comment add`, below), not a request. While the
reviewer hasn't replied to it, it waits for them: leave it open, don't address it. Once they
have replied, act on the reply as on any comment.

A thread you addressed in an earlier round stays `addressed` when the reviewer only replies to
it; read the replies on those as well:

```sh
dv comment list --status addressed
```

3. For each thread that asks something of you, fix the code, then say what you did and mark the
   thread addressed. Once you have edited a file, the numbers you listed for it are stale: before
   you fix a thread in a file you have edited since you listed it, show it again (or find its
   code by its text):

```sh
dv comment show <thread-id>
dv comment reply <thread-id> --author <agent> --model '<model>' --body-file - <<'EOF'
Moved the cache lookup into `getUser` and covered the miss path with a test.
EOF
dv comment address <thread-id> --author <agent> --model '<model>' --note 'Cache lookup moved into getUser'
```

In PowerShell, pipe a here-string instead of the heredoc. In Windows PowerShell 5.1, set
`$OutputEncoding = [System.Text.UTF8Encoding]::new()` before you pipe anything to dv, or text
that isn't ASCII arrives mangled; and set `[Console]::OutputEncoding =
[System.Text.UTF8Encoding]::new()` before you read dv's output (best before step 1), or `⋮`
and any other text that isn't ASCII comes out garbled:

```powershell
@'
Renamed the helper as asked.
'@ | dv comment reply <thread-id> --author <agent> --model '<model>' --body-file -
```

4. When every open thread is answered and addressed (save your own questions still waiting for
   the reviewer), and what the round's summary asks is done, hand the review back by its id,
   naming the round you worked on, with a note on what you did, Markdown the reviewer reads in
   dv. `<n>` is that round's number: `round.n` from the `dv review wait` that gave you the
   round (step 5), or, when you started without waiting, the `n` you kept in step 1 (with no
   round finished yet, leave `--round` out):

```sh
dv review done <review-id> --round <n> --author <agent> --model '<model>' --note-file - <<'EOF'
- Moved the cache lookup into `getUser`
- Covered the miss path with a test
EOF
```

In PowerShell, pipe the note in as in step 3, with `--note-file -`.

If it exits 1 with the code `unseen_round`, the reviewer finished another round while you were
working, round `<n>` in the message, and nothing was handed back. Read that round as in step
2: its summary (the `body` of the last of `rounds` in `dv review show <review-id> --json`) and
its open threads. Act on them, then run the command above again with that round's number.

5. Wait for the reviewer, naming the review by its id (once approved or closed it is no longer
   open, and only its id finds it):

```sh
dv review wait <review-id> --timeout 600 --json
```

It returns once the reviewer finishes the round, or at once if they already have; `outcome`
says how:

- `changes_requested`: go back to step 2, starting with `round.body`; `round.n` is the round
  you now work on (step 4). `threads.open` counts the
  threads waiting, but at 0 the reviewer still wants something: it is in the summary, or in a
  reply on an addressed thread.
- `approved`: you are done. Commit only if the user's instructions say so.
- `closed`: the reviewer closed the review; stop.

Exit code 3 means the timeout passed first: run it again, or tell the user you are waiting.

## More commands

```sh
dv review show <review-id> --json
dv comment add --file src/app.ts --lines 10:12 --author <agent> --model '<model>' --body-file - <<'EOF'
Is this branch still reachable?
EOF
dv comment add --file src/app.ts --author <agent> --model '<model>' --body-file - <<'EOF'
Should this module be split in two?
EOF
```

`dv comment add` asks the reviewer something on lines of a changed file (`--side old` for the
lines as they were), or without `--lines` on the whole file (`--side old` for a deleted
file).

## Handing your work over

When the user asks you to put your work up for review in dv (or their instructions say to),
open the review yourself, explain your change in annotations beside the code, and hand it over:
the reviewer then reads your reasons with the code instead of spending a round asking for them.
Do this once the code is final. An annotation sits on the lines as they are when you write it,
and one whose code you rewrite afterwards can't be shown any more.

```sh
dv review new --author <agent> --model '<model>' --json
```

This opens the review of the working tree's changes, or resumes the one already open; add
`--branch` when your work is committed on the branch checked out (the working tree is then
clean, and there is nothing else to review). Keep `review.id`. If `created` is `false` and
`rounds` isn't empty, the review is already under way: work through it with the loop above, and
annotate only changes nobody asked for.

Annotate what the reviewer would otherwise have to ask about, on the lines it concerns, or
without `--lines` on why a whole file changed:

```sh
dv comment annotate --review <review-id> --author <agent> --model '<model>' --file src/app.ts --lines 10:12 --body-file - <<'EOF'
Reads through the cache first: this query ran on every request, and writes invalidate the entry.
EOF
dv comment annotate --review <review-id> --author <agent> --model '<model>' --file src/app.ts --body-file - <<'EOF'
Split out of `api.ts` so the cache can be tested without the server.
EOF
```

What makes a good annotation, and only those are worth writing (too many, and the reviewer
learns to skip them):

- A decision, not a description: why this approach over the obvious one, a trade-off you took,
  something you deliberately left out or out of scope, code that looks wrong but isn't, a
  behaviour a caller will notice, what the tests cover and what they don't.
- Never a restatement of the diff ("adds a function that…"), a mechanical change (renames,
  formatting, imports, generated files, lockfiles), or praise of your own work.
- Few: at most one per hunk, usually far fewer; a handful in a review of a dozen files. One to
  three sentences each.

The overview belongs in the hand-over's note, not in an annotation. Hand the review over as in
step 4, without `--round` (no round has been finished yet), then wait as in step 5:

```sh
dv review done <review-id> --author <agent> --model '<model>' --note-file - <<'EOF'
- Reads users through a new cache, invalidated on write
- Split the cache into its own module
EOF
```

An annotation waits on no one: `dv comment list --status open` leaves it out, and `--status
annotation` lists them. When the reviewer replies to one, it becomes an open thread you answer
like any other. To correct an annotation, or withdraw one that turned out wrong:

```sh
dv comment edit <thread-id> --author <agent> --model '<model>' --body-file - <<'EOF'
Reads through the cache first; writes invalidate the entry.
EOF
dv comment delete <thread-id> --author <agent> --model '<model>'
```

In a later round, explain a fix in its thread's reply and note, not in an annotation: an
annotation is for a change nobody asked for.

## Exit codes

- `0`: done.
- `1`: the operation failed. With `--json`, the JSON error's `code` says why (`not_found`,
  `not_allowed`, `rejected`, …); without it, there is no code, only `dv: <message>` on stderr,
  which says the same in words. `finished`: the review is approved or closed and takes no more changes.
  `unseen_round` (from `dv review done`): the reviewer finished a round you haven't read; read
  it, then run step 4 again with that round's number.
- `2`: the command line is wrong, or it doesn't say which review. With `ambiguous_review`,
  `error.candidates` lists the open reviews (each with its `worktree`): pick the one you were
  asked about with `--review <review-id>`. With `no_review` and no candidates there is no review
  to answer: tell the user.
- `3`: `dv review wait` timed out.
- `4`: dv couldn't reach the WSL distro the repository is in (WSL, the distro or dv's Linux
  build is missing; the message says which): tell the user.

Errors go to stderr as `dv: <message>`; only with `--json` does stdout also carry `{"schema":
1, "error": {"code", "message"}}`, the one place an error's `code` appears.
