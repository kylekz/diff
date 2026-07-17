//! Live WSL hover + find-references test for Phase 8's vtsls client (S8g,
//! docs/phase-8-lsp-and-polish.md § LSP) — spawns the REAL vtsls (via
//! [`dv_core::lsp::LspHandle::spawn`]) against the same scratch TypeScript
//! repo `wsl_lsp_definition.rs` uses and drives real `textDocument/hover`
//! and `textDocument/references` round trips end to end (P3 finding: S8g
//! shipped both without any committed, repeatable live test — the module's
//! only prior coverage was a pure unit test of `hover_contents_to_text` and
//! an ad-hoc manual verification via automation). This is the one place
//! outside `dv_core::lsp` allowed to name a distro directly, standing in
//! for the app's own "this distro is already live" gate (see
//! `dv_core::provision`'s module doc for why that gate must live in the
//! app, not here).
//!
//! **Fixture**: reuses `wsl_lsp_definition.rs`'s `~/dv-lsp-scratch` — see
//! that file's module doc for the exact setup script. Not recreated here;
//! both tests share the one fixture.
//!
//! Run: `cargo test -p dv-core --test wsl_lsp_hover -- --ignored --nocapture`

use std::time::Duration;

use dv_core::lsp::LspHandle;
use dv_core::provision;
use dv_core::{RepoLocation, lsp};

const DISTRO: &str = "Ubuntu";
const REPO_ROOT: &str = "/home/kyle/dv-lsp-scratch";

const INDEX_TS: &str = "import { greet } from \"./helper\";\n\
\n\
function main(): void {\n\
  console.log(greet(\"world\"));\n\
}\n\
\n\
main();\n";

#[test]
#[ignore = "requires WSL Ubuntu with the scratch TS fixture at ~/dv-lsp-scratch — see wsl_lsp_definition.rs's module docs"]
fn hover_reports_the_real_signature_across_files() {
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
    let handle =
        LspHandle::spawn(&location, &node, &root_uri).expect("spawn the real vtsls --stdio");

    let index_uri = lsp::file_uri(&format!("{REPO_ROOT}/src/index.ts"));
    handle
        .did_open(&index_uri, "typescript", 1, INDEX_TS)
        .expect("didOpen should not fail against a live vtsls");

    // Same call-site position `wsl_lsp_definition.rs` uses for its hop-1
    // assertion: line 3 (0-based), column 16 sits inside the `greet`
    // identifier in `console.log(greet("world"));`.
    let call_site = lsp_types::Position {
        line: 3,
        character: 16,
    };
    let hover = request_hover_with_retry(&handle, &index_uri, call_site)
        .expect("expected a non-empty hover answer once vtsls's semantic server is up");
    let text = lsp::hover_contents_to_text(&hover.contents)
        .expect("hover contents should normalize to non-empty text");
    assert!(
        text.contains("greet(name: string): string"),
        "expected the real `greet` signature in the hover text, got: {text}"
    );

    handle.shutdown();
}

#[test]
#[ignore = "requires WSL Ubuntu with the scratch TS fixture at ~/dv-lsp-scratch — see wsl_lsp_definition.rs's module docs"]
fn references_finds_the_call_site_and_the_declaration() {
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
    let handle =
        LspHandle::spawn(&location, &node, &root_uri).expect("spawn the real vtsls --stdio");

    let index_uri = lsp::file_uri(&format!("{REPO_ROOT}/src/index.ts"));
    handle
        .did_open(&index_uri, "typescript", 1, INDEX_TS)
        .expect("didOpen should not fail against a live vtsls");

    let call_site = lsp_types::Position {
        line: 3,
        character: 16,
    };
    let locations = request_references_with_retry(&handle, &index_uri, call_site);
    // `greet`'s declaration lives in helper.ts, its one call site in
    // index.ts — with `includeDeclaration: true` that's at least 2 spread
    // across both files.
    assert!(
        locations.len() >= 2,
        "expected at least 2 reference locations (declaration + call site), got {}",
        locations.len()
    );
    assert!(
        locations
            .iter()
            .any(|loc| loc.uri.as_str().ends_with("/src/helper.ts")),
        "expected a reference in helper.ts (the declaration), got {locations:?}"
    );
    assert!(
        locations
            .iter()
            .any(|loc| loc.uri.as_str().ends_with("/src/index.ts")),
        "expected a reference in index.ts (the call site), got {locations:?}"
    );

    handle.shutdown();
}

const HELPER_TS: &str = "export function greet(name: string): string {\n\
  return \"hello \" + name;\n\
}\n";

/// Live half of the tautological-hover suppression (R3;
/// `dv_core::lsp::definition_covers_position`'s pure unit tests cover
/// the geometry): against the REAL vtsls, a definition lookup from the
/// `greet` declaration's own name token must cover that position (the
/// app's cue to drop the hover popover), while the same lookup from the
/// call site must not — proving real vtsls `targetSelectionRange`s behave
/// the way the suppression assumes.
#[test]
#[ignore = "requires WSL Ubuntu with the scratch TS fixture at ~/dv-lsp-scratch — see wsl_lsp_definition.rs's module docs"]
fn declaration_hover_is_flagged_tautological_but_call_site_is_not() {
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
    let handle =
        LspHandle::spawn(&location, &node, &root_uri).expect("spawn the real vtsls --stdio");

    let helper_uri = lsp::file_uri(&format!("{REPO_ROOT}/src/helper.ts"));
    let index_uri = lsp::file_uri(&format!("{REPO_ROOT}/src/index.ts"));
    handle
        .did_open(&helper_uri, "typescript", 1, HELPER_TS)
        .expect("didOpen helper.ts");
    handle
        .did_open(&index_uri, "typescript", 1, INDEX_TS)
        .expect("didOpen index.ts");

    // `export function greet(` — "export function " is 16 chars, so col 17
    // sits inside the `greet` name token on line 0.
    let decl_pos = lsp_types::Position {
        line: 0,
        character: 17,
    };
    let targets = request_definition_with_retry(&handle, &helper_uri, decl_pos);
    assert!(
        !targets.is_empty(),
        "definition from the declaration itself should resolve (to itself)"
    );
    assert!(
        lsp::definition_covers_position(&targets, &helper_uri, decl_pos),
        "hovering the declaration name must be flagged tautological, got {targets:?}"
    );

    // The call site in index.ts resolves to helper.ts — not covered, so
    // the hover there stays.
    let call_site = lsp_types::Position {
        line: 3,
        character: 16,
    };
    let targets = request_definition_with_retry(&handle, &index_uri, call_site);
    assert!(
        !targets.is_empty(),
        "definition from the call site should resolve"
    );
    assert!(
        !lsp::definition_covers_position(&targets, &index_uri, call_site),
        "a call-site hover must never be flagged tautological, got {targets:?}"
    );

    handle.shutdown();
}

/// Same retry posture as [`request_hover_with_retry`], for
/// `textDocument/definition` — poll until a non-empty answer (the
/// semantic tsserver's) lands, keeping the last.
fn request_definition_with_retry(
    handle: &LspHandle,
    uri: &str,
    position: lsp_types::Position,
) -> Vec<lsp_types::LocationLink> {
    std::thread::sleep(Duration::from_secs(3));
    let mut last = Vec::new();
    for attempt in 0..6 {
        last = handle
            .definition(uri, position)
            .unwrap_or_else(|err| panic!("definition request failed on attempt {attempt}: {err}"));
        if !last.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    last
}

/// Same warm-up-retry posture as `wsl_lsp_definition.rs`'s
/// `request_definition_with_retry`: vtsls forks a fast-but-wrong `<syntax>`
/// tsserver almost immediately and a slower `<semantic>` one a couple
/// seconds later, so a request that races ahead of the semantic server can
/// get a fast, empty (or wrong) answer. Give it a head start, then poll a
/// handful of times and keep the LAST answer.
fn request_hover_with_retry(
    handle: &LspHandle,
    uri: &str,
    position: lsp_types::Position,
) -> Option<lsp_types::Hover> {
    std::thread::sleep(Duration::from_secs(3));
    let mut last = None;
    for attempt in 0..6 {
        last = handle
            .hover(uri, position)
            .unwrap_or_else(|err| panic!("hover request failed on attempt {attempt}: {err}"));
        std::thread::sleep(Duration::from_millis(500));
    }
    last
}

/// Same retry posture as [`request_hover_with_retry`], for
/// `textDocument/references`.
fn request_references_with_retry(
    handle: &LspHandle,
    uri: &str,
    position: lsp_types::Position,
) -> Vec<lsp_types::Location> {
    std::thread::sleep(Duration::from_secs(3));
    let mut last = Vec::new();
    for attempt in 0..6 {
        last = handle
            .references(uri, position)
            .unwrap_or_else(|err| panic!("references request failed on attempt {attempt}: {err}"));
        std::thread::sleep(Duration::from_millis(500));
    }
    last
}
