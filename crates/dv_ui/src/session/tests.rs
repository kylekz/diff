//! The session against real fixture repos: loading, on-demand diffs and highlights, live
//! refreshes from the watch, the stale-completion guard, fold reloads and writes. Tests that
//! watch let real time pass: the watch delivers from its own thread (`allow_parking`).

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use dv_core::backend::{
    GitOpts, GitOutput, LocalBackendOptions, TextStats, WatchHandle, WatchSink, WatchSpec,
};
use dv_core::diff::{FileDiff, TextDiff};
use dv_core::review::{AnchorSide, Author, ReviewStatus, ReviewStore, ThreadState, Verdict, ops};
use dv_core::{BackendError, LocalBackend, RepoInfo, Target};
use dv_test_support::FixtureRepo;
use gpui::{AppContext as _, Entity, TestAppContext};
use pretty_assertions::assert_eq;

use super::watch::watch_spec;
use super::*;
use crate::syntax::TokenKind;

mod ended;
mod home;
mod outside;
mod placement;
mod remote;
mod rounds;

fn backend(repo: &FixtureRepo) -> Arc<dyn Backend> {
    Arc::new(
        LocalBackend::open_with(
            repo.path(),
            LocalBackendOptions {
                env: repo.env_overlay(),
            },
        )
        .expect("the fixture opens"),
    )
}

fn reviewer() -> Author {
    Author::reviewer(dv_test_support::AUTHOR_NAME)
}

/// A session over `target`'s review in `repo`, opened as the reviewer.
fn session(repo: &FixtureRepo, target: Target, cx: &mut TestAppContext) -> Entity<ReviewSession> {
    let backend = backend(repo);
    let store = ReviewStore::new(backend.clone());
    let opened = ops::open_review(&store, target, reviewer()).unwrap();
    let loaded = store.load(&opened.review.id).unwrap();
    let labels = LabelContext::read(&GitRepo::new(backend.clone()));
    cx.new(|cx| {
        ReviewSession::new(
            OpenedReview {
                origin: backend.repo().location.clone(),
                backend,
                loaded,
                created: opened.created,
                reviewer: reviewer(),
                labels,
                connection: None,
                pr: None,
            },
            cx,
        )
    })
}

/// Runs the session (and lets real time pass, for the watch) until it is settled and `done`
/// holds.
fn wait(
    cx: &mut TestAppContext,
    session: &Entity<ReviewSession>,
    what: &str,
    done: impl Fn(&ReviewSession) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.executor().advance_clock(REFRESH_DEBOUNCE * 2);
        cx.run_until_parked();
        if session.read_with(cx, |session, _| session.is_settled() && done(session)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn text(session: &ReviewSession, path: &str) -> TextDiff {
    match session
        .file_state(path)
        .and_then(FileState::diff)
        .map(|diff| &**diff)
    {
        Some(FileDiff::Text(text)) => text.clone(),
        other => panic!("{path} isn't a loaded text diff: {other:?}"),
    }
}

fn new_side(session: &ReviewSession, path: &str) -> Vec<String> {
    text(session, path)
        .lines
        .iter()
        .filter(|line| line.new_no.is_some())
        .map(|line| line.text.clone())
        .collect()
}

fn fixture() -> FixtureRepo {
    let repo = FixtureRepo::new();
    repo.write("src/a.rs", "fn a() -> u32 {\n    1\n}\n");
    repo.write("src/b.rs", "fn b() {}\n");
    repo.write("README.md", "# readme\n");
    repo.commit("base");
    repo.write("src/a.rs", "fn a() -> u32 {\n    2\n}\n");
    repo.write("src/b.rs", "fn b() { let _ = 1; }\n");
    repo.write("notes.txt", "untracked\n");
    repo
}

#[gpui::test]
fn the_list_loads_first_then_diffs_and_highlights_on_demand(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let session = session(&repo, Target::Working, cx);
    session.read_with(cx, |session, _| assert!(session.is_loading()));
    wait(cx, &session, "the list", |session| !session.is_loading());
    session.read_with(cx, |session, _| {
        let paths: Vec<&str> = session.files().iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["notes.txt", "src/a.rs", "src/b.rs"]);
        assert!(
            session
                .files()
                .iter()
                .all(|f| matches!(session.file_state(&f.path), Some(FileState::Unloaded))),
            "nothing loads until it is asked for"
        );
        assert_eq!(session.target_label(), "working tree vs main");
        assert_eq!(session.error(), None);
        assert!(!session.is_read_only());
        assert_eq!(
            session.watch.spec(),
            Some(WatchSpec::ALL),
            "a working review watches everything"
        );
    });

    session.update(cx, |session, cx| {
        session.ensure_loaded(["src/a.rs".to_string(), "notes.txt".to_string()], cx)
    });
    wait(cx, &session, "the diffs", |session| {
        session
            .file_state("src/a.rs")
            .and_then(FileState::highlights)
            .is_some()
    });
    session.read_with(cx, |session, _| {
        assert_eq!(
            new_side(session, "src/a.rs"),
            ["fn a() -> u32 {", "    2", "}"]
        );
        let highlights = session
            .file_state("src/a.rs")
            .unwrap()
            .highlights()
            .unwrap();
        let new = highlights.side(AnchorSide::New).expect("rust highlights");
        assert!(
            new.line(1)
                .iter()
                .any(|token| token.kind == TokenKind::Keyword),
            "{:?}",
            new.line(1)
        );
        // No grammar for .txt: loaded, never highlighted.
        let notes = session.file_state("notes.txt").unwrap();
        assert!(notes.diff().is_some());
        assert!(notes.highlights().is_none());
        assert!(matches!(
            session.file_state("src/b.rs"),
            Some(FileState::Unloaded)
        ));
    });
}

#[gpui::test]
fn a_worktree_edit_reloads_that_file_only(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let session = session(&repo, Target::Working, cx);
    wait(cx, &session, "the list", |session| !session.is_loading());
    let all = ["src/a.rs".to_string(), "src/b.rs".to_string()];
    session.update(cx, |session, cx| session.ensure_loaded(all.clone(), cx));
    wait(cx, &session, "both diffs", |session| {
        all.iter()
            .all(|path| session.file_state(path).and_then(FileState::diff).is_some())
    });
    let (b_before, generation) = session.read_with(cx, |session, _| {
        (
            session
                .file_state("src/b.rs")
                .unwrap()
                .diff()
                .unwrap()
                .clone(),
            session.generation(),
        )
    });

    repo.write("src/a.rs", "fn a() -> u32 {\n    3\n}\n");
    wait(cx, &session, "the edit to land", |session| {
        session.generation() > generation
            && session
                .file_state("src/a.rs")
                .and_then(FileState::diff)
                .is_some()
            && new_side(session, "src/a.rs")[1] == "    3"
    });
    session.read_with(cx, |session, _| {
        let b_after = session.file_state("src/b.rs").unwrap().diff().unwrap();
        assert!(
            Arc::ptr_eq(&b_before, b_after),
            "an untouched file keeps its diff"
        );
    });
}

#[gpui::test]
fn an_agent_commit_shrinks_the_working_list(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let session = session(&repo, Target::Working, cx);
    wait(cx, &session, "the list", |session| {
        session.files().len() == 3
    });
    // A commit changes HEAD, the index and a ref, but no worktree file.
    repo.git(&["add", "src/a.rs"]);
    repo.commit_staged("agent: a");
    wait(cx, &session, "the commit to land", |session| {
        session.files().len() == 2
    });
    session.read_with(cx, |session, _| {
        assert!(session.file("src/a.rs").is_none());
        assert_eq!(session.target_label(), "working tree vs main");
    });
}

#[gpui::test]
fn a_staged_review_follows_the_index(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let session = session(&repo, Target::Staged, cx);
    wait(cx, &session, "the list", |session| !session.is_loading());
    session.read_with(cx, |session, _| {
        assert!(session.files().is_empty());
        assert_eq!(
            session.watch.spec(),
            Some(WatchSpec {
                worktree: false,
                store: true,
                git: true
            })
        );
    });
    repo.git(&["add", "src/b.rs"]);
    wait(cx, &session, "the staged file", |session| {
        session.files().len() == 1
    });
}

#[gpui::test]
fn a_target_that_doesnt_resolve_says_why_and_recovers(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let feat = Target::Branch {
        branch: "feat".into(),
        base: Some("main".into()),
    };
    let session = session(&repo, feat, cx);
    wait(cx, &session, "the failure", |session| {
        session.error().is_some()
    });
    session.read_with(cx, |session, _| {
        let error = session.error().unwrap();
        assert!(error.message.contains("feat"), "{error:?}");
        assert!(!error.message.contains('\n'), "a one-line headline");
        assert!(session.files().is_empty());
        assert!(!session.is_loading());
    });
    // The branch appears (the agent creates it): the git-state watch refreshes the session.
    repo.branch("feat");
    repo.checkout("feat");
    wait(cx, &session, "the branch", |session| {
        session.error().is_none() && session.files().len() == 3
    });
    session.read_with(cx, |session, _| {
        assert_eq!(session.target_label(), "feat vs main");
    });
}

#[gpui::test]
fn stale_results_are_dropped(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let session = session(&repo, Target::Working, cx);
    cx.run_until_parked();
    // A load starts, and the file changes before its result is applied.
    let token = session.update(cx, |session, cx| {
        session.ensure_loaded(["src/a.rs".to_string()], cx);
        match session.file_state("src/a.rs") {
            Some(FileState::Loading { token }) => *token,
            other => panic!("loading, not {other:?}"),
        }
    });
    repo.write("src/a.rs", "fn a() -> u32 {\n    4\n}\n");
    session.update(cx, |session, cx| {
        session.refresh(Batch::paths(["src/a.rs".to_string()]), cx)
    });
    wait(cx, &session, "the reload", |session| {
        session
            .file_state("src/a.rs")
            .and_then(FileState::diff)
            .is_some()
            && new_side(session, "src/a.rs")[1] == "    4"
    });
    // The first load's result, landing now, is dropped: its file was invalidated since.
    let generation = session.update(cx, |session, cx| {
        let stale = Arc::new(FileDiff::Unmerged);
        session.finish_file_load("src/a.rs", token, Ok(stale), cx);
        assert_eq!(new_side(session, "src/a.rs")[1], "    4");
        session.generation()
    });
    // So is a refresh result from an older generation.
    session.update(cx, |session, cx| {
        session.apply_listed(
            generation - 1,
            Batch::everything(),
            Err(Failure::new("stale")),
            cx,
        );
        assert_eq!(session.error(), None);
        assert_eq!(session.files().len(), 3);
    });
}

#[gpui::test]
fn an_agents_append_reloads_the_fold(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let session = session(&repo, Target::Working, cx);
    wait(cx, &session, "the list", |session| !session.is_loading());
    session.update(cx, |session, cx| {
        session.ensure_loaded(["src/a.rs".to_string()], cx)
    });
    wait(cx, &session, "the diff", |session| {
        session
            .file_state("src/a.rs")
            .and_then(FileState::diff)
            .is_some()
    });
    let anchor = session
        .read_with(cx, |session, _| {
            session.anchor_for("src/a.rs", AnchorSide::New, 2, 2)
        })
        .unwrap();
    assert_eq!(anchor.text, ["    2"]);
    assert_eq!(anchor.context_before, ["fn a() -> u32 {"]);
    let posted = session.update(cx, |session, cx| {
        session.add_thread(anchor, "Why 2?".to_string(), cx)
    });
    wait(cx, &session, "the thread", |session| {
        session.review().live_threads().count() == 1
    });
    let thread = cx.foreground_executor().block_test(posted).unwrap();
    session.read_with(cx, |session, _| {
        let thread = session.review().thread(&thread).unwrap();
        assert_eq!(session.placement(thread).key(thread), "unchanged");
        assert_eq!(session.open_threads_in("src/a.rs"), 1);
    });

    // Another writer, as an agent would through the CLI: the watch reports the append.
    let agent_store = ReviewStore::new(backend(&repo));
    let review = session.read_with(cx, |session, _| session.review_id().clone());
    ops::set_thread_state(
        &agent_store,
        &review,
        &thread,
        ThreadState::Addressed,
        Some("done"),
        Author::agent("claude-code"),
        None,
    )
    .unwrap();
    wait(cx, &session, "the agent's change", |session| {
        session.review().thread(&thread).unwrap().state == ThreadState::Addressed
    });

    // The anchored line changes: the thread is outdated, not inline.
    repo.write("src/a.rs", "fn a() -> u32 {\n    5\n}\n");
    wait(cx, &session, "the edit", |session| {
        session
            .file_state("src/a.rs")
            .and_then(FileState::diff)
            .is_some()
            && new_side(session, "src/a.rs")[1] == "    5"
    });
    session.read_with(cx, |session, _| {
        let thread = session.review().thread(&thread).unwrap();
        assert_eq!(session.placement(thread).key(thread), "outdated");
    });
}

#[gpui::test]
fn the_target_is_named_in_full_before_the_first_listing(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let session = session(&repo, Target::Working, cx);
    session.read_with(cx, |session, _| {
        assert!(session.is_first_load());
        assert_eq!(session.target_label(), "working tree vs main");
    });
    wait(cx, &session, "the list", |session| {
        session.files().len() == 3
    });
    session.read_with(cx, |session, _| {
        assert_eq!(session.target_label(), "working tree vs main")
    });
}

#[gpui::test]
fn writes_go_through_ops_and_reload(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let session = session(&repo, Target::Working, cx);
    cx.run_until_parked();
    let viewed = session.update(cx, |session, cx| session.mark_viewed("src/a.rs", true, cx));
    wait(cx, &session, "viewed", |session| {
        session.is_viewed("src/a.rs")
    });
    assert!(cx.foreground_executor().block_test(viewed).unwrap());
    session.read_with(cx, |session, _| assert_eq!(session.viewed_count(), 1));
    let review = session.read_with(cx, |session, _| session.review_id().clone());
    let events = ReviewStore::new(backend(&repo)).events(&review).unwrap();
    assert!(
        events
            .events
            .iter()
            .any(|event| event.body.kind() == "viewed"),
        "the viewed event is in the log"
    );

    // Finishing the round snapshots it and moves the status; viewed starts over.
    let finished = session.update(cx, |session, cx| {
        session.finish_round(Some(Verdict::RequestChanges), Some("Fix a".into()), cx)
    });
    wait(cx, &session, "the round", |session| {
        session.review().status == ReviewStatus::ChangesRequested
    });
    let finished = cx.foreground_executor().block_test(finished).unwrap();
    assert_eq!(finished.round.n, 1);
    session.read_with(cx, |session, _| {
        assert_eq!(session.review().status, ReviewStatus::ChangesRequested);
        assert_eq!(session.review().current_round(), 2);
        assert_eq!(session.viewed_count(), 0);
        assert!(!session.is_read_only());
    });
    let approved = session.update(cx, |session, cx| {
        session.finish_round(Some(Verdict::Approve), None, cx)
    });
    wait(cx, &session, "the approval", ReviewSession::is_read_only);
    cx.foreground_executor().block_test(approved).unwrap();

    // A refused write reports why and still settles.
    let refused = session.update(cx, |session, cx| {
        session.finish_round(Some(Verdict::Approve), None, cx)
    });
    wait(cx, &session, "the refusal", |_| true);
    let failure = cx.foreground_executor().block_test(refused).unwrap_err();
    assert_eq!(failure.message, "the review is approved");
}

#[gpui::test]
fn load_anyway_lifts_the_caps_for_that_file(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = FixtureRepo::new();
    repo.write("big.rs", "fn x() {}\n");
    repo.commit("base");
    let big: String = (0..20_005).map(|n| format!("// {n}\n")).collect();
    repo.write("big.rs", &big);
    let session = session(&repo, Target::Working, cx);
    wait(cx, &session, "the list", |session| !session.is_loading());
    session.update(cx, |session, cx| {
        session.ensure_loaded(["big.rs".to_string()], cx)
    });
    wait(cx, &session, "the cap", |session| {
        session
            .file_state("big.rs")
            .and_then(FileState::diff)
            .is_some_and(|diff| diff_kind(diff) == "too_large")
    });
    session.update(cx, |session, cx| session.load_anyway("big.rs", cx));
    wait(cx, &session, "the whole file", |session| {
        session
            .file_state("big.rs")
            .and_then(FileState::diff)
            .is_some_and(|diff| diff_kind(diff) == "text")
    });
    session.read_with(cx, |session, _| {
        assert_eq!(text(session, "big.rs").new_line_count(), 20_005);
    });
}

#[test]
fn what_each_target_watches() {
    let oid = dv_core::Oid::parse(&"a".repeat(40)).unwrap();
    let worktree = DiffSpec {
        old: dv_core::Side::Commit(oid.clone()),
        new: dv_core::Side::Worktree,
    };
    let commits = DiffSpec {
        old: dv_core::Side::Commit(oid.clone()),
        new: dv_core::Side::Commit(oid.clone()),
    };
    let branch = Target::Branch {
        branch: "feat".into(),
        base: None,
    };
    assert_eq!(
        watch_spec(&Target::Working, Some(&worktree)),
        WatchSpec::ALL
    );
    let store_and_git = WatchSpec {
        worktree: false,
        store: true,
        git: true,
    };
    assert_eq!(
        watch_spec(&branch, Some(&commits)),
        store_and_git,
        "a branch not checked out here moves with its ref"
    );
    assert_eq!(
        watch_spec(&branch, None),
        store_and_git,
        "a target that didn't resolve waits for the git state to change"
    );
    assert_eq!(
        watch_spec(&Target::Working, None),
        WatchSpec::ALL,
        "a working target's new side is the working tree before it resolves too"
    );
    assert_eq!(
        watch_spec(&Target::Commit { rev: "x".into() }, Some(&commits)),
        WatchSpec::STORE
    );
}

/// A fixture's backend that records what the session asks of it: its git calls, and the
/// thread each watch handle is dropped on. `on_watch` runs before each watch registers, with
/// the call's number (from 0); returning an error fails that watch.
struct Probe {
    inner: Arc<dyn Backend>,
    git_calls: Mutex<Vec<String>>,
    /// `probe_path` calls that read bytes (`read` above 0).
    reads: AtomicUsize,
    watches: AtomicUsize,
    #[allow(clippy::type_complexity)]
    on_watch: Mutex<Box<dyn FnMut(usize) -> Result<(), BackendError> + Send>>,
    stopped_on: Arc<Mutex<Vec<ThreadId>>>,
}

impl Probe {
    fn new(
        repo: &FixtureRepo,
        on_watch: impl FnMut(usize) -> Result<(), BackendError> + Send + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner: backend(repo),
            git_calls: Mutex::new(Vec::new()),
            reads: AtomicUsize::new(0),
            watches: AtomicUsize::new(0),
            on_watch: Mutex::new(Box::new(on_watch)),
            stopped_on: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// The git calls since the last take, each its arguments joined by spaces.
    fn take_git_calls(&self) -> Vec<String> {
        std::mem::take(&mut *self.git_calls.lock().unwrap())
    }

    fn stopped_on(&self) -> Vec<ThreadId> {
        self.stopped_on.lock().unwrap().clone()
    }
}

/// Records the thread a watch handle is dropped on, then stops the watch.
struct StopProbe {
    _handle: WatchHandle,
    stopped_on: Arc<Mutex<Vec<ThreadId>>>,
}

impl Drop for StopProbe {
    fn drop(&mut self) {
        self.stopped_on
            .lock()
            .unwrap()
            .push(std::thread::current().id());
    }
}

impl Backend for Probe {
    fn repo(&self) -> &RepoInfo {
        self.inner.repo()
    }

    fn git(&self, args: &[&str], opts: &GitOpts) -> Result<GitOutput, BackendError> {
        self.git_calls.lock().unwrap().push(args.join(" "));
        self.inner.git(args, opts)
    }

    fn worktree_read(&self, rel: &str) -> Result<Option<Vec<u8>>, BackendError> {
        self.inner.worktree_read(rel)
    }

    fn worktree_len(&self, rel: &str) -> Result<Option<u64>, BackendError> {
        self.inner.worktree_len(rel)
    }

    fn worktree_text_stats(&self, rel: &str) -> Result<Option<TextStats>, BackendError> {
        self.inner.worktree_text_stats(rel)
    }

    fn probe_path(
        &self,
        path: &str,
        read: u64,
    ) -> Result<dv_core::backend::PathProbe, BackendError> {
        if read > 0 {
            self.reads.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.probe_path(path, read)
    }

    fn store_append(&self, rel: &str, line: &[u8]) -> Result<(), BackendError> {
        self.inner.store_append(rel, line)
    }

    fn store_read(&self, rel: &str) -> Result<Option<Vec<u8>>, BackendError> {
        self.inner.store_read(rel)
    }

    fn store_list(&self, dir: &str) -> Result<Vec<String>, BackendError> {
        self.inner.store_list(dir)
    }

    fn store_write_atomic(&self, rel: &str, bytes: &[u8]) -> Result<(), BackendError> {
        self.inner.store_write_atomic(rel, bytes)
    }

    fn store_remove(&self, rel: &str) -> Result<bool, BackendError> {
        self.inner.store_remove(rel)
    }

    fn copy_index_to_store(&self, rel: &str) -> Result<bool, BackendError> {
        self.inner.copy_index_to_store(rel)
    }

    fn watch(&self, spec: WatchSpec, sink: WatchSink) -> Result<WatchHandle, BackendError> {
        let call = self.watches.fetch_add(1, Ordering::SeqCst);
        (self.on_watch.lock().unwrap())(call)?;
        let inner = self.inner.watch(spec, sink)?;
        let worktree_error = inner.worktree_error().map(str::to_string);
        let handle = WatchHandle::new(StopProbe {
            _handle: inner,
            stopped_on: self.stopped_on.clone(),
        });
        Ok(match worktree_error {
            Some(message) => handle.with_worktree_error(message),
            None => handle,
        })
    }

    fn spawn_stdio(
        &self,
        spec: &dv_core::backend::ProcessSpec,
        sink: dv_core::backend::ProcessSink,
    ) -> Result<dv_core::backend::ProcessHandle, BackendError> {
        self.inner.spawn_stdio(spec, sink)
    }
}

/// A session over `target`'s review through `backend`, opened as the reviewer.
fn session_over(
    backend: Arc<dyn Backend>,
    target: Target,
    cx: &mut TestAppContext,
) -> Entity<ReviewSession> {
    let store = ReviewStore::new(backend.clone());
    let opened = ops::open_review(&store, target, reviewer()).unwrap();
    let loaded = store.load(&opened.review.id).unwrap();
    cx.new(|cx| {
        ReviewSession::new(
            OpenedReview {
                origin: backend.repo().location.clone(),
                backend,
                loaded,
                created: opened.created,
                reviewer: reviewer(),
                labels: LabelContext::default(),
                connection: None,
                pr: None,
            },
            cx,
        )
    })
}

fn paths(session: &ReviewSession) -> Vec<&str> {
    session.files().iter().map(|f| f.path.as_str()).collect()
}

#[gpui::test]
fn an_edit_landing_as_the_watch_starts_is_listed(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    // The agent writes a file just before the OS watch registers: no event ever reports it.
    let root = repo.path().to_path_buf();
    let probe = Probe::new(&repo, move |call| {
        if call == 0 {
            std::fs::write(root.join("late.txt"), "late\n").unwrap();
        }
        Ok(())
    });
    let session = session_over(probe.clone(), Target::Working, cx);
    wait(cx, &session, "the late file", |session| {
        session.file("late.txt").is_some()
    });
}

#[gpui::test]
fn reads_made_before_the_watch_registered_are_made_again(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    // No watch starts until `allowed`: the session lists and loads without one.
    let allowed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let probe = Probe::new(&repo, {
        let allowed = allowed.clone();
        move |_| match allowed.load(Ordering::SeqCst) {
            true => Ok(()),
            false => Err(BackendError::Watch {
                path: "repo".into(),
                message: "not now".into(),
            }),
        }
    });
    let session = session_over(probe.clone(), Target::Working, cx);
    wait(cx, &session, "the list", |session| {
        session.files().len() == 3 && session.watch_notice().is_some()
    });
    session.update(cx, |session, cx| {
        session.ensure_loaded(["src/a.rs".to_string()], cx)
    });
    wait(cx, &session, "the diff", |session| {
        session
            .file_state("src/a.rs")
            .and_then(FileState::diff)
            .is_some()
    });
    // Unwatched, the agent adds a file, edits a loaded one and comments in the review.
    repo.write("late.txt", "late\n");
    repo.write("src/a.rs", "fn a() -> u32 {\n    7\n}\n");
    let review = session.read_with(cx, |session, _| session.review_id().clone());
    let anchor = session
        .read_with(cx, |session, _| {
            session.anchor_for("src/a.rs", AnchorSide::New, 1, 1)
        })
        .unwrap();
    let agent = ReviewStore::new(backend(&repo));
    let thread = ops::add_thread(
        &agent,
        &review,
        anchor,
        "Note",
        Author::agent("claude-code"),
    )
    .unwrap();
    // A watch registers: what was read before it is read again.
    allowed.store(true, Ordering::SeqCst);
    session.update(cx, |session, cx| session.ensure_watch(cx));
    wait(cx, &session, "the catch-up", |session| {
        session.file("late.txt").is_some()
            && session.review().thread(&thread).is_some()
            && session
                .file_state("src/a.rs")
                .and_then(FileState::diff)
                .is_some_and(|_| new_side(session, "src/a.rs")[1] == "    7")
    });
    session.read_with(cx, |session, _| {
        assert_eq!(session.watch_notice(), None);
        assert_eq!(
            paths(session),
            ["late.txt", "notes.txt", "src/a.rs", "src/b.rs"]
        );
    });
}

#[gpui::test]
fn a_watch_stops_on_a_thread_of_its_own(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let probe = Probe::new(&repo, |_| Ok(()));
    let session = session_over(probe.clone(), Target::Working, cx);
    wait(cx, &session, "the watch", |session| {
        session.files().len() == 3 && session.watch.is_registered()
    });
    let ui_thread = std::thread::current().id();
    // A restart (what it covers changed) stops the old watch...
    session.update(cx, |session, cx| {
        session.watch.pretend_spec(WatchSpec::STORE);
        session.ensure_watch(cx);
    });
    wait(cx, &session, "the new watch", |session| {
        session.watch.is_registered()
    });
    // ...and so does closing the session.
    drop(session);
    cx.update(|_| {});
    cx.run_until_parked();
    let deadline = Instant::now() + Duration::from_secs(10);
    while probe.stopped_on().len() < 2 {
        assert!(Instant::now() < deadline, "the watches stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        probe.stopped_on().iter().all(|thread| *thread != ui_thread),
        "a watch's handle was dropped on the UI thread"
    );
}

#[gpui::test]
fn labels_are_read_again_only_when_the_git_state_moves(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let probe = Probe::new(&repo, |_| Ok(()));
    let session = session_over(probe.clone(), Target::Working, cx);
    wait(cx, &session, "the list", |session| {
        session.files().len() == 3
    });
    let labels_read = |calls: &[String]| calls.iter().any(|call| call.starts_with("symbolic-ref"));
    probe.take_git_calls();
    let generation = session.read_with(cx, |session, _| session.generation());
    session.update(cx, |session, cx| {
        session.refresh(Batch::paths(["src/a.rs".to_string()]), cx)
    });
    wait(cx, &session, "the worktree refresh", |session| {
        session.generation() > generation
    });
    let calls = probe.take_git_calls();
    assert!(
        !labels_read(&calls),
        "a worktree batch read the labels: {calls:?}"
    );
    let generation = session.read_with(cx, |session, _| session.generation());
    session.update(cx, |session, cx| {
        session.refresh(
            Batch {
                git: true,
                ..Batch::default()
            },
            cx,
        )
    });
    wait(cx, &session, "the git refresh", |session| {
        session.generation() > generation
    });
    assert!(labels_read(&probe.take_git_calls()));
}

#[gpui::test]
fn staging_a_file_keeps_its_loaded_diff(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let session = session(&repo, Target::Working, cx);
    wait(cx, &session, "the list", |session| {
        session.files().len() == 3
    });
    session.update(cx, |session, cx| {
        session.ensure_loaded(["src/a.rs".to_string()], cx)
    });
    wait(cx, &session, "the diff", |session| {
        session
            .file_state("src/a.rs")
            .and_then(FileState::diff)
            .is_some()
    });
    let (before, generation) = session.read_with(cx, |session, _| {
        (
            session
                .file_state("src/a.rs")
                .unwrap()
                .diff()
                .unwrap()
                .clone(),
            session.generation(),
        )
    });
    // `git add` changes the index alone: the entry's working tree id becomes known.
    repo.git(&["add", "src/a.rs"]);
    wait(cx, &session, "the refresh", |session| {
        session.generation() > generation
            && session
                .file("src/a.rs")
                .is_some_and(|entry| entry.new_oid.is_some())
    });
    session.read_with(cx, |session, _| {
        let after = session.file_state("src/a.rs").unwrap().diff().unwrap();
        assert!(Arc::ptr_eq(&before, after), "the staged file loaded again");
    });
}

#[gpui::test]
fn a_file_loading_again_keeps_showing_its_diff(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let session = session(&repo, Target::Working, cx);
    wait(cx, &session, "the list", |session| {
        session.files().len() == 3
    });
    session.update(cx, |session, cx| {
        session.ensure_loaded(["src/a.rs".to_string()], cx)
    });
    wait(cx, &session, "the diff", |session| {
        session
            .file_state("src/a.rs")
            .and_then(FileState::diff)
            .is_some()
    });
    let anchor = session
        .read_with(cx, |session, _| {
            session.anchor_for("src/a.rs", AnchorSide::New, 2, 2)
        })
        .unwrap();
    let posted = session.update(cx, |session, cx| {
        session.add_thread(anchor, "Why 2?".to_string(), cx)
    });
    wait(cx, &session, "the thread", |session| {
        session.review().live_threads().count() == 1
    });
    let thread = cx.foreground_executor().block_test(posted).unwrap();
    let before = session.read_with(cx, |session, _| {
        session
            .file_state("src/a.rs")
            .unwrap()
            .diff()
            .unwrap()
            .clone()
    });
    // The file loads again (Retry): until the new diff lands the old one shows, the thread
    // keeps its place, and a comment still anchors to the lines the reviewer sees.
    session.update(cx, |session, cx| session.reload_file("src/a.rs", cx));
    session.read_with(cx, |session, _| {
        assert!(session.file_state("src/a.rs").unwrap().diff().is_none());
        let shown = session.shown("src/a.rs").expect("the old diff stays shown");
        assert!(Arc::ptr_eq(&shown.diff, &before));
        let thread = session.review().thread(&thread).unwrap();
        assert_eq!(session.placement(thread).key(thread), "unchanged");
        assert!(
            session
                .anchor_for("src/a.rs", AnchorSide::New, 1, 1)
                .is_ok()
        );
    });
    wait(cx, &session, "the new diff", |session| {
        session
            .file_state("src/a.rs")
            .and_then(FileState::diff)
            .is_some_and(|diff| !Arc::ptr_eq(diff, &before))
    });
    session.read_with(cx, |session, _| {
        assert!(
            session.previous.is_empty(),
            "the old diff is let go once the new one lands"
        );
    });
}

#[gpui::test]
fn quick_viewed_toggles_each_flip_what_the_last_one_asked_for(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let session = session(&repo, Target::Working, cx);
    wait(cx, &session, "the list", |session| {
        session.files().len() == 3
    });
    // Two presses before the first lands: back where it started.
    session.update(cx, |session, cx| {
        session.toggle_viewed("src/a.rs", cx);
        session.toggle_viewed("src/a.rs", cx);
        assert!(!session.is_viewed("src/a.rs"), "nothing has landed yet");
        assert!(!session.intends_viewed("src/a.rs"));
    });
    wait(cx, &session, "the two toggles", |_| true);
    session.read_with(cx, |session, _| {
        assert!(!session.is_viewed("src/a.rs"));
        assert!(!session.intends_viewed("src/a.rs"));
    });
    // Three: viewed.
    session.update(cx, |session, cx| {
        for _ in 0..3 {
            session.toggle_viewed("src/a.rs", cx);
        }
        assert!(session.intends_viewed("src/a.rs"));
    });
    wait(cx, &session, "the three toggles", |_| true);
    session.read_with(cx, |session, _| {
        assert!(session.is_viewed("src/a.rs"));
        assert_eq!(session.viewed_count(), 1);
    });
    // The log agrees: what another reader of the store folds.
    let review = session.read_with(cx, |session, _| session.review_id().clone());
    let stored = ReviewStore::new(backend(&repo))
        .load(&review)
        .unwrap()
        .review;
    assert!(stored.is_viewed(1, "src/a.rs"));
}

#[gpui::test]
fn a_failed_viewed_toggle_says_why_until_one_lands(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    let repo = fixture();
    let session = session(&repo, Target::Working, cx);
    wait(cx, &session, "the list", |session| {
        session.files().len() == 3
    });
    let review = session.read_with(cx, |session, _| session.review_id().clone());
    let log = repo
        .path()
        .join(".git")
        .join("dv")
        .join("reviews")
        .join(format!("{review}.jsonl"));
    let set_read_only = |read_only: bool| {
        let mut permissions = std::fs::metadata(&log).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(read_only);
        std::fs::set_permissions(&log, permissions).unwrap();
    };
    set_read_only(true);
    if std::fs::OpenOptions::new().append(true).open(&log).is_ok() {
        set_read_only(false);
        eprintln!("skipped: this user writes read-only files");
        return;
    }
    session.update(cx, |session, cx| session.toggle_viewed("src/a.rs", cx));
    wait(cx, &session, "the failure", |session| {
        session.viewed_error().is_some()
    });
    session.read_with(cx, |session, _| {
        let error = session.viewed_error().unwrap();
        assert!(
            error
                .message
                .starts_with("src/a.rs couldn't be marked viewed: "),
            "{error:?}"
        );
        assert!(!session.is_viewed("src/a.rs"));
    });
    set_read_only(false);
    session.update(cx, |session, cx| session.toggle_viewed("src/a.rs", cx));
    wait(cx, &session, "the toggle", |session| {
        session.is_viewed("src/a.rs")
    });
    session.read_with(cx, |session, _| assert_eq!(session.viewed_error(), None));
}
