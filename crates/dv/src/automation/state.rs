//! The `state` document's parts (`tests/e2e/README.md` "dv's `state`" lists every key).

use dv_backend::ConnectionState;
use dv_core::diff::{FileEntry, FileStatus};
use dv_core::review::{AnchorSide, Author, ReviewStatus, Thread};
use dv_ui::diff_pane::PlaceRef;
use dv_ui::services::reviews::{OpenRequest, RepoRow, ReviewRow};
use dv_ui::session::{Comparison, FileState, ReviewSession, diff_kind};
use dv_ui::submit::SummaryTab;
use dv_ui::switcher::MenuTarget;
use dv_ui::switcher::items::{ItemKind, ReviewBadge};
use dv_ui::system::SystemRequest;
use dv_ui::{ActiveServices as _, Failure, Material, Shell};
use gpui::{App, Focusable as _, Window};
use serde_json::{Value, json};

/// `state.settings`.
pub fn settings(cx: &App) -> Value {
    let settings = cx.services().settings.read(cx);
    let resolved = dv_ui::appearance::resolved(cx);
    let available: Vec<&str> = gpui_appearance::state(cx)
        .support()
        .available()
        .into_iter()
        .map(Material::key)
        .collect();
    json!({
        "palette": settings.palette().key(),
        "material": settings.material().key(),
        "mode": settings.mode().key(),
        "resolved_palette": resolved.palette.key(),
        "resolved_material": resolved.material.key(),
        "resolved_mode": resolved.appearance.key(),
        "available_materials": available,
        "path": settings.path().display().to_string(),
        "save_error": settings.last_error(),
    })
}

/// What AppKit reports for `window` (macOS; `null` elsewhere): its own appearance (`null`
/// follows the system's), `isOpaque`, and `gpui_appearance`'s vibrancy view with its style.
fn window_native(window: &Window) -> Value {
    gpui_appearance::native(window).map_or(Value::Null, |native| {
        json!({
            "forced": native.forced.map(|appearance| appearance.key()),
            "opaque": native.opaque,
            "vibrancy_view": native.vibrancy_view,
            "vibrancy": native.vibrancy.map(|vibrancy| json!({
                "material": vibrancy.material.key(),
                "blending": vibrancy.blending.key(),
                "state": vibrancy.state.key(),
            })),
        })
    })
}

/// `state.shell`, `window` its window.
pub fn shell(shell: &Shell, window: &Window, cx: &App) -> Value {
    let status = shell.status_line(cx);
    json!({
        "panel_visible": shell.panel_visible(),
        "panel_width": cx.services().settings.read(cx).panel_width(),
        "empty_state": shell.empty_state(cx).map(|empty| empty.key()),
        "window_material": shell.applied_material().key(),
        "window_native": window_native(window),
        "opening": shell.opening().is_some(),
        "pending": shell.pending_pick().map(|pending| json!({
            "repo": pending.location.display_name(),
            "target": match &pending.request {
                OpenRequest::Target(target) => json!(target),
                OpenRequest::Review(_) => Value::Null,
            },
            "review": match &pending.request {
                OpenRequest::Review(id) => json!(id),
                OpenRequest::Target(_) => Value::Null,
            },
            "connection": pending.connection().name(),
        })),
        "banner": shell.banner().map(|banner| failure(Some(&banner.failure))),
        "backend_banner": shell.backend_down(cx).map(|down| json!({
            "message": down.failure.message,
            "detail": down.failure.detail,
            "action": down.action,
        })),
        "status": {
            "health": status.health.key(),
            "location": status.location,
            "review": status.review,
            "finish": status.finish,
        },
    })
}

fn badge(badge: Option<&ReviewBadge>) -> Value {
    match badge {
        Some(badge) => json!({
            "id": badge.id,
            "status": badge.status.as_str(),
            "round": badge.round,
            "open_threads": badge.open_threads,
        }),
        None => Value::Null,
    }
}

/// `state.switcher`.
pub fn switcher(shell: &Shell, cx: &App) -> Value {
    let Some(switcher) = shell.switcher() else {
        return json!({
            "open": false,
            "mode": null,
            "query": null,
            "selected": null,
            "items": [],
            "menu": null,
            "closing": null,
            "close_failure": null,
        });
    };
    let switcher = switcher.read(cx);
    let listing = switcher.listing();
    let items: Vec<Value> = listing
        .items
        .iter()
        .map(|item| {
            let repo = item.repo.and_then(|repo| listing.repos.get(repo));
            json!({
                "key": item.key,
                "kind": item.kind_key(),
                "target": match &item.kind {
                    ItemKind::Target(offer) | ItemKind::Base { offer, .. } => Some(offer.kind.key()),
                    _ => None,
                },
                "label": item.label(),
                "detail": item.detail(),
                "starting": item.is_starting(),
                "repo": repo.map(|repo| repo.path.clone()),
                "review": badge(item.review()),
            })
        })
        .collect();
    // A repository header's menu (`repo`, `removable`) or a review row's (`row`, its key).
    let menu = switcher.menu().map(|menu| match &menu.target {
        MenuTarget::Repo(location) => json!({
            "repo": location.display_name(),
            "removable": switcher.can_remove(location),
        }),
        MenuTarget::Review { row } => json!({"row": row}),
    });
    json!({
        "open": true,
        "mode": switcher.mode().key(),
        "query": switcher.query(),
        "selected": switcher.selected_key(),
        "items": items,
        "menu": menu,
        // The row asked to close its review (`Ctrl Shift W`), and why the last close failed.
        "closing": switcher.closing(),
        "close_failure": switcher.close_failure(),
    })
}

/// `state.source_picker`.
pub fn source_picker(shell: &Shell, cx: &App) -> Value {
    let Some(picker) = shell.source_picker() else {
        return json!({"open": false, "selected": null, "items": [], "compare": false});
    };
    let picker = picker.read(cx);
    let items: Vec<Value> = picker
        .offers()
        .iter()
        .map(|offer| {
            json!({
                "kind": offer.kind.key(),
                "label": offer.label,
                "review": badge(offer.review.as_ref()),
            })
        })
        .collect();
    let selected = if picker.compare_selected() {
        Some("compare")
    } else {
        picker.selected().map(|offer| offer.kind.key())
    };
    json!({
        "open": true,
        "selected": selected,
        "items": items,
        "compare": picker.offers_compare(),
    })
}

/// `state.file_list`.
pub fn file_list(shell: &Shell, cx: &App) -> Value {
    let list = shell.file_list().read(cx);
    json!({
        "query": list.query(),
        "shown": list.shown_paths(cx),
        "current_file": shell.diff_pane().read(cx).current_file(),
    })
}

/// `state.system`.
pub fn system(cx: &App) -> Value {
    let (requests, answers_left) = dv_ui::system::requests(cx);
    let requests: Vec<Value> = requests
        .iter()
        .map(|request| match request {
            SystemRequest::OpenUrl(url) => json!({"kind": request.kind(), "url": url}),
            SystemRequest::Reveal(path) => {
                json!({"kind": request.kind(), "path": path.display().to_string()})
            }
            SystemRequest::PickFolder { prompt } => {
                json!({"kind": request.kind(), "prompt": prompt.as_ref()})
            }
            SystemRequest::Clipboard(text) => json!({"kind": request.kind(), "text": text}),
        })
        .collect();
    json!({"requests": requests, "answers_left": answers_left})
}

/// `state.backends`: the app's connections to `dv backend --stdio` servers, WSL distros first.
pub fn backends(cx: &App) -> Value {
    let connections = cx.services().locations.read(cx).connections().all();
    Value::Array(
        connections
            .iter()
            .map(|connection| {
                let state = connection.state();
                json!({
                    "name": connection.name(),
                    "state": connection_state(&state),
                    "attempt": match state {
                        ConnectionState::Reconnecting { attempt } => Some(attempt),
                        _ => None,
                    },
                    "generation": connection.generation(),
                    "error": match &state {
                        ConnectionState::Failed { error } => Some(error.clone()),
                        _ => None,
                    },
                })
            })
            .collect(),
    )
}

/// A connection's state as one word: `idle`, `connecting`, `installing`, `connected`,
/// `reconnecting`, `stopped` or `failed`.
pub fn connection_state(state: &ConnectionState) -> &'static str {
    match state {
        ConnectionState::Idle => "idle",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Installing { .. } => "installing",
        ConnectionState::Connected => "connected",
        ConnectionState::Reconnecting { .. } => "reconnecting",
        ConnectionState::Stopped => "stopped",
        ConnectionState::Failed { .. } => "failed",
    }
}

fn failure(failure: Option<&Failure>) -> Value {
    match failure {
        Some(failure) => json!({"message": failure.message, "detail": failure.detail}),
        None => Value::Null,
    }
}

/// `state.repos`: the known repositories, most recently used first.
pub fn repos(cx: &App) -> Value {
    let reviews = cx.services().reviews.read(cx);
    Value::Array(reviews.rows().into_iter().map(repo).collect())
}

fn repo(row: &RepoRow) -> Value {
    let reviews: Vec<Value> = row
        .reviews
        .iter()
        .map(|review| match review {
            ReviewRow::Ok { summary, label } => json!({
                "id": summary.id,
                "target": summary.target,
                "target_label": label,
                "status": summary.status.as_str(),
                "round": summary.current_round,
                "threads": {
                    "open": summary.threads.open,
                    "addressed": summary.threads.addressed,
                    "resolved": summary.threads.resolved,
                    "annotation": summary.threads.annotation,
                },
            }),
            ReviewRow::Err { id, error } => json!({"id": id, "error": failure(Some(error))}),
        })
        .collect();
    json!({
        "path": row.path,
        "name": row.name,
        "location": row.location,
        "loading": row.loading,
        "error": failure(row.error.as_ref()),
        "stopped": row.stopped,
        "retry": row.retry,
        "reconnecting": row.reconnecting,
        "starting": row.starting,
        "working": row.working.map(|working| json!({
            "files": working.files,
            "additions": working.additions,
            "deletions": working.deletions,
        })),
        "reviews": reviews,
    })
}

/// `state.session`: the open review, or `null`.
pub fn session(shell: &Shell, cx: &App) -> Value {
    let Some(session) = shell.session() else {
        return Value::Null;
    };
    let session = session.read(cx);
    let review = session.review();
    let files: Vec<Value> = session
        .files()
        .iter()
        .map(|entry| file(session, entry))
        .collect();
    let threads: Vec<Value> = review
        .live_threads()
        .map(|thread| thread_state(session, thread))
        .collect();
    let rounds: Vec<Value> = review
        .rounds
        .iter()
        .map(|round| {
            json!({
                "n": round.n,
                "verdict": round.verdict.map(|verdict| verdict.as_str()),
                "old_oid": round.old_oid,
                "new_oid": round.new_oid,
                "body": round.body,
            })
        })
        .collect();
    json!({
        "review_id": review.id,
        "repo": session.backend().repo().toplevel,
        "target": review.target,
        "target_label": session.target_label(),
        "status": status(review.status),
        "round": review.shown_round(),
        "read_only": session.is_read_only(),
        "loading": session.is_loading(),
        "error": failure(session.error()),
        "viewed_error": failure(session.viewed_error()),
        "watch_notice": session.watch_notice(),
        "generation": session.generation(),
        "warnings": session.warnings().len(),
        "files": files,
        "viewed": session.viewed_count(),
        "total": session.files().len(),
        "current_file": shell.diff_pane().read(cx).current_file(),
        "view_mode": shell.diff_pane().read(cx).view_mode().key(),
        // Which pane `cursor`, `selection`, `composer` and `focused_thread` describe.
        "pane": shown_pane(shell),
        "cursor": shell.main_pane().read(cx).cursor().map(|cursor| json!({
            "path": cursor.path,
            "side": side(cursor.side),
            "line": cursor.line,
        })),
        "selection": shell.main_pane().read(cx).selection().map(|selection| json!({
            "path": selection.path,
            "side": side(selection.side),
            "start_line": selection.start_line,
            "end_line": selection.end_line,
        })),
        "threads": threads,
        "composer": composer(shell, cx),
        "focused_thread": shell.main_pane().read(cx).focused_thread(),
        "sheet": sheet(shell, cx),
        "rounds": rounds,
        "comparison": comparison(session.comparison()),
        "comparison_asked": comparison(session.asked_comparison()),
        "comparison_available": session.comparison_available(),
        "comparison_error": failure(session.comparison_error()),
        "ready_note": ready_note(shell, cx),
        // `A` (PLAN §19.3): whether the stream draws the agent's annotations.
        "annotations_shown": session.annotations_shown(),
    })
}

/// `{"kind": "since_round", "round": n}` or `{"kind": "all"}`.
fn comparison(comparison: Comparison) -> Value {
    match comparison {
        Comparison::SinceRound(n) => json!({"kind": comparison.key(), "round": n}),
        Comparison::All => json!({"kind": comparison.key()}),
    }
}

/// `state.session.ready_note`: the agent's handoff the ready banner shows, or `null`.
fn ready_note(shell: &Shell, cx: &App) -> Value {
    match shell.ready_note(cx) {
        Some(ready) => json!({
            "author": ready.author.as_ref().map(author),
            "round": ready.round,
            "note": ready.note,
            "annotations": ready.annotations,
            "open": shell.ready_note_open(),
        }),
        None => Value::Null,
    }
}

/// `state.session.composer`: the open composer, or `null`.
fn composer(shell: &Shell, cx: &App) -> Value {
    let pane = shell.main_pane().read(cx);
    let Some(composer) = pane.composer() else {
        return Value::Null;
    };
    let target = &composer.target;
    json!({
        "path": target.path,
        "side": side(target.side),
        "start_line": target.start_line,
        "end_line": target.end_line,
        // A new comment on the whole file (lines 0).
        "file_level": target.is_file(),
        "reply_to": target.reply_to,
        "text": pane.composer_text(cx),
        "posting": composer.posting,
        "error": failure(composer.error.as_ref()),
    })
}

/// `state.session.sheet`: the "Finish round" sheet, or `null`.
fn sheet(shell: &Shell, cx: &App) -> Value {
    let Some(sheet) = shell.sheet() else {
        return Value::Null;
    };
    let sheet = sheet.read(cx);
    json!({
        "verdict": sheet.verdict().as_str(),
        "can_submit": sheet.can_submit(cx),
        "submitting": sheet.is_submitting(),
        "tab": match sheet.tab() {
            SummaryTab::Write => "write",
            SummaryTab::Preview => "preview",
        },
        "summary": sheet.summary_text(cx),
        "error": failure(sheet.error()),
    })
}

/// `state.comments_list`: the panel's comments (`null` with no review open): the rows shown,
/// in order, whether the resolved threads are among them, and the thread whose row menu is
/// open.
pub fn comments_list(shell: &Shell, cx: &App) -> Value {
    if shell.session().is_none() {
        return Value::Null;
    }
    let list = shell.comments_list().read(cx);
    let rows: Vec<Value> = list
        .rows(cx)
        .into_iter()
        .map(|row| {
            json!({
                "id": row.id,
                "state": row.state.as_str(),
                "outdated": row.outdated,
                "not_in_comparison": row.not_in_comparison,
                "pending": row.pending,
                "location": row.location,
                "file_level": row.file_level,
                "summary": row.summary,
                "snippet": row.snippet,
                "unlisted": row.unlisted,
                "drawable": row.drawable,
            })
        })
        .collect();
    json!({
        "show_resolved": list.shows_resolved(),
        "show_annotations": list.shows_annotations(),
        "expanded": list.expanded(),
        "rows": rows,
        "menu": list.menu_thread(),
    })
}

fn side(side: AnchorSide) -> &'static str {
    match side {
        AnchorSide::Old => "old",
        AnchorSide::New => "new",
    }
}

/// `state.stream`: the diff stream's own state (`null` with no review open): the file at the
/// top, the one whose header is pinned, the sideways scroll, per file whether it is collapsed,
/// its rows, the lines each gap hides and the notice it shows, and the "Not in the diff"
/// section (`null` while it doesn't show): whether it is collapsed and its threads.
pub fn stream(shell: &Shell, window: &Window, cx: &App) -> Value {
    if shell.session().is_none() {
        return Value::Null;
    }
    let pane = shell.diff_pane().read(cx);
    let files: Vec<Value> = pane
        .file_summaries()
        .into_iter()
        .map(|file| {
            json!({
                "path": file.path,
                "collapsed": file.collapsed,
                "rows": file.rows,
                "gaps": file.gaps,
                "notice": file.notice,
                "heading": file.heading,
            })
        })
        .collect();
    let section = pane.section_summary(cx).map(|section| {
        json!({
            "collapsed": section.collapsed,
            "threads": section.threads,
            "empty": section.empty,
        })
    });
    let place = |place: &PlaceRef| json!({"side": side(place.side), "line": place.line, "column": place.column});
    // The text and the caret are the shown pane's: the file view's while it is open.
    let shown = shell.main_pane().read(cx);
    let text_selection = shown.text_selection().map(|selection| {
        json!({
            "path": selection.path,
            "side": selection.side.map(side),
            "from": place(&selection.from),
            "to": place(&selection.to),
            "text": selection.text,
        })
    });
    let caret = shown.caret().map(|(path, caret)| {
        json!({
            "path": path,
            "side": side(caret.side),
            "line": caret.line,
            "column": caret.column,
        })
    });
    json!({
        "top_file": pane.top_file(),
        "pinned_file": pane.pinned_file(),
        "h_offset": pane.h_offset(),
        "focused": shell.focus_handle(cx).is_focused(window),
        "files": files,
        "not_in_diff": section,
        // Which pane `text_selection` and `caret` describe; the rest is the stream's.
        "text_pane": shown_pane(shell),
        "text_selection": text_selection,
        "caret": caret,
    })
}

/// `state.file_view`: the file view while it shows (else `null`): its file and side, the first
/// line in view, how many lines it has, whether the review changes it and which of its lines
/// (the first 200), whether its text is still loading, and the notice it shows instead of lines.
pub fn file_view(shell: &Shell, cx: &App) -> Value {
    if !shell.is_file_view_open() {
        return Value::Null;
    }
    let Some(view) = shell.file_view().read(cx).file_view_summary(cx) else {
        return Value::Null;
    };
    json!({
        "path": view.path,
        "side": side(view.side),
        "top_line": view.top_line,
        "line_count": view.line_count,
        "changed": view.changed,
        "changed_lines": view.changed_lines,
        "loading": view.loading,
        "notice": view.notice,
        "worktree": view.worktree,
        "outside": view.outside,
    })
}

/// `state.file_finder`: the file finder (`Ctrl P`): whether it is open, the query, the first
/// rows (paths, in order), how many files pass the filter, the selected one, whether every
/// file of the side is listed, and whether its field has the keyboard.
pub fn file_finder(shell: &Shell, window: &Window, cx: &App) -> Value {
    let Some(finder) = shell.file_finder() else {
        return json!({"open": false, "query": null, "rows": [], "count": 0, "selected": null});
    };
    let finder = finder.read(cx);
    json!({
        "open": true,
        "query": finder.query(),
        "rows": finder.rows(dv_ui::file_finder::STATE_ROWS),
        "count": finder.count(),
        "selected": finder.selected_path(),
        "complete": finder.is_complete(),
        "focused": finder.is_focused(window, cx),
    })
}

/// `state.palette`: the palette (`Ctrl K`): whether it is open, the query, the first rows in
/// order (each its key, section and label), the selected row's key, and whether its field has
/// the keyboard.
pub fn palette(shell: &Shell, window: &Window, cx: &App) -> Value {
    let Some(palette) = shell.palette() else {
        return json!({"open": false, "query": null, "rows": [], "selected": null});
    };
    let palette = palette.read(cx);
    let rows: Vec<Value> = palette
        .rows()
        .iter()
        .take(dv_ui::palette::STATE_ROWS)
        .map(|row| {
            json!({
                "key": row.key,
                "section": row.section.key(),
                "label": row.label(),
            })
        })
        .collect();
    json!({
        "open": true,
        "query": palette.query(),
        "rows": rows,
        "selected": palette.selected_key(),
        "focused": palette.is_focused(window, cx),
    })
}

/// `state.history`: how many positions are behind and ahead (`Alt ←` / `Alt →`).
pub fn history(shell: &Shell) -> Value {
    let (back, forward) = shell.history().counts();
    json!({"back": back, "forward": forward})
}

/// `state.intel`: the open review's code intelligence (`null` with no review open): its
/// worktree's servers, the specs missing, the hover card, the last go to definition's count,
/// the references list, the panel tab and the notice (`tests/e2e/README.md`).
pub fn intel(shell: &Shell, cx: &App) -> Value {
    let Some(intel) = shell.intel() else {
        return Value::Null;
    };
    let intel = intel.read(cx);
    let servers: Vec<Value> = intel
        .servers(cx)
        .into_iter()
        .map(|server| {
            json!({
                "id": server.spec_id,
                "name": server.name,
                "state": server.state.key(),
                "error": match &server.state {
                    dv_ui::intel::ServerState::Failed(why) => json!(why),
                    _ => Value::Null,
                },
                "root": server.root,
                "version": server.version,
                "command": server.command_line,
                "source": server.source,
                "starts": server.starts,
                "progress": server.progress.iter().map(|progress| json!({
                    "title": progress.title,
                    "message": progress.message,
                    "percentage": progress.percentage,
                })).collect::<Vec<_>>(),
                "stderr": server.stderr,
            })
        })
        .collect();
    let missing: Vec<Value> = intel
        .missing(cx)
        .into_iter()
        .map(|missing| {
            json!({"id": missing.spec_id, "reason": missing.reason, "hint": missing.hint})
        })
        .collect();
    let hover = match intel.hover_card() {
        Some(card) => json!({
            "open": true,
            "pane": pane(card.pane),
            "path": card.at.path,
            "side": side(card.at.side),
            "line": card.at.line,
            "symbol": card.at.name,
            "offset": card.at.offset,
            "span": [card.span.start, card.span.end],
            "text": card.text(),
            "sections": card.sections.iter().map(|section| json!({
                "server": section.spec_id,
                "text": section.text(),
            })).collect::<Vec<_>>(),
            "against_worktree": card.against_worktree,
            "note": card.note,
        }),
        None => match intel.hovering() {
            Some((at, on)) => json!({
                "open": false,
                "pane": pane(on),
                "path": at.path,
                "side": side(at.side),
                "line": at.line,
                "symbol": at.name,
            }),
            None => Value::Null,
        },
    };
    let references = intel.references().map(|list| {
        let shown = list.shown();
        let rows: Vec<Value> = shown
            .iter()
            .flat_map(|(file, rows)| {
                rows.iter().map(move |(row, changed)| {
                    json!({
                        "path": file.label(),
                        "in_repo": file.worktree_path().is_some(),
                        "side": file.side.map(side),
                        "line": row.line,
                        "column": row.location.range.start.column + 1,
                        "text": row.text,
                        "changed": changed,
                    })
                })
            })
            .collect();
        json!({
            "kind": list.kind.key(),
            "symbol": list.from.name,
            "loading": list.is_loading(),
            "reading": list.is_reading(),
            "error": list.error,
            "count": list.count(),
            "in_review": list.in_review,
            "against_worktree": list.against_worktree,
            "files": shown.len(),
            "rows": rows,
        })
    });
    json!({
        "servers": servers,
        "missing": missing,
        "hover": hover,
        "definitions": intel.last_definitions(),
        "references": references,
        "panel_tab": match shell.panel_tab(cx) {
            dv_ui::shell::intel::PanelTab::Review => "review",
            dv_ui::shell::intel::PanelTab::References => "references",
        },
        "notice": intel.notice().map(|notice| notice.message.clone()),
        "rechecks": intel.rechecks(cx),
    })
}

/// The pane the main area shows: `"file_view"` while the file view is open, else `"stream"`.
fn shown_pane(shell: &Shell) -> &'static str {
    if shell.is_file_view_open() {
        "file_view"
    } else {
        "stream"
    }
}

fn pane(pane: dv_ui::intel::Pane) -> &'static str {
    match pane {
        dv_ui::intel::Pane::Stream => "stream",
        dv_ui::intel::Pane::FileView => "file_view",
    }
}

/// `state.find`: the stream's find bar (`null` with no review open): whether it is open, the
/// query searched for, the current match's place (from 1) and how many there are, whether every
/// file has been searched, and how many had no text to search.
pub fn find(shell: &Shell, window: &Window, cx: &App) -> Value {
    // The shown pane's bar: the file view's while it is open.
    let pane = shell.main_pane().read(cx);
    let Some(find) = pane.find_summary() else {
        return Value::Null;
    };
    json!({
        "pane": shown_pane(shell),
        "open": find.open,
        "query": find.query,
        "current": find.current,
        "count": find.count,
        "complete": find.complete,
        "skipped": find.skipped,
        "focused": pane.is_find_focused(window, cx),
    })
}

fn status(status: ReviewStatus) -> &'static str {
    status.as_str()
}

/// A file status as one word: `added`, `modified`, `deleted`, `renamed`, `copied`,
/// `type_changed`, `untracked` or `unmerged`.
fn file_status(status: FileStatus) -> &'static str {
    match status {
        FileStatus::Added => "added",
        FileStatus::Modified => "modified",
        FileStatus::Deleted => "deleted",
        FileStatus::Renamed { .. } => "renamed",
        FileStatus::Copied { .. } => "copied",
        FileStatus::TypeChanged => "type_changed",
        FileStatus::Untracked => "untracked",
        FileStatus::Unmerged => "unmerged",
    }
}

fn file(session: &ReviewSession, entry: &FileEntry) -> Value {
    let state = session.file_state(&entry.path);
    json!({
        "path": entry.path,
        "old_path": entry.old_path,
        "status": file_status(entry.status),
        "additions": entry.additions,
        "deletions": entry.deletions,
        "binary": entry.binary,
        "viewed": session.is_viewed(&entry.path),
        "open_threads": session.open_threads_in(&entry.path),
        "state": state.map_or("unloaded", FileState::key),
        "kind": state.and_then(FileState::diff).map(|diff| diff_kind(diff)),
        "highlighted": state.and_then(FileState::highlights).is_some(),
    })
}

/// An author: `model` the one an agent said it runs on and `family` the model family whose
/// mark it shows (`dv_ui::agents`), each `null` without one.
fn author(author: &Author) -> Value {
    json!({
        "kind": author.kind.as_str(),
        "name": author.name,
        "model": author.model,
        "family": dv_ui::agents::family(author).map(dv_ui::agents::ModelFamily::key),
    })
}

fn thread_state(session: &ReviewSession, thread: &Thread) -> Value {
    let anchor = &thread.anchor;
    let replies: Vec<Value> = thread
        .replies
        .iter()
        .filter(|reply| !reply.is_deleted())
        .map(|reply| json!({"id": reply.id, "author": author(&reply.author), "body": reply.body}))
        .collect();
    let placement = session.placement(thread);
    let lines = |lines: Option<(u32, u32)>| match lines {
        Some((start, end)) => json!({"start": start, "end": end}),
        None => Value::Null,
    };
    let first = thread.first_anchor();
    json!({
        "id": thread.id,
        "path": anchor.path,
        "side": side(anchor.side),
        "start_line": anchor.start_line,
        "end_line": anchor.end_line,
        // A comment on the whole file (lines 0, PLAN §8).
        "file_level": anchor.is_file_level(),
        "state": thread.state.as_str(),
        // An agent opened it as an annotation (PLAN §19), asked about since or not.
        "annotation": thread.annotation,
        // Its card is drawn where it is placed (an annotation hidden, outdated or on a file the
        // review no longer changes isn't, PLAN §19.3).
        "card": session.shows_card(thread, &placement),
        "note": thread.last_change.as_ref().and_then(|change| change.note.clone()),
        "author": author(&thread.author),
        "body": thread.body,
        "replies": replies,
        // `null` while it can't be told: its file's text isn't there yet. A thread the
        // interdiff doesn't show says how it stands in all changes.
        "outdated": if placement.is_outdated() {
            json!(true)
        } else if placement.is_pending() {
            Value::Null
        } else {
            json!(false)
        },
        "placement": placement.key(thread),
        // Its file is among the review's changes; `false` for one on a file the review doesn't
        // change (placed there, or outdated), which the "Not in the diff" section shows.
        "in_diff": placement.placement().in_diff(),
        "file": placement.file(),
        "lines": lines(placement.lines(anchor)),
        // Where an outdated thread's code went since its round (PLAN §8), `null` without one.
        "trail": placement.trail().map(|trail| {
            let (start, end) = trail.lines();
            json!({
                "round": trail.round,
                "went": trail.went.as_str(),
                "start": start,
                "end": end,
            })
        }),
        "original_lines": lines(Some((first.start_line, first.end_line))),
        "round": anchor.round,
    })
}

/// `state.setup` (PLAN §12): the first-run screen, Settings, the status bar's notice and every
/// environment's checks as the setup screen shows them.
pub fn setup(shell: &Shell, cx: &App) -> Value {
    let setup = cx.services().setup.read(cx);
    let environments: Vec<Value> = setup
        .environments()
        .iter()
        .map(|state| {
            let env = &state.env;
            let checks: Vec<Value> = env
                .checks
                .iter()
                .map(|check| {
                    json!({
                        "id": check.id,
                        "title": check.title,
                        "state": check.state.key(),
                        "detail": check.detail,
                        "fix": check.fix.as_ref().map(|fix| json!({
                            "action": fix.action,
                            "command": fix.command,
                        })),
                        "applying": setup.applying(&env.name, &check.id).is_some(),
                        "error": setup.failure(&env.name, &check.id),
                    })
                })
                .collect();
            json!({
                "name": env.name,
                "kind": env.kind.key(),
                "running": env.running,
                "note": env.note,
                "checking": state.checking,
                "starting": state.starting,
                "start_error": state.start_error,
                "checks": checks,
            })
        })
        .collect();
    json!({
        "open": shell.first_run_open(cx) || shell.settings().is_some(),
        "first_run": shell.first_run_open(cx),
        "settings_open": shell.settings().is_some(),
        "checking": setup.is_checking(),
        "notice": setup.notice().map(|notice| json!({
            "environment": notice.environment,
            "text": notice.text,
            "action": notice.action,
        })),
        "environments": environments,
    })
}

/// `state.update` (PLAN D9): the update ready to restart into (`null` when none), whether the
/// restart was asked for, what the confirmation says will close while it asks (`confirm`, else
/// `null`), why the last restart failed (`failure`), and whether the seam's stand-in records it
/// (`scripted`).
pub fn update(cx: &App) -> Value {
    let updates = cx.services().updates.read(cx);
    json!({
        "ready": updates.ready_version(),
        "restart_requested": updates.restart_requested(),
        "confirm": updates.confirm(),
        "failure": updates.failure(),
        "scripted": updates.is_scripted(),
    })
}
