//! Where threads stand in a review's current diff (docs/PLAN.md §8), as the GUI shows them:
//! the review's target resolved now (all changes, never an interdiff) in its home worktree
//! (`Repo::for_review`, PLAN §2.2), its file list, and the full text of each side a
//! thread is anchored on (`file_diff` without caps), handed to
//! `dv_core::review::place_thread`, the one function the GUI places threads with too. A thread
//! found there carries the excerpt of that same diff around its lines (`dv_core::review::excerpt`,
//! PLAN §10.3).
//!
//! A new-side thread of a closed round that the current text alone can't place (its file not
//! listed, or relocation calls it outdated) is read against its round's snapshot
//! (`dv_core::review::read_round_history`, [`Sides::read_history_for`]): only those rounds, for
//! those threads' paths, so its file is followed through a rename since and an outdated one
//! says where its code went, with the excerpt around the lines that replaced it.
//!
//! A write on a thread (`reply`, `address`, `resolve`, `reopen`) records where relocation found
//! it ([`Sides::relocated`], PLAN §8), and `comment add` anchors on a side's text read the same
//! way ([`Sides::side_text`]).
//!
//! A comment on a whole file (a file-level anchor) is placed by its file alone: no diff is read
//! for it, and its round's history only to follow its file.
//!
//! A thread whose file isn't among the review's changes (it never changed, or stopped changing)
//! is placed against that file's text at the review's new side (`dv_core::diff::side_file`,
//! `dv_core::review::place_unlisted`), as the GUI places it: found there, it is placed, with
//! the excerpt of that text around it, and not in the diff (`in_diff: false`).
//!
//! GitHub's threads on a PR review (PLAN §9.6) are placed here too ([`Placed::Github`]): each
//! with the anchor `dv_core::github::threads` reads for it from the stored mirror and the
//! commits it names, through the same diffs, so `comment list` says of them what the app
//! shows. One without a place in the diff (outdated on GitHub, or can't be placed here) is
//! outdated, with nothing read for it. Listing them reads each file a thread is on once
//! ([`locate_with_github`]); showing one reads that thread's file alone ([`locate_github`]).

use std::collections::{BTreeMap, HashMap};

use dv_core::diff::{DiffLimits, FileDiff, FileEntry, file_diff, file_list, side_file};
use dv_core::git::GitRepo;
use dv_core::github::threads::{self, GithubAnchor, Standing};
use dv_core::github::{GithubThread, Mirror, ThreadTexts, anchor_thread};
use dv_core::review::{
    Anchor, AnchorPlace, AnchorSide, History, Placement, Review, RoundHistory, SideText, Thread,
    ThreadFile, Went, entry_for, excerpt, excerpt_lines, is_gone, pins, place_thread,
    place_unlisted_anchor, read_round_history, relocated_anchor, side_lines, thread_file,
    unlisted_path,
};
use dv_core::target::DiffSpec;

use crate::context::Context;
use crate::dto::{CurrentAnchorDto, CurrentCode, Located, TrailCode, side_str};
use crate::error::CliError;
use crate::output;
use crate::repo::Repo;

/// A thread to place: one of the review's own, or one of GitHub's on its pull request with
/// the anchor read for it (PLAN §9.6).
#[derive(Clone, Copy)]
pub(crate) enum Placed<'a> {
    Local(&'a Thread),
    Github(&'a GithubAnchor),
}

impl<'a> Placed<'a> {
    fn anchor(self) -> &'a Anchor {
        match self {
            Self::Local(thread) => &thread.anchor,
            Self::Github(anchored) => &anchored.anchor,
        }
    }

    /// Whether its anchor is code to look for: every thread of the review, and a GitHub
    /// thread that has a place in the diff.
    fn is_anchored(self) -> bool {
        match self {
            Self::Local(_) => true,
            Self::Github(anchored) => anchored.standing == Standing::Anchored,
        }
    }

    /// The entry of `entries` for a file at `path`, renamed from `old_path`, by the rule its
    /// kind of thread is placed with.
    fn entry<'e>(
        self,
        path: &str,
        old_path: Option<&str>,
        entries: &'e [FileEntry],
    ) -> Option<&'e FileEntry> {
        match self {
            Self::Local(_) => entry_for(path, old_path, entries),
            Self::Github(_) => threads::thread_entry(path, old_path, entries),
        }
    }
}

/// A review's current diff: its file list, the diffs of the files read so far, and the round
/// histories read.
pub(crate) struct Sides {
    git: GitRepo,
    spec: DiffSpec,
    pub entries: Vec<FileEntry>,
    diffs: HashMap<String, FileDiff>,
    /// The round in progress, which a relocated anchor is taken in.
    round: u32,
    /// The rounds closed, which a thread's history is read from.
    closed: u32,
    histories: HashMap<u32, RoundHistory>,
    /// The files the review doesn't list that threads are anchored in, as the review's new side
    /// holds them (`Some(None)`: no file there; `None`: the read failed), by path.
    unlisted: HashMap<String, Option<Option<FileDiff>>>,
}

impl Sides {
    /// The file list of `review`'s two sides as they resolve now, its target's or a PR
    /// review's pins (`dv_core::review::pins::resolve`; blocking: their git calls and one file
    /// list). A target that no longer resolves is `target` (exit 1).
    pub fn list(repo: &Repo, review: &Review, ctx: &Context) -> Result<Self, CliError> {
        let repo = repo.for_review(review, ctx)?;
        let spec =
            pins::resolve(&repo.git, &review.id, &review.target).map_err(CliError::target)?;
        let entries = file_list(&repo.git, &spec)?;
        Ok(Self {
            git: repo.git,
            spec,
            entries,
            diffs: HashMap::new(),
            round: review.current_round(),
            closed: review.closed_rounds(),
            histories: HashMap::new(),
            unlisted: HashMap::new(),
        })
    }

    /// Reads the diff of the listed file at `path` (an entry's `path`) unless it is read already
    /// (blocking: one diff).
    pub fn read(&mut self, path: &str) -> Result<&FileDiff, CliError> {
        if !self.diffs.contains_key(path) {
            let entry = self
                .entries
                .iter()
                .find(|entry| entry.path == path)
                .ok_or_else(|| {
                    CliError::not_found(format!("{path} isn't among the review's changed files"))
                })?;
            let diff = file_diff(&self.git, &self.spec, entry, &DiffLimits::UNLIMITED)?;
            self.diffs.insert(path.to_string(), diff);
        }
        Ok(&self.diffs[path])
    }

    /// Reads the diff of the file each of `threads` is anchored in (none for a comment on a
    /// whole file, which places without its text).
    pub fn read_for(&mut self, threads: &[Placed]) -> Result<(), CliError> {
        let paths: Vec<String> = threads
            .iter()
            .filter(|thread| thread.is_anchored() && !thread.anchor().is_file_level())
            .filter_map(|thread| {
                let anchor = thread.anchor();
                thread.entry(&anchor.path, anchor.old_path.as_deref(), &self.entries)
            })
            .map(|entry| entry.path.clone())
            .collect();
        for path in paths {
            self.read(&path)?;
        }
        Ok(())
    }

    /// Reads the round histories `threads` (threads of `review`, their files read:
    /// [`Sides::read_for`]) need (module docs): for each closed round, the paths of its
    /// new-side threads that the current text alone leaves outdated; then the diff of each file
    /// a history says such a thread's file is now. A round whose history can't be read is
    /// warned about on stderr, and its threads are placed without it (PLAN §8).
    pub fn read_history_for(
        &mut self,
        review: &Review,
        threads: &[Placed],
        ctx: &mut Context,
    ) -> Result<(), CliError> {
        let mut wanted: BTreeMap<u32, Vec<String>> = BTreeMap::new();
        for &thread in threads {
            let anchor = thread.anchor();
            if self.history_round(anchor).is_none()
                || !self.place(thread, History::None).is_outdated()
            {
                continue;
            }
            let paths = wanted.entry(anchor.round).or_default();
            if !paths.contains(&anchor.path) {
                paths.push(anchor.path.clone());
            }
        }
        for (n, paths) in wanted {
            let Some(round) = review.rounds.get(n as usize - 1) else {
                continue;
            };
            match read_round_history(&self.git, &self.spec, round, &paths, None) {
                Ok(history) => {
                    self.histories.insert(n, history);
                }
                Err(error) => output::warning(
                    ctx,
                    &format!(
                        "couldn't read round {n}'s snapshot, so its threads are placed without \
                         it: {error}"
                    ),
                )?,
            }
        }
        let followed: Vec<String> = threads
            .iter()
            .filter(|thread| thread.is_anchored() && !thread.anchor().is_file_level())
            .filter_map(|thread| {
                let anchor = thread.anchor();
                match thread_file(anchor, self.history(anchor), |path, old| {
                    thread.entry(path, old, &self.entries)
                }) {
                    ThreadFile::Found(entry) => Some(entry.path.clone()),
                    ThreadFile::Pending(_) | ThreadFile::Gone => None,
                }
            })
            .collect();
        for path in followed {
            self.read(&path)?;
        }
        Ok(())
    }

    /// Reads, at the review's new side, the file each of `threads` whose file left the list is
    /// anchored in (module docs; blocking: a few git calls per file, each file once), after its
    /// round's history had its say ([`Sides::read_history_for`]): at the path that history says
    /// the file has now. A file that can't be read is warned about on stderr and its threads
    /// stay outdated.
    pub fn read_unlisted_for(
        &mut self,
        threads: &[Placed],
        ctx: &mut Context,
    ) -> Result<(), CliError> {
        let mut paths: Vec<String> = threads
            .iter()
            .filter(|&&thread| {
                let history = self.history(thread.anchor());
                thread.is_anchored() && is_gone(&self.place_listed(thread, history))
            })
            .map(|thread| unlisted_path(thread.anchor(), self.history(thread.anchor())))
            .collect();
        paths.sort();
        paths.dedup();
        for path in paths {
            if self.unlisted.contains_key(&path) {
                continue;
            }
            let read = match side_file(&self.git, &self.spec.new, &path, &DiffLimits::UNLIMITED) {
                Ok(read) => Some(read),
                Err(error) => {
                    output::warning(
                        ctx,
                        &format!(
                            "couldn't read {path}, which the review doesn't change, so its \
                             threads show as outdated: {error}"
                        ),
                    )?;
                    None
                }
            };
            self.unlisted.insert(path, read);
        }
        Ok(())
    }

    /// The text of the file at `path` the review doesn't list, when it was read and is text.
    fn unlisted_text(&self, path: &str) -> Option<&dv_core::diff::TextDiff> {
        self.unlisted
            .get(path)
            .and_then(Option::as_ref)
            .and_then(Option::as_ref)
            .and_then(FileDiff::as_text)
    }

    /// Whether the review's new side holds a file at `path` (read; `false` when the read
    /// failed).
    fn unlisted_present(&self, path: &str) -> bool {
        self.unlisted
            .get(path)
            .and_then(Option::as_ref)
            .is_some_and(Option::is_some)
    }

    /// The closed round whose history `anchor` may be read against: its own, on the new side.
    fn history_round(&self, anchor: &Anchor) -> Option<u32> {
        (anchor.side == AnchorSide::New && anchor.round >= 1 && anchor.round <= self.closed)
            .then_some(anchor.round)
    }

    /// The round history of the thread anchored at `anchor` as read
    /// ([`Sides::read_history_for`]): none when it wasn't read for its path (the thread
    /// didn't need it).
    fn history(&self, anchor: &Anchor) -> History<'_> {
        match self
            .history_round(anchor)
            .and_then(|round| self.histories.get(&round))
        {
            Some(history) if history.file(&anchor.path).is_some() => History::Read(history),
            _ => History::None,
        }
    }

    /// `side` of the listed file at `path`, line 1 first, when its diff is read and is text.
    pub fn side_text(&self, path: &str, side: AnchorSide) -> Option<Vec<&str>> {
        self.diffs
            .get(path)
            .and_then(FileDiff::as_text)
            .map(|text| side_lines(text, side))
    }

    /// `thread` placed against the diffs read so far (a file not read counts as one without
    /// text) with `history`, and when its file left the list, against that file's text at the
    /// new side once it is read ([`Sides::read_unlisted_for`]).
    fn place(&self, thread: Placed, history: History<'_>) -> Placement {
        let placement = self.place_listed(thread, history);
        if !is_gone(&placement) || !thread.is_anchored() {
            return placement;
        }
        let anchor = thread.anchor();
        let path = unlisted_path(anchor, history);
        let text = match self.unlisted_text(&path) {
            Some(text) => SideText::Lines(side_lines(text, AnchorSide::New)),
            None => SideText::Unavailable,
        };
        place_unlisted_anchor(anchor, &path, self.unlisted_present(&path), text)
    }

    /// `thread` placed among the listed files alone ([`Sides::place`]).
    fn place_listed(&self, thread: Placed, history: History<'_>) -> Placement {
        let side = thread.anchor().side;
        let text = |entry: &FileEntry| match self.side_text(&entry.path, side) {
            Some(lines) => SideText::Lines(lines),
            None => SideText::Unavailable,
        };
        match thread {
            Placed::Local(thread) => {
                place_thread(thread, &self.entries, history, |entry, _| text(entry))
            }
            Placed::Github(anchored) => threads::place(
                anchored,
                |path, old| threads::thread_entry(path, old, &self.entries),
                |entry, _| text(entry),
            ),
        }
    }

    /// The listed entry at `path`.
    fn entry(&self, path: &str) -> Option<&FileEntry> {
        self.entries.iter().find(|entry| entry.path == path)
    }

    /// Where `thread` stands now, from the diffs and histories read so far (a file not read
    /// counts as one without text, so read the threads' files and histories first:
    /// [`Sides::read_for`], [`Sides::read_history_for`]).
    pub fn locate(&self, thread: Placed) -> Located {
        let anchor = thread.anchor();
        let side = anchor.side;
        let placement = self.place(thread, self.history(anchor));
        let file = placement.file().map(str::to_string);
        let entry = file.as_deref().and_then(|path| self.entry(path));
        let current = match (placement.lines(anchor), entry) {
            (Some((start, end)), Some(entry)) => self.current_code(entry, side, start, end),
            (Some((start, end)), None) => placement.unlisted_path().and_then(|path| {
                let text = self.unlisted_text(path)?;
                code_in(text, path, side, start, end)
            }),
            _ => None,
        };
        let trail = match (placement.trail(), entry) {
            (Some(trail), Some(entry)) => Some(TrailCode {
                trail: *trail,
                path: AnchorPlace::for_entry(entry, AnchorSide::New, self.round).path,
                current: match trail.went {
                    Went::Replaced {
                        start_line,
                        end_line,
                    } => self.current_code(entry, AnchorSide::New, start_line, end_line),
                    Went::Removed { .. } => None,
                },
                at_end: match trail.went {
                    Went::Removed { before_line } => self
                        .side_text(&entry.path, AnchorSide::New)
                        .is_some_and(|lines| before_line as usize > lines.len()),
                    Went::Replaced { .. } => false,
                },
            }),
            _ => None,
        };
        let side_path = entry
            .map(|entry| AnchorPlace::for_entry(entry, side, self.round).path)
            .or_else(|| placement.unlisted_path().map(str::to_string));
        Located {
            placement: match placement {
                // Every side is read in full here, so nothing is left pending.
                Placement::Pending { file } => Placement::Outdated { file, trail: None },
                other => other,
            },
            side_path,
            file,
            current,
            trail,
        }
    }

    /// Lines `start..=end` of `side` of the listed `entry` now: where they are, their text and
    /// the excerpt of the diff around them (PLAN §10.3), all from the one diff read, so the
    /// excerpt never disagrees with the anchor. `None` when the file's diff isn't read or isn't
    /// text.
    fn current_code(
        &self,
        entry: &FileEntry,
        side: AnchorSide,
        start: u32,
        end: u32,
    ) -> Option<CurrentCode> {
        let text = self.diffs.get(&entry.path).and_then(FileDiff::as_text)?;
        let place = AnchorPlace::for_entry(entry, side, self.round);
        code_in(text, &place.path, side, start, end)
    }

    /// Reads what placing `threads` (threads of `review`, or GitHub's on its pull request)
    /// needs: their files, the round histories those that the current text leaves outdated
    /// need, and the files the review doesn't list that the rest are anchored in.
    fn read_all(
        &mut self,
        review: &Review,
        threads: &[Placed],
        ctx: &mut Context,
    ) -> Result<(), CliError> {
        self.read_for(threads)?;
        self.read_history_for(review, threads, ctx)?;
        self.read_unlisted_for(threads, ctx)
    }

    /// The anchor a write on `thread` records (PLAN §8): its anchor re-taken where relocation
    /// found it, in the round in progress with fresh context, when `located` says it moved (or
    /// stands in a file renamed since, its round's history included; a comment on a whole file
    /// only for that, its text unread); `None` otherwise, an outdated thread with a trail
    /// included.
    pub fn relocated(&self, thread: &Thread, located: &Located) -> Option<Anchor> {
        let entry = self.entry(located.file.as_deref()?)?;
        let lines = if thread.anchor.is_file_level() {
            Vec::new()
        } else {
            self.side_text(&entry.path, thread.anchor.side)?
        };
        relocated_anchor(
            &thread.anchor,
            &located.placement,
            entry,
            &lines,
            self.round,
        )
    }
}

/// Lines `start..=end` of `side` of `text` (a file at `path` on that side): where they are,
/// their text and the excerpt of `text` around them ([`Sides::current_code`]).
fn code_in(
    text: &dv_core::diff::TextDiff,
    path: &str,
    side: AnchorSide,
    start: u32,
    end: u32,
) -> Option<CurrentCode> {
    let rows = excerpt(text, side, start, end)?;
    let code = side_lines(text, side)
        .get((start as usize).saturating_sub(1)..end as usize)
        .map(|lines| lines.join("\n"))
        .unwrap_or_default();
    Some(CurrentCode {
        anchor: CurrentAnchorDto {
            path: path.to_string(),
            side: side_str(side),
            start_line: start,
            end_line: end,
        },
        code,
        excerpt: excerpt_lines(text, &rows).join("\n"),
    })
}

/// The current diff of `review` with what `threads` need read: their files, the round
/// histories those that the current text leaves outdated need, and the files the review
/// doesn't list that the rest are anchored in.
fn read(
    repo: &Repo,
    review: &Review,
    threads: &[&Thread],
    ctx: &mut Context,
) -> Result<Sides, CliError> {
    let mut sides = Sides::list(repo, review, ctx)?;
    let placed: Vec<Placed> = threads.iter().map(|thread| Placed::Local(thread)).collect();
    sides.read_all(review, &placed, ctx)?;
    Ok(sides)
}

/// Where each of `threads` (threads of `review`) stands now, in the same order. Blocking: one
/// file list and one diff per file a thread is anchored in, plus the round histories outdated
/// threads need.
pub(crate) fn locate(
    repo: &Repo,
    review: &Review,
    threads: &[&Thread],
    ctx: &mut Context,
) -> Result<Vec<Located>, CliError> {
    let sides = read(repo, review, threads, ctx)?;
    Ok(threads
        .iter()
        .map(|thread| sides.locate(Placed::Local(thread)))
        .collect())
}

/// Where a review's threads stand, its own and GitHub's on its pull request
/// ([`locate_with_github`]).
pub(crate) struct LocatedThreads {
    /// The review's own, in the order they were asked about.
    pub own: Vec<Located>,
    /// GitHub's that were kept, in the mirror's order: each thread's place among the
    /// mirror's threads, its anchor, and where it is.
    pub github: Vec<(usize, GithubAnchor, Located)>,
}

/// [`locate`], and with them where each of GitHub's threads on `review`'s pull request that
/// `keep` keeps stands, as `mirror` has them (PLAN §9.6). `keep` is asked once the review's
/// files are listed, with them: what it rules out is neither anchored nor placed. Blocking,
/// beside [`locate`]'s: each file a kept thread is on read once at its commit, however many
/// of them are on it, and no file only the others are on.
pub(crate) fn locate_with_github(
    repo: &Repo,
    review: &Review,
    threads: &[&Thread],
    mirror: &Mirror,
    keep: impl Fn(&GithubThread, &[FileEntry]) -> bool,
    ctx: &mut Context,
) -> Result<LocatedThreads, CliError> {
    let mut sides = Sides::list(repo, review, ctx)?;
    let texts = ThreadTexts::default();
    let kept: Vec<usize> = (0..mirror.threads.len())
        .filter(|&index| keep(&mirror.threads[index], &sides.entries))
        .collect();
    let anchors: Vec<GithubAnchor> = kept
        .iter()
        .map(|&index| {
            let thread = &mirror.threads[index];
            anchor_thread(&sides.git, mirror, &sides.spec, &texts, thread)
        })
        .collect();
    let placed: Vec<Placed> = threads
        .iter()
        .map(|thread| Placed::Local(thread))
        .chain(anchors.iter().map(Placed::Github))
        .collect();
    sides.read_all(review, &placed, ctx)?;
    let own = threads
        .iter()
        .map(|thread| sides.locate(Placed::Local(thread)))
        .collect();
    let github = anchors
        .iter()
        .map(|anchored| sides.locate(Placed::Github(anchored)))
        .collect::<Vec<_>>();
    Ok(LocatedThreads {
        own,
        github: kept
            .into_iter()
            .zip(anchors)
            .zip(github)
            .map(|((index, anchored), located)| (index, anchored, located))
            .collect(),
    })
}

/// Where `thread`, one of GitHub's on `review`'s pull request as `mirror` has it, stands
/// (PLAN §9.6), with the anchor read for it. Blocking: one file list, and that thread's file
/// alone, at its commit and in the review's diff, whatever else the pull request holds.
pub(crate) fn locate_github(
    repo: &Repo,
    review: &Review,
    mirror: &Mirror,
    thread: &GithubThread,
    ctx: &mut Context,
) -> Result<(GithubAnchor, Located), CliError> {
    let mut sides = Sides::list(repo, review, ctx)?;
    let anchored = anchor_thread(
        &sides.git,
        mirror,
        &sides.spec,
        &ThreadTexts::default(),
        thread,
    );
    sides.read_all(review, &[Placed::Github(&anchored)], ctx)?;
    let located = sides.locate(Placed::Github(&anchored));
    Ok((anchored, located))
}

/// Where `thread` (a thread of `review`) stands now, and the anchor a write on it records when
/// it moved ([`Sides::relocated`]).
pub(crate) fn locate_for_write(
    repo: &Repo,
    review: &Review,
    thread: &Thread,
    ctx: &mut Context,
) -> Result<(Located, Option<Anchor>), CliError> {
    let sides = read(repo, review, &[thread], ctx)?;
    let located = sides.locate(Placed::Local(thread));
    let relocated = sides.relocated(thread, &located);
    Ok((located, relocated))
}
