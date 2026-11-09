use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use dv_test_support::FixtureRepo;

use super::*;
use crate::git::command::apply_overlay;
use crate::testing::{open, open_at, options, same_dir};

fn committed_repo() -> FixtureRepo {
    let repo = FixtureRepo::new();
    repo.write("a.txt", "a\n").write("sub/deep/f.txt", "f\n");
    repo.commit("base");
    repo
}

// ---- opening -----------------------------------------------------------------------------------

#[test]
fn opens_from_the_root_and_from_a_subdirectory() {
    let repo = committed_repo();
    for at in [
        repo.path().to_path_buf(),
        repo.path().join("sub").join("deep"),
    ] {
        let backend = open_at(&repo, &at);
        let info = backend.repo();
        assert!(same_dir(&info.toplevel, repo.path()), "{info:?}");
        assert!(
            same_dir(&info.git_dir, repo.path().join(".git")),
            "{info:?}"
        );
        assert_eq!(info.git_dir, info.common_dir);
        assert!(!info.is_linked_worktree());
        assert_eq!(info.location, Location::Local(backend.toplevel().into()));
        assert_eq!(info.object_format, ObjectFormat::Sha1);
        assert!(info.git_version.at_least(2, 31), "{:?}", info.git_version);
        assert!(Path::new(&info.toplevel).is_absolute());
        if cfg!(windows) {
            assert!(!info.toplevel.contains('/'), "native separators: {info:?}");
        }
        assert_eq!(backend.store_root(), Path::new(&info.common_dir).join("dv"));
    }
}

#[test]
fn opens_a_linked_worktree_whose_git_dir_differs_from_the_common_dir() {
    let repo = committed_repo();
    let worktree = repo.add_worktree("wt", "feat");
    let backend = open_at(&repo, &worktree.join("sub"));
    let info = backend.repo();
    assert!(same_dir(&info.toplevel, &worktree), "{info:?}");
    assert!(
        same_dir(
            &info.git_dir,
            repo.path().join(".git").join("worktrees").join("wt")
        ),
        "{info:?}"
    );
    assert!(
        same_dir(&info.common_dir, repo.path().join(".git")),
        "{info:?}"
    );
    assert!(info.is_linked_worktree());
}

#[test]
fn opens_an_unborn_repo() {
    let repo = FixtureRepo::new();
    let backend = open(&repo);
    assert!(same_dir(&backend.repo().toplevel, repo.path()));
    assert_eq!(backend.repo().object_format, ObjectFormat::Sha1);
}

#[test]
fn opens_a_sha256_repo() {
    let repo = FixtureRepo::new_sha256();
    assert_eq!(open(&repo).repo().object_format, ObjectFormat::Sha256);
}

#[test]
fn refuses_paths_that_are_not_worktrees() {
    let repo = committed_repo();
    let open_err =
        |path: &Path| LocalBackend::open_with(path, options(&repo)).expect_err("must not open");

    let outside = repo.outside_dir("plain");
    let err = open_err(&outside);
    assert!(
        matches!(err, BackendError::NotARepository { .. }),
        "{err:?}"
    );
    assert_eq!(
        err.to_string(),
        format!("not a git repository: {}", outside.display())
    );

    let missing = outside.join("missing");
    assert!(matches!(
        open_err(&missing),
        BackendError::PathNotFound { .. }
    ));

    let file = repo.path().join("a.txt");
    assert!(matches!(
        open_err(&file),
        BackendError::NotARepository { .. }
    ));

    let bare = repo.bare_clone("bare.git");
    let err = open_err(&bare);
    assert!(
        matches!(err, BackendError::BareRepository { .. }),
        "{err:?}"
    );

    let inside_git_dir = repo.path().join(".git").join("refs");
    let err = open_err(&inside_git_dir);
    assert!(matches!(err, BackendError::InsideGitDir { .. }), "{err:?}");
}

// ---- the git policy ----------------------------------------------------------------------------

#[test]
fn git_output_is_plain_whatever_the_user_configures() {
    let repo = FixtureRepo::new();
    // A pair where myers and histogram produce different hunks.
    repo.write("seq.txt", "A\nB\nC\nA\nB\nB\nA\n");
    let base = repo.commit("base");
    repo.write("seq.txt", "C\nB\nA\nB\nA\nC\n");
    let tip = repo.commit("tip");
    repo.config("diff.algorithm", "histogram")
        .config("color.ui", "always")
        .config("color.diff", "always")
        .config("core.pager", "less")
        .config("pager.diff", "true");
    let backend = open(&repo);

    let diff = backend
        .git(&["diff", &base, &tip], &GitOpts::default())
        .unwrap();
    let diff = String::from_utf8(diff.stdout).unwrap();
    assert!(!diff.contains('\x1b'), "colour codes in {diff:?}");
    let myers = repo.git(&["diff", "--no-color", "--diff-algorithm=myers", &base, &tip]);
    let histogram = repo.git(&["diff", "--no-color", &base, &tip]);
    assert_ne!(
        myers, histogram,
        "the fixture must tell the algorithms apart"
    );
    assert_eq!(diff, myers, "dv's diff ignores the user's diff.algorithm");

    let log = backend
        .git(
            &["log", "--oneline", "--decorate", "-1"],
            &GitOpts::default(),
        )
        .unwrap();
    assert!(!log.stdout.contains(&0x1b), "{:?}", log.stdout_utf8());
    let pager = backend
        .git(&["var", "GIT_PAGER"], &GitOpts::default())
        .unwrap();
    assert_eq!(pager.stdout_utf8(), Some("cat\n"));
}

#[test]
fn inherited_git_variables_do_not_redirect_git() {
    let repo = committed_repo();
    let bogus = repo.outside_dir("bogus");
    let mut env = repo.env_overlay();
    for (name, value) in [
        ("GIT_DIR", bogus.as_os_str()),
        ("GIT_WORK_TREE", bogus.as_os_str()),
        ("GIT_INDEX_FILE", bogus.join("index").as_os_str()),
        ("GIT_LITERAL_PATHSPECS", "1".as_ref()),
        ("GIT_DIFF_OPTS", "--unified=0".as_ref()),
    ] {
        env.push((name.into(), Some(value.to_os_string())));
    }
    let backend = LocalBackend::open_with(repo.path(), LocalBackendOptions { env }).unwrap();
    assert!(same_dir(&backend.repo().toplevel, repo.path()));
    // `:(literal)` magic works because `GIT_LITERAL_PATHSPECS` was dropped.
    let listed = backend
        .git(&["ls-files", "--", ":(literal)a.txt"], &GitOpts::default())
        .unwrap();
    assert_eq!(listed.stdout_utf8(), Some("a.txt\n"));
}

#[test]
fn an_inherited_attribute_source_does_not_redirect_attributes() {
    // `git --attr-source=<tree>` exports GIT_ATTR_SOURCE to the hooks, aliases and editors it
    // starts. Inherited, git would read `.gitattributes` from that tree instead of the worktree
    // and index, and in a repo without commits every diff that reads attributes would fail.
    use crate::diff::{FileDiff, FileStatus, file_diff, file_list};
    use crate::target::Target;
    let list = |repo: &FixtureRepo, attr_source: &str| {
        let mut env = repo.env_overlay();
        env.push(("GIT_ATTR_SOURCE".into(), Some(attr_source.into())));
        let backend = LocalBackend::open_with(repo.path(), LocalBackendOptions { env }).unwrap();
        let git = crate::git::GitRepo::new(Arc::new(backend));
        let spec = Target::Working.resolve(&git).unwrap();
        let entries = file_list(&git, &spec).unwrap_or_else(|err| panic!("{err:?}"));
        let diffs: Vec<FileDiff> = entries
            .iter()
            .map(|entry| {
                file_diff(&git, &spec, entry, &Default::default())
                    .unwrap_or_else(|err| panic!("{}: {err:?}", entry.path))
            })
            .collect();
        let summary: Vec<_> = entries
            .into_iter()
            .map(|e| (e.path, e.status, e.additions, e.deletions, e.binary))
            .collect();
        (summary, diffs)
    };
    let is_text = |diff: &FileDiff| matches!(diff, FileDiff::Text(_));

    // No commits: `HEAD` names no tree, which git refuses.
    let unborn = FixtureRepo::new();
    unborn.write("staged.txt", "s\n").write("new.txt", "n\n");
    unborn.git(&["add", "staged.txt"]);
    let (summary, diffs) = list(&unborn, "HEAD");
    assert_eq!(
        summary,
        [
            ("new.txt".to_string(), FileStatus::Untracked, 1, 0, false),
            ("staged.txt".to_string(), FileStatus::Added, 1, 0, false),
        ]
    );
    assert!(diffs.iter().all(is_text), "{diffs:?}");

    // An older commit whose attributes made every file binary; the worktree's have no rule.
    let repo = FixtureRepo::new();
    repo.set_attributes("* -diff\n").write("a.txt", "a\n");
    repo.commit("everything binary");
    repo.remove(".gitattributes");
    repo.commit("attributes removed");
    repo.write("a.txt", "b\n");
    let (summary, diffs) = list(&repo, "HEAD~1");
    assert_eq!(
        summary,
        [("a.txt".to_string(), FileStatus::Modified, 1, 1, false)]
    );
    assert!(diffs.iter().all(is_text), "{diffs:?}");
}

#[test]
fn git_runs_with_the_options_environment_overlay() {
    // The overlay is what keeps dv_core's tests machine-independent (PLAN §5.1, §14.1): every
    // git the backend runs reads the fixture's global config and none of the developer's
    // system or global configuration or attributes.
    let repo = committed_repo();
    std::fs::write(
        repo.home().join(".gitconfig"),
        "[dv]\n\tprobe = from-fixture-home\n",
    )
    .unwrap();
    let backend = open(&repo);
    let run = |args: &[&str]| {
        let output = backend.git(args, &GitOpts::default()).unwrap();
        String::from_utf8(output.stdout).unwrap()
    };
    assert_eq!(run(&["config", "--get", "dv.probe"]), "from-fixture-home\n");
    // Every setting git sees comes from dv's policy (`command`), the repo's own config or the
    // fixture's global file, which holds only the probe.
    let listing = run(&["config", "--list", "--show-scope"]);
    for line in listing.lines() {
        let (scope, setting) = line.split_once('\t').unwrap_or((line, ""));
        assert!(
            matches!(scope, "command" | "local")
                || (scope == "global" && setting == "dv.probe=from-fixture-home"),
            "{line:?} in {listing}"
        );
    }
    assert!(
        listing.contains("global\tdv.probe=from-fixture-home"),
        "{listing}"
    );
    // No system gitattributes (Git for Windows maps `*.doc` to a textconv driver there).
    assert_eq!(
        run(&["check-attr", "diff", "--", "x.doc"]),
        "x.doc: diff: unspecified\n"
    );
}

#[test]
fn read_commands_never_rewrite_the_index() {
    let repo = committed_repo();
    let index = repo.path().join(".git").join("index");
    // An mtime that disagrees with the index makes the entry stat-dirty, so a plain `git status`
    // or `git diff` finds it unchanged, refreshes its stat data and rewrites the index.
    let file = std::fs::File::options()
        .write(true)
        .open(repo.path().join("a.txt"))
        .unwrap();
    file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
        .unwrap();
    drop(file);
    let before = std::fs::read(&index).unwrap();

    let backend = open(&repo);
    let run = |args: &[&str]| {
        let output = backend.git(args, &GitOpts::default()).unwrap();
        assert_eq!(
            std::fs::read(&index).unwrap(),
            before,
            "git {args:?} wrote the index"
        );
        String::from_utf8(output.stdout).unwrap()
    };
    // The worktree forms of `git diff` rewrite the index despite `GIT_OPTIONAL_LOCKS=0`; only
    // the policy's `diff.autoRefreshIndex=false` stops them.
    assert_eq!(run(&["status", "--porcelain"]), "");
    run(&["diff", "--stat"]);
    run(&["diff", "HEAD"]);
    run(&["diff", "--name-status"]);
    run(&["diff", "--cached", "--name-status"]);
    // The price, which the file list handles (PLAN §6): the unchanged stat-dirty file shows in
    // `--name-status`, while `--numstat` and the patch leave it out.
    assert_eq!(run(&["diff", "HEAD", "--name-status"]), "M\ta.txt\n");
    assert_eq!(run(&["diff", "HEAD", "--numstat"]), "");
    assert_eq!(run(&["diff", "HEAD", "--", "a.txt"]), "");

    // Without dv's policy `git diff` does rewrite it, so the checks above mean something.
    repo.git(&["diff", "HEAD", "--name-status"]);
    assert_ne!(std::fs::read(&index).unwrap(), before);
}

#[test]
fn failures_carry_the_command_and_stderr_behind_a_one_line_summary() {
    let repo = committed_repo();
    let backend = open(&repo);
    let err = backend
        .git(
            &["rev-parse", "--verify", "no-such-rev"],
            &GitOpts::default(),
        )
        .unwrap_err();
    let BackendError::Git { args, code, stderr } = &err else {
        panic!("{err:?}");
    };
    assert_eq!(args, &["rev-parse", "--verify", "no-such-rev"]);
    assert_eq!(*code, Some(128));
    assert!(stderr.contains("fatal:"), "{stderr}");
    assert_eq!(err.to_string(), "git rev-parse failed (exit code 128)");
    let detail = err.detail().unwrap();
    assert!(
        detail.starts_with("$ git rev-parse --verify no-such-rev\n"),
        "{detail}"
    );
    assert!(detail.contains("fatal:"), "{detail}");
}

#[test]
fn stdin_and_ok_codes() {
    let repo = committed_repo();
    let backend = open(&repo);
    let hashed = backend
        .git(
            &["hash-object", "--stdin"],
            &GitOpts::new().with_stdin("a\n"),
        )
        .unwrap();
    assert_eq!(
        hashed.stdout_utf8().unwrap().trim(),
        repo.git(&["rev-parse", "HEAD:a.txt"]).trim()
    );

    let quiet_diff = ["diff", "--quiet", "HEAD", "--", "a.txt"];
    std::fs::write(repo.path().join("a.txt"), "changed\n").unwrap();
    let err = backend.git(&quiet_diff, &GitOpts::default()).unwrap_err();
    assert!(
        matches!(err, BackendError::Git { code: Some(1), .. }),
        "{err:?}"
    );
    let output = backend
        .git(&quiet_diff, &GitOpts::new().with_ok_codes(&[0, 1]))
        .unwrap();
    assert_eq!(output.code, 1);
}

#[test]
fn a_timeout_kills_git() {
    let repo = committed_repo();
    let backend = open(&repo);
    let started = Instant::now();
    let err = backend
        .git(
            &["-c", "alias.slow=!sleep 20", "slow"],
            &GitOpts::new().with_timeout(Duration::from_millis(300)),
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            BackendError::Timeout {
                timeout_ms: 300,
                ..
            }
        ),
        "{err:?}"
    );
    // Far sooner than the sleep: the timeout ended it.
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(err.to_string(), "git slow timed out after 300 ms");
}

#[test]
fn what_git_leaves_running_ends_with_it() {
    // git exits at once, but the backgrounded `sleep` inherited its stdout and stderr: it is in
    // git's process tree, so it is killed when git exits, which closes the pipes.
    let repo = committed_repo();
    let backend = open(&repo);
    let started = Instant::now();
    let output = backend
        .git(
            &["-c", "alias.bg=!sleep 20 &", "bg"],
            &GitOpts::new().with_timeout(Duration::from_secs(10)),
        )
        .unwrap();
    assert_eq!(output.code, 0);
    // Far sooner than the sleep: its end closed the pipes.
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

// ---- worktree and store files ------------------------------------------------------------------

#[test]
fn worktree_reads_and_sizes() {
    let repo = committed_repo();
    let backend = open(&repo);
    assert_eq!(
        backend.worktree_read("a.txt").unwrap(),
        Some(b"a\n".to_vec())
    );
    assert_eq!(backend.worktree_len("sub/deep/f.txt").unwrap(), Some(2));
    assert_eq!(backend.worktree_read("missing.txt").unwrap(), None);
    assert_eq!(backend.worktree_len("missing/x").unwrap(), None);
    assert_eq!(backend.worktree_read("a.txt/below-a-file").unwrap(), None);
    for dir in ["sub", "sub/deep"] {
        assert!(matches!(
            backend.worktree_read(dir),
            Err(BackendError::Io { .. })
        ));
        assert!(matches!(
            backend.worktree_len(dir),
            Err(BackendError::Io { .. })
        ));
    }
}

#[test]
fn text_stats_count_like_git() {
    let stats = |bytes: &[u8]| TextStats::of(bytes);
    let text = |len: u64, lines: u64| TextStats {
        len,
        lines,
        binary: false,
        symlink: false,
    };
    assert_eq!(stats(b""), text(0, 0));
    assert_eq!(stats(b"a\n"), text(2, 1));
    assert_eq!(stats(b"a\nb"), text(3, 2));
    assert_eq!(stats(b"\n\n"), text(2, 2));
    assert_eq!(stats(b"a\r\nb\r\n"), text(6, 2));
    assert_eq!(
        stats(b"a\n\0b\n"),
        TextStats {
            len: 5,
            lines: 0,
            binary: true,
            symlink: false
        }
    );
    // Only the first 8000 bytes are sniffed for a NUL, as git does.
    let mut late = vec![b'x'; TextStats::SNIFF_LEN as usize];
    late.push(0);
    assert!(!stats(&late).binary);
    late[TextStats::SNIFF_LEN as usize - 1] = 0;
    assert!(stats(&late).binary);

    // Fed in chunks, the counts are the same, the sniff spanning chunk boundaries.
    let mut counter = TextStatsCounter::default();
    for chunk in late.chunks(3000) {
        counter.feed(chunk);
    }
    assert!(counter.is_binary());
    let mut counter = TextStatsCounter::default();
    for chunk in b"one\ntwo\nthree".chunks(2) {
        counter.feed(chunk);
    }
    assert_eq!(counter.finish(), text(13, 3));
}

#[test]
fn worktree_text_stats_stream_the_file() {
    let repo = committed_repo();
    let mut big = "line\n".repeat(30_000).into_bytes();
    big.extend_from_slice(b"no newline");
    repo.write("big.txt", &big);
    let mut binary = vec![0u8; 16];
    binary.extend(std::iter::repeat_n(b'x', 200_000));
    repo.write("blob.bin", &binary);
    let backend = open(&repo);

    // Larger than one read, so the count spans chunks.
    assert_eq!(
        backend.worktree_text_stats("big.txt").unwrap(),
        Some(TextStats {
            len: big.len() as u64,
            lines: 30_001,
            binary: false,
            symlink: false
        })
    );
    // A binary file stops early but still reports its full size.
    assert_eq!(
        backend.worktree_text_stats("blob.bin").unwrap(),
        Some(TextStats {
            len: binary.len() as u64,
            lines: 0,
            binary: true,
            symlink: false
        })
    );
    assert_eq!(backend.worktree_text_stats("missing.txt").unwrap(), None);
    assert!(matches!(
        backend.worktree_text_stats("sub"),
        Err(BackendError::Io { .. })
    ));
    assert!(matches!(
        backend.worktree_text_stats("../x"),
        Err(BackendError::InvalidPath { .. })
    ));

    // The trait's default (whole-file read) agrees with the streaming implementation.
    struct ReadOnly<'a>(&'a LocalBackend);
    impl Backend for ReadOnly<'_> {
        fn repo(&self) -> &RepoInfo {
            self.0.repo()
        }
        fn git(&self, args: &[&str], opts: &GitOpts) -> Result<GitOutput, BackendError> {
            self.0.git(args, opts)
        }
        fn worktree_read(&self, rel: &str) -> Result<Option<Vec<u8>>, BackendError> {
            self.0.worktree_read(rel)
        }
        fn worktree_len(&self, rel: &str) -> Result<Option<u64>, BackendError> {
            self.0.worktree_len(rel)
        }
        fn store_append(&self, rel: &str, line: &[u8]) -> Result<(), BackendError> {
            self.0.store_append(rel, line)
        }
        fn store_read(&self, rel: &str) -> Result<Option<Vec<u8>>, BackendError> {
            self.0.store_read(rel)
        }
        fn store_list(&self, dir: &str) -> Result<Vec<String>, BackendError> {
            self.0.store_list(dir)
        }
        fn store_write_atomic(&self, rel: &str, bytes: &[u8]) -> Result<(), BackendError> {
            self.0.store_write_atomic(rel, bytes)
        }
        fn store_remove(&self, rel: &str) -> Result<bool, BackendError> {
            self.0.store_remove(rel)
        }
        fn copy_index_to_store(&self, rel: &str) -> Result<bool, BackendError> {
            self.0.copy_index_to_store(rel)
        }
        fn watch(&self, spec: WatchSpec, sink: WatchSink) -> Result<WatchHandle, BackendError> {
            self.0.watch(spec, sink)
        }
        fn spawn_stdio(
            &self,
            spec: &crate::backend::ProcessSpec,
            sink: crate::backend::ProcessSink,
        ) -> Result<crate::backend::ProcessHandle, BackendError> {
            self.0.spawn_stdio(spec, sink)
        }
    }
    let default = ReadOnly(&backend);
    for path in ["big.txt", "blob.bin", "a.txt", "missing.txt"] {
        assert_eq!(
            default.worktree_text_stats(path).unwrap(),
            backend.worktree_text_stats(path).unwrap(),
            "{path}"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlinks_read_as_their_target_paths() {
    let repo = committed_repo();
    let link = |target: &str, name: &str| {
        std::os::unix::fs::symlink(target, repo.path().join(name)).unwrap();
    };
    link("a.txt", "link");
    link("/nonexistent/x", "dangling");
    link("sub", "dirlink");
    let backend = open(&repo);
    for (path, target) in [
        ("link", "a.txt"),
        ("dangling", "/nonexistent/x"),
        ("dirlink", "sub"),
    ] {
        assert_eq!(
            backend.worktree_read(path).unwrap(),
            Some(target.as_bytes().to_vec()),
            "{path}"
        );
        assert_eq!(
            backend.worktree_len(path).unwrap(),
            Some(target.len() as u64),
            "{path}"
        );
        assert_eq!(
            backend.worktree_text_stats(path).unwrap(),
            Some(TextStats {
                len: target.len() as u64,
                lines: 1,
                binary: false,
                symlink: true
            }),
            "{path}"
        );
    }
    // git agrees: a link's blob is its target path.
    repo.git(&["add", "link"]);
    let staged = repo.git(&["ls-files", "-s", "link"]);
    assert_eq!(
        staged,
        format!("120000 {} 0\tlink\n", repo.hash_blob("a.txt"))
    );
}

#[test]
fn relative_paths_are_validated_before_use() {
    let repo = committed_repo();
    let backend = open(&repo);
    let outside = repo.outside_dir("outside");
    std::fs::write(outside.join("secret"), "secret").unwrap();
    for rel in [
        "../outside/secret",
        "/etc/passwd",
        "sub\\..\\..\\x",
        "",
        "a.txt/",
        "./a.txt",
    ] {
        let invalid = |result: Result<(), BackendError>| {
            matches!(result, Err(BackendError::InvalidPath { .. }))
        };
        assert!(invalid(backend.worktree_read(rel).map(drop)), "{rel:?}");
        assert!(invalid(backend.worktree_len(rel).map(drop)), "{rel:?}");
        assert!(invalid(backend.store_append(rel, b"x")), "{rel:?}");
        assert!(invalid(backend.store_read(rel).map(drop)), "{rel:?}");
        assert!(invalid(backend.store_write_atomic(rel, b"x")), "{rel:?}");
        assert!(invalid(backend.store_remove(rel).map(drop)), "{rel:?}");
    }
    for rel in ["../x", "/x", "a\\b"] {
        assert!(matches!(
            backend.store_list(rel),
            Err(BackendError::InvalidPath { .. })
        ));
    }
    assert!(matches!(
        backend.store_append("reviews/r.jsonl", b"two\nlines"),
        Err(BackendError::InvalidLine { .. })
    ));
}

#[test]
fn store_files_live_in_the_common_dir() {
    let repo = committed_repo();
    let backend = open(&repo);
    let rel = "reviews/r-1.jsonl";
    assert_eq!(backend.store_read(rel).unwrap(), None);
    assert_eq!(backend.store_list("reviews").unwrap(), Vec::<String>::new());

    backend.store_append(rel, br#"{"n":1}"#).unwrap();
    backend.store_append(rel, br#"{"n":2}"#).unwrap();
    assert_eq!(
        backend.store_read(rel).unwrap().unwrap(),
        b"{\"n\":1}\n{\"n\":2}\n"
    );
    let on_disk = repo
        .path()
        .join(".git")
        .join("dv")
        .join("reviews")
        .join("r-1.jsonl");
    assert_eq!(std::fs::read(on_disk).unwrap(), b"{\"n\":1}\n{\"n\":2}\n");

    backend
        .store_write_atomic("reviews/r-1.state.json", b"old")
        .unwrap();
    backend
        .store_write_atomic("reviews/r-1.state.json", b"new")
        .unwrap();
    assert_eq!(
        backend
            .store_read("reviews/r-1.state.json")
            .unwrap()
            .unwrap(),
        b"new"
    );

    // Directories and dot files (temp files mid-rename) aren't listed.
    std::fs::create_dir_all(backend.store_root().join("reviews").join("nested")).unwrap();
    std::fs::write(backend.store_root().join("reviews").join(".dv-tmp-x"), "").unwrap();
    assert_eq!(
        backend.store_list("reviews").unwrap(),
        ["r-1.jsonl", "r-1.state.json"]
    );
    assert_eq!(backend.store_list("").unwrap(), Vec::<String>::new());
    assert_eq!(backend.store_list("missing").unwrap(), Vec::<String>::new());

    assert!(backend.store_remove("reviews/r-1.state.json").unwrap());
    assert!(!backend.store_remove("reviews/r-1.state.json").unwrap());
    assert_eq!(backend.store_list("reviews").unwrap(), ["r-1.jsonl"]);
}

#[test]
fn a_linked_worktree_shares_the_main_repos_store() {
    let repo = committed_repo();
    let worktree = repo.add_worktree("wt", "feat");
    let linked = open_at(&repo, &worktree);
    linked
        .store_append("reviews/r-1.jsonl", b"from-the-worktree")
        .unwrap();
    assert!(same_dir(
        linked.store_root(),
        repo.path().join(".git").join("dv")
    ));
    assert_eq!(
        open(&repo)
            .store_read("reviews/r-1.jsonl")
            .unwrap()
            .unwrap(),
        b"from-the-worktree\n"
    );
}

// ---- concurrent appends (PLAN D5) --------------------------------------------------------------

const APPEND_REL: &str = "reviews/r-concurrent.jsonl";
const LINES_PER_WRITER: usize = 200;
const CHILD_TEST: &str = "backend::tests::store_append_child_writer";

/// A line whose length and padding depend on its writer and number, so a torn or interleaved
/// write can't parse back into the same line.
fn append_line(writer: &str, n: usize) -> String {
    let fill = char::from(b'a' + (writer.bytes().map(usize::from).sum::<usize>() % 26) as u8);
    let pad = fill.to_string().repeat(100 + (n * 37) % 900);
    format!(r#"{{"w":"{writer}","n":{n},"pad":"{pad}"}}"#)
}

/// One writer process for `concurrent_appends_never_interleave`: opens the repo the parent names,
/// reports ready, waits for the go signal, appends its lines. Does nothing when run on its own.
#[test]
#[ignore = "run as a child process by concurrent_appends_never_interleave"]
fn store_append_child_writer() {
    let (Some(repo), Some(writer), Some(gate)) = (
        std::env::var_os("DV_TEST_APPEND_REPO"),
        std::env::var("DV_TEST_APPEND_WRITER").ok(),
        std::env::var_os("DV_TEST_APPEND_GATE"),
    ) else {
        return;
    };
    let gate = Path::new(&gate);
    // The parent applied the fixture's environment to this process, so default options match.
    let backend = LocalBackend::open(Path::new(&repo)).unwrap();
    std::fs::write(gate.join(format!("ready-{writer}")), "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !gate.join("go").exists() {
        assert!(Instant::now() < deadline, "no go signal");
        std::thread::sleep(Duration::from_millis(2));
    }
    for n in 0..LINES_PER_WRITER {
        backend
            .store_append(APPEND_REL, append_line(&writer, n).as_bytes())
            .unwrap();
    }
}

#[test]
fn concurrent_appends_never_interleave() {
    let repo = committed_repo();
    let backend = Arc::new(open(&repo));
    let gate = repo.outside_dir("gate");
    let processes: Vec<String> = (0..4).map(|n| format!("p{n}")).collect();
    let threads: Vec<String> = (0..8).map(|n| format!("t{n}")).collect();

    let children: Vec<_> = processes
        .iter()
        .map(|writer| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args([CHILD_TEST, "--exact", "--ignored", "--test-threads=1"]);
            apply_overlay(&mut command, &repo.env_overlay());
            command
                .env("DV_TEST_APPEND_REPO", repo.path())
                .env("DV_TEST_APPEND_WRITER", writer)
                .env("DV_TEST_APPEND_GATE", &gate)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !processes
        .iter()
        .all(|writer| gate.join(format!("ready-{writer}")).exists())
    {
        assert!(Instant::now() < deadline, "child writers never got ready");
        std::thread::sleep(Duration::from_millis(5));
    }

    let barrier = Arc::new(Barrier::new(threads.len() + 1));
    let handles: Vec<_> = threads
        .iter()
        .map(|writer| {
            let (backend, barrier, writer) =
                (Arc::clone(&backend), Arc::clone(&barrier), writer.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for n in 0..LINES_PER_WRITER {
                    backend
                        .store_append(APPEND_REL, append_line(&writer, n).as_bytes())
                        .unwrap();
                }
            })
        })
        .collect();
    std::fs::write(gate.join("go"), "").unwrap();
    barrier.wait();
    for handle in handles {
        handle.join().unwrap();
    }
    for child in children {
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains(" 1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let log = backend.store_read(APPEND_REL).unwrap().unwrap();
    let text = String::from_utf8(log).unwrap();
    assert!(text.ends_with('\n'));
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|err| panic!("a torn or interleaved line ({err}): {line:.120}"));
        let writer = value["w"].as_str().unwrap().to_string();
        let n = usize::try_from(value["n"].as_u64().unwrap()).unwrap();
        assert_eq!(line, append_line(&writer, n), "line content intact");
        assert!(seen.insert((writer, n)), "duplicate line {line:.60}");
    }
    let writers = processes.len() + threads.len();
    assert_eq!(seen.len(), writers * LINES_PER_WRITER);
    assert_eq!(text.lines().count(), writers * LINES_PER_WRITER);
}

// ---- small pieces ------------------------------------------------------------------------------

#[test]
fn git_versions_parse() {
    let cases = [
        (
            "git version 2.53.0.windows.1\n",
            (2, 53, 0),
            "2.53.0.windows.1",
        ),
        (
            "git version 2.39.3 (Apple Git-146)",
            (2, 39, 3),
            "2.39.3 (Apple Git-146)",
        ),
        ("git version 2.43.0", (2, 43, 0), "2.43.0"),
        ("git version 3.0", (3, 0, 0), "3.0"),
    ];
    for (output, (major, minor, patch), text) in cases {
        let version = GitVersion::parse(output).unwrap();
        assert_eq!(
            (version.major, version.minor, version.patch),
            (major, minor, patch)
        );
        assert_eq!(version.text, text);
    }
    assert!(GitVersion::parse("hg version 6").is_none());
    assert!(GitVersion::parse("git version x").is_none());
    let version = GitVersion::parse("git version 2.31.1").unwrap();
    assert!(version.at_least(2, 31) && version.at_least(1, 99) && !version.at_least(2, 32));
}

#[test]
fn subcommand_skips_global_options() {
    let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    assert_eq!(subcommand(&args(&["diff", "--stat"])), "diff");
    assert_eq!(
        subcommand(&args(&["-c", "a=b", "--no-pager", "log"])),
        "log"
    );
    assert_eq!(subcommand(&args(&["-C", "dir", "status"])), "status");
    assert_eq!(subcommand(&args(&["--version"])), "command");
}
