//! The commands run in-process against fixture repos (docs/PLAN.md §10, §14.1): each test builds
//! a [`Context`] over captured streams, the fixture's git environment and an explicit
//! environment (so neither this machine's `CLAUDECODE` nor its git configuration leaks in),
//! and checks the exit code, the JSON document and the text.

mod annotate;
mod backend;
mod comment;
mod comment_write;
mod doctor;
mod errors;
mod escape;
mod file_level;
mod hand_back;
mod history;
mod home;
mod placement;
mod pr;
mod reexec;
mod review;
mod review_write;
#[cfg(windows)]
mod sandbox;
mod setup;
mod setup_wsl;
mod skill;
mod status;
mod wait;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use dv_core::backend::{Backend, LocalBackend, LocalBackendOptions};
use dv_core::git::GitRepo;
use dv_core::review::{
    Anchor, AnchorPlace, AnchorSide, Author, EventBody, Review, ReviewId, ReviewStore, ThreadId,
    Verdict, anchor_selection, ops,
};
use dv_core::target::Target;
use dv_test_support::{FixtureRepo, mock_gh};
use serde_json::Value;

use crate::{Context, Env, Exit};

/// What a command reads: its standard input, and whether a person sits at a terminal.
#[derive(Clone, Default)]
pub(crate) struct Input {
    pub stdin: String,
    pub terminal: bool,
}

/// A stream the test reads back.
#[derive(Clone, Default)]
pub(crate) struct Buffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl Buffer {
    pub fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).expect("the CLI prints UTF-8")
    }
}

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a command did.
#[derive(Debug)]
pub(crate) struct Ran {
    pub exit: Exit,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// stdout as the one JSON document it must be.
    pub fn json(&self) -> Value {
        assert_eq!(
            self.stdout.lines().count(),
            1,
            "one JSON line expected on stdout, got {:?}",
            self.stdout
        );
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|error| panic!("{error}: {:?}", self.stdout))
    }

    /// Asserts a success and returns the JSON document.
    pub fn ok_json(&self) -> Value {
        assert_eq!(self.exit, Exit::Ok, "stderr: {}", self.stderr);
        let json = self.json();
        assert_eq!(json["schema"], 1);
        json
    }

    /// Asserts a failure with `exit` and `code` and returns the error document.
    pub fn error(&self, exit: Exit, code: &str) -> Value {
        assert_eq!(
            self.exit, exit,
            "stdout: {} stderr: {}",
            self.stdout, self.stderr
        );
        // dv's own errors, or clap's.
        assert!(
            self.stderr.starts_with("dv: ") || self.stderr.starts_with("error: "),
            "the error goes to stderr: {:?}",
            self.stderr
        );
        let json = self.json();
        assert_eq!(json["schema"], 1);
        assert_eq!(json["error"]["code"], code, "{json}");
        json["error"].clone()
    }

    /// stdout's lines.
    pub fn lines(&self) -> Vec<&str> {
        self.stdout.lines().collect()
    }
}

/// The lines of a command's text output that start at column 0, where dv's own headers and
/// code rows stand, having checked that nothing in the output moves a terminal's cursor but
/// the end of each line: no control character but the tab, and no Unicode line separator.
pub(crate) fn column_zero(stdout: &str) -> Vec<&str> {
    let moves = |c: char| {
        (c.is_control() && !matches!(c, '\n' | '\t')) || matches!(c, '\u{2028}' | '\u{2029}')
    };
    assert!(!stdout.contains(moves), "{stdout:?}");
    stdout
        .split('\n')
        .filter(|line| !line.is_empty() && !line.starts_with(' '))
        .collect()
}

/// A fixture repo with `src/app.ts` committed on `main` and edited in the working tree, so a
/// `working` review has a file to comment on.
pub(crate) struct Fixture {
    pub repo: FixtureRepo,
    /// The variables commands see; nothing else from this process's environment. It starts
    /// with `DV_LINUX_DIR` naming a folder without a Linux build ([`no_linux_build`]), so a
    /// command for a WSL repository stops at exit 4 before any `wsl.exe` starts (and `dv
    /// doctor` lists no distro); a test that crosses into a distro names the builds itself,
    /// sandboxed (`sandbox`). `DV_USER_PATH_FILE` names a file of the fixture's standing in
    /// for the user Path, and `DV_GH` names `mock_gh` ([`mock_gh`]), so no check reads the
    /// registry or asks GitHub.
    pub env: BTreeMap<String, String>,
}

/// A folder without a Linux build of dv (module docs of [`Fixture::env`]).
pub(crate) fn no_linux_build() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("no-linux-build")
}

/// The committed `src/app.ts`: twenty lines of distinct code.
pub(crate) fn app_lines() -> Vec<String> {
    [
        "import { db } from \"./db\";",
        "import { log } from \"./log\";",
        "",
        "export interface User {",
        "  id: string;",
        "  name: string;",
        "}",
        "",
        "export async function getUser(id: string): Promise<User> {",
        "  const row = await db.query(\"select * from users where id = $1\", [id]);",
        "  if (!row) {",
        "    throw new Error(\"not found\");",
        "  }",
        "  return { id: row.id, name: row.name };",
        "}",
        "",
        "export function formatName(user: User): string {",
        "  return user.name.trim();",
        "}",
        "// end",
    ]
    .map(str::to_string)
    .to_vec()
}

/// `src/app.ts` as the working tree has it: line 10 reads through a cache.
pub(crate) fn edited_lines() -> Vec<String> {
    let mut lines = app_lines();
    lines[9] =
        "  const row = (await cache.get(id)) ?? (await db.query(\"select 1\", [id]));".to_string();
    lines
}

/// [`edited_lines`] with two lines inserted on top and `formatName` (lines 17–19) gone.
pub(crate) fn moved_lines() -> Vec<String> {
    let mut lines = edited_lines();
    lines.drain(16..19);
    lines.insert(0, "// cache first".to_string());
    lines.insert(1, "import { cache } from \"./cache\";".to_string());
    lines
}

pub(crate) fn text_of(lines: &[String]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

impl Fixture {
    pub fn new() -> Self {
        let repo = FixtureRepo::new();
        repo.write("src/app.ts", text_of(&app_lines()));
        repo.write("README.md", "# api\n");
        repo.commit("base");
        repo.write("src/app.ts", text_of(&edited_lines()));
        // The checks' seams (PLAN §12): the user Path a file of the fixture's, `gh` the fake.
        let user_path = repo.outside_dir("user-path").join("Path.txt");
        Self {
            repo,
            env: BTreeMap::from([
                (
                    "DV_LINUX_DIR".to_string(),
                    no_linux_build().display().to_string(),
                ),
                (
                    "DV_USER_PATH_FILE".to_string(),
                    user_path.display().to_string(),
                ),
                ("DV_GH".to_string(), mock_gh().display().to_string()),
            ]),
        }
    }

    pub fn path(&self) -> &Path {
        self.repo.path()
    }

    /// Runs `dv <args>` in the work tree.
    pub fn run(&self, args: &[&str]) -> Ran {
        self.run_in(self.repo.path(), args)
    }

    /// Runs `dv <args>` with `cwd` as the working directory.
    pub fn run_in(&self, cwd: &Path, args: &[&str]) -> Ran {
        self.run_with(cwd, args, &Input::default())
    }

    /// Runs `dv <args>` in the work tree with `stdin` on its standard input (not a terminal).
    pub fn run_stdin(&self, args: &[&str], stdin: &str) -> Ran {
        let input = Input {
            stdin: stdin.to_string(),
            terminal: false,
        };
        self.run_with(self.repo.path(), args, &input)
    }

    /// Runs `dv <args>` with `cwd` as the working directory and `input` on its standard input.
    pub fn run_with(&self, cwd: &Path, args: &[&str], input: &Input) -> Ran {
        let mut ctx = self.context(cwd, input);
        let exit = crate::main_with(
            std::iter::once("dv").chain(args.iter().copied()),
            &mut ctx.0,
        );
        Ran {
            exit,
            stdout: ctx.1.text(),
            stderr: ctx.2.text(),
        }
    }

    /// A context in `cwd` over captured streams, with the fixture's environment, and the
    /// buffers its stdout and stderr write to.
    pub fn context(&self, cwd: &Path, input: &Input) -> (Context, Buffer, Buffer) {
        let (stdout, stderr) = (Buffer::default(), Buffer::default());
        let ctx = Context {
            args: Vec::new(),
            cwd: cwd.to_path_buf(),
            env: Env::Vars(self.env.clone()),
            stdin: Box::new(std::io::Cursor::new(input.stdin.clone().into_bytes())),
            stdout: Box::new(stdout.clone()),
            stderr: Box::new(stderr.clone()),
            stdout_is_terminal: input.terminal,
            stderr_is_terminal: input.terminal,
            stdin_is_terminal: input.terminal,
            process_stdio: false,
            git_env: self.repo.env_overlay(),
        };
        (ctx, stdout, stderr)
    }

    pub fn backend(&self) -> Arc<dyn Backend> {
        self.backend_at(self.repo.path())
    }

    pub fn backend_at(&self, path: &Path) -> Arc<dyn Backend> {
        Arc::new(
            LocalBackend::open_with(
                path,
                LocalBackendOptions {
                    env: self.repo.env_overlay(),
                },
            )
            .unwrap(),
        )
    }

    pub fn store(&self) -> ReviewStore {
        ReviewStore::new(self.backend())
    }

    pub fn git(&self) -> GitRepo {
        GitRepo::new(self.backend())
    }

    /// Opens (or resumes) the review of `target` in the main work tree, as the reviewer.
    pub fn review(&self, target: Target) -> Review {
        ops::open_review(&self.store(), target, reviewer())
            .unwrap()
            .review
    }

    /// Adds a thread on `path`'s new side, lines `start..=end` as the working tree has them now.
    pub fn thread(
        &self,
        review: &Review,
        path: &str,
        start: u32,
        end: u32,
        body: &str,
    ) -> ThreadId {
        let anchor = self.anchor(path, AnchorSide::New, start, end);
        ops::add_thread(&self.store(), &review.id, anchor, body, reviewer()).unwrap()
    }

    /// An anchor on `path`'s `side` in round 1: the working tree's text for the new side, the
    /// committed text for the old one.
    pub fn anchor(&self, path: &str, side: AnchorSide, start: u32, end: u32) -> Anchor {
        let text = match side {
            AnchorSide::New => std::fs::read_to_string(self.repo.path().join(path)).unwrap(),
            AnchorSide::Old => self.repo.git(&["show", &format!("HEAD:{path}")]),
        };
        let lines: Vec<&str> = text.lines().collect();
        let place = AnchorPlace {
            round: 1,
            path: path.to_string(),
            old_path: None,
            side,
        };
        anchor_selection(place, &lines, start, end).unwrap()
    }

    /// Rewrites `src/app.ts` in the working tree.
    pub fn edit_app(&self, lines: &[String]) {
        self.repo.write("src/app.ts", text_of(lines));
    }

    /// A directory outside every repository.
    pub fn outside(&self, name: &str) -> PathBuf {
        self.repo.outside_dir(name)
    }

    /// The review as its log folds now.
    pub fn load(&self, id: &ReviewId) -> Review {
        self.store().load(id).unwrap().review
    }

    /// The review's events, oldest first.
    pub fn events(&self, id: &ReviewId) -> Vec<EventBody> {
        self.store()
            .events(id)
            .unwrap()
            .events
            .into_iter()
            .map(|event| event.body)
            .collect()
    }

    /// The reviewer finishes the round in progress with `verdict`.
    pub fn finish(&self, id: &ReviewId, verdict: Verdict) {
        ops::finish_round(
            &self.store(),
            &self.git(),
            id,
            Some(verdict),
            None,
            &reviewer(),
        )
        .unwrap();
    }

    /// Something that opens the fixture's repository from another thread.
    pub fn opener(&self) -> Opener {
        Opener {
            path: self.repo.path().to_path_buf(),
            env: self.repo.env_overlay(),
        }
    }
}

/// Opens a fixture's repository again, on any thread.
#[derive(Clone)]
pub(crate) struct Opener {
    path: PathBuf,
    env: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
}

impl Opener {
    pub fn store(&self) -> ReviewStore {
        ReviewStore::new(self.backend())
    }

    pub fn git(&self) -> GitRepo {
        GitRepo::new(self.backend())
    }

    fn backend(&self) -> Arc<dyn Backend> {
        Arc::new(
            LocalBackend::open_with(
                &self.path,
                LocalBackendOptions {
                    env: self.env.clone(),
                },
            )
            .unwrap(),
        )
    }

    /// The reviewer finishes the round in progress with `verdict`.
    pub fn finish(&self, id: &ReviewId, verdict: Verdict) {
        ops::finish_round(
            &self.store(),
            &self.git(),
            id,
            Some(verdict),
            None,
            &reviewer(),
        )
        .unwrap();
    }
}

/// The event's author, for the events that record one.
pub(crate) fn author_of(event: &EventBody) -> Option<&Author> {
    match event {
        EventBody::Created { author, .. }
        | EventBody::ThreadAdded { author, .. }
        | EventBody::ReplyAdded { author, .. }
        | EventBody::ThreadState { author, .. } => Some(author),
        EventBody::Status { author, .. } => author.as_ref(),
        _ => None,
    }
}

/// Waits long enough that the next id sorts after the last one (ids lead with the time in
/// milliseconds; within one they are random).
pub(crate) fn later() {
    std::thread::sleep(std::time::Duration::from_millis(3));
}

pub(crate) fn reviewer() -> Author {
    Author::reviewer("dv fixture")
}

pub(crate) fn agent() -> Author {
    Author::agent("claude-code")
}
