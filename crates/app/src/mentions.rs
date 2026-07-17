//! `@`-mention autocomplete for the comment composer
//! (R3 item 2) — a [`CompletionProvider`] over a shared, background-filled
//! pool of the repo's mentionable users.
//!
//! Gated on PR-associated reviews by construction: the pool is only ever
//! filled by `Workspace::ensure_mentions_fetch`, which requires the review
//! to carry a `RemoteRef` — on a plain local review the pool stays empty
//! and [`MentionProvider::completions`] returns nothing, so the provider
//! can be attached to every composer unconditionally.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{Context, Task, Window};
use gpui_component::input::{CompletionProvider, InputState};
use gpui_component::{Rope, RopeExt as _};

/// Most completion items to offer at once.
const MENTION_LIMIT: usize = 50;

/// Reads the shared pool fresh on every keystroke, so entries the
/// background `mentionable_users` fetch adds show up mid-typing session.
pub(crate) struct MentionProvider {
    pub users: Rc<RefCell<Vec<dv_core::Mention>>>,
}

/// If the cursor sits inside an `@mention` token, return the byte offset of
/// the `@` and the (possibly empty) login text typed after it. The `@` must
/// begin a word — preceded by whitespace or the start of the text — matching
/// GitHub's own mention rules, so `foo@bar` never triggers.
fn mention_prefix(text: &Rope, offset: usize) -> Option<(usize, String)> {
    let s = text.to_string();
    let offset = offset.min(s.len());
    let before = &s[..offset];
    // GitHub logins are alphanumeric plus hyphen; walk back over that run.
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '-')
        .last()
        .map(|(i, _)| i)
        .unwrap_or(offset);
    if start == 0 || before.as_bytes()[start - 1] != b'@' {
        return None;
    }
    let at = start - 1;
    if at > 0 && !before[..at].chars().next_back().unwrap().is_whitespace() {
        return None;
    }
    Some((at, before[start..offset].to_string()))
}

/// Rank of `user` against `query` (matched case-insensitively), lower =
/// better; `None` = no match. Login prefix beats name prefix beats
/// substring matches.
fn mention_rank(user: &dv_core::Mention, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let query = &query.to_ascii_lowercase();
    let login = user.login.to_ascii_lowercase();
    let name = user.name.as_ref().map(|n| n.to_ascii_lowercase());
    if login.starts_with(query) {
        Some(0)
    } else if name
        .as_deref()
        .is_some_and(|n| n.split_whitespace().any(|w| w.starts_with(query)))
    {
        Some(1)
    } else if login.contains(query) {
        Some(2)
    } else if name.as_deref().is_some_and(|n| n.contains(query)) {
        Some(3)
    } else {
        None
    }
}

impl CompletionProvider for MentionProvider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: lsp_types::CompletionContext,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> Task<anyhow::Result<lsp_types::CompletionResponse>> {
        let empty = Task::ready(Ok(lsp_types::CompletionResponse::Array(vec![])));
        let Some((at, prefix)) = mention_prefix(text, offset) else {
            return empty;
        };
        // The edit replaces `@prefix` (the token so far) with `@login `.
        let range = lsp_types::Range {
            start: text.offset_to_position(at),
            end: text.offset_to_position(offset),
        };
        let users = self.users.borrow();
        let mut ranked: Vec<(u8, &dv_core::Mention)> = users
            .iter()
            .filter_map(|u| mention_rank(u, &prefix).map(|r| (r, u)))
            .collect();
        // Stable sort keeps GitHub's relevance order within each rank.
        ranked.sort_by_key(|(rank, _)| *rank);
        let items = ranked
            .into_iter()
            .take(MENTION_LIMIT)
            .map(|(_, u)| lsp_types::CompletionItem {
                label: u.login.clone(),
                filter_text: Some(u.login.clone()),
                detail: u.name.clone(),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range,
                    new_text: format!("@{} ", u.login),
                })),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        Task::ready(Ok(lsp_types::CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(
        &self,
        _offset: usize,
        _new_text: &str,
        _cx: &mut Context<InputState>,
    ) -> bool {
        // Cheap to always run; `completions` returns nothing outside a
        // mention token.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mention(login: &str, name: Option<&str>) -> dv_core::Mention {
        dv_core::Mention {
            login: login.to_string(),
            name: name.map(str::to_string),
        }
    }

    // --- mention_prefix ----------------------------------------------------

    #[test]
    fn prefix_at_start_of_text() {
        let rope = Rope::from("@ky");
        assert_eq!(mention_prefix(&rope, 3), Some((0, "ky".to_string())));
    }

    #[test]
    fn prefix_after_whitespace() {
        let rope = Rope::from("hey @kylekz");
        assert_eq!(mention_prefix(&rope, 11), Some((4, "kylekz".to_string())));
    }

    #[test]
    fn bare_at_offers_everyone() {
        let rope = Rope::from("cc @");
        assert_eq!(mention_prefix(&rope, 4), Some((3, String::new())));
    }

    #[test]
    fn email_like_text_never_triggers() {
        // `foo@bar` — the `@` doesn't begin a word (GitHub's own rule).
        let rope = Rope::from("kyle@example.com");
        assert_eq!(mention_prefix(&rope, 8), None);
    }

    #[test]
    fn cursor_outside_the_token_does_not_trigger() {
        let rope = Rope::from("@kylekz done");
        assert_eq!(mention_prefix(&rope, 12), None);
    }

    // --- mention_rank ------------------------------------------------------

    #[test]
    fn login_prefix_beats_name_prefix_beats_substrings() {
        let by_login = mention("kylekz", None);
        let by_name = mention("someone", Some("Kyle Smith"));
        let login_substr = mention("mkyle", None);
        let name_substr = mention("other", Some("Smith-kyleson"));
        let miss = mention("unrelated", Some("Nobody"));
        assert_eq!(mention_rank(&by_login, "kyle"), Some(0));
        assert_eq!(mention_rank(&by_name, "kyle"), Some(1));
        assert_eq!(mention_rank(&login_substr, "kyle"), Some(2));
        assert_eq!(mention_rank(&name_substr, "kyle"), Some(3));
        assert_eq!(mention_rank(&miss, "kyle"), None);
    }

    #[test]
    fn empty_query_matches_everyone_at_top_rank() {
        assert_eq!(mention_rank(&mention("anyone", None), ""), Some(0));
    }

    #[test]
    fn rank_is_case_insensitive() {
        assert_eq!(mention_rank(&mention("KyleKZ", None), "kyle"), Some(0));
    }
}
