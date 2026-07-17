//! `dv review wait` — block until review activity changes, then report what
//! changed (docs/backlog.md: "turns agent loops from polling into
//! submit -> wait -> respond"; pairs with the `dv skill` deliverable).
//!
//! Built on [`ReviewStore::watch`], the same change notification the GUI
//! uses (real file watching locally, dv-host inotify or the digest-poll
//! fallback for WSL repos), so this blocks without polling wherever the GUI
//! would. The watcher only says "something happened" — the actual change
//! report comes from diffing a full store snapshot taken after the watcher
//! is armed (so nothing between arm and snapshot can be missed) against a
//! fresh listing on each wake. Wakes with no observable difference (the
//! watcher fires on any directory event, including our own snapshot's
//! reads on some platforms) loop silently rather than reporting nothing.
//!
//! Exit codes extend the family contract: `0` something changed (report on
//! stdout), `3` the `--timeout` elapsed first (`{"wait":{"outcome":
//! "timeout"}}` in `--json` mode), `1`/`2` the usual operation/usage
//! errors.

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dv_core::{Comment, Review, ReviewStore};
use serde_json::{Value, json};

use crate::{CliError, REVIEW_USAGE, op_err, print_json, resolve_repo, usage_err};

/// How long to keep draining follow-up watcher events after the first wake
/// before snapshotting — a store save is a tempfile write + rename, which
/// some platforms deliver as several events. Bounded by [`DEBOUNCE_ROUNDS`]
/// so a pathological event storm can't starve the deadline forever.
const DEBOUNCE: Duration = Duration::from_millis(200);
const DEBOUNCE_ROUNDS: usize = 10;

pub(crate) struct WaitArgs {
    /// Only report (and exit on) changes to this review.
    pub id: Option<String>,
    pub timeout: Option<Duration>,
}

pub(crate) fn parse_review_wait(args: &[String]) -> Result<WaitArgs, String> {
    let mut id = None;
    let mut timeout = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--timeout" => {
                let value = iter.next().ok_or("--timeout requires <seconds>")?;
                let secs: f64 = value
                    .parse()
                    .map_err(|_| format!("--timeout: not a number: {value:?}"))?;
                if !secs.is_finite() || secs <= 0.0 {
                    return Err(format!(
                        "--timeout must be a positive number, got {value:?}"
                    ));
                }
                timeout = Some(Duration::from_secs_f64(secs));
            }
            other if !other.starts_with("--") && id.is_none() => id = Some(other.to_string()),
            other => return Err(format!("review wait: unexpected argument {other:?}")),
        }
    }
    Ok(WaitArgs { id, timeout })
}

pub(crate) fn cmd_review_wait(
    args: &[String],
    json: bool,
    location: Option<dv_core::RepoLocation>,
) -> Result<(), CliError> {
    let parsed = parse_review_wait(args).map_err(|reason| usage_err(reason, REVIEW_USAGE))?;
    let repo = resolve_repo(location)?;
    let store = ReviewStore::open(repo.location().clone());

    // Arm the watcher BEFORE the baseline snapshot: an external write that
    // lands between the two wakes the loop, whose fresh listing then
    // differs from the baseline — nothing is missed. The other order would
    // silently swallow exactly that window.
    let (tx, rx) = mpsc::channel::<()>();
    let _watcher = store
        .watch(Box::new(move || {
            let _ = tx.send(());
        }))
        .map_err(op_err)?;
    let mut before = snapshot(&store, parsed.id.as_deref())?;
    if let Some(want) = &parsed.id
        && !before.contains_key(want)
    {
        return Err(CliError::Op(format!("no review with id {want:?}")));
    }

    let deadline = parsed.timeout.map(|t| Instant::now() + t);
    loop {
        let woke = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                !remaining.is_zero() && rx.recv_timeout(remaining).is_ok()
            }
            None => rx
                .recv()
                .map_err(|_| CliError::Op("review watcher stopped unexpectedly".to_string()))
                .map(|()| true)?,
        };
        if !woke {
            eprintln!("timed out waiting for review activity");
            if json {
                print_json(&json!({ "wait": { "outcome": "timeout" } }));
            }
            return Err(CliError::Exit(3));
        }
        for _ in 0..DEBOUNCE_ROUNDS {
            if rx.recv_timeout(DEBOUNCE).is_err() {
                break;
            }
        }
        let after = snapshot(&store, parsed.id.as_deref())?;
        let changes = diff_snapshots(&before, &after);
        if !changes.is_empty() {
            report(&changes, json);
            return Ok(());
        }
        before = after;
    }
}

/// `review_id -> Review` for the whole store (or just `id`), keyed for
/// order-stable diffing.
fn snapshot(store: &ReviewStore, id: Option<&str>) -> Result<BTreeMap<String, Review>, CliError> {
    let reviews = store.list().map_err(op_err)?;
    Ok(reviews
        .into_iter()
        .filter(|r| id.is_none_or(|want| r.id == want))
        .map(|r| (r.id.clone(), r))
        .collect())
}

/// One reported change to one review. Serialized as the stable `--json`
/// shape documented on [`report`]; `summary` carries the human line.
struct Change {
    value: Value,
    summary: String,
}

fn diff_snapshots(
    before: &BTreeMap<String, Review>,
    after: &BTreeMap<String, Review>,
) -> Vec<Change> {
    let mut changes = Vec::new();
    for (id, old) in before {
        match after.get(id) {
            None => changes.push(Change {
                value: json!({ "review_id": id, "kind": "deleted" }),
                summary: format!("review {id} deleted"),
            }),
            Some(new) => {
                if let Some(change) = diff_review(old, new) {
                    changes.push(change);
                }
            }
        }
    }
    for (id, new) in after {
        if !before.contains_key(id) {
            changes.push(Change {
                value: json!({ "review_id": id, "kind": "created", "review": new }),
                summary: format!("review {id} created ({} comments)", new.comments.len()),
            });
        }
    }
    changes
}

fn diff_review(old: &Review, new: &Review) -> Option<Change> {
    if serde_json::to_value(old).ok() == serde_json::to_value(new).ok() {
        return None;
    }
    let old_by_id: BTreeMap<&str, &Comment> =
        old.comments.iter().map(|c| (c.id.as_str(), c)).collect();
    let new_by_id: BTreeMap<&str, &Comment> =
        new.comments.iter().map(|c| (c.id.as_str(), c)).collect();

    let comments_added: Vec<&Comment> = new
        .comments
        .iter()
        .filter(|c| !old_by_id.contains_key(c.id.as_str()))
        .collect();
    let comments_removed: Vec<&str> = old
        .comments
        .iter()
        .filter(|c| !new_by_id.contains_key(c.id.as_str()))
        .map(|c| c.id.as_str())
        .collect();
    let mut status_changed = Vec::new();
    let mut comments_updated = Vec::new();
    for (id, new_c) in &new_by_id {
        let Some(old_c) = old_by_id.get(id) else {
            continue;
        };
        if old_c.status != new_c.status {
            status_changed.push(json!({
                "id": id,
                "from": old_c.status,
                "to": new_c.status,
            }));
        }
        // Anything else about the thread (body edit, new reply) — status
        // transitions are reported above, so exclude status AND its
        // side-effect timestamp bump from this comparison, keeping the two
        // categories disjoint (a pure resolve is a status change, not a
        // thread update).
        let strip = |c: &Comment| {
            let mut v = serde_json::to_value(c).unwrap_or_default();
            if let Some(map) = v.as_object_mut() {
                map.remove("status");
                map.remove("updated_ms");
            }
            v
        };
        if strip(old_c) != strip(new_c) {
            comments_updated.push((*id).to_string());
        }
    }
    let state_changed = (old.state != new.state).then(|| {
        json!({
            "from": old.state,
            "to": new.state,
        })
    });

    let mut parts = Vec::new();
    if !comments_added.is_empty() {
        parts.push(format!("+{} comment(s)", comments_added.len()));
    }
    if !comments_removed.is_empty() {
        parts.push(format!("-{} comment(s)", comments_removed.len()));
    }
    for s in &status_changed {
        parts.push(format!(
            "{} {} -> {}",
            s["id"].as_str().unwrap_or("?"),
            s["from"].as_str().unwrap_or("?"),
            s["to"].as_str().unwrap_or("?")
        ));
    }
    if !comments_updated.is_empty() {
        parts.push(format!("{} thread(s) updated", comments_updated.len()));
    }
    if state_changed.is_some() {
        parts.push("state changed".to_string());
    }
    if parts.is_empty() {
        // The serialized JSON differed but nothing we classify did (e.g.
        // only `updated_ms` moved) — still a change worth reporting rather
        // than a silent wake, but keep the summary honest.
        parts.push("metadata updated".to_string());
    }

    Some(Change {
        value: json!({
            "review_id": new.id,
            "kind": "updated",
            "comments_added": comments_added,
            "comments_removed": comments_removed,
            "status_changed": status_changed,
            "comments_updated": comments_updated,
            "state_changed": state_changed,
        }),
        summary: format!("review {} updated: {}", new.id, parts.join(", ")),
    })
}

/// stdout contract: `{"wait":{"outcome":"changed","changes":[...]}}` where
/// each change is `{"review_id", "kind": "created"|"deleted"|"updated",
/// ...}` — `created` carries the full `review`, `updated` carries
/// `comments_added` (full comment objects), `comments_removed` (ids),
/// `status_changed` (`{id, from, to}`), `comments_updated` (ids: body
/// edits / new replies), `state_changed` (`{from, to}` or null).
fn report(changes: &[Change], json: bool) {
    if json {
        print_json(&json!({
            "wait": {
                "outcome": "changed",
                "changes": changes.iter().map(|c| c.value.clone()).collect::<Vec<_>>(),
            }
        }));
    } else {
        for change in changes {
            println!("{}", change.summary);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dv_core::{DiffSource, Side};

    fn draft() -> Review {
        // `new_draft` is pub(crate) to dv-core; build via the store-less
        // constructor path instead: deserialize a minimal JSON review.
        serde_json::from_value(json!({
            "v": 1,
            "id": "r-1-aaaa",
            "source": "WorkingTree",
            "state": "draft",
            "created_ms": 1,
            "updated_ms": 1,
            "comments": [],
        }))
        .expect("valid review json")
    }

    fn add_comment(review: &mut Review, body: &str) -> String {
        review
            .add_comment("src/a.ts", Side::New, 1, 1, None, body, "tester")
            .expect("valid comment")
            .id
            .clone()
    }

    #[test]
    fn parse_accepts_id_and_timeout_anywhere() {
        let args: Vec<String> = ["--timeout", "2.5", "r-1-aaaa"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parsed = parse_review_wait(&args).expect("parses");
        assert_eq!(parsed.id.as_deref(), Some("r-1-aaaa"));
        assert_eq!(parsed.timeout, Some(Duration::from_secs_f64(2.5)));
    }

    #[test]
    fn parse_rejects_bad_timeout_and_extra_positional() {
        for bad in [
            vec!["--timeout"],
            vec!["--timeout", "abc"],
            vec!["--timeout", "0"],
            vec!["--timeout", "-1"],
            vec!["r-1", "r-2"],
            vec!["--frobnicate"],
        ] {
            let args: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert!(parse_review_wait(&args).is_err(), "should reject {bad:?}");
        }
        // sanity: DiffSource is reachable so the json fixture stays honest
        let _ = std::mem::size_of::<DiffSource>();
    }

    #[test]
    fn diff_reports_created_and_deleted() {
        let r = draft();
        let empty = BTreeMap::new();
        let with: BTreeMap<String, Review> = [(r.id.clone(), r.clone())].into();
        let created = diff_snapshots(&empty, &with);
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].value["kind"], "created");
        let deleted = diff_snapshots(&with, &empty);
        assert_eq!(deleted.len(), 1);
        assert_eq!(deleted[0].value["kind"], "deleted");
    }

    #[test]
    fn diff_classifies_added_resolved_and_replied() {
        let mut old = draft();
        let kept = add_comment(&mut old, "please fix");
        let mut new = old.clone();
        // resolve the kept comment, add a fresh one, and reply to the kept
        new.set_status(&kept, dv_core::CommentStatus::Resolved)
            .expect("status");
        new.reply(&kept, "done", "agent").expect("reply");
        let added = add_comment(&mut new, "one more thing");

        let before: BTreeMap<String, Review> = [(old.id.clone(), old)].into();
        let after: BTreeMap<String, Review> = [(new.id.clone(), new)].into();
        let changes = diff_snapshots(&before, &after);
        assert_eq!(changes.len(), 1);
        let v = &changes[0].value;
        assert_eq!(v["kind"], "updated");
        assert_eq!(v["comments_added"][0]["id"], added.as_str());
        assert_eq!(v["status_changed"][0]["id"], kept.as_str());
        assert_eq!(v["status_changed"][0]["to"], "resolved");
        assert_eq!(v["comments_updated"][0], kept.as_str());
        assert!(v["state_changed"].is_null());
    }

    #[test]
    fn identical_reviews_produce_no_change() {
        let r = draft();
        let map: BTreeMap<String, Review> = [(r.id.clone(), r)].into();
        assert!(diff_snapshots(&map, &map.clone()).is_empty());
    }
}
