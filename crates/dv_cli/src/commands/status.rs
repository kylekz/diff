//! `dv status` (docs/PLAN.md §10.1): the repository, who the command acts as, its open
//! reviews, and the current one (PLAN §10.2) with its round and thread counts. An agent's first
//! call: it says which review the other commands will act on.

use super::format;
use crate::args::GlobalArgs;
use crate::context::Context;
use crate::dto::{AuthorDto, BuildDto, RepoDto, ReviewDto, StatusDoc};
use crate::error::{CliError, Exit};
use crate::identity;
use crate::output;
use crate::repo::Repo;
use crate::select::{self, Listing};

pub(crate) fn status(global: &GlobalArgs, ctx: &mut Context) -> Result<Exit, CliError> {
    let repo = Repo::open(global, ctx)?;
    let author = identity::author(global, ctx, repo.backend.as_ref())?;
    let labels = repo.labels();
    let listing = Listing::read(&repo)?;
    let current = select::current(&repo, global, ctx, &listing)?;
    let reviews = super::review::load_all(&repo, listing.open())?;
    let doc = StatusDoc {
        dv: BuildDto::this(),
        repo: RepoDto {
            root: repo.root().to_string(),
            branch: labels.here.branch.clone(),
            head: labels.here.head.clone(),
        },
        author: (&author).into(),
        current: current
            .as_ref()
            .map(|review| ReviewDto::new(review, &labels)),
        reviews: reviews
            .iter()
            .map(|review| ReviewDto::new(review, &labels))
            .collect(),
        unreadable: listing.unreadable,
    };
    if global.json {
        output::json(ctx, &doc)?;
    } else {
        output::text(ctx, &text(&doc))?;
    }
    Ok(Exit::Ok)
}

fn text(doc: &StatusDoc) -> Vec<String> {
    let head = match (&doc.repo.branch, &doc.repo.head) {
        (Some(branch), _) => format!(" (on {branch})"),
        (None, Some(head)) => format!(" (HEAD detached at {head})"),
        (None, None) => String::new(),
    };
    let mut lines = vec![
        format!("repo: {}{head}", doc.repo.root),
        format!("acting as: {}", author(&doc.author)),
    ];
    lines.push(match &doc.current {
        Some(current) => format!("current review: {}", format::review_line(current)),
        _ if !doc.reviews.is_empty() => {
            "current review: none (name one with --review <id>)".to_string()
        }
        _ => "current review: none".to_string(),
    });
    if doc.reviews.is_empty() {
        lines.push("open reviews: none".to_string());
    } else {
        lines.push(format!("open reviews ({}):", doc.reviews.len()));
        lines.extend(
            doc.reviews
                .iter()
                .map(|review| format!("  {}", format::review_line(review))),
        );
    }
    lines.extend(
        doc.unreadable
            .iter()
            .map(|entry| format!("unreadable: {}: {}", entry.id, entry.error)),
    );
    lines
}

/// `agent claude-code`, `agent claude-code (Claude Fable 5.2)`, `reviewer Kyle`.
pub(crate) fn author(author: &AuthorDto) -> String {
    match &author.model {
        Some(model) => format!("{} {} ({model})", author.kind, author.name),
        None => format!("{} {}", author.kind, author.name),
    }
}
