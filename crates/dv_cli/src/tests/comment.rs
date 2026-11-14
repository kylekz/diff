//! `dv comment list` and `dv comment show`: threads placed where their code stands now
//! (docs/PLAN.md §8, §10.1), and threads found by id across reviews (PLAN §10.2).

use dv_core::review::{AnchorSide, ItemId, ThreadState, ops};
use dv_core::target::Target;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use super::*;

fn thread_of<'v>(json: &'v Value, id: &ThreadId) -> &'v Value {
    json["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["id"] == id.as_str())
        .unwrap_or_else(|| panic!("no thread {id} in {json}"))
}

/// `link`, a new link to the folder `target`: a symbolic link, or on Windows a junction, which
/// needs no privilege.
fn link_dir(target: &std::path::Path, link: &std::path::Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        let made = std::process::Command::new("cmd")
            .arg("/c")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(made.status.success(), "mklink /J: {made:?}");
    }
}

fn listed(fixture: &Fixture, args: &[&str]) -> Vec<String> {
    let mut line = vec!["comment", "list", "--json"];
    line.extend_from_slice(args);
    let json = fixture.run(&line).ok_json();
    json["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn threads_are_placed_where_their_code_is_now() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    let edited = fixture.thread(&review, "src/app.ts", 10, 10, "Why a cache?");
    let interface = fixture.thread(&review, "src/app.ts", 4, 7, "Add an email.");
    let format = fixture.thread(&review, "src/app.ts", 18, 18, "Trim earlier.");

    let json = fixture.run(&["comment", "list", "--json"]).ok_json();
    assert_eq!(json["review_id"], review.id.as_str());
    let ids: Vec<&str> = json["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [edited.as_str(), interface.as_str(), format.as_str()]);
    for thread in json["threads"].as_array().unwrap() {
        assert_eq!(thread["placement"], "unchanged");
        assert_eq!(thread["outdated"], false);
        assert_eq!(
            thread["anchor"]["current"]["start_line"],
            thread["anchor"]["original"]["start_line"]
        );
    }

    // The agent inserts two lines on top and deletes formatName.
    fixture.edit_app(&moved_lines());
    let json = fixture.run(&["comment", "list", "--json"]).ok_json();
    let thread = thread_of(&json, &interface);
    assert_eq!(
        thread["anchor"],
        json!({
            "original": {"round": 1, "path": "src/app.ts", "side": "new", "start_line": 4, "end_line": 7},
            "current": {"path": "src/app.ts", "side": "new", "start_line": 6, "end_line": 9},
        })
    );
    assert_eq!(thread["placement"], "moved");
    assert_eq!(thread["outdated"], false);
    assert_eq!(
        thread["code"],
        "export interface User {\n  id: string;\n  name: string;\n}"
    );
    assert_eq!(thread["current_code"], thread["code"]);
    assert_eq!(
        thread_of(&json, &edited)["anchor"]["current"]["start_line"],
        12
    );

    let gone = thread_of(&json, &format);
    assert_eq!(gone["placement"], "outdated");
    assert_eq!(gone["outdated"], true);
    assert_eq!(gone["path"], "src/app.ts");
    assert_eq!(gone["anchor"]["current"], json!(null));
    assert_eq!(gone["current_code"], json!(null));
    assert_eq!(gone["code"], "  return user.name.trim();");

    // Filters: by state, by outdated, by file.
    assert_eq!(
        listed(&fixture, &["--status", "outdated"]),
        [format.to_string()]
    );
    assert_eq!(listed(&fixture, &["--status", "open"]).len(), 3);
    assert_eq!(
        listed(&fixture, &["--status", "addressed"]),
        Vec::<String>::new()
    );
    assert_eq!(listed(&fixture, &["--file", "src/app.ts"]).len(), 3);
    assert_eq!(listed(&fixture, &["--file", "./src\\app.ts"]).len(), 3);
    let absolute = fixture.path().join("src").join("app.ts");
    assert_eq!(
        listed(&fixture, &["--file", absolute.to_str().unwrap()]).len(),
        3,
        "an absolute path inside the work tree"
    );
    // The same file through a link to the work tree (a junction on Windows), which git's root
    // never has: the path counts as the file system resolves it, as one through a Windows 8.3
    // short name does.
    let links = tempfile::tempdir().unwrap();
    let link = links.path().join("linked");
    link_dir(fixture.path(), &link);
    let through_link = link.join("src").join("app.ts");
    assert_eq!(
        listed(&fixture, &["--file", through_link.to_str().unwrap()]).len(),
        3,
        "an absolute path through a link to the work tree"
    );
    assert_eq!(
        listed(&fixture, &["--file", "README.md"]),
        Vec::<String>::new()
    );

    let ran = fixture.run(&["comment", "list", "--oneline"]);
    assert_eq!(
        ran.lines(),
        [
            format!("{edited}  open  src/app.ts:12 (moved from 10)  Why a cache?"),
            format!("{interface}  open  src/app.ts:6-9 (moved from 4-7)  Add an email."),
            format!("{format}  open  src/app.ts (outdated, was 18)  Trim earlier."),
        ]
    );
    for oneline in [&[][..], &["--oneline"]] {
        let mut args = vec!["comment", "list", "--status", "resolved"];
        args.extend_from_slice(oneline);
        assert_eq!(fixture.run(&args).lines(), ["no matching threads"]);
    }
}

/// `dv comment list`'s text (PLAN §10.3): the review's line, then a block per thread with the
/// diff around its code now, every number a line of the file as it is now.
#[test]
fn the_list_shows_each_thread_with_the_diff_around_its_code_now() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    let interface = fixture.thread(&review, "src/app.ts", 4, 7, "Add an email.");
    let old = fixture.anchor("src/app.ts", AnchorSide::Old, 10, 10);
    let old = ops::add_thread(
        &fixture.store(),
        &review.id,
        old,
        "Was this slow?",
        reviewer(),
    )
    .unwrap();
    let returned = fixture.thread(&review, "src/app.ts", 14, 14, "Return the email too.");
    let format = fixture.thread(&review, "src/app.ts", 18, 18, "Trim earlier.");
    ops::reply(
        &fixture.store(),
        &review.id,
        &old,
        "No: the query is indexed.\n\nThe cache is for the burst.",
        agent(),
        None,
    )
    .unwrap();
    fixture.finish(&review.id, Verdict::RequestChanges);
    // The agent inserts two lines on top, returns the email and deletes formatName.
    let mut lines = moved_lines();
    lines[15] = "  return { id: row.id, name: row.name, email: row.email };".to_string();
    fixture.edit_app(&lines);

    let ran = fixture.run(&["comment", "list", "--status", "open"]);
    assert_eq!(ran.exit, Exit::Ok, "{}", ran.stderr);
    let query = r#"const row = await db.query("select * from users where id = $1", [id]);"#;
    let cached = r#"const row = (await cache.get(id)) ?? (await db.query("select 1", [id]));"#;
    let expected = [
        format!(
            "{}  working tree vs main  changes_requested  round 2  4 open, 0 addressed, 0 resolved",
            review.id
        ),
        String::new(),
        // Moved: numbered as the file is now, the removed line under 11 where it stood.
        format!("{interface}  open  src/app.ts:6-9 (moved from 4-7)"),
        r#"  3   | import { db } from "./db";"#.to_string(),
        r#"  4   | import { log } from "./log";"#.to_string(),
        "  5   |".to_string(),
        "> 6   | export interface User {".to_string(),
        "> 7   |   id: string;".to_string(),
        "> 8   |   name: string;".to_string(),
        "> 9   | }".to_string(),
        " 10   |".to_string(),
        " 11   | export async function getUser(id: string): Promise<User> {".to_string(),
        format!("    - |   {query}"),
        format!(" 12 + |   {cached}"),
        "reviewer dv fixture, round 1:".to_string(),
        "    Add an email.".to_string(),
        String::new(),
        // On the old side: the removed line it is on, between the lines as they are now.
        format!("{old}  open  src/app.ts:10 (old)"),
        "  9   | }".to_string(),
        " 10   |".to_string(),
        " 11   | export async function getUser(id: string): Promise<User> {".to_string(),
        format!(">   - |   {query}"),
        format!(" 12 + |   {cached}"),
        " 13   |   if (!row) {".to_string(),
        r#" 14   |     throw new Error("not found");"#.to_string(),
        " 15   |   }".to_string(),
        "reviewer dv fixture, round 1:".to_string(),
        "    Was this slow?".to_string(),
        "agent claude-code:".to_string(),
        "    No: the query is indexed.".to_string(),
        String::new(),
        "    The cache is for the burst.".to_string(),
        String::new(),
        // Edited where it moved to: its scope above, and the line as it was commented on.
        format!("{returned}  open  src/app.ts:16 (moved from 14)"),
        " 11   | export async function getUser(id: string): Promise<User> {".to_string(),
        "  ⋮".to_string(),
        " 13   |   if (!row) {".to_string(),
        r#" 14   |     throw new Error("not found");"#.to_string(),
        " 15   |   }".to_string(),
        "    - |   return { id: row.id, name: row.name };".to_string(),
        ">16 + |   return { id: row.id, name: row.name, email: row.email };".to_string(),
        " 17   | }".to_string(),
        " 18   |".to_string(),
        "    - | export function formatName(user: User): string {".to_string(),
        "    - |   return user.name.trim();".to_string(),
        "    - | }".to_string(),
        " 19   | // end".to_string(),
        "as commented in round 1:".to_string(),
        "> |   return { id: row.id, name: row.name };".to_string(),
        "reviewer dv fixture, round 1:".to_string(),
        "    Return the email too.".to_string(),
        String::new(),
        // Outdated: the lines as written, with what stood around them, unnumbered, then where
        // they went since the round (PLAN §8: removed, before what is line 19 now).
        format!("{format}  open  src/app.ts (outdated, was 18, removed before 19)"),
        "as commented in round 1 (not found in the file now):".to_string(),
        "  | }".to_string(),
        "  |".to_string(),
        "  | export function formatName(user: User): string {".to_string(),
        "> |   return user.name.trim();".to_string(),
        "  | }".to_string(),
        "  | // end".to_string(),
        "code changed since round 1; it was removed: it stood before line 19 of src/app.ts"
            .to_string(),
        "reviewer dv fixture, round 1:".to_string(),
        "    Trim earlier.".to_string(),
    ];
    assert_eq!(ran.lines(), expected, "{}", ran.stdout);
    assert!(
        ran.stdout.lines().all(|line| line == line.trim_end()),
        "no trailing whitespace"
    );

    // The JSON carries the same rows; `code_context` only where there are none.
    let json = fixture
        .run(&["comment", "list", "--status", "open", "--json"])
        .ok_json();
    let rows = |id: &ThreadId| {
        let block = expected
            .iter()
            .skip_while(|line| !line.starts_with(id.as_str()))
            .skip(1)
            .take_while(|line| !line.starts_with("reviewer ") && !line.starts_with("as "))
            .cloned()
            .collect::<Vec<_>>();
        block.join("\n")
    };
    for id in [&interface, &old, &returned] {
        let thread = thread_of(&json, id);
        assert_eq!(thread["excerpt"], rows(id), "{id}");
        assert_eq!(thread["code_context"], json!(null), "{id}");
    }
    let gone = thread_of(&json, &format);
    assert_eq!(gone["excerpt"], json!(null));
    assert_eq!(
        gone["code_context"],
        json!({
            "before": ["}", "", "export function formatName(user: User): string {"],
            "after": ["}", "// end"],
        })
    );
}

#[test]
fn a_thread_shows_in_full() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    let id = fixture.thread(
        &review,
        "src/app.ts",
        4,
        7,
        "Add an **email**.\n\nIt's needed.",
    );
    let store = fixture.store();
    ops::reply(&store, &review.id, &id, "On it.", agent(), None).unwrap();
    let dropped = ops::reply(&store, &review.id, &id, "Oops.", agent(), None).unwrap();
    ops::delete_item(&store, &review.id, &ItemId::Comment(dropped), &agent()).unwrap();
    ops::set_thread_state(
        &store,
        &review.id,
        &id,
        ThreadState::Addressed,
        Some("Added `email`."),
        agent(),
        None,
    )
    .unwrap();
    fixture.edit_app(&moved_lines());

    let loaded = fixture.store().load(&review.id).unwrap().review;
    let thread = loaded.thread(&id).unwrap();
    let json = fixture
        .run(&["comment", "show", id.as_str(), "--json"])
        .ok_json();
    assert_eq!(
        json["thread"],
        json!({
            "id": id.as_str(),
            "review_id": review.id.as_str(),
            "state": "addressed",
            "annotation": false,
            "placement": "moved",
            "outdated": false,
            "in_diff": true,
            "file_level": false,
            "path": "src/app.ts",
            "side": "new",
            "anchor": {
                "original": {"round": 1, "path": "src/app.ts", "side": "new", "start_line": 4, "end_line": 7},
                "current": {"path": "src/app.ts", "side": "new", "start_line": 6, "end_line": 9},
            },
            "code": "export interface User {\n  id: string;\n  name: string;\n}",
            "code_context": null,
            "current_code": "export interface User {\n  id: string;\n  name: string;\n}",
            "excerpt": moved_interface_rows().join("\n"),
            "trail": null,
            "body": "Add an **email**.\n\nIt's needed.",
            "author": {"kind": "reviewer", "name": "dv fixture", "model": null},
            "created_at": thread.created_at,
            "note": "Added `email`.",
            "replies": [{
                "id": thread.replies[0].id.as_str(),
                "author": {"kind": "agent", "name": "claude-code", "model": null},
                "body": "On it.",
                "created_at": thread.replies[0].created_at,
            }],
        })
    );

    // The review's line, then the block: every number a line as the file is now (the code was
    // written on 4-7; its rows are 6-9), what anyone wrote indented, blank lines empty.
    let ran = fixture.run(&["comment", "show", id.as_str()]);
    let mut expected = vec![
        format!(
            "{}  working tree vs main  open  round 1  0 open, 1 addressed, 0 resolved",
            review.id
        ),
        String::new(),
        format!("{id}  addressed  src/app.ts:6-9 (moved from 4-7)"),
    ];
    expected.extend(moved_interface_rows());
    expected.extend(
        [
            "reviewer dv fixture, round 1:",
            "    Add an **email**.",
            "",
            "    It's needed.",
            "agent claude-code:",
            "    On it.",
            "addressed note:",
            "    Added `email`.",
        ]
        .map(str::to_string),
    );
    assert_eq!(ran.lines(), expected);
}

/// The excerpt of the `User` interface (lines 4–7 as written) in [`moved_lines`]: lines 6–9.
fn moved_interface_rows() -> Vec<String> {
    [
        r#"  3   | import { db } from "./db";"#,
        r#"  4   | import { log } from "./log";"#,
        "  5   |",
        "> 6   | export interface User {",
        "> 7   |   id: string;",
        "> 8   |   name: string;",
        "> 9   | }",
        " 10   |",
        " 11   | export async function getUser(id: string): Promise<User> {",
        r#"    - |   const row = await db.query("select * from users where id = $1", [id]);"#,
        r#" 12 + |   const row = (await cache.get(id)) ?? (await db.query("select 1", [id]));"#,
    ]
    .map(str::to_string)
    .to_vec()
}

/// Nothing anyone writes reaches column 0, so a body can't pass for a header or a code row; a
/// list without `--oneline` and a show print the same block.
#[test]
fn what_people_write_stays_indented() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    let body = ">10 + | x\nreviewer x, round 1:\n\n  t-0  open  src/app.ts:1";
    let id = fixture.thread(&review, "src/app.ts", 10, 10, body);
    ops::reply(
        &fixture.store(),
        &review.id,
        &id,
        "agent y:\n> 1 | z",
        agent(),
        None,
    )
    .unwrap();

    let ran = fixture.run(&["comment", "show", id.as_str()]);
    assert_eq!(ran.exit, Exit::Ok, "{}", ran.stderr);
    let lines = ran.lines();
    let at = lines
        .iter()
        .position(|line| *line == "reviewer dv fixture, round 1:")
        .unwrap();
    assert_eq!(
        lines[at..],
        [
            "reviewer dv fixture, round 1:",
            "    >10 + | x",
            "    reviewer x, round 1:",
            "",
            "      t-0  open  src/app.ts:1",
            "agent claude-code:",
            "    agent y:",
            "    > 1 | z",
        ]
    );
    // The commented line is the edited one, line 10 as the working tree has it.
    assert!(
        lines.contains(&format!(">10 + | {}", edited_lines()[9]).as_str()),
        "{}",
        ran.stdout
    );
    let listed = fixture.run(&["comment", "list"]);
    assert_eq!(listed.lines(), lines, "the list's block is the show's");
}

#[test]
fn a_thread_is_found_in_whichever_review_holds_it() {
    let mut fixture = Fixture::new();
    let working = fixture.review(Target::Working);
    later();
    let commit = fixture.review(Target::Commit { rev: "HEAD".into() });
    // A thread on the commit's new side: `src/app.ts` as committed.
    let anchor = fixture.anchor("src/app.ts", AnchorSide::Old, 10, 10);
    let anchor = dv_core::review::Anchor {
        side: AnchorSide::New,
        ..anchor
    };
    let id = ops::add_thread(
        &fixture.store(),
        &commit.id,
        anchor,
        "Old query.",
        reviewer(),
    )
    .unwrap();

    // `comment show` finds it without --review, though the working review is the current one.
    let json = fixture
        .run(&["comment", "show", id.as_str(), "--json"])
        .ok_json();
    assert_eq!(json["thread"]["review_id"], commit.id.as_str());
    assert_eq!(
        json["thread"]["placement"], "unchanged",
        "the commit doesn't move"
    );
    // $DV_REVIEW doesn't narrow the search; --review does.
    fixture
        .env
        .insert("DV_REVIEW".into(), working.id.to_string());
    let json = fixture
        .run(&["comment", "show", id.as_str(), "--json"])
        .ok_json();
    assert_eq!(json["thread"]["review_id"], commit.id.as_str());
    fixture.env.remove("DV_REVIEW");
    let error = fixture
        .run(&[
            "comment",
            "show",
            id.as_str(),
            "--review",
            working.id.as_str(),
            "--json",
        ])
        .error(Exit::Failure, "not_found");
    assert_eq!(
        error["message"],
        format!("no thread {id} in review {}", working.id)
    );

    // `comment list` lists the current review's threads only.
    assert_eq!(listed(&fixture, &[]), Vec::<String>::new());
    assert_eq!(fixture.run(&["comment", "list"]).lines(), ["no threads"]);
    assert_eq!(
        listed(&fixture, &["--review", commit.id.as_str()]),
        [id.to_string()]
    );

    // Unknown and malformed ids.
    let missing = ThreadId::generate();
    let error = fixture
        .run(&["comment", "show", missing.as_str(), "--json"])
        .error(Exit::Failure, "not_found");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with(&format!("no thread {missing} in the reviews of "))
    );
    fixture
        .run(&["comment", "show", "t-../x", "--json"])
        .error(Exit::Usage, "usage");
    fixture
        .run(&["comment", "show", working.id.as_str(), "--json"])
        .error(Exit::Usage, "usage");
    // A deleted thread is gone.
    ops::delete_item(
        &fixture.store(),
        &commit.id,
        &ItemId::Thread(id.clone()),
        &reviewer(),
    )
    .unwrap();
    fixture
        .run(&["comment", "show", id.as_str(), "--json"])
        .error(Exit::Failure, "not_found");
}

#[test]
fn renames_carry_threads_and_lost_sides_outdate_them() {
    let fixture = Fixture::new();
    let util = |b: u32| {
        format!(
            "export const a = 1;\nexport const b = {b};\nexport const c = 3;\n\
             export const d = 4;\nexport const e = 5;\nexport const f = 6;\n"
        )
    };
    fixture.repo.write("src/util.ts", util(2));
    fixture.repo.commit("util");
    fixture.repo.write("src/util.ts", util(20));
    let review = fixture.review(Target::Working);
    let on_util = fixture.thread(&review, "src/util.ts", 2, 2, "Why 20?");
    let old_side = fixture.anchor("src/app.ts", AnchorSide::Old, 10, 10);
    let on_old = ops::add_thread(
        &fixture.store(),
        &review.id,
        old_side,
        "Was fine.",
        reviewer(),
    )
    .unwrap();
    let on_new = fixture.thread(&review, "src/app.ts", 10, 10, "Cache?");

    // The agent renames util.ts (staged, so git pairs it) and deletes app.ts.
    fixture.repo.git(&["mv", "src/util.ts", "src/helpers.ts"]);
    std::fs::remove_file(fixture.path().join("src/app.ts")).unwrap();

    let json = fixture.run(&["comment", "list", "--json"]).ok_json();
    let renamed = thread_of(&json, &on_util);
    assert_eq!(renamed["path"], "src/helpers.ts");
    assert_eq!(renamed["placement"], "unchanged");
    assert_eq!(
        renamed["anchor"]["current"],
        json!({"path": "src/helpers.ts", "side": "new", "start_line": 2, "end_line": 2})
    );
    assert_eq!(renamed["anchor"]["original"]["path"], "src/util.ts");
    assert_eq!(renamed["current_code"], "export const b = 20;");
    // `--file` finds it by either name.
    assert_eq!(
        listed(&fixture, &["--file", "src/helpers.ts"]),
        [on_util.to_string()]
    );
    assert_eq!(
        listed(&fixture, &["--file", "src/util.ts"]),
        [on_util.to_string()]
    );

    // app.ts is deleted: its old side is still there, its new side is gone.
    let old = thread_of(&json, &on_old);
    assert_eq!(old["placement"], "unchanged");
    assert_eq!(old["side"], "old");
    // (`commit` took the working tree's app.ts along.)
    assert_eq!(old["current_code"], edited_lines()[9]);
    let new = thread_of(&json, &on_new);
    assert_eq!(new["placement"], "outdated");
    assert_eq!(new["path"], "src/app.ts");
    assert_eq!(new["anchor"]["current"], json!(null));
}

#[test]
fn a_working_review_is_placed_in_its_own_worktree() {
    let fixture = Fixture::new();
    let review = fixture.review(Target::Working);
    let id = fixture.thread(&review, "src/app.ts", 10, 10, "Why a cache?");
    // Another worktree of the repository, on `HEAD` as committed: app.ts unchanged there.
    let linked = fixture.repo.add_worktree("feature", "feat");

    let json = fixture
        .run_in(&linked, &["comment", "show", id.as_str(), "--json"])
        .ok_json();
    assert_eq!(json["thread"]["placement"], "unchanged", "{json}");
    assert_eq!(json["thread"]["current_code"], edited_lines()[9]);
    let json = fixture
        .run_in(
            &linked,
            &["comment", "list", "--review", review.id.as_str(), "--json"],
        )
        .ok_json();
    assert_eq!(json["threads"][0]["outdated"], false);

    // From the other worktree, an absolute path to the reviewed file in the review's own
    // worktree is that file; one into the worktree the command runs in still works too.
    let in_review = fixture.path().join("src").join("app.ts");
    let in_linked = linked.join("src").join("app.ts");
    for absolute in [&in_review, &in_linked] {
        let file = absolute.to_str().unwrap();
        let json = fixture
            .run_in(
                &linked,
                &[
                    "comment",
                    "list",
                    "--review",
                    review.id.as_str(),
                    "--file",
                    file,
                    "--json",
                ],
            )
            .ok_json();
        assert_eq!(json["threads"][0]["id"], id.as_str(), "{file}: {json}");
        let json = fixture
            .run_in(
                &linked,
                &[
                    "comment",
                    "add",
                    "--review",
                    review.id.as_str(),
                    "--file",
                    file,
                    "--lines",
                    "3",
                    "--body",
                    "And here?",
                    "--json",
                ],
            )
            .ok_json();
        assert_eq!(json["thread"]["path"], "src/app.ts", "{file}: {json}");
    }
}

/// Long code as commented keeps its ends, as the excerpt's commented span does (PLAN §10.3), and
/// nothing printed ends in whitespace: not a line of code, not a line of a body.
#[test]
fn code_as_commented_keeps_its_ends_and_no_line_ends_in_whitespace() {
    let fixture = Fixture::new();
    let fields = |name: &str| -> Vec<String> {
        (1..=40)
            .map(|n| match n {
                10 => "    ".to_string(),
                n => format!("  {name}{n}: \"value number {n}\","),
            })
            .collect()
    };
    fixture
        .repo
        .write("src/edited.ts", text_of(&fields("field")));
    fixture.repo.write("src/gone.ts", text_of(&fields("entry")));
    fixture.repo.commit("fields");
    let with_header = |lines: Vec<String>| {
        let mut lines = lines;
        lines.insert(0, "// v2".to_string());
        lines
    };
    fixture
        .repo
        .write("src/edited.ts", text_of(&with_header(fields("field"))));
    fixture
        .repo
        .write("src/gone.ts", text_of(&with_header(fields("entry"))));
    let review = fixture.review(Target::Working);
    let edited = fixture.thread(
        &review,
        "src/edited.ts",
        6,
        35,
        "Check these.  \nAll of them.",
    );
    let gone = fixture.thread(&review, "src/gone.ts", 6, 35, "Why so many?");
    // The agent edits three of the commented fields and deletes the other file's.
    let mut lines = with_header(fields("field"));
    for at in [7, 20, 30] {
        lines[at].push_str(" // checked");
    }
    fixture.repo.write("src/edited.ts", text_of(&lines));
    let mut lines = with_header(fields("entry"));
    lines.drain(5..35);
    fixture.repo.write("src/gone.ts", text_of(&lines));

    let ran = fixture.run(&["comment", "list"]);
    assert_eq!(ran.exit, Exit::Ok, "{}", ran.stderr);
    assert!(
        ran.stdout.lines().all(|line| line == line.trim_end()),
        "no line ends in whitespace:\n{}",
        ran.stdout
    );
    let lines = ran.lines();
    let block = |id: &ThreadId, from: &str, count: usize| -> Vec<&str> {
        let start = lines
            .iter()
            .position(|line| line.starts_with(id.as_str()))
            .unwrap();
        let at = start
            + lines[start..]
                .iter()
                .position(|line| *line == from)
                .unwrap();
        lines[at..].iter().take(count).copied().collect()
    };
    let field = |n: u32| format!(">   |   field{n}: \"value number {n}\",");
    let mut expected = vec!["as commented in round 1:".to_string()];
    expected.extend((5..=9).map(field));
    expected.push("> |".to_string());
    expected.extend((11..=14).map(field));
    expected.push("  ⋮ 10 lines".to_string());
    expected.extend((25..=34).map(field));
    let expected: Vec<String> = expected
        .into_iter()
        .map(|line| line.replacen(">   |", "> |", 1))
        .collect();
    assert_eq!(
        block(&edited, "as commented in round 1:", 22),
        expected,
        "{}",
        ran.stdout
    );
    assert!(lines.contains(&"    Check these."), "{}", ran.stdout);

    let entry = |n: u32| format!("> |   entry{n}: \"value number {n}\",");
    let mut expected = vec![
        "as commented in round 1 (not found in the file now):".to_string(),
        "  |   entry2: \"value number 2\",".to_string(),
        "  |   entry3: \"value number 3\",".to_string(),
        "  |   entry4: \"value number 4\",".to_string(),
    ];
    expected.extend((5..=9).map(entry));
    expected.push("> |".to_string());
    expected.extend((11..=14).map(entry));
    expected.push("  ⋮ 10 lines".to_string());
    expected.extend((25..=34).map(entry));
    expected.extend((35..=37).map(|n| format!("  |   entry{n}: \"value number {n}\",")));
    expected.push("reviewer dv fixture, round 1:".to_string());
    assert_eq!(
        block(
            &gone,
            "as commented in round 1 (not found in the file now):",
            29
        ),
        expected,
        "{}",
        ran.stdout
    );
}
