//! Live WSL go-to-definition test for Phase 8's vtsls client (S8f,
//! docs/phase-8-lsp-and-polish.md § LSP) — spawns the REAL vtsls (via
//! [`dv_core::lsp::LspHandle::spawn`]) against a scratch TypeScript repo
//! inside the Ubuntu distro and drives a real `textDocument/definition`
//! round trip end to end. This is the one place outside `dv_core::lsp`
//! allowed to name a distro directly, standing in for the app's own
//! "this distro is already live" gate (see `dv_core::provision`'s module
//! doc for why that gate must live in the app, not here).
//!
//! **Fixture setup** (not committed — build it fresh inside Ubuntu's own
//! ext4, never under `/mnt/d/...`'s 9P):
//!
//! ```bash
//! wsl.exe -d Ubuntu --exec bash -lc '
//!   rm -rf ~/dv-lsp-scratch
//!   mkdir -p ~/dv-lsp-scratch/src
//!   cd ~/dv-lsp-scratch
//!   git init -q
//!   cat > tsconfig.json <<EOF
//! { "compilerOptions": { "target": "es2020", "module": "commonjs", "strict": true } }
//! EOF
//!   cat > src/helper.ts <<EOF
//! export function greet(name: string): string {
//!   return "hello " + name;
//! }
//! EOF
//!   cat > src/index.ts <<EOF
//! import { greet } from "./helper";
//!
//! function main(): void {
//!   console.log(greet("world"));
//! }
//!
//! main();
//! EOF
//!   git add -A
//!   git -c user.email=a@b.c -c user.name=dv commit -q -m "initial"
//! '
//! ```
//!
//! Optional (not required for this test to pass, but matches the doc's
//! "requires `node_modules`" caveat for real project-wide resolution):
//! `"$node" "$npm" install --no-save typescript` inside the fixture,
//! using the asdf-resolved `node`/`npm` paths — see
//! `crates/core/src/provision/node.rs`'s module doc for the shebang trap
//! that makes the plain `npm install` form fail under `wsl.exe --exec`.
//!
//! Run: `cargo test -p dv-core --test wsl_lsp_definition -- --ignored --nocapture`
//!
//! Clean up afterward: `wsl.exe -d Ubuntu --exec rm -rf /home/kyle/dv-lsp-scratch`.

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
#[ignore = "requires WSL Ubuntu with the scratch TS fixture at ~/dv-lsp-scratch — see module docs"]
fn go_to_definition_finds_the_real_symbol_across_files() {
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

    // Two real go-to-definition requests against the live server, both
    // VERIFIED to cross into helper.ts against this exact fixture. Getting
    // here required two fixes (P1 finding): (1) `initialize` must advertise
    // `workspaceFolders` (plus the `workspace.workspaceFolders` capability),
    // not just `rootUri` — vtsls's project/module-resolution model keys off
    // the workspace-folder list; with only `rootUri` a call-site click on
    // `greet` landed back on index.ts's own import line instead of crossing
    // files. (2) vtsls forks TWO tsserver instances — a `<syntax>` one that
    // comes up almost immediately and a `<semantic>` one (the one that
    // actually does cross-file module resolution) that forks and starts a
    // couple seconds later (observable via `window/logMessage`). A
    // `definition` request that races ahead of the semantic server's
    // startup gets a fast, confidently WRONG, non-empty answer from the
    // syntax server — which is why the naive "retry until non-empty" helper
    // this test used to have never actually retried (the first, wrong,
    // answer already satisfied its "not empty" stop condition). See
    // `request_definition_with_retry`'s doc for the fixed retry strategy.

    // Hop 1 — line 3 (0-based): `  console.log(greet("world"));`; column 16
    // lands inside the `greet` identifier (2 spaces + "console.log(" = 14
    // chars, +2 more to sit mid-token rather than right at its edge).
    let call_site = lsp_types::Position {
        line: 3,
        character: 16,
    };
    let hop1 = request_definition_with_retry(&handle, &index_uri, call_site);
    assert_eq!(hop1.len(), 1, "expected exactly one hop-1 location");
    assert!(
        hop1[0].target_uri.as_str().ends_with("/src/helper.ts"),
        "hop 1 (call-site click on `greet`) should cross into helper.ts, got {}",
        hop1[0].target_uri.as_str()
    );

    // Hop 2 — line 0: `import { greet } from "./helper";`; character 26 sits
    // inside the `"./helper"` module-specifier string literal.
    let import_site = lsp_types::Position {
        line: 0,
        character: 26,
    };
    let hop2 = request_definition_with_retry(&handle, &index_uri, import_site);
    assert!(
        !hop2.is_empty(),
        "expected at least one hop-2 definition location"
    );
    assert!(
        hop2[0].target_uri.as_str().ends_with("/src/helper.ts"),
        "hop 2 should cross into helper.ts, got {}",
        hop2[0].target_uri.as_str()
    );

    handle.shutdown();
}

/// vtsls's first analysis of a project can legitimately take a few seconds
/// (tsconfig + module resolution) — but a naive "retry only while the
/// result is EMPTY" doesn't actually cover that: vtsls forks a fast
/// `<syntax>` tsserver almost immediately, and a separate `<semantic>`
/// tsserver (the one that actually does cross-file module resolution) a
/// couple seconds later (observable via its own `window/logMessage`
/// notifications). A `definition` request that races ahead of the semantic
/// server's startup gets a fast, NON-empty, but wrong answer from the
/// syntax server — which used to make this helper stop retrying on exactly
/// the wrong answer (P1 finding). So: give the semantic server a head
/// start before the first request, then poll a handful of times and keep
/// the LAST answer rather than the first non-empty one — empirically,
/// once the semantic server is up its answers are stable. Each individual
/// request is still bounded by the client's own internal timeout
/// (`DEFINITION_TIMEOUT`).
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
        std::thread::sleep(Duration::from_millis(500));
    }
    last
}
