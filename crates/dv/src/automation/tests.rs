use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use dv_ui::appearance::MaterialSupport;
use dv_ui::services::{ServicesConfig, WslAccess};
use dv_ui::system::SystemShell;
use gpui::{TestAppContext, VisualTestContext};
use pretty_assertions::assert_eq;

use super::repos::Step;
use super::*;

fn open(
    cx: &mut TestAppContext,
) -> (
    tempfile::TempDir,
    Entity<DvAutomation>,
    &mut VisualTestContext,
) {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().to_path_buf();
    cx.update(|cx| {
        dv_ui::init(cx).unwrap();
        dv_ui::system::install(SystemShell::scripted(), cx);
        dv_ui::services::init(
            ServicesConfig {
                setup: dv_ui::services::SetupConfig::sandboxed(
                    data_dir.join("home"),
                    data_dir.join("user-path.txt"),
                    false,
                ),
                data_dir,
                wsl: WslAccess::scripted(Vec::new()),
            },
            cx,
        );
        dv_ui::appearance::init(MaterialSupport::ALL, cx);
    });
    let (shell, cx) = cx.add_window_view(Shell::new);
    let host = cx.update(|_, cx| cx.new(|cx| DvAutomation::new(shell, cx)));
    (dir, host, cx)
}

fn state(host: &Entity<DvAutomation>, cx: &mut VisualTestContext) -> Value {
    cx.update(|window, cx| host.read(cx).state(window, cx))
}

fn settled(host: &Entity<DvAutomation>, cx: &mut VisualTestContext) -> bool {
    cx.update(|_, cx| host.read(cx).settled(cx))
}

/// Runs a command that finishes later (`custom_task`) to its reply.
fn run(
    host: &Entity<DvAutomation>,
    cmd: &str,
    args: Value,
    cx: &mut VisualTestContext,
) -> Result<Value> {
    let task =
        cx.update(|window, cx| host.update(cx, |host, cx| host.custom_task(cmd, args, window, cx)));
    let task = task.unwrap_or_else(|| panic!("{cmd} is a dv command"));
    cx.run_until_parked();
    cx.foreground_executor().block_test(task)
}

/// Runs a command whose task waits on real processes (`cli`, `cli_spawn`, `cli_join`) to its
/// reply, letting real time pass meanwhile.
fn run_waiting(
    host: &Entity<DvAutomation>,
    cmd: &str,
    args: Value,
    cx: &mut VisualTestContext,
) -> Result<Value> {
    let task =
        cx.update(|window, cx| host.update(cx, |host, cx| host.custom_task(cmd, args, window, cx)));
    let task = task.unwrap_or_else(|| panic!("{cmd} is a dv command"));
    let reply: Rc<RefCell<Option<Result<Value>>>> = Rc::default();
    cx.update(|_, cx| {
        let reply = reply.clone();
        cx.spawn(async move |_| *reply.borrow_mut() = Some(task.await))
            .detach();
    });
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        if let Some(reply) = reply.borrow_mut().take() {
            return reply;
        }
        assert!(Instant::now() < deadline, "{cmd} didn't reply");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Lets real time pass (watch threads deliver from outside the test scheduler) until the host
/// is settled and `done` holds.
fn wait_until(
    host: &Entity<DvAutomation>,
    cx: &mut VisualTestContext,
    what: &str,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        let state = state(host, cx);
        if settled(host, cx) && done(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {state:#}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn setting_changes_parse() {
    let parse = |args: Value| SettingChange::parse(&args);
    assert_eq!(
        parse(json!({"key": "material", "value": "acrylic"})).unwrap(),
        SettingChange::Material(Material::Acrylic)
    );
    assert_eq!(
        parse(json!({"key": "mode", "value": "light"})).unwrap(),
        SettingChange::Mode(Mode::Light)
    );
    assert_eq!(
        parse(json!({"key": "palette", "value": "dracula"})).unwrap(),
        SettingChange::Palette(dv_ui::theme::Palette::Dracula)
    );
    for (args, error) in [
        (json!({}), "needs \"key\""),
        (json!({"key": "material"}), "needs a string \"value\""),
        (
            json!({"key": "material", "value": "glass"}),
            "material must be",
        ),
        (json!({"key": "mode", "value": "auto"}), "mode must be"),
        (
            json!({"key": "palette", "value": "Aura Dark"}),
            "palette must be",
        ),
        (json!({"key": "theme", "value": "dark"}), "unknown setting"),
    ] {
        let message = parse(args.clone()).unwrap_err().to_string();
        assert!(message.contains(error), "{args}: {message}");
    }
}

#[test]
fn steps_parse_strictly() {
    let parse = |value: Value| Step::parse(&value);
    assert_eq!(
        parse(json!({"write": "a.txt", "text": "a\n"})).unwrap(),
        Step::Write {
            path: "a.txt".into(),
            contents: b"a\n".to_vec()
        }
    );
    assert_eq!(
        parse(json!({"write": "b.bin", "bytes": [0, 255]})).unwrap(),
        Step::Write {
            path: "b.bin".into(),
            contents: vec![0, 255]
        }
    );
    assert_eq!(
        parse(json!({"write": "big.txt", "text": "ab\n", "repeat": 3})).unwrap(),
        Step::Write {
            path: "big.txt".into(),
            contents: b"ab\nab\nab\n".to_vec()
        }
    );
    assert_eq!(
        parse(json!({"rename": ["a", "b"]})).unwrap(),
        Step::Rename("a".into(), "b".into())
    );
    assert_eq!(
        parse(json!({"git": ["add", "a.txt"]})).unwrap(),
        Step::Git(vec!["add".into(), "a.txt".into()])
    );
    assert_eq!(
        parse(json!({"config": ["core.autocrlf", "true"]})).unwrap(),
        Step::Config("core.autocrlf".into(), "true".into())
    );
    assert_eq!(
        parse(json!({"worktree": ["wt", "feat"]})).unwrap(),
        Step::Worktree("wt".into(), "feat".into())
    );
    for (step, error) in [
        (json!("write"), "is an object"),
        (json!({}), "exactly one of"),
        (json!({"write": "a", "commit": "m"}), "exactly one of"),
        (json!({"write": "a"}), "needs a string \"text\""),
        (json!({"write": "a", "bytes": [256]}), "numbers 0-255"),
        (
            json!({"write": "a", "text": "x", "repeat": 0}),
            "a count from 1",
        ),
        (json!({"commit": "m", "text": "x"}), "unknown key \"text\""),
        (json!({"rename": ["a"]}), "takes 2 strings"),
        (json!({"worktree": ["wt"]}), "takes 2 strings"),
        (json!({"git": []}), "at least one argument"),
        (json!({"remove": 3}), "needs a string"),
    ] {
        let message = parse(step.clone()).unwrap_err().to_string();
        assert!(message.contains(error), "{step}: {message}");
    }
}

#[gpui::test]
fn state_describes_the_empty_shell(cx: &mut TestAppContext) {
    let (dir, host, cx) = open(cx);
    let state = state(&host, cx);
    assert_eq!(state["app"], "dv");
    assert_eq!(state["data_dir"], dir.path().display().to_string());
    assert_eq!(
        state["shell"],
        json!({
            "panel_visible": true,
            "panel_width": 280.0,
            "empty_state": "no_review",
            "window_material": dv_ui::appearance::default_material().key(),
            "window_native": null,
            "opening": false,
            "pending": null,
            "banner": null,
            "backend_banner": null,
            "status": {"health": "idle", "location": "No repo", "review": null, "finish": false},
        })
    );
    assert_eq!(state["backends"], json!([]));
    assert_eq!(
        state["switcher"],
        json!({
            "open": false,
            "mode": null,
            "query": null,
            "selected": null,
            "items": [],
            "menu": null,
            "closing": null,
            "close_failure": null,
        })
    );
    assert_eq!(
        state["source_picker"],
        json!({"open": false, "selected": null, "items": [], "compare": false})
    );
    assert_eq!(
        state["file_list"],
        json!({"query": "", "shown": [], "current_file": null})
    );
    assert_eq!(state["settings"]["mode"], "system");
    assert_eq!(
        state["settings"]["available_materials"],
        json!(["mica", "acrylic", "vibrancy", "opaque"])
    );
    assert_eq!(state["system"], json!({"requests": [], "answers_left": 0}));
    assert_eq!(state["repos"], json!([]));
    assert_eq!(state["session"], Value::Null);
    assert_eq!(state["comments_list"], Value::Null);
    assert_eq!(state["cli"], json!({"spawned": []}));
    assert!(settled(&host, cx));
}

#[gpui::test]
fn set_setting_changes_the_theme_and_saves(cx: &mut TestAppContext) {
    let (dir, host, cx) = open(cx);
    let reply = cx.update(|window, cx| {
        host.update(cx, |host, cx| {
            host.custom(
                "set_setting",
                json!({"key": "material", "value": "acrylic"}),
                window,
                cx,
            )
        })
    });
    let reply = reply.expect("a dv command").unwrap();
    assert_eq!(reply["settings"]["material"], "acrylic");
    cx.update(|window, cx| {
        host.update(cx, |host, cx| {
            host.custom(
                "set_setting",
                json!({"key": "mode", "value": "light"}),
                window,
                cx,
            )
        })
    })
    .expect("a dv command")
    .unwrap();
    cx.run_until_parked();

    let state = state(&host, cx);
    assert_eq!(state["settings"]["resolved_mode"], "light");
    // Resolved once the change has reached `gpui_appearance`, as the window shows it.
    assert_eq!(state["settings"]["resolved_material"], "acrylic");
    assert_eq!(state["shell"]["window_material"], "acrylic");
    assert!(settled(&host, cx));
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        saved,
        json!({"v": 1, "material": "acrylic", "mode": "light"})
    );

    // Unknown commands are the driver's to report.
    let unknown = cx.update(|window, cx| {
        host.update(cx, |host, cx| host.custom("nope", json!({}), window, cx))
    });
    assert!(unknown.is_none());
    let unknown = cx.update(|window, cx| {
        host.update(cx, |host, cx| {
            host.custom_task("nope", json!({}), window, cx)
        })
    });
    assert!(unknown.is_none());
}

#[gpui::test]
fn a_seeded_repo_opens_as_a_review_and_the_agent_answers(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (dir, host, cx) = open(cx);
    let seeded = run(
        &host,
        "seed_repo",
        json!({"name": "api-server", "steps": [
            {"write": "src/app.ts", "text": "export const a = 1;\nexport const b = 2;\n"},
            {"write": "README.md", "text": "# api\n"},
            {"commit": "base"},
            {"write": "src/app.ts", "text": "export const a = 1;\nexport const b = 3;\n"},
            {"write": "src/new.ts", "text": "export const c = 4;\n"},
        ]}),
        cx,
    )
    .unwrap();
    let path = seeded["path"].as_str().unwrap().to_string();
    // Both canonical: macOS's temp dir is under `/var`, a link to `/private/var`, and Windows'
    // canonical paths carry the `\\?\` prefix.
    let repos = dir.path().join("repos").canonicalize().unwrap();
    let seeded_at = std::path::Path::new(&path).canonicalize().unwrap();
    assert!(seeded_at.starts_with(&repos), "{path}");
    assert_eq!(seeded["commits"].as_array().unwrap().len(), 1);
    let error = run(&host, "seed_repo", json!({"name": "api-server"}), cx).unwrap_err();
    assert!(error.to_string().contains("already seeded"), "{error}");
    let error = run(&host, "seed_repo", json!({"name": "../escape"}), cx).unwrap_err();
    assert!(error.to_string().contains("plain file name"), "{error}");

    // `open` adds the repo, computes its row and opens the switcher on it.
    let opened = run(&host, "open", json!({"path": path}), cx).unwrap();
    // The same folder, though maybe not the same spelling: a Windows temp dir can come as an 8.3
    // short name (`RUNNER~1`) on one side and the long one on the other.
    let opened_at = std::path::Path::new(opened["path"].as_str().unwrap())
        .canonicalize()
        .unwrap();
    assert_eq!(opened_at, seeded_at);
    // The app's spelling from here on.
    let path = opened["path"].as_str().unwrap().to_string();
    let state = wait_until(&host, cx, "the repo's row", |state| {
        state["repos"][0]["working"].is_object()
    });
    assert_eq!(state["switcher"]["open"], true);
    assert_eq!(state["switcher"]["selected"], "api-server/working");
    assert_eq!(state["switcher"]["items"][0]["target"], "working");
    assert_eq!(state["switcher"]["items"][0]["repo"], path);
    assert_eq!(state["switcher"]["items"][0]["review"], Value::Null);
    let repo = &state["repos"][0];
    assert_eq!(repo["name"], "api-server");
    assert_eq!(
        repo["working"],
        json!({"files": 2, "additions": 2, "deletions": 1})
    );
    assert_eq!(repo["reviews"], json!([]));

    // `open_review` opens the working review and shows it.
    let reply = run(&host, "open_review", json!({"path": path}), cx).unwrap();
    assert_eq!(reply["created"], true);
    let review_id = reply["review_id"].as_str().unwrap().to_string();
    let state = wait_until(&host, cx, "the file list", |state| {
        state["session"]["total"] == 2
    });
    let session = &state["session"];
    assert_eq!(session["review_id"], review_id);
    assert_eq!(session["target"], "working");
    assert_eq!(session["target_label"], "working tree vs main");
    assert_eq!(session["status"], "open");
    assert_eq!(session["round"], 1);
    assert_eq!(session["read_only"], false);
    assert_eq!(session["files"][0]["path"], "src/app.ts");
    assert_eq!(session["files"][0]["status"], "modified");
    assert_eq!(session["files"][1]["status"], "untracked");
    assert_eq!(state["shell"]["empty_state"], Value::Null);
    assert_eq!(
        state["switcher"]["open"], false,
        "opening a review closes it"
    );
    assert_eq!(
        state["file_list"],
        json!({"query": "", "shown": ["src/app.ts", "src/new.ts"], "current_file": "src/app.ts"})
    );
    assert_eq!(session["current_file"], "src/app.ts");
    assert_eq!(
        state["shell"]["status"]["location"],
        "Local · watching for changes"
    );
    let state = wait_until(&host, cx, "the review in the repo's row", |state| {
        state["repos"][0]["reviews"]
            .as_array()
            .is_some_and(|r| r.len() == 1)
    });
    assert_eq!(
        state["repos"][0]["reviews"][0]["target_label"],
        "working tree vs main"
    );
    // Opening it again resumes it.
    let again = run(&host, "open_review", json!({"path": path}), cx).unwrap();
    assert_eq!(again, json!({"review_id": review_id, "created": false}));

    // The reviewer comments through the session; the agent answers through review_op.
    let session = cx.update(|_, cx| host.read(cx).shell.read(cx).session().cloned().unwrap());
    session.update(cx, |session, cx| {
        session.ensure_loaded(["src/app.ts".to_string()], cx)
    });
    wait_until(&host, cx, "the diff", |state| {
        state["session"]["files"][0]["state"] == "loaded"
    });
    let anchor = session
        .read_with(cx, |session, _| {
            session.anchor_for("src/app.ts", dv_core::review::AnchorSide::New, 2, 2)
        })
        .unwrap();
    let posted = session.update(cx, |session, cx| {
        session.add_thread(anchor, "Why 3?".into(), cx)
    });
    cx.run_until_parked();
    let thread = cx.foreground_executor().block_test(posted).unwrap();
    let state = wait_until(&host, cx, "the thread", |state| {
        state["session"]["threads"]
            .as_array()
            .is_some_and(|t| t.len() == 1)
    });
    assert_eq!(state["session"]["threads"][0]["state"], "open");
    assert_eq!(state["session"]["threads"][0]["outdated"], false);
    assert_eq!(state["session"]["files"][0]["open_threads"], 1);
    assert_eq!(state["session"]["threads"][0]["round"], 1);
    assert_eq!(state["session"]["composer"], Value::Null);
    assert_eq!(state["session"]["sheet"], Value::Null);
    assert_eq!(state["session"]["focused_thread"], Value::Null);
    assert_eq!(
        state["comments_list"],
        json!({"show_resolved": false, "show_annotations": false, "expanded": null, "rows": [{
            "id": thread, "state": "open", "outdated": false, "not_in_comparison": false,
            "pending": false, "location": "src/app.ts:2", "file_level": false, "summary": "Why 3?",
            "snippet": null, "unlisted": false, "drawable": true,
        }], "menu": null})
    );
    let thread_state = &state["session"]["threads"][0];
    assert_eq!(thread_state["placement"], "unchanged");
    assert_eq!(thread_state["lines"], json!({"start": 2, "end": 2}));
    assert_eq!(
        thread_state["original_lines"],
        json!({"start": 2, "end": 2})
    );
    assert_eq!(state["session"]["comparison"], json!({"kind": "all"}));
    assert_eq!(state["session"]["comparison_available"], false);
    assert_eq!(state["session"]["ready_note"], Value::Null);

    run(
        &host,
        "review_op",
        json!({"op": "address", "thread": thread, "note": "Now 3 by design", "author": "claude-code"}),
        cx,
    )
    .unwrap();
    run(
        &host,
        "review_op",
        json!({"op": "reply", "thread": thread, "body": "See the spec."}),
        cx,
    )
    .unwrap();
    let state = wait_until(&host, cx, "the agent's answer", |state| {
        state["session"]["threads"][0]["replies"]
            .as_array()
            .is_some_and(|r| r.len() == 1)
    });
    let thread_state = &state["session"]["threads"][0];
    assert_eq!(thread_state["state"], "addressed");
    assert_eq!(thread_state["note"], "Now 3 by design");
    assert_eq!(thread_state["replies"][0]["author"]["kind"], "agent");
    let error = run(
        &host,
        "review_op",
        json!({"op": "resolve", "thread": thread}),
        cx,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("\"reply\", \"address\" or \"done\""),
        "{error}"
    );
    // Done, with a note for the reviewer (`dv review done --note`).
    let done = run(
        &host,
        "review_op",
        json!({"op": "done", "note": "Ready for another look.", "author": "claude-code"}),
        cx,
    )
    .unwrap();
    assert_eq!(done, json!({"changed": true}));
    let state = wait_until(&host, cx, "the handoff", |state| {
        state["session"]["status"] == "ready"
    });
    assert_eq!(
        state["session"]["ready_note"],
        json!({
            "author": {"kind": "agent", "name": "claude-code", "model": null, "family": "claude"},
            "round": 1,
            "note": "Ready for another look.",
            "annotations": 0,
            "open": false,
        })
    );
    let change = session
        .read_with(cx, |session, _| session.review().status_change.clone())
        .expect("the status change");
    assert_eq!(change.note.as_deref(), Some("Ready for another look."));
    assert_eq!(
        change.author,
        Some(dv_core::review::Author::agent("claude-code"))
    );

    // The agent edits the repo: the session's list follows.
    run(
        &host,
        "repo_edit",
        json!({"name": "api-server", "steps": [{"remove": "src/new.ts"}]}),
        cx,
    )
    .unwrap();
    wait_until(&host, cx, "the list to drop the removed file", |state| {
        state["session"]["total"] == 1
    });
}

#[test]
fn seeded_repos_are_found_again_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repos").join("kept");
    let fixture = FixtureRepo::persistent(&path);
    fixture.write("a.txt", "a\n");
    fixture.commit("one");
    drop(fixture);
    let found = repos::reopen_seeded(dir.path());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, "kept");
    // A directory that isn't a seeded repo is passed over.
    std::fs::create_dir_all(dir.path().join("repos").join("other")).unwrap();
    assert_eq!(repos::reopen_seeded(dir.path()).len(), 1);
}

#[gpui::test]
fn a_seeded_repos_linked_worktrees_get_its_environment(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (dir, host, cx) = open(cx);
    run(
        &host,
        "seed_repo",
        json!({"name": "api", "steps": [
            {"write": "a.txt", "text": "a
        "},
            {"commit": "base"},
            {"worktree": ["wt", "feat"]},
        ]}),
        cx,
    )
    .unwrap();
    // Beside the work tree, outside it; and one more added later, elsewhere again.
    let repos = dir.path().join("repos");
    let linked = repos.join("api.fixture").join("worktrees").join("wt");
    assert!(linked.join("a.txt").is_file());
    run(
        &host,
        "repo_edit",
        json!({"name": "api", "steps": [{"git": ["worktree", "add", "-q", "-b", "late", "../late"]}]}),
        cx,
    )
    .unwrap();
    let late = repos.join("late");
    // Only the fixture's environment points git at this global configuration.
    std::fs::write(
        repos.join("api.fixture").join("home").join(".gitconfig"),
        "[dv-test]
	marker = from-the-overlay
",
    )
    .unwrap();
    // A backend opened on either runs git with the fixture's environment, not this machine's.
    for root in [&linked, &late] {
        let locations = cx.update(|_, cx| cx.services().locations.clone());
        let open = locations.update(cx, |locations, cx| {
            locations.open(dv_core::Location::Local(root.clone()), cx)
        });
        cx.run_until_parked();
        let backend = cx.foreground_executor().block_test(open).unwrap();
        let marker = backend
            .git(
                &["config", "--get", "dv-test.marker"],
                &dv_core::backend::GitOpts::default().with_ok_codes(&[0, 1]),
            )
            .unwrap();
        assert_eq!(
            marker.stdout_utf8().map(str::trim),
            Some("from-the-overlay"),
            "{}",
            root.display()
        );
    }
    // So does a `cli` child started in one.
    host.update(cx, |host, _| host.cli_exe = Some(PathBuf::from("git")));
    for root in [&linked, &late] {
        let reply = run_waiting(
            &host,
            "cli",
            json!({"args": ["config", "--get", "dv-test.marker"], "cwd": root}),
            cx,
        )
        .unwrap();
        assert_eq!(
            reply["stdout"],
            "from-the-overlay
",
            "{reply}"
        );
    }
}

#[gpui::test]
fn the_cli_commands_run_children_off_the_ui_thread(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_dir, host, cx) = open(cx);
    let seeded = run(
        &host,
        "seed_repo",
        json!({"name": "api", "steps": [
            {"write": "a.txt", "text": "a
        "},
            {"commit": "base"},
        ]}),
        cx,
    )
    .unwrap();
    let path = seeded["path"].as_str().unwrap().to_string();
    // git stands in for dv here: the commands start whatever `cli_exe` names (dv itself in the
    // app), and a test binary can't name the dv executable.
    host.update(cx, |host, _| host.cli_exe = Some(PathBuf::from("git")));

    // One command through the host: the line's args, cwd and `env` reach the child (the name
    // the line's, the email the seeded repo's), and its output comes back as the reply
    // (agent_cli's tests cover the child itself).
    let reply = run_waiting(
        &host,
        "cli",
        json!({"args": ["var", "GIT_AUTHOR_IDENT"], "cwd": path, "env": {"GIT_AUTHOR_NAME": "claude-code"}}),
        cx,
    )
    .unwrap();
    assert_eq!(reply["exit"], 0, "{reply}");
    assert_eq!(reply["stderr"], "", "{reply}");
    assert!(
        reply["stdout"]
            .as_str()
            .unwrap()
            .starts_with("claude-code <fixture@dv.invalid> "),
        "{reply}"
    );
    // A command line that would open the app is refused before anything starts.
    let error = run(&host, "cli", json!({"args": ["open", path]}), cx).unwrap_err();
    assert!(error.to_string().contains("would open a window"), "{error}");
    let error = run(&host, "cli", json!({"args": ["status"], "bogus": 1}), cx).unwrap_err();
    assert!(error.to_string().contains("not \"bogus\""), "{error}");

    // `cli_spawn` replies at once and lists the child in `state.cli`; `cli_join` collects it.
    let spawned = run_waiting(
        &host,
        "cli_spawn",
        json!({"name": "hash", "args": ["hash-object", "--stdin"], "cwd": path, "stdin": "hello
"}),
        cx,
    )
    .unwrap();
    assert_eq!(spawned["name"], "hash");
    assert!(spawned["pid"].is_u64());
    let cli = &state(&host, cx)["cli"];
    assert_eq!(cli["spawned"][0]["name"], "hash");
    assert_eq!(cli["spawned"][0]["args"], json!(["hash-object", "--stdin"]));
    let error = run_waiting(
        &host,
        "cli_spawn",
        json!({"name": "hash", "args": ["--version"]}),
        cx,
    )
    .unwrap_err();
    assert!(error.to_string().contains("hasn't been joined"), "{error}");
    let joined = run_waiting(
        &host,
        "cli_join",
        json!({"name": "hash", "timeout_ms": 10000}),
        cx,
    )
    .unwrap();
    assert_eq!(joined["exit"], 0);
    assert_eq!(
        joined["stdout"],
        "ce013625030ba8dba906f756967f9e9ca394464a
"
    );
    assert_eq!(state(&host, cx)["cli"], json!({"spawned": []}));
    let error = run(&host, "cli_join", json!({"name": "hash"}), cx).unwrap_err();
    assert!(
        error.to_string().contains("no cli_spawn named \"hash\""),
        "{error}"
    );
    // Children never hold up `settled`.
    assert!(settled(&host, cx));

    // A child that outlives `cli_join`'s wait keeps running; the app's quit kills it.
    host.update(cx, |host, _| {
        host.cli_exe = Some(std::env::current_exe().unwrap())
    });
    run_waiting(
        &host,
        "cli_spawn",
        json!({
            "name": "sleeper",
            "args": ["--exact", "automation::agent_cli::tests::sleeping_child", "--nocapture"],
            "env": {"DV_TEST_SLEEPING_CHILD": "1"},
        }),
        cx,
    )
    .unwrap();
    let error = run_waiting(
        &host,
        "cli_join",
        json!({"name": "sleeper", "timeout_ms": 300}),
        cx,
    )
    .unwrap_err();
    assert!(error.to_string().contains("still running"), "{error}");
    assert_eq!(state(&host, cx)["cli"]["spawned"][0]["running"], true);
    assert!(settled(&host, cx));
    let children = host.read_with(cx, |host, _| host.children.clone());
    // The app's quit hooks, run as a quit runs them. (`TestAppContext::quit`, the test macro's
    // own end of the test, crashes on macOS when a test calls it mid-way.)
    cx.cx.update(|cx| cx.shutdown());
    assert_eq!(
        agent_cli::lock(&children).spawned_state(),
        Vec::<Value>::new()
    );
}

/// Lets real time pass until `done` holds of the state, settled or not (a connection's
/// respawns run on their own threads).
fn wait_for_state(
    host: &Entity<DvAutomation>,
    cx: &mut VisualTestContext,
    what: &str,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.run_until_parked();
        let state = state(host, cx);
        if done(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:#}",
            state["backends"]
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[gpui::test]
fn a_loopback_repo_rides_out_a_kill_and_a_block(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let (_dir, host, cx) = open(cx);
    // A test can't name the dv executable: the loopback's server runs on a thread here.
    host.update(cx, |host, _| {
        host.loopback = backends::Loopback {
            server: Some(dv_backend::Launcher::InProcess),
            backoff: vec![Duration::from_millis(50); 3],
        }
    });
    let error = run(
        &host,
        "seed_repo",
        json!({"name": "web", "backend": "wsl"}),
        cx,
    )
    .unwrap_err();
    assert!(error.to_string().contains("\"loopback\""), "{error}");
    let seeded = run(
        &host,
        "seed_repo",
        json!({"name": "api", "backend": "loopback", "steps": [
            {"write": "a.txt", "text": "a\n"},
            {"commit": "base"},
            {"write": "a.txt", "text": "b\n"},
        ]}),
        cx,
    )
    .unwrap();
    let path = seeded["path"].as_str().unwrap().to_string();
    assert_eq!(state(&host, cx)["backends"][0]["state"], "idle");
    run(&host, "open_review", json!({"path": path}), cx).unwrap();
    let state = wait_until(&host, cx, "the review", |state| {
        state["session"]["total"] == 1
    });
    assert_eq!(
        state["backends"],
        json!([{"name": "loopback", "state": "connected", "attempt": null, "generation": 1, "error": null}])
    );
    assert_eq!(
        state["shell"]["status"]["location"],
        "loopback · watching for changes"
    );

    // A kill: it reconnects by itself, and the edit made meanwhile shows.
    let killed = run_waiting(&host, "backend_kill", json!({"name": "loopback"}), cx).unwrap();
    assert_eq!(killed["state"], "reconnecting");
    run(
        &host,
        "repo_edit",
        json!({"name": "api", "steps": [{"write": "b.txt", "text": "b\n"}]}),
        cx,
    )
    .unwrap();
    let state = wait_for_state(&host, cx, "the reconnect's rescan", |state| {
        state["backends"][0]["state"] == "connected" && state["session"]["total"] == 2
    });
    assert_eq!(state["backends"][0]["generation"], 2);
    assert_eq!(state["shell"]["backend_banner"], Value::Null);

    // Blocked, a kill runs out of respawns: failed, the banner and the status bar say so.
    let reply = cx
        .update(|window, cx| {
            host.update(cx, |host, cx| {
                host.custom(
                    "backend_block",
                    json!({"name": "loopback", "blocked": true}),
                    window,
                    cx,
                )
            })
        })
        .expect("a dv command")
        .unwrap();
    assert_eq!(reply, json!({"name": "loopback", "blocked": true}));
    run_waiting(&host, "backend_kill", json!({"name": "loopback"}), cx).unwrap();
    // The banner follows the connection's state a notification later, so wait for both.
    let state = wait_for_state(&host, cx, "the connection to fail", |state| {
        state["backends"][0]["state"] == "failed" && !state["shell"]["backend_banner"].is_null()
    });
    assert!(
        state["backends"][0]["error"]
            .as_str()
            .is_some_and(|error| error.contains("backend_block")),
        "{:#}",
        state["backends"]
    );
    assert_eq!(
        state["shell"]["backend_banner"]["message"],
        "Lost the connection to dv in loopback"
    );
    assert_eq!(
        state["shell"]["status"]["location"],
        "loopback · disconnected"
    );
    assert_eq!(state["shell"]["status"]["health"], "failing");

    // Unblocked, the banner's Retry connects again.
    cx.update(|window, cx| {
        host.update(cx, |host, cx| {
            host.custom(
                "backend_block",
                json!({"name": "loopback", "blocked": false}),
                window,
                cx,
            )
        })
    })
    .expect("a dv command")
    .unwrap();
    let shell = host.read_with(cx, |host, _| host.shell.clone());
    shell.update(cx, |shell, cx| shell.retry_backend(cx));
    let state = wait_for_state(&host, cx, "the retry", |state| {
        state["backends"][0]["state"] == "connected" && state["shell"]["backend_banner"].is_null()
    });
    assert_eq!(
        state["shell"]["status"]["location"],
        "loopback · watching for changes"
    );

    let error = run_waiting(&host, "backend_kill", json!({"name": "nowhere"}), cx).unwrap_err();
    assert!(error.to_string().contains("no connection named"), "{error}");
    let error = cx
        .update(|window, cx| {
            host.update(cx, |host, cx| {
                host.custom(
                    "backend_block",
                    json!({"name": "nowhere", "blocked": true}),
                    window,
                    cx,
                )
            })
        })
        .expect("a dv command")
        .unwrap_err();
    assert!(error.to_string().contains("no loopback"), "{error}");

    // `backend_hold` holds the loopback's starts (released here: a held start would wait on
    // this test's thread, where its background work runs).
    let hold = |args: Value, cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            host.update(cx, |host, cx| host.custom("backend_hold", args, window, cx))
        })
        .expect("a dv command")
    };
    assert_eq!(
        hold(json!({"name": "loopback", "held": false}), cx).unwrap(),
        json!({"name": "loopback", "held": false})
    );
    let error = hold(json!({"name": "loopback"}), cx).unwrap_err();
    assert!(error.to_string().contains("boolean \"held\""), "{error}");
    let error = hold(json!({"name": "nowhere", "held": true}), cx).unwrap_err();
    assert!(error.to_string().contains("no loopback"), "{error}");
}

#[gpui::test]
fn wsl_distros_script_the_stand_in(cx: &mut TestAppContext) {
    let (_dir, host, cx) = open(cx);
    let custom = |args: Value, cx: &mut VisualTestContext| {
        cx.update(|window, cx| {
            host.update(cx, |host, cx| host.custom("wsl_distros", args, window, cx))
        })
        .expect("a dv command")
    };
    let reply = custom(
        json!({"distros": [{"name": "Ubuntu", "running": false}]}),
        cx,
    )
    .unwrap();
    assert_eq!(
        reply,
        json!({"distros": [{"name": "Ubuntu", "running": false}]})
    );
    let distros = cx.update(|_, cx| cx.services().locations.read(cx).wsl().distros());
    assert_eq!(
        distros.unwrap(),
        [dv_backend::wsl::Distro {
            name: "Ubuntu".into(),
            running: false
        }]
    );
    let error = custom(json!({"distros": [{"name": "Ubuntu"}]}), cx).unwrap_err();
    assert!(error.to_string().contains("\"running\""), "{error}");
    let error = custom(json!({}), cx).unwrap_err();
    assert!(error.to_string().contains("needs a list"), "{error}");
}
