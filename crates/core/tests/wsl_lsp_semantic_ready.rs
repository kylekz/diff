//! Live verification for the syntax-vs-semantic warm-up gate
//! (docs/backlog.md "vtsls syntax-vs-semantic server race at
//! session-Ready"; see `dv_core::lsp::client::SemanticReadiness`'s doc for
//! the wire signal). Before the gate, a `textDocument/definition` fired
//! IMMEDIATELY at first-Ready could be answered by vtsls's fast `<syntax>`
//! tsserver with a non-empty, same-file-only WRONG result that the
//! empty-result warm-up retries never re-asked. With the gate, the request
//! blocks (bounded) until the `$/progress` project-load cycle completes,
//! so the very FIRST answer — no retries, no sleeps — must already be the
//! correct cross-file one. Run across several cold sessions because the
//! race is timing-dependent.
//!
//! Uses the same scratch fixture as `wsl_lsp_definition.rs` (see that
//! module's doc for the setup script).
//!
//! Run: `cargo test -p dv-core --test wsl_lsp_semantic_ready -- --ignored --nocapture`
//! (add `DV_LSP_TRACE=1` to watch the wire signal and gate timing live).

use std::time::Instant;

use dv_core::lsp::{LspHandle, SemanticWaitOutcome};
use dv_core::provision;
use dv_core::{RepoLocation, lsp};

const DISTRO: &str = "Ubuntu";
const REPO_ROOT: &str = "/home/kyle/dv-lsp-scratch";
const COLD_SESSIONS: usize = 5;

const INDEX_TS: &str = "import { greet } from \"./helper\";\n\
\n\
function main(): void {\n\
  console.log(greet(\"world\"));\n\
}\n\
\n\
main();\n";

#[test]
#[ignore = "requires WSL Ubuntu with the scratch TS fixture at ~/dv-lsp-scratch — see wsl_lsp_definition.rs"]
fn first_post_ready_definition_is_cross_file_across_cold_sessions() {
    let location = RepoLocation::Wsl {
        distro: DISTRO.to_string(),
        path: REPO_ROOT.to_string(),
    };
    let node = provision::detect_node_vtsls(DISTRO, None)
        .expect("detect_node_vtsls against the real Ubuntu distro");
    assert!(
        node.vtsls_path.is_some(),
        "vtsls must be installed for this live test to mean anything"
    );
    let root_uri = lsp::file_uri(REPO_ROOT);
    let index_uri = lsp::file_uri(&format!("{REPO_ROOT}/src/index.ts"));

    // Line 3 (0-based): `  console.log(greet("world"));` — column 16 sits
    // inside the `greet` identifier. The syntax server's wrong answer for
    // this position is a same-file import-line hit; the semantic server's
    // correct answer crosses into helper.ts.
    let call_site = lsp_types::Position {
        line: 3,
        character: 16,
    };

    for session in 0..COLD_SESSIONS {
        let spawn_started = Instant::now();
        let handle =
            LspHandle::spawn(&location, &node, &root_uri).expect("spawn the real vtsls --stdio");
        let ready_at = spawn_started.elapsed();
        handle
            .did_open(&index_uri, "typescript", 1, INDEX_TS)
            .expect("didOpen should not fail against a live vtsls");

        // The point of this test: the FIRST request, fired immediately at
        // first-Ready — no sleeps, no retries, no keep-the-last-answer
        // polling.
        let request_started = Instant::now();
        let links = handle
            .definition(&index_uri, call_site)
            .expect("definition request failed");
        let answered_in = request_started.elapsed();

        // The gate must have resolved via the REAL wire signal — a
        // NoSignal/TimedOut degrade here would mean the semantic-ready
        // signal was not actually observed and this test proves nothing.
        let outcome = handle.wait_semantic_ready();
        println!(
            "session {session}: ready {:.2}s, first answer {:.2}s, gate {outcome:?}, targets {:?}",
            ready_at.as_secs_f64(),
            answered_in.as_secs_f64(),
            links
                .iter()
                .map(|l| l.target_uri.as_str().to_string())
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            outcome,
            SemanticWaitOutcome::Ready,
            "session {session}: semantic-ready signal was not observed on the wire"
        );

        assert_eq!(
            links.len(),
            1,
            "session {session}: expected exactly one definition target"
        );
        assert!(
            links[0].target_uri.as_str().ends_with("/src/helper.ts"),
            "session {session}: FIRST post-Ready answer should already cross into \
             helper.ts (the semantic server's answer), got {}",
            links[0].target_uri.as_str()
        );

        handle.shutdown();
    }
}
