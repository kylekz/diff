//! Minimal LSP client for Phase 8's go-to-definition, hover, and
//! find-references (docs/phase-8-lsp-and-polish.md § LSP). Speaks just
//! enough of the Language Server Protocol over stdio to drive `vtsls` (the
//! TypeScript server Zed itself wraps): `initialize`, `textDocument/didOpen`,
//! `textDocument/definition`, `textDocument/hover`,
//! `textDocument/references`, `shutdown`/`exit`.
//!
//! **Transport DEVIATION (plan doc-deviation 2, binding).** `dv-host`'s
//! `proc/exec` and every [`crate::command::CommandBuilder::run`]-family
//! method are run-to-completion (wait-for-exit) — structurally unable to
//! host a persistent, bidirectional stdio server. [`client::LspHandle::spawn`]
//! therefore builds a raw [`std::process::Command`] via
//! [`crate::command::CommandBuilder::command`] (reusing its `wsl.exe -d
//! <distro> --exec`/`CREATE_NO_WINDOW` routing) and owns the child
//! directly, OUTSIDE the `dv-host`/manager registry, with its own reader
//! thread doing LSP's `Content-Length`-framed JSON-RPC — a new, parallel
//! long-lived-child primitive, not a `dv-host` request.
//!
//! **Lifecycle.** [`client::LspClient`]'s `Drop` attempts a graceful
//! `shutdown`/`exit` handshake, then kills the child if it hasn't exited
//! within a short grace period — a leaked `wsl.exe --exec node` would
//! otherwise hold a distro handle open forever. Callers (the app) must only
//! ever construct one of these for an ACTIVELY opened WSL TypeScript repo —
//! never from a passive walk (mirrors the boot-storm contract
//! [`crate::provision`] documents, for the identical reason: this also
//! routes through [`crate::command::CommandBuilder::new_spawn_only`], which
//! boots a stopped distro).
//!
//! **Scope.** This is a MINIMAL client: server-to-client requests
//! (`client/registerCapability`, `window/workDoneProgress/create`, …) are
//! answered with a bare `null` result just to keep the protocol moving —
//! not actually implemented — and most notifications (diagnostics, log
//! messages) are read and silently discarded. Two exceptions:
//! `workspace/configuration`, which the spec requires an ARRAY reply
//! for (one entry per `params.items`) — vtsls issues it during
//! `initialize`, and a bare `null` there is a protocol violation that can
//! corrupt its settings handling (P2 finding: `dispatch_message` special-
//! cases it, replying with an array of nulls sized to `params.items`
//! instead) — and `$/progress` begin/end, which feeds the
//! syntax-vs-semantic warm-up gate every definition/hover/references
//! request runs through (see `client::SemanticReadiness`'s doc for the
//! observed wire signal; docs/backlog.md "vtsls syntax-vs-semantic server
//! race at session-Ready"). Go-to-definition landed in S8f; S8g adds hover
//! and find-references ([`client::LspHandle::hover`]/
//! [`client::LspHandle::references`]) reusing this exact same transport —
//! no second `vtsls` spawn, no new client type.

pub mod client;

pub use client::{
    LspClient, LspError, LspHandle, SemanticWaitOutcome, definition_covers_position,
    hover_contents_to_text, node_modules_present,
};

use crate::location::RepoLocation;

/// Map a repo path to the `languageId` vtsls expects in
/// `textDocument/didOpen` — restricted to the TypeScript family (this
/// phase's only served language; docs/phase-8-lsp-and-polish.md § LSP:
/// "TypeScript first"). `None` for anything else, which callers treat as
/// "don't attach an LSP session for this file at all" (mirrors
/// `crate::highlight`-style extension dispatch in the app crate, but this
/// one lives in dv-core since [`client::LspHandle::spawn`]'s caller needs
/// it too, gpui-free).
pub fn language_id_for_path(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next().filter(|e| *e != path)?;
    match ext.to_ascii_lowercase().as_str() {
        "ts" | "cts" | "mts" => Some("typescript"),
        "tsx" => Some("typescriptreact"),
        "js" | "cjs" | "mjs" => Some("javascript"),
        "jsx" => Some("javascriptreact"),
        _ => None,
    }
}

/// `file://<percent-encoded posix-path>` — the only URI scheme vtsls (and
/// this client) ever speaks. `posix_path` must already be an absolute POSIX
/// path (callers pass a WSL in-distro path). vtsls is an RFC-3986-
/// conformant LSP server: it percent-encodes spaces/non-ASCII bytes in every
/// `file://` URI it hands back (`textDocument/definition`'s `targetUri`),
/// so a repo path or target filename containing either would otherwise
/// never round-trip through [`path_from_file_uri`]'s prefix-match (P3
/// finding) — this encodes on the way out to match.
pub fn file_uri(posix_path: &str) -> String {
    format!("file://{}", percent_encode_path(posix_path))
}

/// The inverse of [`file_uri`]: strip the `file://` scheme and percent-
/// decode the remainder back to a bare POSIX path. `None` for anything not
/// carrying that exact prefix (a `vtsls`-internal `file://` URI outside the
/// repo, or some other scheme entirely) or whose percent-encoding is
/// malformed — callers treat either as "can't resolve this target", never a
/// panic.
pub fn path_from_file_uri(uri: &str) -> Option<String> {
    percent_decode_path(uri.strip_prefix("file://")?)
}

/// RFC 3986 §2.3's unreserved set (plus `/`, a POSIX path separator that
/// must stay literal) passes through; everything else becomes `%XX`.
fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The inverse of [`percent_encode_path`]. `None` on a truncated/non-hex
/// `%XX` escape or a decoded byte sequence that isn't valid UTF-8 — never a
/// panic on a malformed URI from an external process.
fn percent_decode_path(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Normalize an absolute Windows path (`C:\foo\bar` or `C:/foo/bar`, either
/// separator, as `RepoLocation::Local`/vtsls hand it around) into the
/// canonical `C:/forward/slash/form` — uppercase drive letter, forward
/// slashes throughout, no percent-encoding. [`windows_file_uri`],
/// [`path_from_windows_file_uri`], and [`rel_path_from_uri`]'s `Local` arm
/// all build on this ONE representation so a root-vs-target prefix
/// comparison never trips over `\` vs `/` or drive-letter case (`git
/// rev-parse --show-toplevel` on Windows Git already answers with forward
/// slashes — see `crate::git::GitRepo::open` — but a target URI vtsls hands
/// back decodes to whatever case it was opened with). `None` when `path`
/// doesn't start with a drive letter (`X:`) — a UNC path or a relative path,
/// neither of which this module supports (mirrors `file_uri`'s POSIX-only
/// contract: callers pass an already-resolved repo root).
fn normalize_windows_path(path: &str) -> Option<String> {
    let forward = path.replace('\\', "/");
    let bytes = forward.as_bytes();
    if bytes.len() < 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return None;
    }
    let drive = (bytes[0] as char).to_ascii_uppercase();
    Some(format!("{drive}:{}", &forward[2..]))
}

/// `file:///<drive>:/<percent-encoded rest>` for a Windows-style absolute
/// path — matching vscode-uri's `Uri.file()` wire format EXACTLY (verified
/// against vscode-uri's own test suite: `Uri.file("C:/win/path").toString()
/// === "file:///c%3A/win/path"` — lowercase drive letter, its colon
/// PERCENT-ENCODED as `%3A`, not left literal). This isn't an arbitrary
/// choice: `vtsls` wraps the VS Code TypeScript extension (see this module's
/// doc comment), whose project/file resolution is keyed off `vscode-uri`
/// parsing, so matching its exact encoding is what makes cross-file
/// definitions/references resolve at all rather than silently missing a
/// same-file match. `None` when [`normalize_windows_path`] can't make sense
/// of `windows_path`.
pub fn windows_file_uri(windows_path: &str) -> Option<String> {
    let normalized = normalize_windows_path(windows_path)?;
    let drive = normalized.as_bytes()[0].to_ascii_lowercase() as char;
    let rest = normalized[2..]
        .strip_prefix('/')
        .unwrap_or(&normalized[2..]);
    let encoded_rest = percent_encode_path(rest);
    Some(format!("file:///{drive}%3A/{encoded_rest}"))
}

/// The inverse of [`windows_file_uri`] — also tolerant of a literal,
/// unencoded drive-letter colon (`file:///c:/win/path`), since
/// [`percent_decode_path`]'s generic `%XX` decode leaves a literal `:`
/// untouched either way: some non-vscode-uri LSP servers use that simpler
/// convention, and there's no ambiguity to resolve by rejecting it. `None`
/// for anything not shaped like a `file:///<drive>:/...` URI.
pub fn path_from_windows_file_uri(uri: &str) -> Option<String> {
    let after_scheme = uri.strip_prefix("file:///")?;
    let decoded = percent_decode_path(after_scheme)?;
    normalize_windows_path(&decoded)
}

/// The `rootUri` vtsls should be initialized with for `location` — WSL's
/// existing POSIX [`file_uri`], or [`windows_file_uri`] for a local repo.
/// `None` only for a `Local` location whose path isn't a well-formed
/// absolute drive-letter path (see [`normalize_windows_path`]) — the
/// caller's cue to degrade rather than spawn vtsls against a nonsensical
/// root.
pub fn root_uri_for_location(location: &RepoLocation) -> Option<String> {
    match location {
        RepoLocation::Wsl { path, .. } => Some(file_uri(path)),
        RepoLocation::Local(path) => windows_file_uri(&path.to_string_lossy()),
    }
}

/// The `file://` URI for `rel_path` (forward-slash, repo-root-relative —
/// dv's own internal convention, matching git's own pathspec style) resolved
/// against `location`'s root. Shared by every LSP gesture that needs to ask
/// vtsls about a specific file: go-to-definition/hover/references all
/// build their `textDocument.uri` this way, for either location kind.
pub fn file_uri_for_rel_path(location: &RepoLocation, rel_path: &str) -> Option<String> {
    match location {
        RepoLocation::Wsl { path, .. } => {
            let root = path.trim_end_matches('/');
            Some(file_uri(&format!("{root}/{rel_path}")))
        }
        RepoLocation::Local(path) => {
            let root = normalize_windows_path(&path.to_string_lossy())?;
            let full = format!("{}/{rel_path}", root.trim_end_matches('/'));
            windows_file_uri(&full)
        }
    }
}

/// The inverse of [`file_uri_for_rel_path`]: given a `file://` URI vtsls
/// handed back (a `textDocument/definition`/`references` target), resolve
/// it to a repo-root-relative path (forward-slash) IF it falls under
/// `location`'s own root — `None` for a target outside the repo (a bundled
/// library file, e.g.) exactly as the WSL-only code this generalizes always
/// treated that case, or for a URI/location combination that can't be
/// resolved at all (wrong scheme, malformed percent-encoding, an
/// unparseable `Local` root).
pub fn rel_path_from_uri(location: &RepoLocation, uri: &str) -> Option<String> {
    match location {
        RepoLocation::Wsl { path, .. } => {
            let posix = path_from_file_uri(uri)?;
            let root_prefix = format!("{}/", path.trim_end_matches('/'));
            posix.strip_prefix(&root_prefix).map(str::to_string)
        }
        RepoLocation::Local(path) => {
            let root = normalize_windows_path(&path.to_string_lossy())?;
            let target = path_from_windows_file_uri(uri)?;
            let root_prefix = format!("{}/", root.trim_end_matches('/'));
            // Windows paths are case-INSENSITIVE (capstone P3-7):
            // `normalize_windows_path` canonicalizes separators and the
            // drive letter, but the rest of the root's casing comes from
            // `git rev-parse --show-toplevel` while the target's comes from
            // however vtsls/TypeScript resolved the file — the two commonly
            // disagree (`C:/CODE/Proj` vs `C:/code/proj`), and a
            // case-sensitive prefix match silently rejected every in-repo
            // target. ASCII-case-insensitive comparison is the right scope
            // for drive-letter paths; the rel path is sliced by matched
            // LENGTH so it keeps the TARGET's own casing.
            let prefix_len = root_prefix.len();
            let candidate = target.get(..prefix_len)?;
            if candidate.eq_ignore_ascii_case(&root_prefix) {
                Some(target[prefix_len..].to_string())
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_id_for_path_covers_the_ts_family() {
        assert_eq!(language_id_for_path("src/foo.ts"), Some("typescript"));
        assert_eq!(language_id_for_path("src/foo.tsx"), Some("typescriptreact"));
        assert_eq!(language_id_for_path("src/foo.mts"), Some("typescript"));
        assert_eq!(language_id_for_path("src/foo.cts"), Some("typescript"));
        assert_eq!(language_id_for_path("src/foo.js"), Some("javascript"));
        assert_eq!(language_id_for_path("src/foo.cjs"), Some("javascript"));
        assert_eq!(language_id_for_path("src/foo.jsx"), Some("javascriptreact"));
        assert_eq!(language_id_for_path("README.md"), None);
        assert_eq!(language_id_for_path("Makefile"), None);
    }

    #[test]
    fn file_uri_prefixes_the_scheme() {
        assert_eq!(
            file_uri("/home/kyle/proj/src/foo.ts"),
            "file:///home/kyle/proj/src/foo.ts"
        );
    }

    #[test]
    fn path_from_file_uri_round_trips() {
        let uri = file_uri("/home/kyle/proj/src/foo.ts");
        assert_eq!(
            path_from_file_uri(&uri).as_deref(),
            Some("/home/kyle/proj/src/foo.ts")
        );
        assert_eq!(path_from_file_uri("https://example.com"), None);
    }

    #[test]
    fn file_uri_percent_encodes_spaces_and_non_ascii() {
        assert_eq!(
            file_uri("/home/kyle/my proj/déjà.ts"),
            "file:///home/kyle/my%20proj/d%C3%A9j%C3%A0.ts"
        );
    }

    #[test]
    fn path_from_file_uri_decodes_a_vtsls_style_percent_encoded_uri() {
        // What a real RFC-3986-conformant server (vtsls) actually sends
        // back on the wire — not just what `file_uri` itself produces.
        assert_eq!(
            path_from_file_uri("file:///home/kyle/my%20proj/src/x.ts").as_deref(),
            Some("/home/kyle/my proj/src/x.ts")
        );
    }

    #[test]
    fn space_and_non_ascii_paths_round_trip_through_file_uri() {
        let path = "/home/kyle/my proj/déjà vu/x.ts";
        let uri = file_uri(path);
        assert_eq!(path_from_file_uri(&uri).as_deref(), Some(path));
    }

    #[test]
    fn path_from_file_uri_rejects_a_truncated_percent_escape() {
        assert_eq!(path_from_file_uri("file:///a%2"), None);
        assert_eq!(path_from_file_uri("file:///a%zz"), None);
    }

    // --- windows_file_uri / path_from_windows_file_uri: the local-repo ----
    // --- URI convention (verified against vscode-uri's own test suite:   ----
    // --- see the module doc comment on `windows_file_uri`)               ----

    #[test]
    fn windows_file_uri_lowercases_the_drive_and_percent_encodes_its_colon() {
        // The exact case from vscode-uri's own test suite:
        // `Uri.file("C:/win/path").toString() === "file:///c%3A/win/path"`.
        assert_eq!(
            windows_file_uri(r"C:\win\path").as_deref(),
            Some("file:///c%3A/win/path")
        );
        assert_eq!(
            windows_file_uri("C:/win/path").as_deref(),
            Some("file:///c%3A/win/path")
        );
    }

    #[test]
    fn windows_file_uri_accepts_an_already_lowercase_drive_letter() {
        assert_eq!(
            windows_file_uri(r"c:\win\path").as_deref(),
            Some("file:///c%3A/win/path")
        );
    }

    #[test]
    fn windows_file_uri_percent_encodes_spaces_and_non_ascii() {
        assert_eq!(
            windows_file_uri(r"C:\my proj\déjà.ts").as_deref(),
            Some("file:///c%3A/my%20proj/d%C3%A9j%C3%A0.ts")
        );
    }

    #[test]
    fn windows_file_uri_rejects_a_path_without_a_drive_letter() {
        assert_eq!(windows_file_uri(r"\\server\share\x"), None);
        assert_eq!(windows_file_uri("relative/path"), None);
    }

    #[test]
    fn path_from_windows_file_uri_round_trips_through_windows_file_uri() {
        let path = r"C:\my proj\déjà vu\x.ts";
        let uri = windows_file_uri(path).unwrap();
        // The decoded form is normalized (forward slashes, uppercase
        // drive) — not byte-identical to the backslash input, but the
        // same location.
        assert_eq!(
            path_from_windows_file_uri(&uri).as_deref(),
            Some("C:/my proj/déjà vu/x.ts")
        );
    }

    #[test]
    fn path_from_windows_file_uri_tolerates_a_literal_unescaped_colon() {
        // Some non-vscode-uri servers don't percent-encode the drive
        // letter's colon — decoding must not be strict about which form
        // it receives (see this fn's doc comment).
        assert_eq!(
            path_from_windows_file_uri("file:///c:/Users/kyle/x.ts").as_deref(),
            Some("C:/Users/kyle/x.ts")
        );
    }

    #[test]
    fn path_from_windows_file_uri_rejects_a_non_windows_uri() {
        assert_eq!(path_from_windows_file_uri("file:///home/kyle/x.ts"), None);
        assert_eq!(path_from_windows_file_uri("https://example.com"), None);
    }

    // --- root_uri_for_location / file_uri_for_rel_path / rel_path_from_uri:
    // --- the location-dispatching helpers `workspace.rs` uses instead of
    // --- matching `RepoLocation` at every LSP call site ------------------

    fn local(path: &str) -> RepoLocation {
        RepoLocation::Local(std::path::PathBuf::from(path))
    }

    fn wsl(path: &str) -> RepoLocation {
        RepoLocation::Wsl {
            distro: "Ubuntu".to_string(),
            path: path.to_string(),
        }
    }

    #[test]
    fn root_uri_for_location_dispatches_on_location_kind() {
        assert_eq!(
            root_uri_for_location(&wsl("/home/kyle/proj")).as_deref(),
            Some("file:///home/kyle/proj")
        );
        assert_eq!(
            root_uri_for_location(&local(r"C:\code\proj")).as_deref(),
            Some("file:///c%3A/code/proj")
        );
    }

    #[test]
    fn root_uri_for_location_is_none_for_an_unresolvable_local_root() {
        assert_eq!(root_uri_for_location(&local(r"\\server\share")), None);
    }

    #[test]
    fn file_uri_for_rel_path_joins_the_root_and_rel_path_for_both_kinds() {
        assert_eq!(
            file_uri_for_rel_path(&wsl("/home/kyle/proj"), "src/a.ts").as_deref(),
            Some("file:///home/kyle/proj/src/a.ts")
        );
        assert_eq!(
            file_uri_for_rel_path(&local(r"C:\code\proj"), "src/a.ts").as_deref(),
            Some("file:///c%3A/code/proj/src/a.ts")
        );
    }

    #[test]
    fn file_uri_for_rel_path_handles_a_trailing_slash_on_the_root() {
        assert_eq!(
            file_uri_for_rel_path(&wsl("/home/kyle/proj/"), "src/a.ts").as_deref(),
            Some("file:///home/kyle/proj/src/a.ts")
        );
        assert_eq!(
            file_uri_for_rel_path(&local(r"C:\code\proj\"), "src/a.ts").as_deref(),
            Some("file:///c%3A/code/proj/src/a.ts")
        );
    }

    #[test]
    fn rel_path_from_uri_round_trips_with_file_uri_for_rel_path() {
        let wsl_loc = wsl("/home/kyle/proj");
        let uri = file_uri_for_rel_path(&wsl_loc, "src/a.ts").unwrap();
        assert_eq!(
            rel_path_from_uri(&wsl_loc, &uri).as_deref(),
            Some("src/a.ts")
        );

        let local_loc = local(r"C:\code\proj");
        let uri = file_uri_for_rel_path(&local_loc, "src/a.ts").unwrap();
        assert_eq!(
            rel_path_from_uri(&local_loc, &uri).as_deref(),
            Some("src/a.ts")
        );
    }

    #[test]
    fn rel_path_from_uri_handles_nested_dirs_spaces_and_unicode_locally() {
        let local_loc = local(r"C:\code\my proj");
        let uri = file_uri_for_rel_path(&local_loc, "src/déjà/a b.ts").unwrap();
        assert_eq!(
            uri,
            "file:///c%3A/code/my%20proj/src/d%C3%A9j%C3%A0/a%20b.ts"
        );
        assert_eq!(
            rel_path_from_uri(&local_loc, &uri).as_deref(),
            Some("src/déjà/a b.ts")
        );
    }

    #[test]
    fn rel_path_from_uri_is_none_for_a_target_outside_the_local_repo() {
        let local_loc = local(r"C:\code\proj");
        // A target in a sibling directory, not under the repo root at all.
        let outside = windows_file_uri(r"C:\code\other\lib.ts").unwrap();
        assert_eq!(rel_path_from_uri(&local_loc, &outside), None);
    }

    #[test]
    fn rel_path_from_uri_is_case_insensitive_on_the_drive_letter() {
        // The repo root as dv holds it (`RepoLocation::Local`) may carry
        // whatever drive-letter case the caller passed in, while a target
        // URI vtsls hands back always normalizes to lowercase (vscode-uri
        // convention) — `normalize_windows_path` uppercases both sides
        // before comparing, so this must resolve regardless.
        let local_loc = local(r"c:\code\proj");
        let uri = "file:///c%3A/code/proj/src/a.ts";
        assert_eq!(
            rel_path_from_uri(&local_loc, uri).as_deref(),
            Some("src/a.ts")
        );
    }

    #[test]
    fn rel_path_from_uri_local_is_case_insensitive_beyond_the_drive_letter() {
        // Capstone P3-7: Windows paths are case-insensitive END TO END,
        // not just at the drive letter — git may report the root as
        // `C:/code/proj` while vtsls resolves targets under `C:/CODE/Proj`
        // (or vice versa). The prefix must match ASCII-case-insensitively,
        // and the returned rel path keeps the TARGET's own casing.
        let local_loc = local(r"C:\code\proj");
        let uri = "file:///c%3A/CODE/Proj/src/Widget.ts";
        assert_eq!(
            rel_path_from_uri(&local_loc, uri).as_deref(),
            Some("src/Widget.ts")
        );

        // And the mirror direction: an upper-cased stored root against a
        // lower-cased target URI.
        let upper_root = local(r"C:\CODE\Proj");
        let uri = "file:///c%3A/code/proj/src/widget.ts";
        assert_eq!(
            rel_path_from_uri(&upper_root, uri).as_deref(),
            Some("src/widget.ts")
        );

        // Still None for a genuinely different path, casing aside.
        let outside = "file:///c%3A/CODE/Other/lib.ts";
        assert_eq!(rel_path_from_uri(&local_loc, outside), None);
    }
}
