//! Helpers shared by every dv crate's tests, chiefly [`FixtureRepo`]: real git repos built in temp
//! dirs through the `git` CLI, so tests exercise git's own behaviour (docs/PLAN.md §14.2);
//! [`FixtureGithub`], a GitHub repository and its clone without GitHub (PLAN §9.11); and where
//! the fake `gh` is ([`mock_gh`]).
//!
//! gpui: **never**. Headless crates' tests depend on it, and `script/check` fails when gpui
//! appears in its dependency tree (PLAN §4, §13.2).
//!
//! These are test helpers: a fixture that can't be built is a broken test, so every method panics
//! with a message naming the step that failed (and git's stderr) instead of returning an error.

mod fixture_github;
mod fixture_repo;

use std::path::PathBuf;

pub use fixture_github::{FixtureGithub, GITHUB_OWNER};
pub use fixture_repo::{AUTHOR_EMAIL, AUTHOR_NAME, EPOCH, FixtureRepo, TICK_SECS};

/// `mock_gh`, the fake `gh` (PLAN §14.2), where cargo builds it beside the test executable
/// that asks (`target/debug/deps`'s parent): `cargo test --workspace` builds it with every
/// crate, as `script/check` runs it. A run of one crate's tests alone needs it built first.
pub fn mock_gh() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|err| panic!("this executable: {err}"));
    let mock = exe
        .parent()
        .and_then(|deps| deps.parent())
        .unwrap_or_else(|| panic!("{} isn't in a target directory", exe.display()))
        .join(format!("mock_gh{}", std::env::consts::EXE_SUFFIX));
    assert!(
        mock.is_file(),
        "{} isn't built: run `cargo build -p mock_gh` (or `cargo test --workspace`) first",
        mock.display()
    );
    mock
}
