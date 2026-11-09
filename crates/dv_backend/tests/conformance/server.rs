//! What only a server does with its processes (PLAN §5.1, §18.3), spoken to in raw lines, on a
//! thread of this process and as a process of its own (`dv_test_child serve`): it kills them
//! when its input ends, also one a write is blocked on and one a `spawn` is still starting,
//! and a process printing faster than the client reads waits for it without the server
//! buffering without end or the other answers waiting behind it.

use std::io::{self, BufRead as _, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine as _;
use dv_test_support::FixtureRepo;
use pretty_assertions::assert_eq;
use serde_json::{Value, json};

use crate::process::{WITHIN, assert_gone, running};
use crate::{committed, helper};

/// A server spoken to in raw lines: requests written to `input`, replies and process lines
/// read from `output` on a thread.
struct Raw {
    input: Option<Box<dyn Write + Send>>,
    lines: Receiver<Value>,
}

/// An `open` request's `path` and `env` for a fixture repo.
type Opening = (String, Vec<(String, Option<String>)>);

fn opening(repo: &FixtureRepo) -> Opening {
    let env = repo
        .env_overlay()
        .into_iter()
        .map(|(name, value)| {
            (
                name.into_string().unwrap(),
                value.map(|value| value.into_string().unwrap()),
            )
        })
        .collect();
    (repo.path().to_str().unwrap().to_string(), env)
}

/// Where a raw server runs.
enum Server {
    Thread(JoinHandle<io::Result<()>>),
    Process(Child),
}

impl Server {
    /// Waits for the server to end, once its input has.
    fn finish(self) {
        match self {
            Server::Thread(thread) => thread.join().unwrap().unwrap(),
            Server::Process(mut child) => assert!(child.wait().unwrap().success()),
        }
    }
}

/// `serve` on a thread of this process.
fn on_a_thread() -> (Raw, Server) {
    let (server_in, input) = io::pipe().unwrap();
    let (output, server_out) = io::pipe().unwrap();
    let server = std::thread::spawn(move || dv_backend::server::serve(server_in, server_out));
    (Raw::new(input, output), Server::Thread(server))
}

/// The helper's `serve`, a process of its own.
fn as_a_process() -> (Raw, Server) {
    let mut child = Command::new(helper())
        .arg("serve")
        .env_remove("RUST_LOG")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let raw = Raw::new(child.stdin.take().unwrap(), child.stdout.take().unwrap());
    (raw, Server::Process(child))
}

impl Raw {
    fn new(input: impl Write + Send + 'static, output: impl Read + Send + 'static) -> Self {
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { return };
                if tx.send(serde_json::from_str(&line).unwrap()).is_err() {
                    return;
                }
            }
        });
        Self {
            input: Some(Box::new(input)),
            lines,
        }
    }

    fn send(&mut self, request: &Value) {
        let input = self.input.as_mut().unwrap();
        input.write_all(format!("{request}\n").as_bytes()).unwrap();
        input.flush().unwrap();
    }

    /// Ends the server's input.
    fn close(&mut self) {
        drop(self.input.take());
    }

    fn reply(&mut self, request: Value) -> Value {
        let id = request["id"].clone();
        self.send(&request);
        let deadline = Instant::now() + WITHIN;
        loop {
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("the server answers");
            if line["id"] == id {
                assert!(line.get("err").is_none(), "{line}");
                return line["ok"].clone();
            }
        }
    }

    /// Says hello and opens `repo`: its handle.
    fn open(&mut self, repo: &FixtureRepo) -> Value {
        self.open_with(&opening(repo))
    }

    /// [`Raw::open`] with the request's `path` and `env` made beforehand ([`opening`]).
    fn open_with(&mut self, (path, env): &Opening) -> Value {
        let proto = dv_backend::proto::PROTO;
        assert_eq!(
            self.reply(json!({"id": 1, "op": "hello", "proto": proto}))["proto"],
            proto
        );
        self.reply(json!({"id": 2, "op": "open", "path": path, "env": env}))["repo"].clone()
    }

    /// The stdout of process `process` until it holds `child <n>` and `pid <n>`: the two pids.
    fn pids(&self, process: &Value) -> (u32, u32) {
        let mut stdout = String::new();
        let deadline = Instant::now() + WITHIN;
        loop {
            let found = |word: &str| {
                stdout
                    .lines()
                    .find_map(|line| line.strip_prefix(word)?.trim().parse::<u32>().ok())
            };
            if let (Some(child), Some(pid)) = (found("child "), found("pid ")) {
                return (child, pid);
            }
            let line = self
                .lines
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("the process prints");
            if line["proc"] == *process
                && let Some(out) = line["out"].as_str()
            {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(out)
                    .unwrap();
                stdout.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }

    /// Opens `repo` and starts the helper's `child pid sleep` in it: its id on the server, its
    /// pid and its child's.
    fn start_sleeper(&mut self, repo: &FixtureRepo) -> (Value, u32, u32) {
        let handle = self.open(repo);
        let spawned = self.reply(json!({
            "id": 3, "op": "spawn", "repo": handle,
            "spec": {"program": helper(), "args": ["child", "pid", "sleep", "600000"]},
        }));
        let (child, pid) = self.pids(&spawned["proc"]);
        assert_eq!(spawned["pid"], pid);
        (spawned["proc"].clone(), pid, child)
    }
}

#[test]
fn the_server_kills_its_processes_when_its_input_ends() {
    let repo = committed();
    for (mut raw, server) in [on_a_thread(), as_a_process()] {
        let (_, pid, child) = raw.start_sleeper(&repo);
        raw.close();
        server.finish();
        assert_gone(pid, "the process");
        assert_gone(child, "the process it started");
    }
}

#[test]
fn a_write_blocked_on_a_full_pipe_doesnt_keep_its_process() {
    let repo = committed();
    for (mut raw, server) in [on_a_thread(), as_a_process()] {
        let (process, pid, child) = raw.start_sleeper(&repo);
        // The process never reads: a megabyte fills its stdin pipe and the write waits.
        let bytes = base64::engine::general_purpose::STANDARD.encode(vec![b'x'; 1024 * 1024]);
        raw.send(&json!({"id": 9, "op": "proc_write", "proc": process, "bytes": bytes}));
        std::thread::sleep(Duration::from_millis(300));
        raw.close();
        server.finish();
        assert_gone(pid, "the process");
        assert_gone(child, "the process it started");
    }
}

#[test]
fn a_spawn_racing_the_end_of_the_input_keeps_nothing() {
    let repo = committed();
    let dir = tempfile::tempdir().unwrap();
    let mut pidfiles = Vec::new();
    // A server of its own each round: one on a thread would kill a process it kept anyway once
    // its last reference went, which hides the race. The end of the input lands at staggered
    // moments around the spawn, every stagger once in each batch of rounds run side by side.
    let opening = opening(&repo);
    for batch in 0..6u64 {
        std::thread::scope(|scope| {
            let rounds: Vec<_> = (0..10u64)
                .map(|stagger| {
                    let (opening, pidfile) =
                        (&opening, dir.path().join(format!("pid-{batch}-{stagger}")));
                    scope.spawn(move || {
                        let (mut raw, server) = as_a_process();
                        let handle = raw.open_with(opening);
                        raw.send(&json!({
                            "id": 3, "op": "spawn", "repo": handle, "spec": {
                                "program": helper(),
                                "args": ["pidfile", pidfile.to_str().unwrap(), "sleep", "600000"],
                            },
                        }));
                        std::thread::sleep(Duration::from_micros(stagger * 400));
                        raw.close();
                        server.finish();
                        pidfile
                    })
                })
                .collect();
            pidfiles.extend(rounds.into_iter().map(|round| round.join().unwrap()));
        });
    }
    // A process kept past its server's end would have written its pid by now, and still run.
    std::thread::sleep(Duration::from_millis(500));
    for pidfile in pidfiles {
        if let Some(pid) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
        {
            assert_gone(pid, &format!("the process of {}", pidfile.display()));
        }
    }
}

#[test]
fn a_process_printing_faster_than_the_client_reads_waits_for_it() {
    let repo = committed();
    let dir = tempfile::tempdir().unwrap();
    let total = dir.path().join("flooded");
    // This test reads the server's output itself, only when it wants to.
    let (server_in, mut input) = io::pipe().unwrap();
    let (output, server_out) = io::pipe().unwrap();
    let server = std::thread::spawn(move || dv_backend::server::serve(server_in, server_out));
    let mut output = BufReader::new(output);
    let mut send = |request: Value| {
        input.write_all(format!("{request}\n").as_bytes()).unwrap();
    };
    let mut next_line = || {
        let mut line = String::new();
        assert_ne!(output.read_line(&mut line).unwrap(), 0, "the output ended");
        serde_json::from_str::<Value>(&line).unwrap()
    };
    let (path, env) = opening(&repo);
    send(json!({"id": 1, "op": "open", "path": path, "env": env}));
    let opened = next_line();
    send(json!({
        "id": 2, "op": "spawn", "repo": opened["ok"]["repo"],
        "spec": {"program": helper(), "args": ["flood", total.to_str().unwrap()]},
    }));
    let spawned = loop {
        let line = next_line();
        if line["id"] == 2 {
            break line;
        }
    };
    let pid = u32::try_from(spawned["ok"]["pid"].as_u64().unwrap()).unwrap();

    // Nobody reads: the process waits once the budget, the local queue and the pipes are full.
    let flooded = || -> usize {
        std::fs::read_to_string(&total)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    };
    // It has stalled once its count holds still between two looks; one that never stalls
    // passes the bound below on the way. The bound is what may wait unread: the local queue's
    // 256 chunks of 64 KiB (`toolbox_process`), the 1 MiB budget, and the pipes.
    let deadline = Instant::now() + WITHIN;
    let mut stalled = flooded();
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let now = flooded();
        assert!(
            now < 24 * 1024 * 1024,
            "the process printed {now} bytes nobody read"
        );
        if now > 0 && now == stalled {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the count never held still: {now}"
        );
        stalled = now;
    }
    std::thread::sleep(Duration::from_millis(300));
    let later = flooded();
    assert!(
        later - stalled <= 128 * 1024,
        "the process went on printing: {stalled} then {later} bytes"
    );
    assert!(running(pid));

    // Another answer gets through while it floods.
    let asked = Instant::now();
    send(json!({"id": 3, "op": "worktree_len", "repo": opened["ok"]["repo"], "rel": "a.txt"}));
    let answer = loop {
        let line = next_line();
        if line["id"] == 3 {
            break line;
        }
        assert!(asked.elapsed() < WITHIN, "no answer within {WITHIN:?}");
    };
    assert_eq!(answer["ok"], 2);

    // The end of the input ends it; whatever is left of the output is drained meanwhile.
    drop(input);
    let drain = std::thread::spawn(move || io::copy(&mut output, &mut io::sink()));
    server.join().unwrap().unwrap();
    drain.join().unwrap().unwrap();
    assert_gone(pid, "the flooding process");
}
