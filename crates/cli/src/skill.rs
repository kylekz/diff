//! `dv skill <install|show|path>` — distribute the dv-review agent skill
//! (docs/backlog.md "Agent skill + `dv skill install`"). The skill is a
//! SKILL.md teaching a coding agent the review-response workflow (the
//! canonical `dv comment list --status open --json` query, anchor
//! semantics, reply-then-resolve etiquette, `dv review wait` loops);
//! `install` writes it where Claude Code discovers skills
//! (`~/.claude/skills/dv-review/SKILL.md`), which is what makes it reach
//! agents working in *reviewed* repos — dv's own CLAUDE.md only reaches
//! agents working on dv itself.
//!
//! The content ships embedded in the binary (`include_str!`) rather than as
//! a sidecar file: the CLI is installed bare into WSL distros
//! (`~/.local/bin/dv`, see crates/core/src/remote/install.rs) where a
//! data-file lookup relative to the exe would be one more thing to drift.

use std::path::PathBuf;

use serde_json::json;

use crate::{CliError, print_json, usage_err};

const SKILL_MD: &str = include_str!("../skill/SKILL.md");

/// Directory name under `skills/`; also the skill's frontmatter `name`.
const SKILL_DIR: &str = "dv-review";

pub(crate) const SKILL_USAGE: &str = "\
usage: dv skill <install|show|path> [options]

  install [--dir <skills-dir>]   write SKILL.md to <skills-dir>/dv-review/
                                 (default: ~/.claude/skills)
  show                           print the skill content to stdout
  path                           print the default install path

global options:
  --json                         machine-readable output on stdout";

/// `~/.claude/skills` (via HOME / USERPROFILE), the directory Claude Code
/// scans for user-level skills.
fn default_skills_dir() -> Result<PathBuf, CliError> {
    #[allow(deprecated)] // home_dir un-deprecated in newer std; fine on win+linux
    std::env::home_dir()
        .map(|home| home.join(".claude").join("skills"))
        .ok_or_else(|| CliError::Op("cannot determine home directory".to_string()))
}

pub(crate) fn skill_router(sub: &str, args: &[String], json: bool) -> Result<(), CliError> {
    match sub {
        "install" => {
            let mut dir_override = None;
            let mut iter = args.iter();
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--dir" => {
                        let value = iter
                            .next()
                            .ok_or_else(|| usage_err("--dir requires a path", SKILL_USAGE))?;
                        dir_override = Some(PathBuf::from(value));
                    }
                    other => {
                        return Err(usage_err(
                            format!("skill install: unexpected argument {other:?}"),
                            SKILL_USAGE,
                        ));
                    }
                }
            }
            let skills_dir = match dir_override {
                Some(dir) => dir,
                None => default_skills_dir()?,
            };
            let dir = skills_dir.join(SKILL_DIR);
            let path = dir.join("SKILL.md");
            let existed = path.exists();
            let updated = !std::fs::read_to_string(&path)
                .map(|current| current == SKILL_MD)
                .unwrap_or(false);
            std::fs::create_dir_all(&dir)
                .map_err(|e| CliError::Op(format!("creating {}: {e}", dir.display())))?;
            std::fs::write(&path, SKILL_MD)
                .map_err(|e| CliError::Op(format!("writing {}: {e}", path.display())))?;
            if json {
                print_json(&json!({
                    "skill": {
                        "path": path.display().to_string(),
                        "existed": existed,
                        "updated": updated,
                    }
                }));
            } else {
                let verb = match (existed, updated) {
                    (false, _) => "installed",
                    (true, true) => "updated",
                    (true, false) => "already up to date",
                };
                println!("{verb}: {}", path.display());
            }
            Ok(())
        }
        "show" => {
            if let Some(extra) = args.first() {
                return Err(usage_err(
                    format!("skill show: unexpected argument {extra:?}"),
                    SKILL_USAGE,
                ));
            }
            // Raw content on stdout in both modes — the point of `show` is
            // piping the markdown somewhere, and wrapping it in JSON would
            // just force a decode step.
            print!("{SKILL_MD}");
            Ok(())
        }
        "path" => {
            if let Some(extra) = args.first() {
                return Err(usage_err(
                    format!("skill path: unexpected argument {extra:?}"),
                    SKILL_USAGE,
                ));
            }
            let path = default_skills_dir()?.join(SKILL_DIR).join("SKILL.md");
            if json {
                print_json(&json!({ "skill": { "path": path.display().to_string() } }));
            } else {
                println!("{}", path.display());
            }
            Ok(())
        }
        other => Err(usage_err(
            format!("unknown skill subcommand: {other}"),
            SKILL_USAGE,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_content_has_frontmatter_and_canonical_query() {
        assert!(SKILL_MD.starts_with("---\nname: dv-review\n"));
        assert!(SKILL_MD.contains("description:"));
        assert!(SKILL_MD.contains("dv comment list --status open --json"));
        assert!(SKILL_MD.contains("dv review wait"));
    }

    #[test]
    fn install_writes_and_reports_idempotently() {
        let tmp = std::env::temp_dir().join(format!("dv-skill-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let args = vec!["--dir".to_string(), tmp.display().to_string()];
        skill_router("install", &args, false).expect("first install");
        let path = tmp.join(SKILL_DIR).join("SKILL.md");
        assert_eq!(
            std::fs::read_to_string(&path).expect("skill written"),
            SKILL_MD
        );
        // Second run: same content, still succeeds (reported as up to date).
        skill_router("install", &args, false).expect("re-install");
        // Drifted content gets repaired.
        std::fs::write(&path, "stale").expect("stale write");
        skill_router("install", &args, false).expect("repair install");
        assert_eq!(
            std::fs::read_to_string(&path).expect("skill repaired"),
            SKILL_MD
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unknown_subcommand_is_usage_error() {
        assert!(skill_router("frobnicate", &[], false).is_err());
    }
}
