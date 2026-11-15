//! Lists read whole (PLAN §9.2, D7): gh's own paging for REST, dv's loop for GraphQL.

use super::*;

const THREADS: &[&str] = &["repository", "pullRequest", "reviewThreads"];

/// A scenario entry answering the GraphQL request that holds `needle` with one page of the
/// connection at [`THREADS`].
fn page(needle: &str, nodes: Value, next: Option<&str>) -> Value {
    json!({
        "argv": ["api", "graphql", "**"],
        "stdin_contains": needle,
        "stdout": {"data": {"repository": {"pullRequest": {"reviewThreads": connection(nodes, next)}}}}
    })
}

/// A connection's page: its nodes, and the cursor of the next page when there is one.
fn connection(nodes: Value, next: Option<&str>) -> Value {
    json!({"nodes": nodes, "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next}})
}

fn threads(query: &str) -> Connection<'_> {
    Connection {
        query,
        variables: variables(json!({"owner": "acme", "repo": "api", "number": 482})),
        path: THREADS,
        nested: None,
    }
}

#[test]
fn a_rest_list_is_every_page_gh_slurped() {
    let mock = MockGh::new(json!([{
        "argv": ["api", "**"],
        "stdout": [
            [{"filename": "a.rs"}, {"filename": "b.rs"}],
            [{"filename": "c.rs"}],
            []
        ]
    }]));
    let files = mock.gh.repo_list(&repo(), "pulls/482/files").unwrap();
    assert_eq!(
        files,
        [
            json!({"filename": "a.rs"}),
            json!({"filename": "b.rs"}),
            json!({"filename": "c.rs"})
        ]
    );
    assert_eq!(
        mock.calls(),
        [json!({
            "argv": [
                "api",
                "repos/acme/api/pulls/482/files?per_page=100",
                "--hostname=github.com",
                "--paginate",
                "--slurp"
            ],
            "stdin": null,
            "matched": 0
        })]
    );
}

#[test]
fn a_list_with_a_query_of_its_own_keeps_it() {
    let mock = MockGh::new(json!([{"argv": ["api", "**"], "stdout": [[]]}]));
    mock.gh
        .repo_list(&repo(), "pulls?state=open&sort=updated")
        .unwrap();
    assert_eq!(
        mock.calls()[0]["argv"][1],
        json!("repos/acme/api/pulls?state=open&sort=updated&per_page=100")
    );
}

#[test]
fn a_rest_list_that_is_not_pages_of_items_is_unexpected() {
    for stdout in [json!({"message": "?"}), json!([{"filename": "a.rs"}])] {
        let mock = MockGh::new(json!([{"argv": ["api", "**"], "stdout": stdout}]));
        let error = mock.gh.repo_list(&repo(), "pulls/482/files").unwrap_err();
        assert!(
            matches!(error, GithubError::UnexpectedOutput { .. }),
            "{error:?}"
        );
    }
    let mock = MockGh::new(json!([
        {"argv": ["api", "**"], "stderr": "gh: Not Found (HTTP 404)\n", "exit": 1}
    ]));
    let error = mock.gh.repo_list(&repo(), "pulls/999/files").unwrap_err();
    assert!(matches!(error, GithubError::NotFound { .. }), "{error:?}");
}

#[test]
fn a_connection_is_read_to_its_last_page() {
    let mock = MockGh::new(json!([
        page("\"cursor\":\"c2\"", json!([{"id": "T5"}]), None),
        page(
            "\"cursor\":\"c1\"",
            json!([{"id": "T3"}, {"id": "T4"}]),
            Some("c2")
        ),
        page(
            "\"cursor\":null",
            json!([{"id": "T1"}, {"id": "T2"}]),
            Some("c1")
        ),
    ]));
    let query = "query($owner: String!, $repo: String!, $number: Int!, $first: Int!, $cursor: String) { … }";
    let paged = mock.gh.connection(HOST, &threads(query)).unwrap();
    assert_eq!(paged.truncated, None);
    let ids: Vec<&str> = paged
        .nodes
        .iter()
        .map(|node| node["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["T1", "T2", "T3", "T4", "T5"]);

    // Each request is the same query, 100 a page, from the cursor before it.
    let request = |cursor: Value| {
        json!({
            "query": query,
            "variables": {"owner": "acme", "repo": "api", "number": 482, "first": 100, "cursor": cursor}
        })
    };
    assert_eq!(
        mock.requests(),
        [
            request(Value::Null),
            request(json!("c1")),
            request(json!("c2"))
        ]
    );
    assert_eq!(PAGE_SIZE, 100);
}

#[test]
fn a_nested_connection_with_more_pages_is_read_on_from_its_node() {
    let comments_of = |cursor: &str, nodes: Value, next: Option<&str>| {
        json!({
            "argv": ["api", "graphql", "**"],
            "stdin_contains": format!("\"cursor\":\"{cursor}\""),
            "stdout": {"data": {"node": {"comments": connection(nodes, next)}}}
        })
    };
    let mock = MockGh::new(json!([
        comments_of("k1", json!([{"id": "C3"}, {"id": "C4"}]), Some("k2")),
        comments_of("k2", json!([{"id": "C5"}]), None),
        page(
            "\"cursor\":null",
            json!([
                {"id": "T1", "comments": connection(json!([{"id": "C1"}]), None)},
                {"id": "T2", "comments": connection(json!([{"id": "C2"}]), Some("k1"))},
                {"id": "T3", "comments": connection(json!([]), None)}
            ]),
            None
        ),
    ]));
    let follow_up = "query($id: ID!, $first: Int!, $cursor: String) { node(id: $id) { … } }";
    let paged = mock
        .gh
        .connection(
            HOST,
            &Connection {
                nested: Some(Nested {
                    field: "comments",
                    query: follow_up,
                    path: &["node", "comments"],
                }),
                ..threads("query { … }")
            },
        )
        .unwrap();
    assert_eq!(paged.truncated, None);
    let comments = |thread: usize| -> Vec<&str> {
        paged.nodes[thread]["comments"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|comment| comment["id"].as_str().unwrap())
            .collect()
    };
    assert_eq!(comments(0), ["C1"]);
    assert_eq!(comments(1), ["C2", "C3", "C4", "C5"]);
    assert_eq!(comments(2), [""; 0]);

    // Only the thread with more pages is asked about, from where its first page ended.
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    for (request, cursor) in requests[1..].iter().zip(["k1", "k2"]) {
        assert_eq!(
            *request,
            json!({
                "query": follow_up,
                "variables": {"id": "T2", "first": 100, "cursor": cursor}
            })
        );
    }
}

#[test]
fn a_cursor_that_repeats_stops_the_loop() {
    let mock = MockGh::new(json!([
        // GitHub hands back the cursor the page was asked from.
        page("\"cursor\":\"c1\"", json!([{"id": "T2"}]), Some("c1")),
        page("\"cursor\":null", json!([{"id": "T1"}]), Some("c1")),
    ]));
    let paged = mock.gh.connection(HOST, &threads("query { … }")).unwrap();
    assert_eq!(paged.truncated, Some(Truncated::RepeatedCursor));
    assert_eq!(paged.nodes, [json!({"id": "T1"}), json!({"id": "T2"})]);
    assert_eq!(mock.calls().len(), 2, "it asked from c1 once");

    // A cycle longer than one page stops too, and a nested loop's stop is the whole read's.
    let mock = MockGh::new(json!([
        {
            "argv": ["api", "graphql", "**"],
            "stdin_contains": "\"cursor\":\"k2\"",
            "stdout": {"data": {"node": {"comments": connection(json!([{"id": "C3"}]), Some("k1"))}}}
        },
        {
            "argv": ["api", "graphql", "**"],
            "stdin_contains": "\"cursor\":\"k1\"",
            "stdout": {"data": {"node": {"comments": connection(json!([{"id": "C2"}]), Some("k2"))}}}
        },
        page(
            "\"cursor\":null",
            json!([{"id": "T1", "comments": connection(json!([{"id": "C1"}]), Some("k1"))}]),
            None
        ),
    ]));
    let paged = mock
        .gh
        .connection(
            HOST,
            &Connection {
                nested: Some(Nested {
                    field: "comments",
                    query: "query { … }",
                    path: &["node", "comments"],
                }),
                ..threads("query { … }")
            },
        )
        .unwrap();
    assert_eq!(paged.truncated, Some(Truncated::RepeatedCursor));
    assert_eq!(
        paged.nodes[0]["comments"]["nodes"],
        json!([{"id": "C1"}, {"id": "C2"}, {"id": "C3"}])
    );
    assert_eq!(mock.calls().len(), 3);
}

#[test]
fn a_connection_stops_at_fifty_pages() {
    assert_eq!(MAX_PAGES, 50);
    // Page n holds one node and always says there is another.
    let entries: Vec<Value> = (0..=MAX_PAGES)
        .map(|n| {
            let needle = match n {
                0 => "\"cursor\":null".to_string(),
                n => format!("\"cursor\":\"c{n}\""),
            };
            page(&needle, json!([{"id": n}]), Some(&format!("c{}", n + 1)))
        })
        .collect();
    let mock = MockGh::new(Value::Array(entries));
    let paged = mock.gh.connection(HOST, &threads("query { … }")).unwrap();
    assert_eq!(paged.truncated, Some(Truncated::PageLimit));
    assert_eq!(paged.nodes.len(), MAX_PAGES);
    assert_eq!(mock.calls().len(), MAX_PAGES);
}

#[test]
fn a_page_that_fails_or_is_no_connection_fails_the_read() {
    let mock = MockGh::new(json!([
        {
            "argv": ["api", "graphql", "**"],
            "stdin_contains": "\"cursor\":\"c1\"",
            "stderr": "gh: Server Error (HTTP 502)\n",
            "exit": 1
        },
        page("\"cursor\":null", json!([{"id": "T1"}]), Some("c1")),
    ]));
    let error = mock
        .gh
        .connection(HOST, &threads("query { … }"))
        .unwrap_err();
    assert!(
        matches!(error, GithubError::Unavailable { .. }),
        "{error:?}"
    );

    for data in [
        json!({"repository": {"pullRequest": null}}),
        json!({"repository": {"pullRequest": {"reviewThreads": {"nodes": []}}}}),
        json!({"repository": {"pullRequest": {"reviewThreads": {
            "pageInfo": {"hasNextPage": true, "endCursor": null}, "nodes": []
        }}}}),
        json!({"repository": {"pullRequest": {"reviewThreads": {
            "pageInfo": {"hasNextPage": false, "endCursor": null}
        }}}}),
    ] {
        let mock = MockGh::new(json!([
            {"argv": ["api", "graphql", "**"], "stdout": {"data": data}}
        ]));
        let error = mock
            .gh
            .connection(HOST, &threads("query { … }"))
            .unwrap_err();
        assert!(
            matches!(&error, GithubError::UnexpectedOutput { command, .. } if command == "api graphql"),
            "{error:?}"
        );
    }
}
