//! Lists read whole (docs/PLAN.md D7, §9.2).
//!
//! - **REST** lists through gh's own paging, `gh api --paginate --slurp`, [`PAGE_SIZE`] items a
//!   request.
//! - **GraphQL** connections through a loop of dv's own ([`Gh::connection`]): the query takes
//!   `$first` and `$cursor`, the loop asks again from each page's `endCursor` until
//!   `hasNextPage` is false. It stops after [`MAX_PAGES`] pages or at a cursor it has already
//!   asked from, with a warning ([`Truncated`]), so it can't spin forever.
//! - **A nested connection** that has more pages (a thread's comments past the first hundred)
//!   is read on with a follow-up query on its node ([`Nested`]).

use serde_json::{Map, Value};

use super::super::error::{GithubError, unexpected};
use super::super::repo::GithubRepo;
use super::{Call, Gh, Kind, api_args, response};

/// How many items one request asks for: the most GitHub gives.
pub const PAGE_SIZE: u64 = 100;
/// The most pages a GraphQL connection is read for.
pub const MAX_PAGES: usize = 50;

/// A GraphQL connection to read whole.
#[derive(Debug, Clone)]
pub struct Connection<'a> {
    /// A query that declares `$first: Int!` and `$cursor: String`, gives them to the
    /// connection (`first: $first, after: $cursor`) and selects its `nodes` and `pageInfo {
    /// hasNextPage endCursor }`.
    pub query: &'a str,
    /// The query's other variables.
    pub variables: Map<String, Value>,
    /// Where the connection is in the answer's `data`: `["repository", "pullRequest",
    /// "reviewThreads"]`.
    pub path: &'a [&'a str],
    /// A connection inside each node to read whole as well.
    pub nested: Option<Nested<'a>>,
}

/// A connection inside each node of another (a review thread's `comments`).
#[derive(Debug, Clone, Copy)]
pub struct Nested<'a> {
    /// The node's field holding it, which the outer query selects with `nodes` and
    /// `pageInfo`, beside the node's `id`.
    pub field: &'a str,
    /// The follow-up query on one node: it declares `$id: ID!`, `$first: Int!` and `$cursor:
    /// String` (`node(id: $id) { ... on PullRequestReviewThread { comments(first: $first,
    /// after: $cursor) { … } } }`).
    pub query: &'a str,
    /// Where the connection is in the follow-up's `data`: `["node", "comments"]`.
    pub path: &'a [&'a str],
}

/// A connection's nodes, each nested connection's `nodes` whole too.
#[derive(Debug, Clone, PartialEq)]
pub struct Paged {
    pub nodes: Vec<Value>,
    /// Why the nodes aren't all of them, when the loop stopped early.
    pub truncated: Option<Truncated>,
}

/// Why a connection's loop stopped before its last page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truncated {
    /// GitHub handed back a cursor the loop had already asked from.
    RepeatedCursor,
    /// [`MAX_PAGES`] pages were read and there were more.
    PageLimit,
}

impl Gh {
    /// Every item of a REST list of `repo`: `gh api repos/<owner>/<repo>/<list>?per_page=100
    /// --hostname=<host> --paginate --slurp` (`list`: `pulls/482/files`, or with a query of
    /// its own, `pulls?state=open`).
    pub fn repo_list(&self, repo: &GithubRepo, list: &str) -> Result<Vec<Value>, GithubError> {
        let endpoint = format!(
            "repos/{}/{}/{list}{}per_page={PAGE_SIZE}",
            repo.owner,
            repo.repo,
            if list.contains('?') { '&' } else { '?' }
        );
        let call = Call {
            args: api_args(&endpoint, &repo.host, &["--paginate", "--slurp"]),
            body: None,
            host: &repo.host,
            kind: Kind::List,
            without: &[],
        };
        // `--slurp` wraps the pages, each a JSON array, in one array.
        let pages = match response::json(&call.args, &self.run(&call)?)? {
            Value::Array(pages) => pages,
            other => return Err(not_a_list("the pages aren't an array", &other)),
        };
        let mut items = Vec::new();
        for page in pages {
            match page {
                Value::Array(page) => items.extend(page),
                other => return Err(not_a_list("a page isn't an array", &other)),
            }
        }
        Ok(items)
    }

    /// Every node of a GraphQL connection (module docs).
    pub fn connection(
        &self,
        host: &str,
        connection: &Connection<'_>,
    ) -> Result<Paged, GithubError> {
        let mut paged = self.pages(
            host,
            connection.query,
            &connection.variables,
            connection.path,
            None,
        )?;
        let Some(nested) = connection.nested else {
            return Ok(paged);
        };
        for node in &mut paged.nodes {
            let Some(inner) = node.get(nested.field).filter(|inner| inner.is_object()) else {
                continue;
            };
            let Some(cursor) = next_cursor(inner)? else {
                continue;
            };
            let id = node
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| not_a_connection("a node with more pages has no id", node))?;
            let variables = Map::from_iter([("id".to_string(), Value::from(id))]);
            let rest = self.pages(host, nested.query, &variables, nested.path, Some(cursor))?;
            match node
                .get_mut(nested.field)
                .and_then(|inner| inner.get_mut("nodes")?.as_array_mut())
            {
                Some(nodes) => nodes.extend(rest.nodes),
                None => return Err(not_a_connection("a connection without nodes", node)),
            }
            paged.truncated = paged.truncated.or(rest.truncated);
        }
        Ok(paged)
    }

    /// The nodes of the connection at `path` in `query`'s answer, from `start` (`None`: its
    /// beginning) to its end.
    fn pages(
        &self,
        host: &str,
        query: &str,
        variables: &Map<String, Value>,
        path: &[&str],
        start: Option<String>,
    ) -> Result<Paged, GithubError> {
        let mut variables = variables.clone();
        variables.insert("first".to_string(), Value::from(PAGE_SIZE));
        let mut nodes = Vec::new();
        let mut asked: Vec<String> = start.iter().cloned().collect();
        let mut cursor = start;
        for _ in 0..MAX_PAGES {
            variables.insert("cursor".to_string(), Value::from(cursor.take()));
            let mut data = self.graphql(host, query, &variables)?;
            let Some(connection) = data
                .pointer_mut(&format!("/{}", path.join("/")))
                .filter(|connection| connection.is_object())
            else {
                return Err(not_a_connection(
                    &format!("no connection at {}", path.join(".")),
                    &data,
                ));
            };
            let next = next_cursor(connection)?;
            match connection.get_mut("nodes").map(Value::take) {
                Some(Value::Array(page)) => nodes.extend(page),
                _ => return Err(not_a_connection("a connection without nodes", connection)),
            }
            let Some(next) = next else {
                return Ok(Paged {
                    nodes,
                    truncated: None,
                });
            };
            if asked.contains(&next) {
                log::warn!(
                    "gh api graphql: {} gave the cursor {next} again; stopped after {} items",
                    path.join("."),
                    nodes.len()
                );
                return Ok(Paged {
                    nodes,
                    truncated: Some(Truncated::RepeatedCursor),
                });
            }
            asked.push(next.clone());
            cursor = Some(next);
        }
        log::warn!(
            "gh api graphql: {} has more than {MAX_PAGES} pages; stopped after {} items",
            path.join("."),
            nodes.len()
        );
        Ok(Paged {
            nodes,
            truncated: Some(Truncated::PageLimit),
        })
    }
}

/// The cursor a connection's next page starts from; `None` on its last page.
fn next_cursor(connection: &Value) -> Result<Option<String>, GithubError> {
    let info = connection.get("pageInfo");
    let field = |name: &str| info.and_then(|info| info.get(name));
    match (
        field("hasNextPage").and_then(Value::as_bool),
        field("endCursor").and_then(Value::as_str),
    ) {
        (Some(false), _) => Ok(None),
        (Some(true), Some(cursor)) => Ok(Some(cursor.to_string())),
        _ => Err(not_a_connection(
            "a connection without pageInfo { hasNextPage endCursor }",
            connection,
        )),
    }
}

/// A REST list's answer that isn't pages of items, with what it was.
fn not_a_list(detail: &str, answer: &Value) -> GithubError {
    unexpected("api", detail, &answer.to_string())
}

/// A GraphQL answer without the connection asked for, with the part that should hold it.
fn not_a_connection(detail: &str, answer: &Value) -> GithubError {
    unexpected("api graphql", detail, &answer.to_string())
}
