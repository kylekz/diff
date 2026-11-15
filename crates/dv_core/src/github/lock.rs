//! The lock an open holds while it finds or makes a pull request's draft (docs/PLAN.md §9.3):
//! "at most one open draft per pull request" (PLAN §2.4) between processes, the app and the
//! CLI opening the same pull request at the same moment.
//!
//! The store has no lock of its own. A ref has what one needs: git creates it only while it
//! doesn't exist and deletes it only while it holds a given value, each atomically for every
//! process of the repository, as the pins' compare-and-swap relies on (`review::pins`).
//!
//! - **The lock** is `refs/dv/opening/<owner>/<repo>/<n>`, outside `refs/dv/pr/`, which goes
//!   with a pull request's last review. It points at a blob of the holder's own, its
//!   **token**: a fresh ULID, so random and stamped with the time it was made.
//! - **Taking** is `git update-ref <lock> <token> <zeros>`: only while no lock stands. git's
//!   "reference already exists" is another holder. Any other "cannot lock ref" may be
//!   another process at the same ref this instant (a release removes the lock's emptied
//!   folders under a take that needs them) and is tried again, [`UNSURE_TAKES`] times; what
//!   fails still, and any other failure of git, is reported.
//! - **Releasing** is `git update-ref -d <lock> <token>`: only the holder's own lock, tried
//!   once more when it fails. One that was taken over meanwhile is left alone.
//! - **Taking over** a lock its holder left behind (a process that died): a waiter that has
//!   seen the same token for the whole of its wait ([`LOCK_WAIT`]; finding or making a draft
//!   takes a moment) deletes it by compare-and-swap, `git update-ref -d <lock> <that
//!   token>`, and takes its turn like any other. A token that changes under a waiter starts
//!   its wait over: someone else holds the lock now. No clock but the waiter's own is read,
//!   so two machines' clocks (Windows and a distro) need not agree.
//! - **A lock nobody waits for** (its pull request never opened again) goes with the next
//!   sweep ([`sweep`]), once its token is older than [`ABANDONED`].
//!
//! A released token stays in the object store as a loose blob of a few bytes, unreachable,
//! until git's own garbage collection takes it.

use std::time::{Duration, Instant};

use ulid::Ulid;

use super::error::GithubError;
use super::spelling::spell;
use crate::backend::{Backend, BackendError, GitOpts};
use crate::git::{GitError, GitRepo, Oid};
use crate::review::PrLink;
use crate::review::event::now_ms;

/// How long a waiter sees one token before it takes the lock over, and how often it looks.
pub(super) const LOCK_WAIT: Duration = Duration::from_secs(10);
const LOCK_POLL: Duration = Duration::from_millis(50);

/// How many takes in a row may fail on the ref's lock, other than on a holder, before the
/// failure is git's own.
const UNSURE_TAKES: u32 = 3;

/// How old a lock's token is before a sweep removes it: far longer than any find-or-create,
/// and than the clocks of two machines sharing a repository are likely to differ by.
pub(super) const ABANDONED: Duration = Duration::from_secs(60);

/// Where every pull request's lock is.
const LOCKS: &str = "refs/dv/opening/";

/// The ref that stands while a draft of `link`'s pull request is found or made.
pub(super) fn lock_ref(link: &PrLink) -> String {
    format!(
        "{LOCKS}{}/{}/{}",
        spell(&link.owner),
        spell(&link.repo),
        link.number
    )
}

/// A new token in the repository's object store: a blob holding a fresh ULID.
pub(super) fn token(git: &GitRepo) -> Result<Oid, GithubError> {
    let output = git.backend().git(
        &["hash-object", "-w", "--stdin"],
        &GitOpts::new().with_stdin(format!("{}\n", Ulid::generate())),
    )?;
    let text = String::from_utf8_lossy(&output.stdout);
    Oid::parse(text.trim()).ok_or_else(|| {
        GitError::UnexpectedOutput {
            command: "hash-object",
            detail: format!("not an object id: {:?}", text.trim()),
        }
        .into()
    })
}

/// Runs `find_or_make` holding the lock of `link`'s pull request (module docs), waiting for
/// whoever holds it and taking over a lock whose token stood for all of `wait`.
pub(super) fn locked<T>(
    git: &GitRepo,
    link: &PrLink,
    wait: Duration,
    find_or_make: impl FnOnce() -> Result<T, GithubError>,
) -> Result<T, GithubError> {
    let name = lock_ref(link);
    let own = token(git)?;
    let zeros = "0".repeat(git.object_format().hex_len());
    let held = || -> Result<Option<Oid>, GithubError> { Ok(git.ref_oids(&[&name])?.remove(&name)) };
    let delete = |token: &Oid| {
        git.backend().git(
            &["update-ref", "-d", &name, token.as_str()],
            &GitOpts::new(),
        )
    };
    // The token this waiter found the lock holding, and since when.
    let mut seen: Option<(Oid, Instant)> = None;
    // How many takes in a row failed on the ref's lock for another reason than a holder.
    let mut unsure = 0;
    loop {
        match git.backend().git(
            &["update-ref", &name, own.as_str(), &zeros],
            &GitOpts::new(),
        ) {
            Ok(_) => break,
            Err(BackendError::Git { stderr, .. })
                if stderr.contains("reference already exists") =>
            {
                unsure = 0;
            }
            // Another process at the same ref: a release removes the lock's emptied folders,
            // and a take that lands then can't make them ("unable to create directory").
            // Over in a moment when it is that; git's own failure when it stays.
            Err(BackendError::Git { stderr, .. })
                if stderr.contains("cannot lock ref") && unsure < UNSURE_TAKES =>
            {
                unsure += 1;
                std::thread::sleep(LOCK_POLL);
                continue;
            }
            Err(error) => return Err(error.into()),
        }
        match (held()?, &seen) {
            // Let go between the two calls.
            (None, _) => seen = None,
            (Some(token), Some((stood, since))) if token == *stood => {
                if since.elapsed() >= wait {
                    // Its holder is gone. Deleted only while it is still that holder's: a
                    // delete that fails with the lock unchanged is git's own failure.
                    if let Err(error) = delete(&token)
                        && held()?.as_ref() == Some(&token)
                    {
                        return Err(error.into());
                    }
                    seen = None;
                    continue;
                }
            }
            (Some(token), _) => seen = Some((token, Instant::now())),
        }
        std::thread::sleep(LOCK_POLL);
    }
    let found = find_or_make();
    // Once more when it fails: left standing, the lock costs the next opener its whole wait.
    if delete(&own).is_err()
        && let Err(error) = delete(&own)
    {
        log::warn!("{link}: the lock on its draft wasn't this opener's to release: {error}");
    }
    found
}

/// Removes every lock whose token is older than [`ABANDONED`], or which git reads as
/// something that is no token: left by a process that died, for a pull request nobody
/// opened again. Each is deleted only while it still holds the token read here. Best
/// effort: a lock git fails to read, or to remove, stays for the next sweep.
pub(crate) fn sweep(backend: &dyn Backend) -> Result<(), BackendError> {
    let output = backend.git(
        &["for-each-ref", "--format=%(refname) %(objectname)", LOCKS],
        &GitOpts::new(),
    )?;
    let text = String::from_utf8_lossy(&output.stdout);
    let now = now_ms();
    let abandoned_after = u64::try_from(ABANDONED.as_millis()).unwrap_or(u64::MAX);
    for (name, token) in text.lines().filter_map(|line| line.split_once(' ')) {
        // Only what git reads and says is no token of dv's, or an old one, is abandoned: a
        // git that fails to read it leaves the lock alone.
        let Ok(kind) = backend.git(&["cat-file", "-t", token], &GitOpts::new()) else {
            continue;
        };
        let abandoned = if String::from_utf8_lossy(&kind.stdout).trim() == "blob" {
            let Ok(blob) = backend.git(&["cat-file", "blob", token], &GitOpts::new()) else {
                continue;
            };
            match Ulid::from_string(String::from_utf8_lossy(&blob.stdout).trim()) {
                Ok(token) => now.saturating_sub(token.timestamp_ms()) >= abandoned_after,
                Err(_) => true,
            }
        } else {
            true
        };
        if abandoned
            && let Err(error) = backend.git(&["update-ref", "-d", name, token], &GitOpts::new())
        {
            log::debug!("{name}: an abandoned lock wasn't removed: {error}");
        }
    }
    Ok(())
}
