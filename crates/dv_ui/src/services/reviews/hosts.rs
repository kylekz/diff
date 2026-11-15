//! What `gh` said about which hosts are GitHub's, remembered for a little while (PLAN §9.1,
//! §9.9): a repository's GitHub name is read beside every listing of it, and for a host that
//! isn't plainly GitHub's that read asks `gh auth status --hostname=<host>`, whose "no" `gh`'s
//! runner never remembers (signing in later must be seen). A known repository on another
//! forge would cost one `gh` process per listing; here its answer stands for [`REMEMBERED`],
//! so a menu opened again and again asks once a minute at most, and a `gh auth login` is
//! still seen a minute later.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long an answer about a host stands.
pub const REMEMBERED: Duration = Duration::from_secs(60);

/// The answers remembered, by host.
#[derive(Debug, Default)]
pub struct HostAnswers(Mutex<HashMap<String, (bool, Instant)>>);

impl HostAnswers {
    /// Whether `host` is GitHub's: the answer `ask` gave within [`REMEMBERED`] before
    /// `now`, else what it gives now.
    pub fn is_github(&self, host: &str, now: Instant, ask: impl FnOnce() -> bool) -> bool {
        let remembered = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(host)
            .copied();
        match remembered {
            Some((answer, at)) if now.saturating_duration_since(at) < REMEMBERED => answer,
            _ => {
                // Asked without the lock: `gh` may take its whole timeout to answer.
                let answer = ask();
                self.0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(host.to_string(), (answer, now));
                answer
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_stands_for_a_minute_then_is_asked_again() {
        let answers = HostAnswers::default();
        let start = Instant::now();
        let mut asked = 0;
        let mut is_github = |host: &str, after: Duration, says: bool| {
            answers.is_github(host, start + after, || {
                asked += 1;
                says
            })
        };
        assert!(!is_github("git.example", Duration::ZERO, false));
        // Within the minute the answer stands, whatever gh would say now.
        assert!(!is_github("git.example", Duration::from_secs(59), true));
        // Another host is another question.
        assert!(is_github("ghe.example", Duration::from_secs(1), true));
        // A minute on, gh is asked again: signing in meanwhile is seen.
        assert!(is_github("git.example", Duration::from_secs(60), true));
        assert!(is_github("git.example", Duration::from_secs(61), false));
        assert_eq!(asked, 3);
    }
}
