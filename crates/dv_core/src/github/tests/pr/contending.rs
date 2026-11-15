//! Openers and fetches of one pull request at once, and a backend that fails under an open
//! (PLAN §9.3): one draft however many openers, a ref's lock another fetch holds, a
//! connection that dies, and nothing left behind by an open that fails.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use dv_test_support::FixtureRepo;

use super::*;
use crate::backend::{
    BackendError, GitOpts, GitOutput, LocalBackend, ProcessHandle, ProcessSink, ProcessSpec,
    RepoInfo, WatchHandle, WatchSink, WatchSpec,
};
use crate::github::forget;
use crate::github::lock::{ABANDONED, LOCK_WAIT, lock_ref, locked, token};
use crate::review::{ListEntry, PrLink};

/// A backend whose next `git fetch` calls (or whichever git calls `fails` picks) fail with
/// the errors it was given, in order, whose appends to the store take `slow` longer, whose
/// first `git fetch` that succeeds is followed by the removal of `vanishing` refs, whose
/// first read of the store's file `rewritten` names is followed by another process
/// replacing that file, and which is the fixture's own otherwise.
pub(super) struct FetchFails {
    inner: LocalBackend,
    fails: fn(&[&str]) -> bool,
    failures: Mutex<Vec<BackendError>>,
    slow: Duration,
    vanishing: Mutex<Vec<String>>,
    rewritten: Mutex<Option<(String, Vec<u8>)>>,
}

impl FetchFails {
    /// The fixture's backend whose git calls `fails` picks fail with `failures`, in order.
    pub(super) fn failing_at(
        repo: &FixtureRepo,
        fails: fn(&[&str]) -> bool,
        mut failures: Vec<BackendError>,
    ) -> Arc<dyn Backend> {
        failures.reverse();
        Arc::new(Self {
            inner: open_backend(repo),
            fails,
            failures: Mutex::new(failures),
            slow: Duration::ZERO,
            vanishing: Mutex::default(),
            rewritten: Mutex::default(),
        })
    }

    fn failing(repo: &FixtureRepo, mut failures: Vec<BackendError>) -> Arc<dyn Backend> {
        failures.reverse();
        Arc::new(Self {
            inner: open_backend(repo),
            fails: is_fetch,
            failures: Mutex::new(failures),
            slow: Duration::ZERO,
            vanishing: Mutex::default(),
            rewritten: Mutex::default(),
        })
    }

    /// The fixture's backend beside a process that removes `refs` right after this one's
    /// first fetch: an open of the same pull request that failed there.
    fn vanishing(repo: &FixtureRepo, refs: &[String]) -> Arc<dyn Backend> {
        Arc::new(Self {
            inner: open_backend(repo),
            fails: is_fetch,
            failures: Mutex::default(),
            slow: Duration::ZERO,
            vanishing: Mutex::new(refs.to_vec()),
            rewritten: Mutex::default(),
        })
    }

    /// The fixture's backend beside a process that writes `bytes` to the store's file `rel`
    /// right after this one first reads it.
    pub(super) fn rewriting(repo: &FixtureRepo, rel: &str, bytes: Vec<u8>) -> Arc<dyn Backend> {
        Arc::new(Self {
            inner: open_backend(repo),
            fails: is_fetch,
            failures: Mutex::default(),
            slow: Duration::ZERO,
            vanishing: Mutex::default(),
            rewritten: Mutex::new(Some((rel.to_string(), bytes))),
        })
    }

    /// The fixture's backend, a slow disk under its store: every append `slow` late.
    fn slow(repo: &FixtureRepo, slow: Duration) -> Arc<dyn Backend> {
        Arc::new(Self {
            inner: open_backend(repo),
            fails: is_fetch,
            failures: Mutex::default(),
            slow,
            vanishing: Mutex::default(),
            rewritten: Mutex::default(),
        })
    }
}

impl Backend for FetchFails {
    fn repo(&self) -> &RepoInfo {
        self.inner.repo()
    }
    fn git(&self, args: &[&str], opts: &GitOpts) -> Result<GitOutput, BackendError> {
        if (self.fails)(args)
            && let Some(failure) = self.failures.lock().unwrap().pop()
        {
            return Err(failure);
        }
        let output = self.inner.git(args, opts)?;
        if args.contains(&"fetch") {
            for name in self.vanishing.lock().unwrap().drain(..) {
                self.inner
                    .git(&["update-ref", "-d", &name], &GitOpts::new())?;
            }
        }
        Ok(output)
    }
    fn worktree_read(&self, rel: &str) -> Result<Option<Vec<u8>>, BackendError> {
        self.inner.worktree_read(rel)
    }
    fn worktree_len(&self, rel: &str) -> Result<Option<u64>, BackendError> {
        self.inner.worktree_len(rel)
    }
    fn store_append(&self, rel: &str, line: &[u8]) -> Result<(), BackendError> {
        std::thread::sleep(self.slow);
        self.inner.store_append(rel, line)
    }
    fn store_read(&self, rel: &str) -> Result<Option<Vec<u8>>, BackendError> {
        let read = self.inner.store_read(rel)?;
        let mut rewritten = self.rewritten.lock().unwrap();
        if rewritten.as_ref().is_some_and(|(path, _)| path == rel)
            && let Some((path, bytes)) = rewritten.take()
        {
            self.inner.store_write_atomic(&path, &bytes)?;
        }
        Ok(read)
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
        self.inner.watch(spec, sink)
    }
    fn spawn_stdio(
        &self,
        spec: &ProcessSpec,
        sink: ProcessSink,
    ) -> Result<ProcessHandle, BackendError> {
        self.inner.spawn_stdio(spec, sink)
    }
}

fn is_fetch(args: &[&str]) -> bool {
    args.contains(&"fetch")
}

/// A take of a pull request's lock: `update-ref <lock> <token> <zeros>`.
fn is_take(args: &[&str]) -> bool {
    matches!(args, ["update-ref", name, _, _] if name.starts_with("refs/dv/opening/"))
}

/// A release of one: `update-ref -d <lock> <token>`.
fn is_release(args: &[&str]) -> bool {
    matches!(args, ["update-ref", "-d", name, _] if name.starts_with("refs/dv/opening/"))
}

/// git's failure when a take lands as a release removes the lock's emptied folders.
fn no_folder() -> BackendError {
    BackendError::Git {
        args: vec!["update-ref".to_string()],
        code: Some(128),
        stderr: "fatal: update_ref failed for ref 'refs/dv/opening/dv-fixture/api/7': cannot \
                 lock ref 'refs/dv/opening/dv-fixture/api/7': unable to create directory for \
                 .git/refs/dv/opening/dv-fixture/api/7\n"
            .to_string(),
    }
}

/// git's failure when a ref it must write is locked by another git.
fn ref_locked() -> BackendError {
    BackendError::Git {
        args: vec!["fetch".to_string()],
        code: Some(1),
        stderr: "error: cannot lock ref 'refs/dv/pr/dv-fixture/api/7/head': Unable to create \
                 '/r/.git/refs/dv/pr/dv-fixture/api/7/head.lock': File exists.\n"
            .to_string(),
    }
}

fn link() -> PrLink {
    PrLink {
        owner: GITHUB_OWNER.to_string(),
        repo: "api".to_string(),
        number: NUMBER,
    }
}

/// The store's open drafts.
fn drafts(fixture: &PrFixture) -> Vec<crate::review::ReviewId> {
    fixture
        .store()
        .list()
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry {
            ListEntry::Ok(summary) if summary.status.is_open() => Some(summary.id),
            _ => None,
        })
        .collect()
}

#[test]
fn openers_at_once_end_with_the_one_draft() {
    let fixture = PrFixture::new();
    // Each opener its own process, as the app and the CLI are: its own backend and `gh`.
    // A draft's log is slow to write, so every opener has looked for one before any is made.
    let mut calls = vec![fixture_gh::viewer(VIEWER)];
    calls.extend(fixture.pr.calls("api"));
    let clone = fixture.github.checkout();
    let openers: Vec<(Arc<dyn Backend>, MockGh)> = (0..6)
        .map(|_| {
            (
                FetchFails::slow(clone, Duration::from_millis(150)),
                MockGh::new(Value::Array(calls.clone())),
            )
        })
        .collect();
    let opened: Vec<OpenedPr> = std::thread::scope(|scope| {
        let running: Vec<_> = openers
            .iter()
            .map(|(backend, mock)| {
                scope.spawn(move || open(backend, &mock.gh, &PrRef::Hash(NUMBER), reviewer))
            })
            .collect();
        running
            .into_iter()
            .map(|opener| opener.join().unwrap().unwrap())
            .collect()
    });
    // One draft, which every opener returned, and one of them made.
    let drafts = drafts(&fixture);
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    for opener in &opened {
        assert_eq!(opener.review.id, drafts[0]);
        assert!(opener.refreshed.stale.is_none(), "{:?}", opener.refreshed);
    }
    assert_eq!(opened.iter().filter(|opener| opener.created).count(), 1);
    assert_eq!(fixture.pins(&opened[0].review).head, fixture.head());
    // No lock is left.
    assert_eq!(fixture.refs("refs/dv/opening/"), []);
}

/// A blob holding a token made `age` ago, as a lock taken then points at.
fn token_aged(git: &GitRepo, age: Duration) -> Oid {
    let made = crate::review::event::now_ms() - u64::try_from(age.as_millis()).unwrap();
    let output = git
        .backend()
        .git(
            &["hash-object", "-w", "--stdin"],
            &GitOpts::new().with_stdin(format!("{}\n", ulid::Ulid::from_parts(made, 7))),
        )
        .unwrap();
    oid(String::from_utf8_lossy(&output.stdout).trim())
}

/// Has pull request 7's lock hold `token`, as a holder that is gone left it.
fn leave_lock(fixture: &PrFixture, token: &Oid) {
    fixture
        .github
        .checkout()
        .git(&["update-ref", &lock_ref(&link()), token.as_str()]);
}

#[test]
fn a_lock_its_holder_left_behind_is_taken_over_by_one_waiter_at_a_time() {
    let fixture = PrFixture::new();
    let git = fixture.git();
    leave_lock(&fixture, &token(&git).unwrap());
    // Two openers find it together and wait it out together. Each makes the draft only when
    // there is none, slowly, so two of them inside at once would make two.
    let wait = Duration::from_millis(400);
    let (inside, drafts) = (Mutex::new(0_u32), Mutex::new(0_u32));
    let waiters = [fixture.git(), fixture.git()];
    let started = Instant::now();
    std::thread::scope(|scope| {
        for git in &waiters {
            scope.spawn(|| {
                locked(git, &link(), wait, || {
                    {
                        let mut inside = inside.lock().unwrap();
                        *inside += 1;
                        assert_eq!(*inside, 1, "one holder at a time");
                    }
                    let none = *drafts.lock().unwrap() == 0;
                    std::thread::sleep(Duration::from_millis(150));
                    if none {
                        *drafts.lock().unwrap() += 1;
                    }
                    *inside.lock().unwrap() -= 1;
                    Ok(())
                })
                .unwrap()
            });
        }
    });
    assert!(started.elapsed() >= wait, "{:?}", started.elapsed());
    assert_eq!(*drafts.lock().unwrap(), 1);
    assert_eq!(fixture.refs("refs/dv/opening/"), []);

    // Free, it is taken at once, and released whatever the work came to.
    let started = Instant::now();
    let failed: Result<(), _> = locked(&git, &link(), LOCK_WAIT, || {
        assert_eq!(fixture.refs("refs/dv/opening/").len(), 1);
        Err(GithubError::Moved { number: NUMBER })
    });
    assert!(matches!(failed, Err(GithubError::Moved { .. })));
    assert!(started.elapsed() < LOCK_WAIT);
    assert_eq!(fixture.refs("refs/dv/opening/"), []);
}

#[test]
fn a_lock_that_changes_hands_starts_the_wait_over() {
    let fixture = PrFixture::new();
    let git = fixture.git();
    leave_lock(&fixture, &token(&git).unwrap());
    // Half-way through the wait another holder has it: no take-over at the first deadline.
    let wait = Duration::from_millis(600);
    let other = token(&git).unwrap();
    let (elsewhere, name) = (fixture.git(), lock_ref(&link()));
    let started = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(wait / 2);
            elsewhere
                .backend()
                .git(&["update-ref", &name, other.as_str()], &GitOpts::new())
                .unwrap();
        });
        locked(&git, &link(), wait, || Ok(())).unwrap();
    });
    assert!(
        started.elapsed() >= wait + wait / 2,
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_release_leaves_a_lock_that_is_someone_elses() {
    let fixture = PrFixture::new();
    let git = fixture.git();
    let other = token(&git).unwrap();
    // Taken over while this holder was still at work: its release removes nothing.
    locked(&git, &link(), LOCK_WAIT, || {
        leave_lock(&fixture, &other);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        fixture.refs("refs/dv/opening/"),
        [(lock_ref(&link()), other.to_string())]
    );
}

#[test]
fn a_git_that_fails_for_another_reason_is_reported_at_once() {
    let fixture = PrFixture::new();
    let git = fixture.git();
    // A ref where the lock's folder goes: git can't create the lock, and not because
    // someone holds it.
    fixture.github.checkout().git(&[
        "update-ref",
        "refs/dv/opening/dv-fixture/api",
        fixture.main().as_str(),
    ]);
    let started = Instant::now();
    let error = locked(&git, &link(), LOCK_WAIT, || Ok(())).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5), "{error:?}");
    assert_eq!(error.kind(), "git");
    assert!(
        !error
            .git_said()
            .unwrap()
            .contains("reference already exists"),
        "{error:?}"
    );
}

#[test]
fn a_take_that_lands_under_a_release_is_tried_again() {
    let fixture = PrFixture::new();
    let clone = fixture.github.checkout();
    // Once, and three times running: over each time, the lock is taken and released.
    for failures in [1, 3] {
        let backend =
            FetchFails::failing_at(clone, is_take, (0..failures).map(|_| no_folder()).collect());
        let started = Instant::now();
        let found = locked(&GitRepo::new(backend), &link(), LOCK_WAIT, || {
            assert_eq!(fixture.refs("refs/dv/opening/").len(), 1);
            Ok(7)
        })
        .unwrap();
        assert_eq!(found, 7);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(fixture.refs("refs/dv/opening/"), []);
    }
    // A fourth time it is git's own failure, reported well before any take-over.
    let backend = FetchFails::failing_at(clone, is_take, (0..4).map(|_| no_folder()).collect());
    let started = Instant::now();
    let error = locked(&GitRepo::new(backend), &link(), LOCK_WAIT, || Ok(())).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5), "{error:?}");
    assert!(
        error
            .git_said()
            .unwrap()
            .contains("unable to create directory"),
        "{error:?}"
    );
    assert_eq!(fixture.refs("refs/dv/opening/"), []);
}

#[test]
fn a_release_that_fails_is_tried_once_more() {
    let fixture = PrFixture::new();
    let backend = FetchFails::failing_at(fixture.github.checkout(), is_release, vec![ref_locked()]);
    locked(&GitRepo::new(backend), &link(), LOCK_WAIT, || Ok(())).unwrap();
    assert_eq!(fixture.refs("refs/dv/opening/"), []);
}

#[test]
fn the_sweep_leaves_a_lock_git_fails_to_read() {
    let fixture = PrFixture::new();
    let git = fixture.git();
    leave_lock(&fixture, &token_aged(&git, ABANDONED * 2));
    // git fails under the read (the object store out of reach, say): nothing says the
    // lock is abandoned.
    fn is_read(args: &[&str]) -> bool {
        args.first() == Some(&"cat-file")
    }
    let failed = BackendError::Git {
        args: vec!["cat-file".to_string()],
        code: Some(128),
        stderr: "fatal: unable to read object\n".to_string(),
    };
    let backend = FetchFails::failing_at(fixture.github.checkout(), is_read, vec![failed]);
    crate::github::lock::sweep(backend.as_ref()).unwrap();
    assert_eq!(fixture.refs("refs/dv/opening/").len(), 1);
    // Read, it is an old token: gone.
    crate::github::lock::sweep(backend.as_ref()).unwrap();
    assert_eq!(fixture.refs("refs/dv/opening/"), []);
}

#[test]
fn the_sweep_drops_locks_nobody_holds_any_more() {
    let fixture = PrFixture::new();
    let git = fixture.git();
    let lock = |number: u64| lock_ref(&PrLink { number, ..link() });
    let set = |number: u64, value: &str| {
        fixture
            .github
            .checkout()
            .git(&["update-ref", &lock(number), value]);
    };
    // One just taken, one left long ago, and a ref there that is no lock's token.
    let fresh = token(&git).unwrap();
    set(7, fresh.as_str());
    set(8, token_aged(&git, ABANDONED * 2).as_str());
    set(9, fixture.main().as_str());
    forget::unreviewed(&fixture.store()).unwrap();
    assert_eq!(
        fixture.refs("refs/dv/opening/"),
        [(lock(7), fresh.to_string())]
    );
    // Deleting a review sweeps too.
    set(8, token_aged(&git, ABANDONED * 2).as_str());
    let local = fixture
        .store()
        .create(crate::target::Target::Working, reviewer())
        .unwrap();
    fixture.store().delete(&local.id).unwrap();
    assert_eq!(
        fixture.refs("refs/dv/opening/"),
        [(lock(7), fresh.to_string())]
    );
}

#[test]
fn a_fetch_that_finds_a_ref_locked_runs_once_more() {
    let fixture = PrFixture::new();
    let clone = fixture.github.checkout();
    let once = FetchFails::failing(clone, vec![ref_locked()]);
    let opened = open(&once, &fixture.mock.gh, &PrRef::Hash(NUMBER), reviewer).unwrap();
    assert!(opened.created);
    assert_eq!(fixture.pins(&opened.review).head, fixture.head());

    // Locked again the second time, the failure is the fetch's.
    let twice = FetchFails::failing(clone, vec![ref_locked(), ref_locked()]);
    let refreshed = refresh(&twice, &fixture.mock.gh, &opened.review);
    let stale = refreshed.stale.unwrap();
    assert_eq!(stale.kind(), "git");
    assert_eq!(
        stale.git_said(),
        Some(
            "cannot lock ref 'refs/dv/pr/dv-fixture/api/7/head': Unable to create \
             '/r/.git/refs/dv/pr/dv-fixture/api/7/head.lock': File exists."
        )
    );
}

#[test]
fn a_lost_connection_is_the_opens_failure_though_a_review_is_here() {
    let fixture = PrFixture::new();
    let first = fixture.open();
    let lost = || BackendError::Disconnected {
        location: "Ubuntu".to_string(),
        reason: "the connection closed".to_string(),
    };
    // Nothing about GitHub: the caller opens again once the connection is back (PLAN §5.2),
    // rather than be handed the review as it was last read.
    let backend = FetchFails::failing(fixture.github.checkout(), vec![lost()]);
    let error = open(&backend, &fixture.mock.gh, &PrRef::Hash(NUMBER), reviewer).unwrap_err();
    assert!(error.is_disconnected(), "{error:?}");
    assert!(
        matches!(
            error,
            GithubError::Backend(BackendError::Disconnected { .. })
        ),
        "{error:?}"
    );
    // Any other failure of the fetch opens the review that is here.
    let failed = BackendError::Git {
        args: vec!["fetch".to_string()],
        code: Some(128),
        stderr: "fatal: unable to access 'https://github.com/dv-fixture/api.git/': Could not \
                 resolve host: github.com\n"
            .to_string(),
    };
    let backend = FetchFails::failing(fixture.github.checkout(), vec![failed]);
    let opened = open(&backend, &fixture.mock.gh, &PrRef::Hash(NUMBER), reviewer).unwrap();
    assert_eq!(opened.review.id, first.review.id);
    let stale = opened.refreshed.stale.unwrap();
    assert!(!stale.is_disconnected());
    assert_eq!(
        stale.git_said(),
        Some(
            "unable to access 'https://github.com/dv-fixture/api.git/': Could not resolve \
             host: github.com"
        )
    );
}

#[test]
fn an_open_that_fails_leaves_nothing_it_fetched() {
    let fixture = PrFixture::new();
    // The commits are fetched, and then the store can't take the draft: a file where its
    // folder goes.
    let reviews = fixture.github.checkout().path().join(".git/dv/reviews");
    std::fs::create_dir_all(reviews.parent().unwrap()).unwrap();
    std::fs::write(&reviews, "").unwrap();
    let error = fixture.open_ref(&PrRef::Hash(NUMBER)).unwrap_err();
    assert!(matches!(error, GithubError::Store(_)), "{error:?}");
    std::fs::remove_file(&reviews).unwrap();
    assert_eq!(fixture.refs("refs/dv/"), []);
    assert!(fixture.store().list().unwrap().is_empty());

    // The head keeps moving under the mirror's reads, after the review was made: the draft
    // stays, stale, and so do its commits, until it is deleted.
    fixture.answer(vec![serde_json::json!({
        "argv": ["pr", "view", "7", "--json", "headRefOid", "**"],
        "stdout": {"headRefOid": "f".repeat(40)}
    })]);
    let opened = fixture.open();
    assert!(matches!(
        opened.refreshed.stale,
        Some(GithubError::Moved { .. })
    ));
    assert_eq!(fixture.refs("refs/dv/pr/").len(), 2);
    fixture.store().delete(&opened.review.id).unwrap();
    assert_eq!(fixture.refs("refs/dv/"), []);
    fixture.answer(Vec::new());

    // With a review of the pull request here, a failed open keeps what that review needs.
    let opened = fixture.open();
    fixture.answer(vec![fixture_gh::signed_out()]);
    fixture.open();
    assert_eq!(fixture.refs("refs/dv/pr/").len(), 2);
    assert_eq!(fixture.pins(&opened.review).head, fixture.head());
}

#[test]
fn an_open_that_fails_touches_no_other_pull_request() {
    let fixture = PrFixture::new();
    // Another process is opening pull request 8: fetched, its review not made yet.
    let fetched = ["base", "head"].map(|name| {
        let name = format!("refs/dv/pr/dv-fixture/api/8/{name}");
        fixture
            .github
            .checkout()
            .git(&["update-ref", &name, fixture.main().as_str()]);
        (name, fixture.main().to_string())
    });
    // And one of pull request 70, whose refs start as pull request 7's do.
    let seventy = "refs/dv/pr/dv-fixture/api/70/head".to_string();
    fixture
        .github
        .checkout()
        .git(&["update-ref", &seventy, fixture.main().as_str()]);

    // Pull request 7's open fails after its fetch.
    let reviews = fixture.github.checkout().path().join(".git/dv/reviews");
    std::fs::create_dir_all(reviews.parent().unwrap()).unwrap();
    std::fs::write(&reviews, "").unwrap();
    fixture.open_ref(&PrRef::Hash(NUMBER)).unwrap_err();
    std::fs::remove_file(&reviews).unwrap();
    let mut left = fetched.to_vec();
    left.insert(0, (seventy, fixture.main().to_string()));
    assert_eq!(fixture.refs("refs/dv/pr/"), left);
}

#[test]
fn refs_that_vanish_under_the_fetch_are_fetched_once_more() {
    let fixture = PrFixture::new();
    let refs = ["head", "base"].map(|name| format!("refs/dv/pr/dv-fixture/api/7/{name}"));
    // An open of the same pull request fails elsewhere and removes what it fetched, between
    // this one's fetch and its read of the refs.
    let backend = FetchFails::vanishing(fixture.github.checkout(), &refs);
    let opened = open(&backend, &fixture.mock.gh, &PrRef::Hash(NUMBER), reviewer).unwrap();
    assert!(opened.created);
    assert_eq!(fixture.pins(&opened.review).head, fixture.head());
    assert_eq!(fixture.refs("refs/dv/pr/").len(), 2);
}

#[test]
fn a_draft_whose_pins_cant_be_written_is_left_and_the_error_says_so() {
    let fixture = PrFixture::new();
    // The pins' transaction fails once (the disk, a hook): the draft is already made.
    fn is_pin(args: &[&str]) -> bool {
        args == ["update-ref", "--stdin"]
    }
    let failed = BackendError::Git {
        args: vec!["update-ref".to_string(), "--stdin".to_string()],
        code: Some(128),
        stderr: "fatal: unable to write new index file\n".to_string(),
    };
    let backend = FetchFails::failing_at(fixture.github.checkout(), is_pin, vec![failed]);
    let error = open(&backend, &fixture.mock.gh, &PrRef::Hash(NUMBER), reviewer).unwrap_err();
    let GithubError::DraftLeft { review, source } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(source.kind(), "git");
    assert_eq!(error.kind(), "git");
    let message = error.to_string();
    assert!(
        message.starts_with(&format!("a draft was made ({review})")),
        "{message}"
    );
    assert!(
        message.contains(&format!("`dv review delete {review}`")),
        "{message}"
    );
    // The one line says what git said where it says what failed, before what to do.
    let line = error.one_line();
    let (said, advice) = (
        line.find("unable to write new index file")
            .expect("git's words"),
        line.find("; open the pull request again")
            .expect("the advice"),
    );
    assert!(said < advice, "{line}");
    assert!(
        line.ends_with(&format!("`dv review delete {review}`")),
        "{line}"
    );
    // Nothing was swept: the draft is the pull request's review, and its commits stay.
    let reviews = fixture.store().list().unwrap();
    assert_eq!(reviews.len(), 1);
    assert_eq!(fixture.refs("refs/dv/pr/").len(), 2);
    // Opening again resumes that draft, and its refresh pins it.
    let opened = fixture.open();
    assert!(!opened.created);
    assert_eq!(opened.review.id.to_string(), *review);
    assert_eq!(fixture.store().list().unwrap().len(), 1);
    assert_eq!(opened.refreshed.stale.as_ref().map(|e| e.to_string()), None);
    assert_eq!(fixture.pins(&opened.review).head, fixture.head());
}
