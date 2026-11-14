//! The `dv-review` skill (PLAN §10.5): its frontmatter, and every command line it shows works
//! exactly as written: each parses, and the loop it teaches runs against a fixture repo.

use clap::Parser as _;
use dv_core::review::{ReviewStatus, ThreadState};
use dv_core::target::Target;
use pretty_assertions::assert_eq;

use super::*;
use crate::Cli;
use crate::skill::{SKILL_MD, SKILL_NAME};

/// A command line from one of the skill's fenced blocks, with the text it pipes in.
#[derive(Debug, Clone)]
struct Shown {
    line: String,
    stdin: String,
}

/// Every command line the skill's fenced blocks show, in order: `dv …` lines, a heredoc after
/// `<<'EOF'`, and PowerShell's `@'…'@ | dv …`. Any other line in a block fails the test, so a
/// new shape is checked here before it ships.
fn shown() -> Vec<Shown> {
    let mut commands = Vec::new();
    let mut lines = SKILL_MD.lines();
    while let Some(line) = lines.next() {
        if !line.starts_with("```") {
            continue;
        }
        let mut block = Vec::new();
        for line in lines.by_ref() {
            if line.starts_with("```") {
                break;
            }
            block.push(line);
        }
        let mut block = block.into_iter();
        while let Some(line) = block.next() {
            if let Some(command) = line.strip_suffix(" <<'EOF'") {
                let stdin: Vec<&str> = block.by_ref().take_while(|line| *line != "EOF").collect();
                commands.push(Shown {
                    line: command.to_string(),
                    stdin: stdin.join("\n") + "\n",
                });
            } else if line == "@'" {
                let mut stdin = Vec::new();
                let command = loop {
                    let line = block.next().expect("a here-string ends");
                    if let Some(command) = line.strip_prefix("'@ | ") {
                        break command;
                    }
                    stdin.push(line);
                };
                commands.push(Shown {
                    line: command.to_string(),
                    stdin: stdin.join("\n") + "\n",
                });
            } else if line.starts_with("dv ") {
                commands.push(Shown {
                    line: line.to_string(),
                    stdin: String::new(),
                });
            } else {
                panic!("a line the skill test doesn't know: {line:?}");
            }
        }
    }
    commands
}

/// `line` split into words as a POSIX shell does for these lines: blanks separate words,
/// quotes group them.
fn words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.get_or_insert_default().push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                word.get_or_insert_default();
            }
            (None, ' ') => words.extend(word.take()),
            (None, c) => word.get_or_insert_default().push(c),
        }
    }
    assert_eq!(quote, None, "unbalanced quotes in {line:?}");
    words.extend(word);
    words
}

/// The note the thread's last state change carried.
fn answered_note<'r>(review: &'r dv_core::review::Review, id: &ThreadId) -> Option<&'r str> {
    review
        .thread(id)
        .and_then(|thread| thread.last_change.as_ref())
        .and_then(|change| change.note.as_deref())
}

/// The model the test's agents say they run on, where the skill writes `<model>`.
const MODEL: &str = "Claude Fable 5.2";

/// `shown`'s arguments after `dv`, its placeholders filled in (`<agent>`: the agent's name, as
/// the skill tells it to write its own; `<model>`: [`MODEL`]; `<n>`: the round the agent
/// worked on).
fn args(shown: &Shown, thread: &str, review: &str, agent: &str, round: u32) -> Vec<String> {
    let words = words(
        &shown
            .line
            .replace("<thread-id>", thread)
            .replace("<review-id>", review)
            .replace("<agent>", agent)
            .replace("<model>", MODEL)
            .replace("<n>", &round.to_string()),
    );
    assert!(
        !words.iter().any(|word| word.contains('<')),
        "a placeholder the test doesn't fill: {:?}",
        shown.line
    );
    assert_eq!(words[0], "dv");
    words[1..].to_vec()
}

#[test]
fn the_skill_is_named_and_says_when_to_use_it() {
    assert!(
        SKILL_MD.starts_with(&format!("---\nname: {SKILL_NAME}\ndescription: ")),
        "frontmatter first"
    );
    let frontmatter = SKILL_MD.split("\n---\n").next().unwrap();
    let description = frontmatter
        .lines()
        .find_map(|line| line.strip_prefix("description: "))
        .unwrap();
    for trigger in [
        "address review comments",
        "dv review",
        "the reviewer left comments",
        "hand your work over for review",
    ] {
        assert!(
            description.contains(trigger),
            "{trigger:?} in {description:?}"
        );
    }
    for rule in [
        "--author <agent>",
        "--model '<model>'",
        "target.kind",
        "git add",
        "--json",
        "--body-file -",
        "--note-file -",
        "single quotes",
        "author.kind",
        "dv comment resolve",
        "anchor.current",
        "round.body",
        "dv review new",
        "PowerShell",
        "unseen_round",
        "dv comment annotate",
        "Do this once the code is final",
    ] {
        assert!(SKILL_MD.contains(rule), "{rule:?}");
    }
}

#[test]
fn every_command_the_skill_shows_parses() {
    let shown = shown();
    let lines: Vec<&str> = shown.iter().map(|shown| shown.line.as_str()).collect();
    assert_eq!(
        lines,
        [
            "dv status --json",
            "dv comment list --status open",
            "dv comment list --status addressed",
            "dv comment show <thread-id>",
            "dv comment reply <thread-id> --author <agent> --model '<model>' --body-file -",
            "dv comment address <thread-id> --author <agent> --model '<model>' --note 'Cache lookup moved into getUser'",
            "dv comment reply <thread-id> --author <agent> --model '<model>' --body-file -",
            "dv review done <review-id> --round <n> --author <agent> --model '<model>' --note-file -",
            "dv review wait <review-id> --timeout 600 --json",
            "dv review show <review-id> --json",
            "dv comment add --file src/app.ts --lines 10:12 --author <agent> --model '<model>' --body-file -",
            "dv comment add --file src/app.ts --author <agent> --model '<model>' --body-file -",
            "dv review new --author <agent> --model '<model>' --json",
            "dv comment annotate --review <review-id> --author <agent> --model '<model>' --file src/app.ts --lines 10:12 --body-file -",
            "dv comment annotate --review <review-id> --author <agent> --model '<model>' --file src/app.ts --body-file -",
            "dv review done <review-id> --author <agent> --model '<model>' --note-file -",
            "dv comment edit <thread-id> --author <agent> --model '<model>' --body-file -",
            "dv comment delete <thread-id> --author <agent> --model '<model>'",
        ],
        "the test below runs each of these in turn; update it with the skill"
    );
    for shown in &shown {
        let args = args(
            shown,
            "t-01k6aaaaaaaaaaaaaaaaaaaaaa",
            "r-01k6aaaaaaaaaaaaaaaaaaaaaa",
            "claude-code",
            1,
        );
        if let Err(error) = Cli::try_parse_from(std::iter::once("dv".to_string()).chain(args)) {
            panic!("{:?}: {error}", shown.line);
        }
    }
}

#[test]
fn the_loop_the_skill_teaches_runs() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    fixture.thread(&review, "src/app.ts", 10, 10, "Why a cache?");
    fixture.finish(&review.id, Verdict::RequestChanges);
    let shown = shown();
    // Each agent fills `<agent>` with its own name, and `<n>` with the round it worked on.
    let run_with = |index: usize, thread: &str, agent: &str, round: u32| {
        let shown = &shown[index];
        let args = args(shown, thread, review.id.as_str(), agent, round);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let ran = fixture.run_stdin(&args, &shown.stdin);
        assert_eq!(ran.exit, Exit::Ok, "{}: {}", shown.line, ran.stderr);
        ran
    };
    let run = |index: usize, thread: &str| run_with(index, thread, "claude-code", 0);
    // The same command with `--json`, where the test reads values the text prints too.
    let run_json = |index: usize, thread: &str| {
        let shown = &shown[index];
        let mut args = args(shown, thread, review.id.as_str(), "claude-code", 0);
        args.push("--json".to_string());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let ran = fixture.run_stdin(&args, &shown.stdin);
        assert_eq!(ran.exit, Exit::Ok, "{} --json: {}", shown.line, ran.stderr);
        ran.json()
    };
    // The thread's code row as the text prints it: its line now, marked.
    let commented_row = format!(">10 + | {}", edited_lines()[9]);
    let note = || {
        fixture
            .load(&review.id)
            .status_change
            .and_then(|change| change.note)
    };
    let step4 = "- Moved the cache lookup into `getUser`\n- Covered the miss path with a test";

    // 1. The review this work is under, and the round the reviewer finished.
    let json = run(0, "").json();
    assert_eq!(json["current"]["id"], review.id.as_str());
    let worked_on = json["current"]["rounds"][0]["n"].as_u64().unwrap() as u32;
    assert_eq!(worked_on, 1);
    // 2. The open threads, as text: the review's line, then each thread's block, whose header
    // starts with the id the agent takes, and whose `>` row is the line commented on, numbered
    // as it is now. Nothing addressed yet.
    let listed = run(1, "");
    let lines = listed.lines();
    assert!(
        lines[0].starts_with(review.id.as_str()),
        "{}",
        listed.stdout
    );
    let thread = lines[2].split_whitespace().next().unwrap().to_string();
    assert_eq!(lines[2], format!("{thread}  open  src/app.ts:10"));
    assert!(lines.contains(&commented_row.as_str()), "{}", listed.stdout);
    assert_eq!(lines.last().copied(), Some("    Why a cache?"));
    let json = run_json(1, "");
    assert_eq!(json["threads"].as_array().unwrap().len(), 1);
    assert_eq!(json["threads"][0]["id"], thread.as_str());
    assert_eq!(json["threads"][0]["current_code"], edited_lines()[9]);
    let id = ThreadId::parse(&thread).unwrap();
    assert_eq!(run(2, "").lines(), ["no matching threads"]);
    // 3. Read it again, reply and address.
    let shown_again = run(3, &thread);
    assert!(
        shown_again.lines().contains(&commented_row.as_str()),
        "{}",
        shown_again.stdout
    );
    let json = run_json(3, &thread);
    assert_eq!(json["thread"]["anchor"]["current"]["start_line"], 10);
    run(4, &thread);
    run(5, &thread);
    // 4. Hand back round 1, by the review's id. The reviewer finished round 2 while the agent
    // worked, so the hand-back is refused, naming the round and the command to run again.
    fixture.finish(&review.id, Verdict::RequestChanges);
    let refused = {
        let args = args(
            &shown[7],
            &thread,
            review.id.as_str(),
            "claude-code",
            worked_on,
        );
        let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
        args.push("--json");
        fixture.run_stdin(&args, &shown[7].stdin)
    };
    let error = refused.error(Exit::Failure, "unseen_round");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains(&format!("dv review done {} --round 2", review.id)),
        "{error}"
    );
    assert_eq!(
        fixture.load(&review.id).status,
        ReviewStatus::ChangesRequested
    );
    // Read, the same command with round 2 hands back.
    run_with(7, &thread, "claude-code", 2);
    let loaded = fixture.load(&review.id);
    assert_eq!(loaded.status, ReviewStatus::Ready);
    assert_eq!(note().as_deref(), Some(step4));
    assert_eq!(
        answered_note(&loaded, &id),
        Some("Cache lookup moved into getUser")
    );
    let answered = loaded.thread(&id).unwrap();
    assert_eq!(answered.state, ThreadState::Addressed);
    // What it wrote carries the model it named, quoted as the skill writes it.
    assert_eq!(
        answered.replies[0].author,
        Author::agent("claude-code").with_model(MODEL)
    );
    assert_eq!(
        answered.replies[0].body,
        "Moved the cache lookup into `getUser` and covered the miss path with a test."
    );

    // The reviewer replies on the addressed thread and asks for more in the round's summary;
    // 5. the wait returns with it, and step 2 finds both.
    ops::reply(
        &fixture.store(),
        &review.id,
        &id,
        "Also cover the hit path.",
        reviewer(),
        None,
    )
    .unwrap();
    ops::finish_round(
        &fixture.store(),
        &fixture.git(),
        &review.id,
        Some(Verdict::RequestChanges),
        Some("Add a test for square.".to_string()),
        &reviewer(),
    )
    .unwrap();
    let json = run(8, &thread).json();
    assert_eq!(json["outcome"], "changes_requested");
    assert_eq!(json["round"]["body"], "Add a test for square.");
    assert_eq!(json["threads"]["open"], 0);
    let worked_on = json["round"]["n"].as_u64().unwrap() as u32;
    assert_eq!(worked_on, 3);
    assert_eq!(run(1, "").lines(), ["no matching threads"]);
    let addressed = run(2, "");
    let lines = addressed.lines();
    assert_eq!(lines[2], format!("{thread}  addressed  src/app.ts:10"));
    let at = lines
        .iter()
        .rposition(|line| *line == "reviewer dv fixture:")
        .unwrap_or_else(|| panic!("the reviewer's reply in {}", addressed.stdout));
    assert_eq!(lines[at + 1], "    Also cover the hit path.");
    let json = run_json(2, "");
    assert_eq!(json["threads"][0]["id"], thread.as_str());
    let replies = json["threads"][0]["replies"].as_array().unwrap().clone();
    assert_eq!(replies.last().unwrap()["body"], "Also cover the hit path.");

    // The other commands, on the open review: the PowerShell reply, as Codex.
    run_with(6, &thread, "codex", 0);
    assert_eq!(run(9, &thread).json()["review"]["id"], review.id.as_str());
    run(10, &thread);
    let loaded = fixture.load(&review.id);
    let asked = loaded.live_threads().last().unwrap();
    assert_eq!(asked.body, "Is this branch still reachable?");
    assert_eq!(
        (
            asked.anchor.start_line,
            asked.anchor.end_line,
            asked.anchor.round
        ),
        (10, 12, 4)
    );
    // The same without `--lines`: a question on the whole file, which reads as one.
    let asked_file = run(11, &thread);
    let added = asked_file.lines()[0].to_string();
    assert!(added.ends_with("  src/app.ts (file)"), "{added}");
    let loaded = fixture.load(&review.id);
    let on_file = loaded.live_threads().last().unwrap();
    assert_eq!(on_file.body, "Should this module be split in two?");
    assert!(on_file.anchor.is_file_level(), "{:?}", on_file.anchor);
    let shown = fixture.run(&["comment", "show", on_file.id.as_str()]);
    assert_eq!(
        shown.lines()[2..4],
        [
            format!("{}  open  src/app.ts (file)", on_file.id).as_str(),
            "on the whole file"
        ]
    );
    let replies = &loaded.thread(&id).unwrap().replies;
    assert_eq!(replies.last().unwrap().body, "Renamed the helper as asked.");
    assert_eq!(
        replies.last().unwrap().author,
        Author::agent("codex").with_model(MODEL)
    );
    assert_eq!(loaded.status, ReviewStatus::InProgress);
    // 4. Hand round 3 back, with a Markdown list for a note, piped in.
    run_with(7, &thread, "claude-code", worked_on);
    assert_eq!(fixture.load(&review.id).status, ReviewStatus::Ready);
    assert_eq!(note().as_deref(), Some(step4));

    // The reviewer approves; the same wait says so.
    fixture.finish(&review.id, Verdict::Approve);
    assert_eq!(run(8, &thread).json()["outcome"], "approved");
}

/// The command lines of the skill's "Handing your work over" section, by their place in
/// [`shown`].
const NEW: usize = 12;
const ANNOTATE_LINES: usize = 13;
const ANNOTATE_FILE: usize = 14;
const HAND_OVER: usize = 15;
const EDIT: usize = 16;
const DELETE: usize = 17;

#[test]
fn the_hand_over_the_skill_teaches_runs() {
    let fixture = Fixture::new();
    let shown = shown();
    let run = |index: usize, thread: &str, review: &str| {
        let shown = &shown[index];
        let args = args(shown, thread, review, "codex", 0);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let ran = fixture.run_stdin(&args, &shown.stdin);
        assert_eq!(ran.exit, Exit::Ok, "{}: {}", shown.line, ran.stderr);
        ran
    };

    // The review, opened by the agent.
    let json = run(NEW, "", "").json();
    assert_eq!(json["created"], true);
    let review = ReviewId::parse(json["review"]["id"].as_str().unwrap()).unwrap();
    assert_eq!(json["review"]["rounds"], serde_json::json!([]));

    // Annotations on lines and on the whole file, waiting on no one.
    run(ANNOTATE_LINES, "", review.as_str());
    run(ANNOTATE_FILE, "", review.as_str());
    let loaded = fixture.load(&review);
    let [on_lines, on_file] = [&loaded.threads[0], &loaded.threads[1]];
    assert_eq!(on_lines.state, ThreadState::Annotation);
    assert_eq!(on_lines.author, Author::agent("codex").with_model(MODEL));
    assert_eq!(
        (on_lines.anchor.start_line, on_lines.anchor.end_line),
        (10, 12)
    );
    assert!(on_file.anchor.is_file_level());
    let (on_lines, on_file) = (on_lines.id.clone(), on_file.id.clone());
    assert_eq!(
        fixture
            .run(&["comment", "list", "--status", "open"])
            .lines(),
        ["no matching threads"]
    );

    // Corrected, and withdrawn.
    run(EDIT, on_lines.as_str(), review.as_str());
    run(DELETE, on_file.as_str(), review.as_str());
    let loaded = fixture.load(&review);
    assert_eq!(
        loaded.thread(&on_lines).unwrap().body,
        "Reads through the cache first; writes invalidate the entry."
    );
    assert!(loaded.thread(&on_file).unwrap().is_deleted());

    // Handed over with the overview, before any round was finished.
    run(HAND_OVER, "", review.as_str());
    let loaded = fixture.load(&review);
    assert_eq!(loaded.status, ReviewStatus::Ready);
    assert_eq!(
        loaded
            .status_change
            .and_then(|change| change.note)
            .as_deref(),
        Some(
            "- Reads users through a new cache, invalidated on write\n- Split the cache into its own module"
        )
    );

    // A review already under way is resumed, rounds and all: the loop, not a hand-over.
    fixture.finish(&review, Verdict::RequestChanges);
    let json = run(NEW, "", "").json();
    assert_eq!(json["created"], false);
    assert_eq!(json["review"]["id"], review.as_str());
    assert_eq!(json["review"]["rounds"].as_array().unwrap().len(), 1);
}
