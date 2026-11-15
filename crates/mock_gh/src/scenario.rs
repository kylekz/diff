//! The scenario file `DV_MOCK_GH` names: which argv gets which answer (docs/PLAN.md §14.2).
//!
//! ```json
//! {"calls": [
//!   {"argv": ["pr", "view", "482", "--json", "*"], "stdout": {"number": 482}, "times": 1},
//!   {"argv": ["api", "**"], "stdin_contains": "\"event\":\"REQUEST_CHANGES\"", "stdout": {"id": 7}},
//!   {"argv": ["api", "**"], "stderr": "gh: HTTP 422\n", "exit": 1},
//!   {"argv": ["pr", "list", "**"], "stdout": [], "delay_ms": 2000}
//! ]}
//! ```
//!
//! An `argv` pattern is an exact string, `"*"` (any one argument) or `"**"` (all remaining
//! arguments, none included; only last). The first entry whose pattern and `stdin_contains` match
//! and that has calls left (`times`, unlimited when absent) answers, after `delay_ms` when it
//! has one (a GitHub that is slow to answer, or never does within the caller's timeout).

use std::path::Path;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::Value;

/// A parsed scenario file.
#[derive(Debug, Default)]
pub struct Scenario {
    pub calls: Vec<Entry>,
}

/// One scripted answer.
#[derive(Debug)]
pub struct Entry {
    pub argv: Vec<Pattern>,
    pub stdin_contains: Option<String>,
    pub stdout: Option<Value>,
    pub stderr: Option<String>,
    pub exit: u8,
    /// How many calls this entry answers over the whole call log; `None` is unlimited.
    pub times: Option<u32>,
    /// How long the answer takes, in ms.
    pub delay_ms: u64,
}

/// One element of an entry's `argv`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    Exact(String),
    /// `"*"`: any single argument.
    One,
    /// `"**"`: every remaining argument, possibly none.
    Rest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenario {
    calls: Vec<RawEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    argv: Vec<String>,
    stdin_contains: Option<String>,
    stdout: Option<Value>,
    stderr: Option<String>,
    #[serde(default)]
    exit: u8,
    times: Option<u32>,
    #[serde(default)]
    delay_ms: u64,
}

impl Scenario {
    /// Reads and validates the scenario file at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the scenario {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("in the scenario {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let raw: RawScenario = serde_json::from_str(text)?;
        let calls = raw
            .calls
            .into_iter()
            .enumerate()
            .map(|(index, entry)| Entry::from_raw(entry).with_context(|| format!("calls[{index}]")))
            .collect::<Result<_>>()?;
        Ok(Self { calls })
    }

    /// The index of the entry that answers `argv` with `stdin` (`None` when the call doesn't read
    /// stdin), given how often each entry has answered already (`used(index)`).
    pub fn pick(
        &self,
        argv: &[String],
        stdin: Option<&str>,
        used: impl Fn(usize) -> u32,
    ) -> Option<usize> {
        self.calls.iter().enumerate().find_map(|(index, entry)| {
            let left = entry.times.is_none_or(|times| used(index) < times);
            (left && entry.matches(argv, stdin)).then_some(index)
        })
    }
}

impl Entry {
    fn from_raw(raw: RawEntry) -> Result<Self> {
        let last = raw.argv.len().saturating_sub(1);
        let argv = raw
            .argv
            .into_iter()
            .enumerate()
            .map(|(position, arg)| match arg.as_str() {
                "*" => Ok(Pattern::One),
                "**" if position == last => Ok(Pattern::Rest),
                "**" => bail!("argv: \"**\" matches every remaining argument, so it must be last"),
                _ => Ok(Pattern::Exact(arg)),
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            argv,
            stdin_contains: raw.stdin_contains,
            stdout: raw.stdout,
            stderr: raw.stderr,
            exit: raw.exit,
            times: raw.times,
            delay_ms: raw.delay_ms,
        })
    }

    /// Whether this entry's `argv` patterns and `stdin_contains` match the call.
    pub fn matches(&self, argv: &[String], stdin: Option<&str>) -> bool {
        let stdin_ok = match &self.stdin_contains {
            None => true,
            Some(needle) => stdin.is_some_and(|stdin| stdin.contains(needle.as_str())),
        };
        stdin_ok && argv_matches(&self.argv, argv)
    }

    /// The bytes written to stdout: a string as-is, any other JSON value as compact JSON plus a
    /// newline (as `gh --json` prints).
    pub fn stdout_bytes(&self) -> Vec<u8> {
        match &self.stdout {
            None => Vec::new(),
            Some(Value::String(text)) => text.clone().into_bytes(),
            Some(value) => format!("{value}\n").into_bytes(),
        }
    }
}

fn argv_matches(patterns: &[Pattern], argv: &[String]) -> bool {
    match (patterns.split_first(), argv.split_first()) {
        (Some((Pattern::Rest, _)), _) => true,
        (None, None) => true,
        (Some((Pattern::One, patterns)), Some((_, argv))) => argv_matches(patterns, argv),
        (Some((Pattern::Exact(want), patterns)), Some((arg, argv))) => {
            want == arg && argv_matches(patterns, argv)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    fn entry(argv: &[&str]) -> Entry {
        Entry::from_raw(RawEntry {
            argv: argv.iter().map(|arg| arg.to_string()).collect(),
            stdin_contains: None,
            stdout: None,
            stderr: None,
            exit: 0,
            times: None,
            delay_ms: 0,
        })
        .unwrap()
    }

    #[test]
    fn patterns_match_exact_arguments_one_wildcard_and_the_rest() {
        let exact = entry(&["pr", "view", "482"]);
        assert!(exact.matches(&args("pr view 482"), None));
        assert!(!exact.matches(&args("pr view 483"), None));
        assert!(!exact.matches(&args("pr view"), None));
        assert!(!exact.matches(&args("pr view 482 --json"), None));

        let one = entry(&["pr", "view", "*", "--json", "*"]);
        assert!(one.matches(&args("pr view 9 --json number,title"), None));
        assert!(!one.matches(&args("pr view 9 --json"), None));
        assert!(!one.matches(&args("pr view 9 --jq number"), None));

        let rest = entry(&["api", "**"]);
        assert!(rest.matches(&args("api"), None));
        assert!(rest.matches(&args("api repos/o/r/pulls --paginate"), None));
        assert!(!rest.matches(&args("pr list"), None));

        assert!(entry(&["**"]).matches(&[], None));
        assert!(entry(&[]).matches(&[], None));
        assert!(!entry(&[]).matches(&args("auth"), None));
    }

    #[test]
    fn stdin_contains_needs_stdin_holding_the_text() {
        let mut needs = entry(&["api", "**"]);
        needs.stdin_contains = Some("\"event\":\"APPROVE\"".into());
        let argv = args("api --input -");
        assert!(needs.matches(&argv, Some("{\"event\":\"APPROVE\",\"body\":\"\"}")));
        assert!(!needs.matches(&argv, Some("{\"event\":\"COMMENT\"}")));
        assert!(!needs.matches(&argv, None));
    }

    #[test]
    fn the_first_matching_entry_with_calls_left_answers() {
        let scenario = Scenario::parse(
            r#"{"calls": [
                {"argv": ["pr", "view", "*"], "stdout": "first", "times": 2},
                {"argv": ["pr", "**"], "stdout": "fallback"},
                {"argv": ["pr", "view", "1"], "stdout": "never reached"}
            ]}"#,
        )
        .unwrap();
        let argv = args("pr view 1");
        assert_eq!(scenario.pick(&argv, None, |_| 0), Some(0));
        assert_eq!(scenario.pick(&argv, None, |_| 1), Some(0));
        assert_eq!(
            scenario.pick(&argv, None, |index| if index == 0 { 2 } else { 0 }),
            Some(1)
        );
        assert_eq!(scenario.pick(&args("auth status"), None, |_| 0), None);
    }

    #[test]
    fn times_zero_never_answers() {
        let scenario = Scenario::parse(r#"{"calls": [{"argv": ["**"], "times": 0}]}"#).unwrap();
        assert_eq!(scenario.pick(&args("anything"), None, |_| 0), None);
    }

    #[test]
    fn stdout_is_a_string_as_is_or_json_with_a_newline() {
        let scenario = Scenario::parse(
            r#"{"calls": [
                {"argv": ["a"], "stdout": "raw text"},
                {"argv": ["b"], "stdout": {"number": 482, "title": "T"}},
                {"argv": ["c"]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(scenario.calls[0].stdout_bytes(), b"raw text");
        assert_eq!(
            String::from_utf8(scenario.calls[1].stdout_bytes()).unwrap(),
            "{\"number\":482,\"title\":\"T\"}\n"
        );
        assert_eq!(scenario.calls[2].stdout_bytes(), b"");
        assert_eq!(scenario.calls[2].exit, 0);
    }

    #[test]
    fn bad_scenarios_are_rejected_with_the_entry() {
        for (text, wanted) in [
            (r#"{"calls": [{"argv": ["**", "x"]}]}"#, "must be last"),
            (
                r#"{"calls": [{"argv": ["a"], "exitt": 1}]}"#,
                "unknown field",
            ),
            (r#"{"calls": [{"argv": ["a"], "exit": 256}]}"#, "256"),
            (r#"{"calls": [{"argv": ["a"], "times": -1}]}"#, "-1"),
            (r#"{"call": []}"#, "unknown field"),
            ("not json", "expected"),
        ] {
            let err = format!("{:#}", Scenario::parse(text).unwrap_err());
            assert!(err.contains(wanted), "{text}: {err}");
        }
        let err = format!(
            "{:#}",
            Scenario::parse(r#"{"calls": [{"argv": ["a"]}, {"argv": ["**", "b"]}]}"#).unwrap_err()
        );
        assert!(err.contains("calls[1]"), "{err}");
    }
}
