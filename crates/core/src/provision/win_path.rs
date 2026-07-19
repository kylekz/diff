//! The "dv on PATH" onboarding component (Windows only): detects whether
//! the directory holding the running `dv.exe` is reachable from a fresh
//! shell, and — behind an explicit consent click, per the module-level
//! consent contract in [`super`] — appends it to the **user** PATH
//! (`HKCU\Environment`), never the machine PATH (no elevation, no effect
//! on other accounts).
//!
//! Registry access shells out to `reg.exe` rather than linking a registry
//! crate, matching the repo-wide subprocess policy (CLAUDE.md: git/gh/wsl
//! all route through subprocesses). Two deliberate choices worth knowing:
//!
//! - **Read raw, write same-type.** `[Environment]::SetEnvironmentVariable`
//!   (and naive read-modify-write through an *expanded* view) flattens
//!   `REG_EXPAND_SZ` entries like `%USERPROFILE%\bin` into their current
//!   expansion. `reg query` returns the raw unexpanded data, and the append
//!   writes back with the value's existing type (defaulting to
//!   `REG_EXPAND_SZ` for a fresh value), so nothing the user had is
//!   rewritten.
//! - **Broadcast `WM_SETTINGCHANGE`** after the write (via a direct
//!   `user32` call — no new dependency), so Explorer and newly launched
//!   apps pick the change up. Already-open terminals keep their old PATH;
//!   the consent row's success detail says so.

use super::{ComponentState, ConsentAction};

/// Detect the state of the "dv on PATH" row. Never fails hard (module
/// contract): every problem reduces to a [`ComponentState`].
///
/// - Running from a cargo `target/` tree (a dev build) → `Skipped`:
///   offering to put `target\debug` on the user's PATH would be actively
///   harmful, and a dev machine already runs dv via `cargo run`.
/// - Exe dir already present on the process PATH (covers the machine PATH
///   and expanded entries) or in the raw `HKCU\Environment` value (covers
///   "added moments ago; this process predates the broadcast") → `Ok`.
/// - Otherwise → `NeedsConsent` with [`ConsentAction::AddDvToPath`].
pub fn check_dv_on_path() -> ComponentState {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            return ComponentState::Failed {
                error: format!("could not resolve dv's own location: {err}"),
            };
        }
    };
    let Some(dir) = exe.parent().map(|p| p.to_path_buf()) else {
        return ComponentState::Failed {
            error: "dv's own location has no parent directory".to_string(),
        };
    };
    if is_cargo_target_dir(&dir) {
        return ComponentState::Skipped {
            reason: "running from a cargo target dir (transient — cargo clean deletes it); \
                     launch the packaged dv.exe to add it to PATH"
                .to_string(),
        };
    }
    let dir_str = dir.display().to_string();
    let process_path = std::env::var("PATH").unwrap_or_default();
    if path_contains_dir(&process_path, &dir_str) {
        return ComponentState::Ok {
            detail: format!("`dv` runs from any terminal ({dir_str})"),
        };
    }
    match read_user_path() {
        Ok(user_path) if path_contains_dir(&user_path.data, &dir_str) => ComponentState::Ok {
            detail: format!("on your user PATH ({dir_str}) — new terminals will see it"),
        },
        Ok(_) => ComponentState::NeedsConsent {
            action: ConsentAction::AddDvToPath {
                dir: dir_str.clone(),
            },
            detail: format!("adds {dir_str} to your user PATH so `dv` works in any new terminal"),
        },
        Err(err) => ComponentState::Failed {
            error: format!("could not read the user PATH: {err}"),
        },
    }
}

/// Append `dir` to the user PATH (`HKCU\Environment`), preserving the
/// value's existing registry type, then broadcast `WM_SETTINGCHANGE`.
/// Idempotent: a `dir` already present (per [`path_contains_dir`]'s
/// normalization) is a no-op success.
pub fn add_dv_to_path(dir: &str) -> anyhow::Result<()> {
    let current = read_user_path()?;
    if path_contains_dir(&current.data, dir) {
        return Ok(());
    }
    let new_value = if current.data.trim_end_matches(';').is_empty() {
        dir.to_string()
    } else {
        format!("{};{}", current.data.trim_end_matches(';'), dir)
    };
    let output = reg_command()
        .args([
            "add",
            "HKCU\\Environment",
            "/v",
            "Path",
            "/t",
            &current.value_type,
            "/d",
            &new_value,
            "/f",
        ])
        .output()
        .map_err(|err| anyhow::anyhow!("could not spawn reg.exe: {err}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "reg.exe add failed: {}",
            crate::command::decode_output(&output.stderr).trim()
        );
    }
    broadcast_environment_change();
    Ok(())
}

/// The raw (unexpanded) `HKCU\Environment` `Path` value plus its registry
/// type. A missing value — a perfectly normal fresh profile — reads as an
/// empty `REG_EXPAND_SZ`, distinguishing it from a genuinely failed query.
struct UserPath {
    value_type: String,
    data: String,
}

fn read_user_path() -> anyhow::Result<UserPath> {
    let output = reg_command()
        .args(["query", "HKCU\\Environment", "/v", "Path"])
        .output()
        .map_err(|err| anyhow::anyhow!("could not spawn reg.exe: {err}"))?;
    if !output.status.success() {
        // `reg query` exits 1 both for "value not set" and "key missing" —
        // either way there is no user Path yet, which is a valid state.
        return Ok(UserPath {
            value_type: "REG_EXPAND_SZ".to_string(),
            data: String::new(),
        });
    }
    let stdout = crate::command::decode_output(&output.stdout);
    match parse_reg_value(&stdout, "Path") {
        Some((value_type, data)) => Ok(UserPath { value_type, data }),
        None => anyhow::bail!("unrecognized reg.exe query output: {}", stdout.trim()),
    }
}

fn reg_command() -> std::process::Command {
    use std::os::windows::process::CommandExt as _;
    let mut cmd = std::process::Command::new("reg.exe");
    cmd.creation_flags(crate::command::CREATE_NO_WINDOW);
    cmd
}

/// Pull `(type, data)` for `value_name` out of `reg.exe query` output,
/// which looks like:
///
/// ```text
/// HKEY_CURRENT_USER\Environment
///     Path    REG_EXPAND_SZ    C:\a;C:\b
/// ```
///
/// Anchors on the `REG_*` type token rather than column positions (the
/// data itself can contain any number of spaces); the value name and type
/// tokens are not localized, unlike `reg.exe`'s status messages.
fn parse_reg_value(output: &str, value_name: &str) -> Option<(String, String)> {
    for line in output.lines() {
        let trimmed = line.trim();
        let rest = match trimmed.get(..value_name.len()) {
            Some(head) if head.eq_ignore_ascii_case(value_name) => &trimmed[value_name.len()..],
            _ => continue,
        };
        // The name must be a whole token ("Path", not "PathExt").
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let rest = rest.trim_start();
        let Some(type_token) = rest.split_whitespace().next() else {
            continue;
        };
        if !type_token.starts_with("REG_") {
            continue;
        }
        // Everything after the type token is the data (may itself be empty
        // for an empty value).
        let data = rest[type_token.len()..].trim_start().trim_end().to_string();
        return Some((type_token.to_string(), data));
    }
    None
}

/// Whether `dir` appears as an entry of the `;`-separated `path` value.
/// Comparison is case-insensitive with trailing separators and quotes
/// stripped — the normalizations PATH resolution itself is insensitive to.
/// Deliberately does NOT expand `%VAR%` entries: the raw-registry caller
/// pairs this with a process-PATH check that sees the expanded view.
fn path_contains_dir(path: &str, dir: &str) -> bool {
    let want = normalize_path_entry(dir);
    !want.is_empty()
        && path
            .split(';')
            .any(|entry| normalize_path_entry(entry) == want)
}

fn normalize_path_entry(entry: &str) -> String {
    entry
        .trim()
        .trim_matches('"')
        .trim_end_matches(['\\', '/'])
        .to_ascii_lowercase()
}

/// Whether `dir` sits inside a cargo build tree — any ancestor directory
/// literally named `target` (the standard cargo layout, including custom
/// `CARGO_TARGET_DIR`s, whose leaf dirs are still `target/<profile>`-shaped
/// only by convention; plain `target` catches the common case without
/// false-positiving on e.g. `D:\targets\dv`).
fn is_cargo_target_dir(dir: &std::path::Path) -> bool {
    dir.components().any(
        |c| matches!(c, std::path::Component::Normal(name) if name.eq_ignore_ascii_case("target")),
    )
}

/// Tell every top-level window the environment changed (the documented
/// post-`HKCU\Environment`-write step; Explorer re-reads it so new
/// processes launched from the shell inherit the updated PATH).
/// `SMTO_ABORTIFHUNG` + a short timeout so one wedged window can't stall
/// the consent action.
fn broadcast_environment_change() {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SendMessageTimeoutW(
            hwnd: isize,
            msg: u32,
            wparam: usize,
            lparam: isize,
            flags: u32,
            timeout_ms: u32,
            result: *mut usize,
        ) -> isize;
    }
    const HWND_BROADCAST: isize = 0xffff;
    const WM_SETTINGCHANGE: u32 = 0x001a;
    const SMTO_ABORTIFHUNG: u32 = 0x0002;
    let param: Vec<u16> = "Environment\0".encode_utf16().collect();
    let mut result = 0usize;
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            param.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            2000,
            &mut result,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reg_value_extracts_type_and_data() {
        let out = "\r\nHKEY_CURRENT_USER\\Environment\r\n    Path    REG_EXPAND_SZ    C:\\a bin;%USERPROFILE%\\b\r\n\r\n";
        let (ty, data) = parse_reg_value(out, "Path").expect("should parse");
        assert_eq!(ty, "REG_EXPAND_SZ");
        assert_eq!(data, "C:\\a bin;%USERPROFILE%\\b");
    }

    #[test]
    fn parse_reg_value_matches_name_case_insensitively_but_not_prefixes() {
        let out = "    PATHEXT    REG_SZ    .COM;.EXE\r\n    PATH    REG_SZ    C:\\x\r\n";
        let (ty, data) = parse_reg_value(out, "Path").expect("should parse");
        assert_eq!(ty, "REG_SZ");
        assert_eq!(data, "C:\\x");
    }

    #[test]
    fn parse_reg_value_handles_empty_data() {
        let out = "    Path    REG_EXPAND_SZ\r\n";
        let (ty, data) = parse_reg_value(out, "Path").expect("should parse");
        assert_eq!(ty, "REG_EXPAND_SZ");
        assert_eq!(data, "");
    }

    #[test]
    fn path_contains_dir_normalizes_case_trailing_slash_and_quotes() {
        let path = r#"C:\Windows;"D:\Apps\dv\";C:\Users\k\bin"#;
        assert!(path_contains_dir(path, r"d:\apps\DV"));
        assert!(path_contains_dir(path, r"C:\WINDOWS\"));
        assert!(!path_contains_dir(path, r"D:\Apps"));
        assert!(!path_contains_dir("", r"D:\Apps"));
        assert!(!path_contains_dir(r"C:\x", ""));
    }

    #[test]
    fn is_cargo_target_dir_flags_target_trees_only() {
        assert!(is_cargo_target_dir(std::path::Path::new(
            r"D:\Software\diff\target\release"
        )));
        assert!(is_cargo_target_dir(std::path::Path::new(
            r"C:\Users\k\.cache\dv-target\target\debug"
        )));
        assert!(!is_cargo_target_dir(std::path::Path::new(
            r"D:\Software\diff\dist"
        )));
        assert!(!is_cargo_target_dir(std::path::Path::new(r"D:\targets\dv")));
    }
}
