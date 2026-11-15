//! The `state` document's pull-request parts (`tests/e2e/README.md` "dv's `state`"):
//! `pr_picker` (the PR picker, `Ctrl G`) and `pr_strip` (the strip under the title bar,
//! PLAN §9.9).

use dv_ui::pr_picker::{ListState, RowKind, STATE_ROWS, listed, repo_title};
use dv_ui::session::{Commits, MoveKind};
use dv_ui::shell::pr_strip::{ReadLine, state_label};
use dv_ui::{Failure, Shell};
use gpui::{App, Window};
use serde_json::{Value, json};

fn failure(failure: &Failure) -> Value {
    json!({"message": failure.message, "detail": failure.detail})
}

/// `state.pr_picker`.
pub fn pr_picker(shell: &Shell, window: &Window, cx: &App) -> Value {
    let Some(picker) = shell.pr_picker() else {
        return json!({
            "open": false,
            "query": null,
            "repo": null,
            "github": null,
            "list": null,
            "loading": false,
            "listed": null,
            "failure": null,
            "unmarked": null,
            "rows": [],
            "selected": null,
            "notice": null,
        });
    };
    let picker = picker.read(cx);
    let list = picker.list(cx);
    let rows: Vec<Value> = picker
        .rows()
        .iter()
        .take(STATE_ROWS)
        .map(|row| match &row.kind {
            RowKind::Reference {
                name,
                title,
                clone,
                location,
                ..
            } => json!({
                "key": row.key,
                "kind": "reference",
                "name": name,
                "title": title,
                "clone": clone,
                "repo": location.display_name(),
            }),
            RowKind::ChooseFolder { repository, .. } => json!({
                "key": row.key,
                "kind": "choose_folder",
                "repository": repository,
            }),
            RowKind::Listed { pr, .. } => json!({
                "key": row.key,
                "kind": "listed",
                "number": pr.number,
                // As drawn: one line each, whatever GitHub's text holds.
                "title": pr.title,
                "author": pr.author,
                "draft": pr.draft,
                "review_requested": pr.review_requested,
                "updated_at": pr.updated_at,
            }),
        })
        .collect();
    json!({
        "open": true,
        "focused": picker.is_focused(window, cx),
        "query": picker.query(),
        // The repository in view, and its name over the list.
        "repo": picker.location().map(|location| location.display_name()),
        "github": picker.location().map(|location| repo_title(location, list)),
        // What stands where the list goes.
        "list": picker.list_state().key(),
        "loading": list.is_some_and(|list| list.loading),
        "listed": listed(list).map(|prs| prs.len()),
        "failure": match (picker.list_state(), list.and_then(|list| list.failure.as_ref())) {
            (ListState::Failed(failed), _) | (_, Some(failed)) => json!({
                "kind": failed.kind.key(),
                "message": failed.failure.message,
                "detail": failed.failure.detail,
            }),
            _ => Value::Null,
        },
        // Why the list's pull requests aren't marked as asking for the viewer's review.
        "unmarked": list
            .and_then(|list| list.unmarked.as_ref())
            .map(|failure| failure.message.clone()),
        // Whether the field names another repository's pull request: the repository in
        // view's line and its list's state aren't drawn over that one's rows.
        "another_repository": picker.names_another_repository(),
        "rows": rows,
        "selected": picker.selected_key(),
        "notice": picker.notice(),
    })
}

/// `state.pr_strip`: `null` unless a PR review is open.
pub fn pr_strip(shell: &Shell, cx: &App) -> Value {
    let Some(strip) = shell.pr_strip(cx) else {
        return Value::Null;
    };
    let now = dv_core::review::event::now_ms();
    json!({
        "number": strip.number,
        "title": strip.title,
        "author": strip.author,
        "branches": strip.branches,
        "state": strip.state.map(state_label),
        "checks": strip.checks,
        "round": strip.round,
        "moved": strip.moved.as_ref().map(|moved| json!({
            "kind": match moved.kind {
                MoveKind::Ahead => "ahead",
                MoveKind::Followed => "followed",
            },
            "from": moved.from,
            "to": moved.to,
            "commits": match moved.commits {
                Some(Commits::New(n)) => json!(n),
                Some(Commits::Rewritten) => json!("rewritten"),
                Some(Commits::Earlier) => json!("earlier"),
                None => Value::Null,
            },
            "text": strip.moved_text(),
        })),
        "read": {
            "kind": strip.read.key(),
            "text": strip.read.text(now),
            "at": match &strip.read {
                ReadLine::Read(at) => json!(at),
                _ => Value::Null,
            },
            "failure": strip.read.failure().map(failure),
        },
        "refreshing": strip.refreshing,
        // Whether the failed refresh shows whole (`banner/pr`, or the open's own banner
        // about this review).
        "error_open": shell.pr_error_open(cx),
        "url": strip.url,
        "checks_url": strip.checks_url,
    })
}
