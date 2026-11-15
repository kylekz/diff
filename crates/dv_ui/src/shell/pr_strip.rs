//! The PR strip (PLAN §9.9, DESIGN §2, §4.5): a 34px row of `chrome` under the title bar
//! while a PR review is open, with the pull request as GitHub last told it and how the
//! review stands against it.
//!
//! One row: `#482`, the title, the author, `base ← head` (`owner:head` for a fork), the
//! pull request's state and its checks' counts, whose click opens the checks page; then the
//! review's PR round, how far the pull request moved ([`PrMove`]: GitHub's head against the
//! head the review shows, which are the mirror's and the pins', else the head the last
//! refresh moved the review from), when the mirror was read, and the buttons **Refresh**
//! (`Ctrl R`, reading "Refreshing…" and inert while one runs) and **Open on GitHub**.
//!
//! **In a narrow window** its parts give way in a fixed order (DESIGN §4.5), each only once
//! the ones before it have nothing left to give: how far it moved, then the author and the
//! branches (each down to an ellipsis, its whole text in its tooltip), then the title,
//! which the title bar names too. The number, the state, the checks, the round, when it
//! was read and the buttons don't shrink at all: the line that says what shows is stale is
//! never the one cut.
//!
//! **A refresh that failed** (the mirror is stale, PLAN §9.4) reads "Couldn't refresh ·
//! details" in `yellow` in place of when the mirror was read: its tooltip is the failure's
//! one line, and a click shows the failure whole as a banner at the top of the main area
//! (`banner/pr`: Retry, the command `gh` ran and what it printed in Details, Dismiss). A
//! refresh that read the pull request and then couldn't move the draft to its head reads
//! "Couldn't move the review · details" the same way: what shows of GitHub is what it
//! holds now. While the open's own banner about that review is up ("couldn't read #482
//! from GitHub", `shell::pr`), that banner is the one: "details" shows no second.
//!
//! **What it draws is GitHub's text** (a title, a login, branch names): each is drawn as one
//! line whatever it holds (`crate::thread::one_line`) and truncates before it pushes
//! anything else out of the strip. **What it opens is dv's own address**, built from the
//! mirror's key (`MirrorKey::pr_url`), through the system seam: never an address GitHub's
//! answer holds.
//!
//! The write side's parts (Start round n+1, "since your last review", a pending review's
//! "The PR moved", PLAN §9.12) aren't here.

use dv_core::github::{Checks, HeadRepo, PrState};
use gpui::{
    AnyElement, App, Context, FontWeight, Hsla, InteractiveElement as _, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, prelude::FluentBuilder as _,
    px,
};

use super::Shell;
use crate::automation::AutomationTargetExt as _;
use crate::banner::{Banner, BannerAction};
use crate::components::{chip, icon, kbd, small_button, small_icon_button, tooltip};
use crate::failure::Failure;
use crate::icons::DvIcon;
use crate::keymap;
use crate::services::github::PrRead;
use crate::session::{Commits, MoveKind, PrMove, ReviewSession};
use crate::theme::{DvTheme, StyledText as _};
use crate::thread::one_line;

/// The strip's height (DESIGN §4.5).
pub const STRIP_HEIGHT: f32 = 34.;

/// How readily each part of the strip gives way when the window is narrow (DESIGN §4.5):
/// how far the pull request moved first, then the author and the branches, then the
/// title. Flex shrinking shares a shortfall by factor times size, so at
/// these ratios a part's share rounds to nothing until the ones before it have none left
/// (`super::title_bar` gives its label way the same way).
const MOVED_SHRINK: f32 = 1e12;
const BRANCHES_SHRINK: f32 = 1e6;
const TITLE_SHRINK: f32 = 1.;

/// The least the strip keeps of each part that gives way before the title (how far the pull
/// request moved, the author, the branches): room for an ellipsis, so each stays under the
/// pointer with its whole text as its tooltip. A text shorter than this takes its own
/// width.
const GIVEN_MIN_WIDTH: f32 = 36.;

/// When, and whether, the pull request was read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadLine {
    /// Nothing was read of it and nothing is being read (no `gh` to ask).
    NotRead,
    /// This session's first read of it runs: nothing shows yet, or what was read of it
    /// before the review opened (the stored mirror, PLAN §9.4).
    Reading,
    /// Read then (ms since the Unix epoch).
    Read(u64),
    /// The last refresh failed: what shows is what was read before it, if anything.
    Stale(Failure),
    /// The last refresh read the pull request and then failed to move the draft to its
    /// head (PLAN §9.4, `Refreshed::fresh` with `stale`): what shows of GitHub is fresh, the
    /// review's sides aren't.
    Unmoved(Failure),
}

impl ReadLine {
    /// `"not_read"`, `"reading"`, `"read"`, `"stale"` or `"unmoved"`, as automation state
    /// spells it.
    pub fn key(&self) -> &'static str {
        match self {
            Self::NotRead => "not_read",
            Self::Reading => "reading",
            Self::Read(_) => "read",
            Self::Stale(_) => "stale",
            Self::Unmoved(_) => "unmoved",
        }
    }

    /// What the strip says: "Read 3m ago", "Couldn't refresh · details".
    pub fn text(&self, now: u64) -> String {
        match self {
            Self::NotRead => "Not read from GitHub yet".to_string(),
            Self::Reading => "Reading from GitHub…".to_string(),
            Self::Read(at) => format!("Read {}", crate::thread::relative_time(*at, now)),
            Self::Stale(_) => "Couldn't refresh · details".to_string(),
            Self::Unmoved(_) => "Couldn't move the review · details".to_string(),
        }
    }

    /// Why the last refresh failed, when it did.
    pub fn failure(&self) -> Option<&Failure> {
        match self {
            Self::Stale(failure) | Self::Unmoved(failure) => Some(failure),
            Self::NotRead | Self::Reading | Self::Read(_) => None,
        }
    }
}

/// What the strip shows (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrStrip {
    pub number: u64,
    /// The title, the author and the branches as GitHub last told them, each on one line;
    /// `None` before the pull request was read.
    pub title: Option<String>,
    pub author: Option<String>,
    /// `main ← feat`; `main ← octo:feat` for a fork's.
    pub branches: Option<String>,
    pub state: Option<PrState>,
    pub checks: Checks,
    /// The review's PR round, once the store was listed.
    pub round: Option<u32>,
    pub moved: Option<PrMove>,
    /// Whether the review follows the pull request's head (a draft), for how `moved` reads.
    pub draft: bool,
    pub read: ReadLine,
    /// A refresh was asked for and isn't done.
    pub refreshing: bool,
    /// The pull request's page and its checks page, built from the mirror's key.
    pub url: Option<String>,
    pub checks_url: Option<String>,
}

impl PrStrip {
    /// The strip of `session`'s review; `None` for a local review.
    pub fn of(session: &ReviewSession) -> Option<Self> {
        let review = session.review();
        let link = review.pr_target()?;
        let read = session.pr();
        let mirror = read.and_then(|read| read.mirror.as_ref());
        let facts = session.pr_facts();
        let refreshing = session.is_refreshing_pr();
        let line = crate::thread::drawn;
        Some(Self {
            number: link.number,
            title: mirror.map(|mirror| line(&mirror.pr.title)),
            author: mirror.map(|mirror| line(&mirror.pr.author)),
            branches: mirror.map(|mirror| {
                let head = match &mirror.pr.head_repo {
                    HeadRepo::Fork { owner: Some(owner) } => {
                        format!("{owner}:{}", mirror.pr.head_ref)
                    }
                    HeadRepo::Fork { owner: None } | HeadRepo::Same => mirror.pr.head_ref.clone(),
                };
                line(&format!("{} ← {head}", mirror.pr.base_ref))
            }),
            state: mirror.map(|mirror| mirror.pr.state),
            checks: mirror.map(|mirror| mirror.checks).unwrap_or_default(),
            round: facts.round,
            moved: facts.moved.clone(),
            draft: review.status == dv_core::review::ReviewStatus::Draft,
            read: match (read.and_then(|read| read.stale.as_ref()), mirror) {
                (Some(stale), _) if read.is_some_and(|read| read.fresh) => {
                    ReadLine::Unmoved(stale.clone())
                }
                (Some(stale), _) => ReadLine::Stale(stale.clone()),
                (None, Some(_)) if refreshing && read.is_some_and(PrRead::is_stored) => {
                    ReadLine::Reading
                }
                (None, Some(mirror)) => ReadLine::Read(mirror.read_at),
                (None, None) if refreshing => ReadLine::Reading,
                (None, None) => ReadLine::NotRead,
            },
            refreshing,
            url: mirror.and_then(|mirror| mirror.key.pr_url()),
            checks_url: mirror.and_then(|mirror| mirror.key.checks_url()),
        })
    }

    /// How far the pull request moved, in the strip's words: "2 new commits since this
    /// draft's head" while GitHub's head is another commit than the one the review shows,
    /// "Updated to 2 new commits" once a refresh moved the review there.
    pub fn moved_text(&self) -> Option<String> {
        let moved = self.moved.as_ref()?;
        let whose = if self.draft {
            "this draft's head"
        } else {
            "this review's head"
        };
        let new = |n: u32| match n {
            1 => "1 new commit".to_string(),
            n => format!("{n} new commits"),
        };
        Some(match (moved.kind, moved.commits) {
            (MoveKind::Ahead, Some(Commits::New(n))) => format!("{} since {whose}", new(n)),
            (MoveKind::Ahead, Some(Commits::Rewritten)) => {
                format!("Force-pushed since {whose}")
            }
            (MoveKind::Ahead, Some(Commits::Earlier)) => {
                format!("GitHub has an earlier commit than {whose}")
            }
            (MoveKind::Ahead, None) => format!("The pull request moved since {whose}"),
            (MoveKind::Followed, Some(Commits::New(n))) => format!("Updated to {}", new(n)),
            (MoveKind::Followed, Some(Commits::Rewritten)) => {
                "Updated to a force-pushed head".to_string()
            }
            (MoveKind::Followed, Some(Commits::Earlier)) => {
                "Updated to an earlier commit".to_string()
            }
            (MoveKind::Followed, None) => "Updated to a new head".to_string(),
        })
    }
}

/// How often the strip is drawn again while a PR review is open, so "Read 3m ago" keeps
/// time.
const TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// Draws the shell again every [`TICK`], for as long as the returned subscription is kept.
pub(super) fn tick(cx: &mut Context<Shell>) -> gpui::Subscription {
    let task = cx.spawn(async move |this, cx| {
        loop {
            cx.background_executor().timer(TICK).await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                break;
            }
        }
    });
    gpui::Subscription::new(move || drop(task))
}

/// A pull request's state as its chip words it.
pub fn state_label(state: PrState) -> &'static str {
    match state {
        PrState::Open => "Open",
        // Not "Draft": that is the review's own status, in the status bar beside it.
        PrState::Draft => "Draft PR",
        PrState::Merged => "Merged",
        PrState::Closed => "Closed",
    }
}

impl Shell {
    /// What the PR strip shows; `None` unless a PR review is open.
    pub fn pr_strip(&self, cx: &App) -> Option<PrStrip> {
        PrStrip::of(self.session.as_ref()?.read(cx))
    }

    /// Whether the failed refresh shows whole: as the main area's `banner/pr`, or as the
    /// open's own banner about this review, which stands in for it (module docs).
    pub fn pr_error_open(&self, cx: &App) -> bool {
        self.unread_banner_is_up(cx)
            || (self.pr_error_open
                && self
                    .pr_strip(cx)
                    .is_some_and(|strip| strip.read.failure().is_some()))
    }

    /// The strip's "Couldn't refresh · details": shows the failure whole, or hides it. The
    /// open's banner about this review is that failure already: it is the one hidden, and
    /// no second one shows beside it.
    pub fn toggle_pr_error(&mut self, cx: &mut Context<Self>) {
        if self.unread_banner_is_up(cx) {
            self.pr_error_open = false;
            self.dismiss_banner(cx);
            return;
        }
        self.pr_error_open = !self.pr_error_open;
        self.pr_banner_expanded = false;
        cx.notify();
    }

    /// The failed refresh as a banner at the top of the main area (module docs), while the
    /// strip's "details" has it open.
    pub(super) fn pr_banner(&self, cx: &mut Context<Self>) -> Option<Banner> {
        if self.unread_banner_is_up(cx) {
            return None;
        }
        let strip = self.pr_strip(cx).filter(|_| self.pr_error_open)?;
        let (stale, what) = match strip.read {
            ReadLine::Stale(stale) => (stale, "couldn't refresh"),
            ReadLine::Unmoved(stale) => (stale, "couldn't move the review to the head of"),
            ReadLine::NotRead | ReadLine::Reading | ReadLine::Read(_) => return None,
        };
        let this = cx.weak_entity();
        let on = move |f: fn(&mut Shell, &mut Context<Shell>)| {
            let this = this.clone();
            move |_: &mut gpui::Window, cx: &mut App| {
                this.update(cx, f).ok();
            }
        };
        let failure = Failure {
            message: format!("{what} #{}: {}", strip.number, stale.message),
            ..stale
        };
        Some(
            Banner::new("banner/pr", failure)
                .disclosure(
                    self.pr_banner_expanded,
                    on(|this, cx| {
                        this.pr_banner_expanded = !this.pr_banner_expanded;
                        cx.notify();
                    }),
                )
                .action(BannerAction::new(
                    "retry",
                    "Retry",
                    on(|this, cx| this.refresh_pull_request(cx)),
                ))
                .action(BannerAction::new(
                    "dismiss",
                    "Dismiss",
                    on(|this, cx| this.toggle_pr_error(cx)),
                )),
        )
    }

    /// The strip itself; `None` unless a PR review is open.
    pub(super) fn render_pr_strip(
        &self,
        theme: &DvTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let strip = self.pr_strip(cx)?;
        let c = &theme.colors;
        let now = dv_core::review::event::now_ms();
        let text = |text: &str| SharedString::from(text.to_string());

        let number = div()
            .flex_none()
            .font_family(theme.fonts.mono.clone())
            .text_size(px(12.))
            .text_color(c.fg3)
            .child(format!("#{}", strip.number))
            .automation_target("pr_strip/number");
        let title = strip.title.as_deref().map(|title| {
            div()
                .id("pr-strip-title")
                .min_w_0()
                .flex_shrink(TITLE_SHRINK)
                .truncate()
                .font_weight(FontWeight::SEMIBOLD)
                .child(text(title))
                .tooltip(tooltip(title.to_string()))
                .automation_target("pr_strip/title")
        });
        let author = strip.author.as_deref().map(|author| {
            div()
                .id("pr-strip-author")
                .min_w(px(GIVEN_MIN_WIDTH))
                .max_w(px(140.))
                .flex_shrink(BRANCHES_SHRINK)
                .truncate()
                .text_spec(&theme.text.secondary)
                .text_color(c.fg2)
                .child(text(author))
                .tooltip(tooltip(author.to_string()))
                .automation_target("pr_strip/author")
        });
        let branches = strip.branches.as_deref().map(|branches| {
            div()
                .id("pr-strip-branches")
                .min_w(px(GIVEN_MIN_WIDTH))
                .max_w(px(280.))
                .flex_shrink(BRANCHES_SHRINK)
                .truncate()
                .font_family(theme.fonts.mono.clone())
                .text_size(px(12.))
                .text_color(c.fg2)
                .child(text(branches))
                .tooltip(tooltip(branches.to_string()))
                .automation_target("pr_strip/branches")
        });
        let state = strip.state.map(|state| {
            let (fg, bg): (Hsla, Hsla) = match state {
                PrState::Open => (c.green, c.green_soft),
                PrState::Draft => (c.fg2, c.ctrl),
                PrState::Merged => (c.accent_text, c.accent_soft),
                PrState::Closed => (c.red, c.red_soft),
            };
            chip(fg, bg, theme)
                .h(px(18.))
                .text_size(px(11.5))
                .font_weight(FontWeight::SEMIBOLD)
                // Grey on `chrome` needs an edge to read as a chip.
                .when(state == PrState::Draft, |this| {
                    this.border_1().border_color(c.ctrl_line)
                })
                .child(state_label(state))
                .automation_target("pr_strip/state")
        });
        let round = strip.round.map(|round| {
            div()
                .flex_none()
                .text_spec(&theme.text.secondary)
                .text_color(c.fg3)
                .child(format!("Round {round}"))
                .automation_target("pr_strip/round")
        });
        let ahead = strip
            .moved
            .as_ref()
            .is_some_and(|moved| moved.kind == MoveKind::Ahead);
        let moved = strip.moved_text().map(|moved| {
            div()
                .id("pr-strip-moved")
                .min_w(px(GIVEN_MIN_WIDTH))
                .flex_shrink(MOVED_SHRINK)
                .truncate()
                .text_spec(&theme.text.secondary)
                .text_color(if ahead { c.yellow } else { c.fg2 })
                .child(SharedString::from(moved.clone()))
                .tooltip(tooltip(moved))
                .automation_target("pr_strip/moved")
        });
        let stale = strip.read.failure().is_some();
        let error_open = self.pr_error_open(cx);
        let read_text = strip.read.text(now);
        let read = div()
            .id("pr-strip-status")
            .flex_none()
            .h(theme.sizes.control_small_height)
            .flex()
            .items_center()
            .px(theme.spacing.sm)
            .rounded(theme.radii.control)
            .text_spec(&theme.text.secondary)
            .text_color(if stale { c.yellow } else { c.fg3 })
            .child(SharedString::from(read_text.clone()))
            .map(|this| match strip.read.failure() {
                Some(failure) => {
                    let hover = c.control_hover;
                    this.cursor_pointer()
                        .when(error_open, |this| this.bg(c.control_hover))
                        .hover(move |style| style.bg(hover))
                        .tooltip(tooltip(format!(
                            "{read_text}: {}",
                            one_line(&failure.message)
                        )))
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_pr_error(cx)))
                }
                None => this.tooltip(tooltip(read_text)),
            })
            .automation_target("pr_strip/status");
        let refresh = small_button(
            "pr-strip-refresh",
            if strip.refreshing {
                "Refreshing…"
            } else {
                "Refresh"
            },
            theme,
        )
        .map(|this| {
            if strip.refreshing {
                this.opacity(0.6).cursor_default()
            } else {
                this.child(kbd(keymap::label("secondary-r"), theme))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_pull_request(cx)))
            }
        })
        .automation_target("pr_strip/refresh");
        let open = strip.url.clone().map(|url| {
            small_icon_button("pr-strip-open", DvIcon::ExternalLink, theme)
                .tooltip(tooltip("Open on GitHub"))
                .on_click(move |_, _, cx| crate::system::open_url(&url, cx))
                .automation_target("pr_strip/open")
        });

        // One row. The number, the state, the checks, the round and the buttons keep their
        // size (`flex_none`); what gives way does so in the order of its shrink factor,
        // each truncating with an ellipsis (module docs).
        Some(
            div()
                .id("pr-strip")
                .flex_none()
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .w_full()
                .h(px(STRIP_HEIGHT))
                .px(theme.spacing.lg)
                .overflow_hidden()
                .bg(c.chrome)
                .border_b_1()
                .border_color(c.line)
                .text_spec(&theme.text.body)
                .text_color(c.fg)
                .child(number)
                .children(title)
                .children(author)
                .children(branches)
                .children(state)
                .children(self.render_checks(&strip, theme, cx))
                .child(div().flex_1().min_w_0())
                .children(round)
                .children(moved)
                .child(read)
                .child(refresh)
                .children(open)
                .automation_target("pr_strip"),
        )
    }

    /// The checks' counts (`✓ 12` green, `✗ 1` red, `● 2` yellow), the ones that aren't zero;
    /// a click opens the pull request's checks page.
    fn render_checks(
        &self,
        strip: &PrStrip,
        theme: &DvTheme,
        _cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let c = &theme.colors;
        let Checks {
            passing,
            failing,
            pending,
        } = strip.checks;
        if passing + failing + pending == 0 {
            return None;
        }
        let count = |glyph: DvIcon, n: u32, color: Hsla| {
            (n > 0).then(|| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(3.))
                    .text_color(color)
                    .child(icon(glyph, px(11.), color))
                    .child(n.to_string())
            })
        };
        let hover = c.control_hover;
        let checks = div()
            .id("pr-strip-checks")
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .h(theme.sizes.control_small_height)
            .px(theme.spacing.sm)
            .rounded(theme.radii.control)
            .text_spec(&theme.text.secondary)
            .font_weight(FontWeight::MEDIUM)
            .children(count(DvIcon::Check, passing, c.green))
            .children(count(DvIcon::X, failing, c.red))
            .children(count(DvIcon::Circle, pending, c.yellow));
        let summary = [
            (passing, "passing"),
            (failing, "failing"),
            (pending, "pending"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect::<Vec<_>>()
        .join(", ");
        Some(match strip.checks_url.clone() {
            Some(url) => checks
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .tooltip(tooltip(format!("Checks: {summary} · open on GitHub")))
                .on_click(move |_, _, cx| crate::system::open_url(&url, cx))
                .automation_target("pr_strip/checks"),
            None => checks
                .tooltip(tooltip(format!("Checks: {summary}")))
                .automation_target("pr_strip/checks"),
        })
    }
}
