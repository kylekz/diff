//! Minimal LSP client for Phase 8's go-to-definition
//! (docs/phase-8-lsp-and-polish.md § LSP). Speaks just enough of the
//! Language Server Protocol over stdio to drive `vtsls` (the TypeScript
//! server Zed itself wraps): `initialize`, `textDocument/didOpen`,
//! `textDocument/definition`, `shutdown`/`exit`.
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
//! not actually implemented — and notifications (diagnostics, log
//! messages, progress) are read and silently discarded. The one exception
//! is `workspace/configuration`, which the spec requires an ARRAY reply
//! for (one entry per `params.items`) — vtsls issues it during
//! `initialize`, and a bare `null` there is a protocol violation that can
//! corrupt its settings handling (P2 finding: `dispatch_message` special-
//! cases it, replying with an array of nulls sized to `params.items`
//! instead). Enough for go-to-definition (S8f); hover/find-references
//! (S8g, not this slice) reuse the same transport.

pub mod client;

pub use client::{LspClient, LspError, LspHandle, node_modules_present};

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
}
