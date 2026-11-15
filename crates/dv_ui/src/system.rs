//! What dv asks of the operating system beyond its own windows (PLAN §11 `System`): opening a web
//! page, showing a file in its folder, the folder picker and writing the clipboard.
//!
//! Views and services call these rather than gpui's `App::open_url`, `reveal_path`,
//! `prompt_for_paths` and `write_to_clipboard`, so a scripted run never opens anything outside
//! dv and never replaces what the person at the machine has on their clipboard. The app installs
//! [`SystemShell::Real`]; automation installs [`SystemShell::Scripted`], which opens nothing:
//! every request is recorded for the `state` document ([`requests`]), and the folder picker is
//! answered with the next answer queued ([`queue_answer`]), or as if cancelled when none was.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use gpui::{App, AppContext as _, ClipboardItem, Global, PathPromptOptions, SharedString, Task};

/// How many requests a scripted shell remembers.
const LOG_LEN: usize = 50;

/// One thing asked of the system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemRequest {
    /// A web page opened in the browser.
    OpenUrl(String),
    /// A file or folder shown selected in its folder.
    Reveal(PathBuf),
    /// The folder picker, with its button's label.
    PickFolder { prompt: SharedString },
    /// Text written to the clipboard ("Copy as prompt", "Copy path", "Copy diagnostics").
    Clipboard(String),
}

impl SystemRequest {
    /// `"open_url"`, `"reveal"`, `"pick_folder"` or `"clipboard"`, as automation state spells
    /// it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::OpenUrl(_) => "open_url",
            Self::Reveal(_) => "reveal",
            Self::PickFolder { .. } => "pick_folder",
            Self::Clipboard(_) => "clipboard",
        }
    }
}

/// Where requests go.
pub enum SystemShell {
    /// The operating system's own.
    Real,
    /// Nowhere: recorded, the folder picker answered from a queue.
    Scripted {
        requests: VecDeque<SystemRequest>,
        /// Answers for the next pickers, oldest first: the folder chosen, or `None` for a
        /// cancelled one.
        answers: VecDeque<Option<PathBuf>>,
    },
}

impl Global for SystemShell {}

impl SystemShell {
    pub fn scripted() -> Self {
        Self::Scripted {
            requests: VecDeque::new(),
            answers: VecDeque::new(),
        }
    }
}

/// Installs `shell` for the app.
pub fn install(shell: SystemShell, cx: &mut App) {
    cx.set_global(shell);
}

/// Whether requests are scripted (an automation run).
pub fn is_scripted(cx: &App) -> bool {
    matches!(
        cx.try_global::<SystemShell>(),
        Some(SystemShell::Scripted { .. })
    )
}

/// Queues the answer to the next folder picker of a scripted shell: the folder chosen, or
/// `None` for a cancelled one. Refused unless the shell is scripted.
pub fn queue_answer(answer: Option<PathBuf>, cx: &mut App) -> anyhow::Result<()> {
    if !is_scripted(cx) {
        anyhow::bail!("picker answers can only be queued in an automation run");
    }
    if let SystemShell::Scripted { answers, .. } = cx.global_mut::<SystemShell>() {
        answers.push_back(answer);
    }
    Ok(())
}

/// What a scripted shell was asked, oldest first, and how many queued answers are left.
pub fn requests(cx: &App) -> (Vec<SystemRequest>, usize) {
    match cx.try_global::<SystemShell>() {
        Some(SystemShell::Scripted { requests, answers }) => {
            (requests.iter().cloned().collect(), answers.len())
        }
        _ => (Vec::new(), 0),
    }
}

/// Records `request` when scripted: `Some(answer)` (the next queued one, else a cancel) for the
/// picker, `Some(None)` otherwise. `None` when the shell is the real one.
fn scripted(request: SystemRequest, cx: &mut App) -> Option<Option<PathBuf>> {
    if !is_scripted(cx) {
        return None;
    }
    let SystemShell::Scripted { requests, answers } = cx.global_mut::<SystemShell>() else {
        return None;
    };
    let picker = matches!(request, SystemRequest::PickFolder { .. });
    log::info!("automation: {} (not opened)", request.kind());
    if requests.len() == LOG_LEN {
        requests.pop_front();
    }
    requests.push_back(request);
    Some(if picker {
        answers.pop_front().flatten()
    } else {
        None
    })
}

/// Whether `url` is a web address: `http` or `https`, with a host. Links reach dv from GitHub
/// and from comment bodies, and the shell would hand anything else (a `file:` or UNC path, an
/// `ms-…:` protocol handler) to whatever program claims it.
///
/// And its authority, everything between `//` and the first `/`, `?` or `#`, holds only
/// the characters RFC 3986 gives an authority ([`is_authority_char`]). Programs don't agree
/// on what an address with anything else there names: after `https://github.com\@evil.example/`
/// a browser, Windows and Linux read `\` as `/` and go to github.com, macOS's `NSURL` reads
/// it as part of a login and goes to evil.example. dv doesn't choose between them: such an
/// address is no web address here, so nothing says which host it has and nothing opens it,
/// on any platform. That holds for a backslash, a space, a tab, a line break, and a host
/// written in another script than ASCII (which opens once it is spelled as the browser's
/// address bar spells it, `xn--…`). And for brackets anywhere but around one whole IP
/// literal that is the host ([`brackets_hold_the_host`]): `https://[github.com]/` and
/// `https://evil.example@[github.com]/` name no host two programs agree on either.
pub fn is_web_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.trim().split_once(':') else {
        return false;
    };
    let Some(address) = rest.strip_prefix("//") else {
        return false;
    };
    let authority = address.split(['/', '?', '#']).next().unwrap_or_default();
    (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
        && !authority.is_empty()
        && authority.chars().all(is_authority_char)
        && brackets_hold_the_host(authority)
}

/// Whether the brackets in `authority`, if it has any, are those of one IP literal that is
/// its whole host (RFC 3986 §3.2.2): after the last `@`, `[`, hexadecimal digits with `:`
/// and `.`, `]`, and at most a port, `:` and digits. No bracket stands anywhere else.
fn brackets_hold_the_host(authority: &str) -> bool {
    let (login, host) = match authority.rsplit_once('@') {
        Some((login, host)) => (login, host),
        None => ("", authority),
    };
    let literal = host.strip_prefix('[').and_then(|rest| rest.split_once(']'));
    !login.contains(['[', ']'])
        && match literal {
            Some((address, port)) => {
                !address.is_empty()
                    && address
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.'))
                    && (port.is_empty()
                        || port
                            .strip_prefix(':')
                            .is_some_and(|port| port.chars().all(|c| c.is_ascii_digit())))
            }
            None => !host.contains(['[', ']']),
        }
}

/// Whether `c` may stand in a URL's authority (RFC 3986 §3.2): an unreserved character, a
/// percent-encoding's `%`, a sub-delimiter, the `:` before a port, the `@` after a login
/// and the brackets of an IP literal.
fn is_authority_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | '%'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | ':'
                | '@'
                | '['
                | ']'
        )
}

/// Opens `url` in the browser. Anything but a web address is refused and logged, never handed
/// to the shell ([`is_web_url`]).
pub fn open_url(url: &str, cx: &mut App) {
    let url = url.trim();
    if !is_web_url(url) {
        log::warn!("not opening {url:?}: only web addresses are opened");
        return;
    }
    if scripted(SystemRequest::OpenUrl(url.to_string()), cx).is_none() {
        cx.open_url(url);
    }
}

/// Shows `path` selected in its folder (Explorer, Finder, the file manager).
pub fn reveal(path: &Path, cx: &mut App) {
    if scripted(SystemRequest::Reveal(path.to_path_buf()), cx).is_none() {
        cx.reveal_path(path);
    }
}

/// Puts `text` on the clipboard.
pub fn write_clipboard(text: impl Into<String>, cx: &mut App) {
    let text = text.into();
    if scripted(SystemRequest::Clipboard(text.clone()), cx).is_none() {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
}

/// The folder picker: the folder chosen, `None` when cancelled.
pub fn pick_folder(
    prompt: impl Into<SharedString>,
    cx: &mut App,
) -> Task<anyhow::Result<Option<PathBuf>>> {
    let prompt = prompt.into();
    if let Some(answer) = scripted(
        SystemRequest::PickFolder {
            prompt: prompt.clone(),
        },
        cx,
    ) {
        return Task::ready(Ok(answer));
    }
    let chosen = cx.prompt_for_paths(PathPromptOptions {
        files: false,
        directories: true,
        multiple: false,
        prompt: Some(prompt),
    });
    cx.background_spawn(
        async move { Ok(chosen.await??.and_then(|paths| paths.into_iter().next())) },
    )
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    use super::*;

    #[gpui::test]
    fn a_scripted_shell_records_and_answers_from_its_queue(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(SystemShell::scripted(), cx);
            assert!(is_scripted(cx));
            queue_answer(Some(PathBuf::from("D:/repos/api")), cx).unwrap();
            reveal(Path::new("D:/repos/api/src/main.rs"), cx);
            open_url("https://github.com/kylekz/difftest/pull/1", cx);
            write_clipboard("src/main.rs:12", cx);
        });
        let first = cx.update(|cx| pick_folder("Open repository", cx));
        assert_eq!(
            futures::executor::block_on(first).unwrap(),
            Some(PathBuf::from("D:/repos/api"))
        );
        // Nothing queued: as if cancelled.
        let nothing = cx.update(|cx| pick_folder("Open repository", cx));
        assert_eq!(futures::executor::block_on(nothing).unwrap(), None);
        cx.update(|cx| {
            let (requests, left) = requests(cx);
            assert_eq!(left, 0);
            let kinds: Vec<&str> = requests.iter().map(SystemRequest::kind).collect();
            assert_eq!(
                kinds,
                [
                    "reveal",
                    "open_url",
                    "clipboard",
                    "pick_folder",
                    "pick_folder"
                ]
            );
            assert_eq!(
                requests[2],
                SystemRequest::Clipboard("src/main.rs:12".into())
            );
        });
        assert!(!cx.did_prompt_for_paths());
        assert_eq!(
            cx.read_from_clipboard(),
            None,
            "a scripted run never writes the clipboard"
        );
    }

    #[test]
    fn only_web_addresses_are_opened() {
        for url in [
            "https://github.com/kylekz/difftest/pull/1",
            "http://example.com",
            " HTTPS://Example.com/a?b ",
            "https://user:pw@example.com:8443/x",
            "http://[2001:db8::1]:8080/",
            "http://user@[::ffff:192.0.2.1]/",
            "https://xn--80a1acny.example/",
            // Whatever the path holds: only the authority says where it goes.
            "https://example.com/a\\b c\u{202e}d",
        ] {
            assert!(is_web_url(url), "{url}");
        }
        for url in [
            "file:///C:/Windows/System32/calc.exe",
            "\\\\server\\share\\tool.exe",
            "//server/share",
            "http:///C:/x",
            "https:\\\\server\\share",
            "http:",
            "ms-settings:privacy",
            "mailto:someone@example.com",
            "javascript:alert(1)",
            "",
            // An authority that programs read two ways: after the backslash macOS goes to
            // evil.example, a browser to github.com.
            "https://github.com\\@evil.example/login",
            "https://github.com\\evil.example/login",
            // Brackets that aren't one IP literal standing as the host.
            "https://[github.com]/",
            "https://evil.example@[github.com]/",
            "https://[::1]@evil.example/",
            "https://[::1]evil.example/",
            "https://evil.example[::1]/",
            "https://[::1]:80a/",
            "https://[]/",
            "https://[::1/",
            "https://github.com\t.evil.example/",
            "https://github.com .evil.example/",
            "https://github.com\r\n.evil.example/",
            // A host in another script, or with a slash that isn't `/`.
            "https://p\u{430}ypal.com/",
            "https://github.com\u{2215}kylekz@evil.example/",
            "https://exa\u{202e}mple.com/",
            "https://",
            "https:///x",
        ] {
            assert!(!is_web_url(url), "{url:?}");
        }
    }

    /// The same string a browser and macOS would take to two hosts is opened by neither.
    #[gpui::test]
    fn an_address_two_programs_read_two_ways_is_never_opened(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(SystemShell::scripted(), cx);
            open_url("https://github.com\\@evil.example/login", cx);
            open_url(" https://github.com/login ", cx);
            assert_eq!(
                requests(cx).0,
                [SystemRequest::OpenUrl("https://github.com/login".into())]
            );
        });
    }

    #[gpui::test]
    fn a_link_that_isnt_a_web_address_is_never_opened(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(SystemShell::scripted(), cx);
            open_url("file:///C:/Windows/System32/calc.exe", cx);
            open_url("https://example.com/source", cx);
            let (requests, _) = requests(cx);
            assert_eq!(
                requests,
                [SystemRequest::OpenUrl("https://example.com/source".into())]
            );
        });
    }

    #[gpui::test]
    fn the_real_shell_writes_the_clipboard(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install(SystemShell::Real, cx);
            write_clipboard("copied", cx);
            assert_eq!(requests(cx), (Vec::new(), 0));
        });
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("copied".to_string())
        );
    }

    #[gpui::test]
    fn answers_are_refused_without_automation(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(!is_scripted(cx));
            assert!(queue_answer(None, cx).is_err());
            install(SystemShell::Real, cx);
            assert!(queue_answer(None, cx).is_err());
            assert_eq!(requests(cx), (Vec::new(), 0));
        });
    }
}
