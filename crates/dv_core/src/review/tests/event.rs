//! The log format: one JSON object per line, and the forgiving reader (PLAN §7).

use super::*;

/// One event of every kind.
fn one_of_each() -> Vec<EventBody> {
    let thread = ThreadId::generate();
    let comment = CommentId::generate();
    vec![
        created(Target::Branch {
            branch: "feat".to_string(),
            base: None,
        }),
        EventBody::RoundClosed {
            n: 1,
            new_oid: oid('b'),
            old_oid: oid('a'),
            verdict: Some(Verdict::RequestChanges),
            body: Some("Two things before this lands.".to_string()),
        },
        EventBody::Viewed {
            round: 1,
            path: "src/lib.rs".to_string(),
            on: true,
        },
        // A file-level anchor (lines 0..=0, no text or context, PLAN §2.5) round-trips too.
        EventBody::ThreadAdded {
            id: thread.clone(),
            anchor: Anchor::file_level(1, "src/lib.rs".to_string(), None, AnchorSide::New),
            body: "Why?".to_string(),
            author: reviewer(),
            annotation: false,
        },
        EventBody::ReplyAdded {
            thread: thread.clone(),
            id: comment.clone(),
            body: "Fixed.".to_string(),
            author: agent(),
            anchor: Some(anchor("src/lib.rs", 9)),
        },
        EventBody::ThreadState {
            thread: thread.clone(),
            state: ThreadState::Addressed,
            note: Some("renamed it".to_string()),
            author: agent(),
            anchor: Some(anchor("src/lib.rs", 10)),
        },
        EventBody::BodyEdited {
            target_id: ItemId::Comment(comment.clone()),
            body: "Fixed in 3f2a.".to_string(),
        },
        EventBody::Deleted {
            target_id: ItemId::Comment(comment),
        },
        EventBody::PrLinked {
            owner: "kylekz".to_string(),
            repo: "difftest".to_string(),
            number: 7,
        },
        EventBody::Submitted {
            github_review_id: 123,
            url: "https://github.com/kylekz/difftest/pull/7#pullrequestreview-123".to_string(),
            event: Verdict::Approve,
        },
        EventBody::Status {
            status: ReviewStatus::Ready,
            note: Some("Both threads addressed.".to_string()),
            author: Some(agent()),
            if_status: Some(ReviewStatus::ChangesRequested),
            if_round: None,
        },
    ]
}

#[test]
fn every_kind_round_trips_and_is_listed() {
    let bodies = one_of_each();
    let kinds: Vec<&str> = bodies.iter().map(EventBody::kind).collect();
    assert_eq!(kinds, EventBody::KINDS, "one test event per kind, in order");
    for body in bodies {
        let event = Event::new(body);
        let line = event.to_line().unwrap();
        assert!(!line.contains(&b'\n'));
        let value: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(value["kind"], event.body.kind());
        let parsed = parse_log(&line);
        assert_eq!(
            parsed.events,
            std::slice::from_ref(&event),
            "{}",
            event.body.kind()
        );
        assert!(parsed.warnings.is_empty());
    }
}

#[test]
fn a_line_carries_v_eid_at_and_the_kind_beside_its_fields() {
    let thread = ThreadId::parse("t-01j9z3k4m5n6p7q8r9s0t1v2w3").unwrap();
    let event = Event {
        v: EVENT_VERSION,
        eid: EventId::parse("e-01j9z3k4m5n6p7q8r9s0t1v2w3").unwrap(),
        at: 1_759_560_000_000,
        body: EventBody::ThreadState {
            thread,
            state: ThreadState::Resolved,
            note: None,
            author: reviewer(),
            anchor: None,
        },
    };
    let line = String::from_utf8(event.to_line().unwrap()).unwrap();
    assert_eq!(
        line,
        r#"{"v":1,"eid":"e-01j9z3k4m5n6p7q8r9s0t1v2w3","at":1759560000000,"kind":"thread_state","thread":"t-01j9z3k4m5n6p7q8r9s0t1v2w3","state":"resolved","author":{"kind":"reviewer","name":"Fixture Reviewer"}}"#
    );
    // Targets keep their own stable shape inside `created` (target.rs).
    let created = Event::new(created(Target::Working)).to_line().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&created).unwrap();
    assert_eq!(value["target"], "working");
    assert_eq!(value["worktree"], "/repo");
}

#[test]
fn bodies_with_newlines_stay_on_one_line() {
    let event = Event::new(EventBody::BodyEdited {
        target_id: ItemId::Thread(ThreadId::generate()),
        body: "line one\nline two\r\n\u{2028}end".to_string(),
    });
    let line = event.to_line().unwrap();
    assert!(!line.contains(&b'\n') && !line.contains(&b'\r'));
    assert_eq!(parse_log(&line).events, [event]);
}

#[test]
fn bad_lines_become_warnings_and_the_rest_still_parses() {
    let good: Vec<Event> = (0..3)
        .map(|n| event_at(n, status(ReviewStatus::Ready)))
        .collect();
    let mut log = Vec::new();
    log.extend(good[0].to_line().unwrap());
    log.extend(b"\n");
    log.extend(b"not json\n");
    log.extend(b"[1,2]\n");
    log.extend(br#"{"kind":"status","status":"ready"}"#);
    log.extend(b"\n");
    log.extend(br#"{"v":0,"kind":"status"}"#);
    log.extend(b"\n");
    log.extend(br#"{"v":"1","kind":"status"}"#);
    log.extend(b"\n");
    log.extend(br#"{"v":1,"eid":"e-bad","at":1,"kind":"status","status":"ready"}"#);
    log.extend(b"\n");
    log.extend(br#"{"v":1,"at":1,"kind":"status","status":"ready"}"#);
    log.extend(b"\n");
    log.extend(
        br#"{"v":1,"eid":"e-01j9z3k4m5n6p7q8r9s0t1v2w3","at":1,"kind":"status","status":"bogus"}"#,
    );
    log.extend(b"\n");
    log.extend(b"\n   \n");
    log.extend(good[1].to_line().unwrap());
    log.extend(b"\r\n");
    log.extend(good[2].to_line().unwrap());
    log.extend(b"\n");

    let parsed = parse_log(&log);
    assert_eq!(
        parsed.events, good,
        "a CRLF line ending is whitespace to JSON"
    );
    let lines: Vec<usize> = parsed
        .warnings
        .iter()
        .map(|warning| match warning {
            LogWarning::Malformed { line, .. } => *line,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(lines, [2, 3, 4, 5, 6, 7, 8, 9]);
    assert_eq!(parsed.newer_events, 0);
}

#[test]
fn a_torn_last_line_is_reported_and_skipped() {
    let first = Event::new(created(Target::Working));
    let second = Event::new(status(ReviewStatus::Closed));
    let mut log = log_of(std::slice::from_ref(&first));
    let torn = second.to_line().unwrap();
    log.extend(&torn[..torn.len() / 2]);

    let parsed = parse_log(&log);
    assert_eq!(parsed.events, std::slice::from_ref(&first));
    let [LogWarning::Malformed { line: 2, error }] = &parsed.warnings[..] else {
        panic!("{:?}", parsed.warnings);
    };
    assert!(error.starts_with("incomplete last line"), "{error}");

    // A complete last line that only lacks its newline is still an event.
    let mut log = log_of(std::slice::from_ref(&first));
    log.extend(second.to_line().unwrap());
    assert_eq!(parse_log(&log).events, [first, second]);
}

#[test]
fn newer_events_are_counted_not_reported() {
    let known = Event::new(created(Target::Working));
    let mut log = log_of(std::slice::from_ref(&known));
    // A kind this build doesn't know, at the current version: a newer dv's addition.
    log.extend(br#"{"v":1,"eid":"e-01j9z3k4m5n6p7q8r9s0t1v2w3","at":5,"kind":"reaction_added","emoji":"+1"}"#);
    log.extend(b"\n");
    // A future version, whatever its shape.
    log.extend(br#"{"v":2,"kind":"status","status":{"nested":"differently"}}"#);
    log.extend(b"\n");
    log.extend(br#"{"v":2}"#);
    log.extend(b"\n");
    // Unknown fields on a known kind are ignored: optional additions need no version bump.
    log.extend(br#"{"v":1,"eid":"e-01j9z3k4m5n6p7q8r9s0t1v2w4","at":6,"kind":"status","status":"ready","reason":"new field"}"#);
    log.extend(b"\n");

    let parsed = parse_log(&log);
    assert_eq!(parsed.newer_events, 3);
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    assert_eq!(parsed.events.len(), 2);
    assert_eq!(parsed.events[0], known);
    assert_eq!(parsed.events[1].body, status(ReviewStatus::Ready));
}

#[test]
fn an_agents_model_rides_with_its_name_and_is_left_out_without_one() {
    let thread = ThreadId::parse("t-01j9z3k4m5n6p7q8r9s0t1v2w3").unwrap();
    let line = r#"{"v":1,"eid":"e-01j9z3k4m5n6p7q8r9s0t1v2w4","at":6,"kind":"reply_added","thread":"t-01j9z3k4m5n6p7q8r9s0t1v2w3","id":"c-01j9z3k4m5n6p7q8r9s0t1v2w3","body":"Done.","author":{"kind":"agent","name":"claude-code","model":"Claude Fable 5.2"}}"#;
    let parsed = parse_log(line.as_bytes());
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    let EventBody::ReplyAdded {
        author, thread: t, ..
    } = &parsed.events[0].body
    else {
        panic!("a reply: {:?}", parsed.events[0].body);
    };
    assert_eq!(*t, thread);
    assert_eq!(author, &agent().with_model("Claude Fable 5.2"));
    assert_eq!(
        String::from_utf8(parsed.events[0].to_line().unwrap()).unwrap(),
        line
    );

    // Who wrote it is the kind and the name: the model only says what it ran on.
    assert!(author.is(&agent()));
    assert!(author.is(&agent().with_model("Claude Opus 5.5")));
    assert!(!author.is(&Author::agent("codex").with_model("Claude Fable 5.2")));
    assert!(!author.is(&Author::reviewer("claude-code")));
    let plain = serde_json::to_value(agent()).unwrap();
    assert_eq!(
        plain,
        serde_json::json!({"kind": "agent", "name": "claude-code"})
    );
}

#[test]
fn optional_anchors_notes_and_authors_are_left_out_when_absent_and_read_when_missing() {
    // As written before replies, state changes and statuses carried them: optional fields
    // need no version bump (PLAN §7).
    let thread = ThreadId::parse("t-01j9z3k4m5n6p7q8r9s0t1v2w3").unwrap();
    let lines = [
        r#"{"v":1,"eid":"e-01j9z3k4m5n6p7q8r9s0t1v2w3","at":5,"kind":"status","status":"ready"}"#,
        r#"{"v":1,"eid":"e-01j9z3k4m5n6p7q8r9s0t1v2w4","at":6,"kind":"reply_added","thread":"t-01j9z3k4m5n6p7q8r9s0t1v2w3","id":"c-01j9z3k4m5n6p7q8r9s0t1v2w3","body":"Done.","author":{"kind":"agent","name":"claude-code"}}"#,
        r#"{"v":1,"eid":"e-01j9z3k4m5n6p7q8r9s0t1v2w5","at":7,"kind":"thread_state","thread":"t-01j9z3k4m5n6p7q8r9s0t1v2w3","state":"addressed","author":{"kind":"agent","name":"claude-code"}}"#,
    ];
    let log = lines.join("\n");
    let parsed = parse_log(log.as_bytes());
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    assert_eq!(parsed.events[0].body, status(ReviewStatus::Ready));
    assert!(matches!(
        &parsed.events[1].body,
        EventBody::ReplyAdded { anchor: None, thread: t, .. } if *t == thread
    ));
    assert!(matches!(
        &parsed.events[2].body,
        EventBody::ThreadState {
            anchor: None,
            note: None,
            ..
        }
    ));
    for (event, line) in parsed.events.iter().zip(lines) {
        assert_eq!(String::from_utf8(event.to_line().unwrap()).unwrap(), line);
    }

    // Present, they ride beside the event's other fields.
    let done = Event::new(EventBody::Status {
        status: ReviewStatus::Ready,
        note: Some("Renamed both.".to_string()),
        author: Some(agent()),
        if_status: None,
        if_round: None,
    });
    let value: serde_json::Value = serde_json::from_slice(&done.to_line().unwrap()).unwrap();
    assert_eq!(value["note"], "Renamed both.");
    assert_eq!(value["author"]["name"], "claude-code");
    let moved = Event::new(EventBody::ThreadState {
        thread,
        state: ThreadState::Resolved,
        note: None,
        author: reviewer(),
        anchor: Some(anchor("src/lib.rs", 7)),
    });
    let value: serde_json::Value = serde_json::from_slice(&moved.to_line().unwrap()).unwrap();
    assert_eq!(value["anchor"]["start_line"], 7);
    assert_eq!(value["anchor"]["path"], "src/lib.rs");
}

#[test]
fn a_round_closed_line_without_a_body_still_reads() {
    // As written before rounds carried a summary: optional fields need no version bump (PLAN §7).
    let line = format!(
        r#"{{"v":1,"eid":"e-01j9z3k4m5n6p7q8r9s0t1v2w3","at":5,"kind":"round_closed","n":1,"new_oid":"{}","old_oid":"{}","verdict":"approve"}}"#,
        oid('b'),
        oid('a')
    );
    let parsed = parse_log(line.as_bytes());
    assert!(parsed.warnings.is_empty(), "{:?}", parsed.warnings);
    assert_eq!(
        parsed.events[0].body,
        EventBody::RoundClosed {
            n: 1,
            new_oid: oid('b'),
            old_oid: oid('a'),
            verdict: Some(Verdict::Approve),
            body: None,
        }
    );
    // And an empty body is left out of the line.
    let written = String::from_utf8(parsed.events[0].to_line().unwrap()).unwrap();
    assert_eq!(written, line);
}
