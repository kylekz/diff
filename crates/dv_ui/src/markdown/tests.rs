//! The sanitiser's guarantee (no construct that makes `TextView` fetch a URL or render markup
//! survives; code stays verbatim) and the parser's tolerance of hostile input, headless: the
//! same `markdown::to_mdast` call `TextView` makes before rendering anything.

use markdown::mdast::Node;
use markdown::{ParseOptions, to_mdast};

use super::{coarse_escape, sanitize_untrusted, with_line_breaks};

fn parse(source: &str) -> Result<Node, markdown::message::Message> {
    to_mdast(source, &ParseOptions::gfm())
}

/// The options `TextView` parses a body with (gpui-base's
/// `MarkdownExtensions::parse_options` with nothing registered): GFM, with inline and block
/// math.
fn view_options() -> ParseOptions {
    let mut options = ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options
}

/// Whether `text`, read as `TextView` reads a body, holds an `Image`, `ImageReference` or
/// `Html` node anywhere: exactly the nodes it turns into fetches or markup. As `TextView`
/// does (gpui-base's `flatten_unclaimed_math`), the source of a math node is read again
/// without math, as the prose it shows it as.
fn fetchable(text: &str) -> Option<String> {
    fn walk(node: &Node, source: &str) -> Option<String> {
        match node {
            Node::Image(_) | Node::ImageReference(_) | Node::Html(_) => Some(format!("{node:?}")),
            Node::InlineMath(_) | Node::Math(_) => {
                let position = node.position()?;
                let literal = &source[position.start.offset..position.end.offset];
                let prose = to_mdast(literal, &ParseOptions::gfm()).expect("math reads as prose");
                walk(&prose, literal)
            }
            other => other
                .children()
                .into_iter()
                .flatten()
                .find_map(|child| walk(child, source)),
        }
    }
    walk(
        &to_mdast(text, &view_options()).expect("sanitised output parses"),
        text,
    )
}

/// After sanitising, nothing `TextView` would fetch or render as markup is left, and
/// nothing it draws reorders or hides text.
fn assert_inert(sanitized: &str) {
    if let Some(node) = fetchable(sanitized) {
        panic!("sanitised output still has a fetchable node: {node}\nin {sanitized:?}");
    }
    assert_reads_as_written(sanitized);
}

/// Everything `text` reads as, as `TextView` reads a body: every node's own text and what
/// it carries beside it (a link's address and title, a code block's language), a math
/// node's source read again as prose.
fn read_out(text: &str, options: &ParseOptions) -> String {
    fn walk(node: &Node, source: &str, out: &mut String) {
        match node {
            Node::Text(text) => out.push_str(&text.value),
            Node::InlineCode(code) => out.push_str(&code.value),
            Node::Code(code) => {
                out.push_str(&code.value);
                out.extend(code.lang.as_deref());
                out.extend(code.meta.as_deref());
            }
            Node::Link(link) => {
                out.push_str(&link.url);
                out.extend(link.title.as_deref());
            }
            Node::Definition(definition) => {
                out.push_str(&definition.url);
                out.extend(definition.title.as_deref());
                out.extend(definition.label.as_deref());
            }
            Node::LinkReference(reference) => out.extend(reference.label.as_deref()),
            Node::InlineMath(_) | Node::Math(_) => {
                let position = node.position().unwrap();
                let literal = &source[position.start.offset..position.end.offset];
                out.push_str(&read_out(literal, &ParseOptions::gfm()));
            }
            _ => {}
        }
        for child in node.children().into_iter().flatten() {
            walk(child, source, out);
        }
    }
    let mut out = String::new();
    walk(
        &to_mdast(text, options).expect("sanitised output parses"),
        text,
        &mut out,
    );
    out
}

/// PLAN §10.3: what a sanitised body reads as holds no explicit bidirectional control (an
/// embedding, an override or an isolate) and no tag character: the body's own were
/// spelled out, and none came out of a character reference.
fn assert_reads_as_written(sanitized: &str) {
    let read = read_out(sanitized, &view_options());
    let stray: Vec<char> = read
        .chars()
        .filter(|c| {
            matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{e0000}'..='\u{e007f}')
        })
        .collect();
    assert!(
        stray.is_empty(),
        "sanitised output reads as text holding {stray:?}\nin {sanitized:?}"
    );
}

/// Whether dv opens `url` at all (PLAN §11), decided here without the app's code: `http`
/// or `https`, between `//` and the path only what RFC 3986 gives an authority, and
/// brackets only around a whole IP literal that is the host, so no two programs read its
/// host differently.
fn opens(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let Some(address) = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
    else {
        return false;
    };
    let authority = address.split(['/', '?', '#']).next().unwrap();
    let host = authority.rsplit('@').next().unwrap();
    let bracketed = authority.bytes().filter(|b| b"[]".contains(b)).count();
    let literal = host.starts_with('[')
        && host[1..].find(']').is_some_and(|close| {
            let (inside, port) = (&host[1..=close], &host[close + 2..]);
            inside.len() > 1
                && inside[..close]
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() || b":.".contains(&b))
                && (port.is_empty()
                    || port
                        .strip_prefix(':')
                        .is_some_and(|port| port.bytes().all(|b| b.is_ascii_digit())))
        });
    !authority.is_empty()
        && authority
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~%!$&'()*+,;=:@[]".contains(&b))
        && (bracketed == 0 || (bracketed == 2 && literal))
}

/// Whether `text` holds a character that can draw a line in another order than it is
/// written with no explicit control (PLAN §11): one of a script written right to left,
/// one of its numbers, or a right-to-left mark. The ranges are written out here, not taken
/// from the sanitiser.
fn runs_right_to_left(text: &str) -> bool {
    text.chars().any(|c| {
        let c = u32::from(c);
        (0x0590..=0x08FF).contains(&c)
            || c == 0x200F
            || (0xFB1D..=0xFDFF).contains(&c)
            || (0xFE70..=0xFEFF).contains(&c)
            || (0x10800..=0x10FFF).contains(&c)
            || (0x1E800..=0x1EFFF).contains(&c)
    })
}

/// Every link to the web in `text`, read as `TextView` reads a body: its text (a hard
/// break in it as a line separator, U+2028), where it goes, what follows it, and whether
/// it stands in another link's text. A math node's source is read again as prose.
fn web_links(text: &str) -> Vec<(String, String, String, bool)> {
    web_links_with(text, &view_options(), false)
}

fn web_links_with(
    text: &str,
    options: &ParseOptions,
    in_link: bool,
) -> Vec<(String, String, String, bool)> {
    fn text_of(node: &Node, out: &mut String) {
        match node {
            Node::Text(text) => out.push_str(&text.value),
            Node::InlineCode(code) => out.push_str(&code.value),
            Node::Break(_) => out.push('\u{2028}'),
            other => {
                for child in other.children().into_iter().flatten() {
                    text_of(child, out);
                }
            }
        }
    }
    fn definitions(node: &Node, found: &mut Vec<(String, String)>) {
        if let Node::Definition(definition) = node {
            found.push((definition.identifier.clone(), definition.url.clone()));
        }
        for child in node.children().into_iter().flatten() {
            definitions(child, found);
        }
    }
    fn walk(
        node: &Node,
        source: &str,
        defined: &[(String, String)],
        in_link: bool,
        found: &mut Vec<(String, String, String, bool)>,
    ) {
        let url = match node {
            Node::Link(link) => Some(link.url.clone()),
            Node::LinkReference(reference) => defined
                .iter()
                .find(|(identifier, _)| *identifier == reference.identifier)
                .map(|(_, url)| url.clone()),
            Node::InlineMath(_) | Node::Math(_) => {
                let position = node.position().unwrap();
                let literal = &source[position.start.offset..position.end.offset];
                found.extend(web_links_with(literal, &ParseOptions::gfm(), in_link));
                return;
            }
            _ => None,
        };
        if let Some(url) = url
            && opens(&url)
        {
            let mut text = String::new();
            text_of(node, &mut text);
            let position = node.position().unwrap();
            let after = &source[position.end.offset..];
            found.push((text, url, after.to_string(), in_link));
        }
        let in_link = in_link || matches!(node, Node::Link(_) | Node::LinkReference(_));
        for child in node.children().into_iter().flatten() {
            walk(child, source, defined, in_link, found);
        }
    }
    let ast = to_mdast(text, options).expect("sanitised output parses");
    let (mut defined, mut found) = (Vec::new(), Vec::new());
    definitions(&ast, &mut defined);
    walk(&ast, text, &defined, in_link, &mut found);
    found
}

/// The rule of PLAN §11, checked here without the sanitiser's code: after sanitising,
/// every link to the web is followed by its address's host, whatever its text is; none
/// stands in another's text; its text is one line; and a body that holds one holds no
/// character that runs right to left, written or read out of a character reference. The
/// hosts these tests link to are read here; one that isn't plain must at least be
/// followed by a host dv wrote.
fn assert_links_named(sanitized: &str) {
    let links = web_links(sanitized);
    if !links.is_empty() {
        assert!(
            !runs_right_to_left(sanitized)
                && !runs_right_to_left(&read_out(sanitized, &view_options())),
            "a body with a link holds text that runs right to left: {sanitized:?}"
        );
    }
    for (text, url, after, in_link) in links {
        let address = url.split_once("://").unwrap().1;
        let authority = address.split(['/', '?', '#']).next().unwrap();
        let host = authority.rsplit('@').next().unwrap();
        let host = match host.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap(),
            None => host.split(':').next().unwrap(),
        };
        let plain = !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
        let host = host.to_ascii_lowercase().replace("www.", "www\\.");
        let expected = if plain {
            format!(" ({host})")
        } else {
            " (".to_string()
        };
        assert!(
            after.starts_with(&expected),
            "the link {text:?} to {url:?} is followed by {after:?}\nin {sanitized:?}"
        );
        assert!(
            !text.contains([
                '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}', '\u{2029}'
            ]),
            "the link {text:?} to {url:?} runs over a line in {sanitized:?}"
        );
        assert!(
            !in_link,
            "the link {text:?} to {url:?} stands in a link's text in {sanitized:?}"
        );
    }
}

/// Links that would pass for another site's, or hide where they go.
const LINKS: [&str; 9] = [
    "[github.com/login](https://evil.example/login)",
    "[https://github.com/login](https://evil.example/x)",
    "[the docs][d]\n\n[d]: https://evil.example/docs",
    "[`code`](https://github.com@evil.example:8443/p)",
    "[![alt](https://evil.example/p.png)](https://evil.example/q)",
    "[a](<https://evil.example/a b> \"t\")",
    "[github.com/login/](https://github.com/login/)",
    "[x&#x202E;y](https://evil.example/r)",
    RIGHT_TO_LEFT_LINK,
];

/// A link whose text is written right to left: no body with it keeps a link.
const RIGHT_TO_LEFT_LINK: &str = "[\u{5d0}\u{5d1}\u{5d2}](https://evil.example/h)";

/// Bodies that get an image past a sanitiser that parses once, with other options than
/// `TextView`'s, and never reads what it wrote.
const BYPASSES: [&str; 3] = [
    // Escaping the tag frees the backtick inside it, which then closes the code span that
    // held the image's tag.
    "<a title=\"`\"> x `<img src=\"https://evil.example/p.png\">` ",
    // Escaping the HTML block ends it, and the indented line after it is prose, not code.
    "<!-- x -->\n    ![x](https://evil.example/p.png)",
    // With math on, the dollars pair up and the backticks no longer hold the image.
    "$`$ ![x](https://evil.example/p.png) `",
];

#[test]
fn what_one_parse_misses_is_caught_by_reading_the_output_as_the_view_does() {
    for source in BYPASSES {
        assert_inert(&sanitize_untrusted(source));
    }
}

/// The three tricks nested in each other and in what surrounds a body, with other line
/// endings and indents.
#[test]
fn the_bypasses_stay_inert_however_they_are_combined() {
    let wraps: [fn(&str) -> String; 9] = [
        |body| body.to_string(),
        |body| body.replace('\n', "\r\n"),
        |body| body.replace("    ", "\t"),
        |body| format!("> {}", body.replace('\n', "\n> ")),
        |body| format!("- {}", body.replace('\n', "\n  ")),
        |body| format!("$ {body} $"),
        |body| format!("$$\n{body}\n$$\n\n{body}"),
        |body| format!("`` {body}"),
        |body| format!("<details>\n\n{body}\n\n</details>"),
    ];
    let mut count = 0;
    for first in BYPASSES {
        for second in BYPASSES {
            for joint in [" ", "\n", "\n\n", "` ", "$ "] {
                for wrap in wraps {
                    let source = wrap(&format!("{first}{joint}{second}"));
                    assert_inert(&sanitize_untrusted(&source));
                    count += 1;
                }
            }
        }
    }
    assert_eq!(count, 405);
}

/// Links beside the tricks, and beside each other, in the same surroundings: whatever
/// still reads as a link to the web says where it goes.
#[test]
fn links_say_where_they_go_however_they_are_combined() {
    let wraps: [fn(&str) -> String; 9] = [
        |body| body.to_string(),
        |body| body.replace('\n', "\r\n"),
        |body| body.replace("    ", "\t"),
        |body| format!("> {}", body.replace('\n', "\n> ")),
        |body| format!("- {}", body.replace('\n', "\n  ")),
        |body| format!("$ {body} $"),
        |body| format!("$$\n{body}\n$$\n\n{body}"),
        |body| format!("`` {body}"),
        |body| format!("<details>\n\n{body}\n\n</details>"),
    ];
    let (mut count, mut linked) = (0, 0);
    let others = BYPASSES.iter().chain(LINKS.iter());
    for link in LINKS {
        for other in others.clone() {
            for joint in [" ", "\n", "\n\n", "` ", "$ ", "\\"] {
                for wrap in wraps {
                    for source in [
                        wrap(&format!("{link}{joint}{other}")),
                        wrap(&format!("{other}{joint}{link}")),
                    ] {
                        let out = sanitize_untrusted(&source);
                        assert_inert(&out);
                        assert_links_named(&out);
                        count += 1;
                        let links = web_links(&out);
                        // Text that runs right to left beside a link: the body is drawn
                        // without Markdown.
                        if source.contains(RIGHT_TO_LEFT_LINK) {
                            assert_eq!(links, [], "{source:?} -> {out:?}");
                        }
                        linked += usize::from(!links.is_empty());
                    }
                }
            }
        }
    }
    assert_eq!(count, 11_664);
    // The sanitiser doesn't pass by escaping everything: of the 8,640 combinations
    // without text that runs right to left, most still hold a link, which neither fallback
    // leaves one of.
    assert!(linked >= 6_400, "{linked} of {count} still hold a link");
}

/// The fallback for a body that can't be edited safely is sound under any parse: no `<`
/// and no `![` survive it, so nothing in it can open a tag or an image, however a parser
/// reads the rest.
#[test]
fn the_coarse_escape_leaves_no_tag_and_no_image_syntax() {
    let hostile = [
        "<img src=x>",
        "![a](b) ![c][d] !![e](f) !\\![g](h)",
        "!<[x](y)",
        "<![CDATA[ ![x](y) ]]>",
        "&lt;img&gt; &#33;[x](y) &#60;b&#62;",
        "!\u{0}[x](y) !\n[x](y)",
        "<<<<![[[[",
    ];
    for source in hostile.iter().copied().chain(BYPASSES) {
        let out = coarse_escape(source);
        assert!(
            !out.contains('<') && !out.contains("!["),
            "{source:?} -> {out:?}"
        );
        assert_inert(&out);
    }
}

#[test]
fn hostile_input_never_panics() {
    for source in [
        "```rust\nfn main() {\n// never closed".to_string(),
        "*".repeat(500) + "text" + &"*".repeat(500),
        "a".repeat(200_000),
        "]()[ [[[]]] ()()( ![]( [text](".to_string(),
        "hello \0 world \u{7} \u{1b}[31m fake ansi".to_string(),
        "héllo 🎉 \u{200b} zero-width \u{feff} bom-in-middle".to_string(),
        String::new(),
        "   \n\n\t  ".to_string(),
    ] {
        let _ = parse(&source);
        assert_inert(&sanitize_untrusted(&source));
    }
}

#[test]
fn plain_markdown_passes_through_unchanged() {
    let source =
        "## Title\n\nsome *emphasis*, `https://example.com`, [top](#top) and\n\n- a list\n";
    assert_eq!(sanitize_untrusted(source), source);
}

/// DESIGN §4.5, PLAN §11: every link to the web is followed by the host it opens, whatever
/// its text is and whatever stands beside it.
#[test]
fn every_link_to_the_web_says_where_it_goes() {
    for (source, expected) in [
        (
            "see [the docs](https://docs.rs/gpui/latest) first",
            "see [the docs](https://docs.rs/gpui/latest) (docs.rs) first",
        ),
        // Text that reads as another site's address.
        (
            "[https://github.com/login](https://evil.example/login)",
            "[https\\:\\/\\/github\\.com\\/login](https://evil.example/login) (evil.example)",
        ),
        (
            "[github.com](https://github.com.evil.example)",
            "[github.com](https://github.com.evil.example) (github.com.evil.example)",
        ),
        // The login before the host and the port after it aren't the host.
        (
            "[sign in](https://github.com@evil.example:8443/x?y#z)",
            "[sign in](https://github.com@evil.example:8443/x?y#z) (evil.example)",
        ),
        (
            "[v6](http://[2001:db8::1]:8080/)",
            "[v6](http://[2001:db8::1]:8080/) (2001:db8::1)",
        ),
        // A character a host's name doesn't have is spelled out; capitals aren't another
        // host.
        (
            "[pay](https://paypal.com%2eevil.example/) [me](https://pay_pal.com/)",
            "[pay](https://paypal.com%2eevil.example/) (paypal.com{U+0025}2eevil.example) [me](https://pay_pal.com/) (pay{U+005F}pal.com)",
        ),
        (
            "[Docs](HTTPS://Docs.RS/x)",
            "[Docs](HTTPS://Docs.RS/x) (docs.rs)",
        ),
        // The host isn't a second link.
        (
            "[home](https://www.example.com/)",
            "[home](https://www.example.com/) (www\\.example.com)",
        ),
        // A reference says where its definition goes, in each of its forms.
        (
            "[the docs][d] and [d] and [d][]\n\n[d]: https://docs.rs/x",
            "[the docs][d] (docs.rs) and [d] (docs.rs) and [d][] (docs.rs)\n\n[d]: https://docs.rs/x",
        ),
        // After the edits inside the link, and before what follows it.
        (
            "[![logo](https://evil.example/p.png)](https://evil.example/)![x](y)",
            "[[image: logo]](https://evil.example/) (evil.example)[image: x]",
        ),
        // A link written as its own address says its host like any other: in angle
        // brackets, bare, by `www.`, with or without its scheme as its text, with a port,
        // with a login, an IPv6 literal.
        (
            "<https://example.com/a> https://example.com/b www.example.com \
             [example.com/c](https://example.com/c) [https://example.com/d](https://example.com/d)",
            "<https://example.com/a> (example.com) https://example.com/b (example.com) www.example.com (www\\.example.com) [example.com/c](https://example.com/c) (example.com) [https\\:\\/\\/example\\.com\\/d](https://example.com/d) (example.com)",
        ),
        (
            "<http://localhost:8080/x>",
            "<http://localhost:8080/x> (localhost)",
        ),
        (
            "<https://github.com@evil.example/>",
            "<https://github.com@evil.example/> (evil.example)",
        ),
        (
            "[github.com@evil.example](https://github.com@evil.example)",
            "[github.com@evil.example](https://github.com@evil.example) (evil.example)",
        ),
        (
            "<http://[2001:db8::1]:8080/> [http://[::1]/x](http://[::1]/x)",
            "<http://[2001:db8::1]:8080/> (2001:db8::1) [http://[::1]/x](http://[::1]/x) (::1)",
        ),
        // So links glued to each other, or to the text after them, can't read as one
        // address of another site: each half says its own host where it ends.
        (
            "[github.com/login/](https://github.com/login/)[evil.example/oauth](https://evil.example/oauth)",
            "[github.com/login/](https://github.com/login/) (github.com)[evil.example/oauth](https://evil.example/oauth) (evil.example)",
        ),
        (
            "[evil.example](https://evil.example).github.com/login",
            "[evil.example](https://evil.example) (evil.example).github.com/login",
        ),
        (
            "https://github.com/login?return_to=<https://evil.example/x>",
            "https://github.com/login?return_to= (github.com)<https://evil.example/x> (evil.example)",
        ),
        // What opens nothing has no host to say.
        (
            "[mail](mailto:a@example.com) [file](file:///etc/passwd) [here](#top) [rel](../x.md)",
            "[mail](mailto:a@example.com) [file](file:///etc/passwd) [here](#top) [rel](../x.md)",
        ),
        // Code is no link.
        (
            "`[the docs](https://evil.example)`",
            "`[the docs](https://evil.example)`",
        ),
        // An address whose authority two programs would read two ways opens nothing, so no
        // host is claimed for it (PLAN §11): a backslash (one in the destination, written
        // escaped or in an autolink), a slash that isn't `/`, a letter of another script, a
        // tab, brackets that aren't one IP literal standing as the host.
        (
            "[pay](https://p\u{430}ypal.com/)",
            "[pay](https://p\u{430}ypal.com/)",
        ),
        (
            "[Sign in](https://github.com\\\\@evil.example/login)",
            "[Sign in](https://github.com\\\\@evil.example/login)",
        ),
        (
            "<https://github.com\\@evil.example/login>",
            "<https://github.com\\@evil.example/login>",
        ),
        (
            "<https://github.com\u{2215}kylekz\u{2215}dv\u{2215}pull\u{2215}12@evil.example/>",
            "<https://github.com\u{2215}kylekz\u{2215}dv\u{2215}pull\u{2215}12@evil.example/>",
        ),
        ("<https://p\u{430}ypal.com/>", "<https://p\u{430}ypal.com/>"),
        (
            "[https://github.com&#9;.evil.example/](https://github.com&#9;.evil.example/)",
            "[https\\:\\/\\/github\\.com\\&\\#9\\;\\.evil\\.example\\/](https://github.com&#9;.evil.example/)",
        ),
        ("[x](https://[github.com]/)", "[x](https://[github.com]/)"),
        (
            "[x](https://evil.example@[github.com]/)",
            "[x](https://evil.example@[github.com]/)",
        ),
        // An address in a link's text, which GFM reads as a link too, is nothing a click
        // opens: the outer link's host is the one written.
        (
            "[https://evil.example/x&#9;](https://github.com/)",
            "[https\\:\\/\\/evil\\.example\\/x\\&\\#9;](https://github.com/) (github.com)",
        ),
        // A link's text is one line: a hard break in it is a line break that draws as a
        // space, so the host stands on the line the link starts on. In a quote too.
        (
            "[Sign in\\\nto GitHub](https://evil.example/login)",
            "[Sign in\nto GitHub](https://evil.example/login) (evil.example)",
        ),
        (
            "[Sign in  \r\nto GitHub](https://evil.example/login) and\\\nmore",
            "[Sign in\r\nto GitHub](https://evil.example/login) (evil.example) and\\\nmore",
        ),
        (
            "> [Sign in\\\n> to GitHub](https://evil.example/login)",
            "> [Sign in\n> to GitHub](https://evil.example/login) (evil.example)",
        ),
    ] {
        let out = sanitize_untrusted(source);
        assert_eq!(out, expected, "{source:?}");
        assert_inert(&out);
        assert_links_named(&out);
    }
}

/// PLAN §10.3, §11, DESIGN §4.5: a body's explicit bidirectional controls and tag
/// characters are spelled out before it is parsed, in prose and in code alike; and a body
/// that holds both a link to the web and text that runs right to left is drawn without
/// Markdown, so nothing draws a link's host anywhere but directly after its link. Each
/// row says how many links to the web its output holds.
#[test]
fn nothing_reorders_a_line_that_holds_a_link() {
    for (source, expected, links) in [
        // An override before a link: without it spelled out, the line and the host would
        // draw reversed, "(elpmaxe.live.github.com) See details".
        (
            "\u{202e}[sliated eeS](https://moc.buhtig.evil.example/login)",
            "\\u{202e}[sliated eeS](https://moc.buhtig.evil.example/login) (moc.buhtig.evil.example)",
            1,
        ),
        // An override after the host, to make what follows read as its end.
        (
            "[the style guide](https://evil.example/guide)\u{202e} :moc.buhtig ta",
            "[the style guide](https://evil.example/guide) (evil.example)\\u{202e} :moc.buhtig ta",
            1,
        ),
        // Every embedding, override and isolate, and a tag character; in code too.
        (
            "a\u{202a}b\u{202b}c\u{202c}d\u{202d}e\u{2066}f\u{2067}g\u{2068}h\u{2069}i\u{e0041}j `x\u{202e}y`\n\n```\n\u{2066}code\n```",
            "a\\u{202a}b\\u{202b}c\\u{202c}d\\u{202d}e\\u{2066}f\\u{2067}g\\u{2068}h\\u{2069}i\\u{e0041}j `x\\u{202e}y`\n\n```\n\\u{2066}code\n```",
            0,
        ),
        (
            "[x](https://evil.example/)\u{2069}\u{2066}",
            "[x](https://evil.example/) (evil.example)\\u{2069}\\u{2066}",
            1,
        ),
        // Two right-to-left marks make the paragraph run right to left, with a host of
        // the body's own choosing where dv's would stand.
        (
            "\u{200f}(github.com) \u{200f}[See details](https://evil.example/login)",
            "\u{200f}(github.com) \u{200f}\\[See details](https:\\/\\/evil.example/login)",
            0,
        ),
        // `TextView` shapes a code span, and each line after a hard break, on its own: a
        // mark inside the link's text turns only the piece after it.
        (
            "[`Sign in`\u{200f}](https://evil.example/login)\u{200f} (github.com)",
            "\\[`Sign in`\u{200f}](https:\\/\\/evil.example/login)\u{200f} (github.com)",
            0,
        ),
        (
            "[Sign in\\\n\u{200f}](https://evil.example/login)\u{200f} (github.com)",
            "\\[Sign in\\\\\n\u{200f}](https:\\/\\/evil.example/login)\u{200f} (github.com)",
            0,
        ),
        // Math in a link's text is drawn decoded: the marks come out of references.
        (
            "\u{200f}[$evil.example/&#x200F;#&#x200F;github.com/login/*x*$](https://evil.example/&amp;#x200F;#&amp;#x200F;github.com/login/*x*)",
            "\u{200f}\\[$evil.example/&amp;#x200F;#&amp;#x200F;github.com/login/*x*$](https:\\/\\/evil.example/&amp;amp;#x200F;#&amp;amp;#x200F;github.com/login/*x*)",
            0,
        ),
        (
            "[$a&#x200F;b$](https://evil.example/) and [x&#x5D0;y](https://evil.example/)",
            "\\[$a&amp;#x200F;b$](https:\\/\\/evil.example/) and \\[x&amp;#x5D0;y](https:\\/\\/evil.example/)",
            0,
        ),
        // A mark in the address alone, a number written right to left far from the link.
        (
            "[evil.example/\u{200f}/github.com/login](https://evil.example/\u{200f}/github.com/login)",
            "\\[evil.example/\u{200f}/github.com/login](https:\\/\\/evil.example/\u{200f}/github.com/login)",
            0,
        ),
        (
            "<https://evil.example/x>\n\n- \u{661}\u{662}\u{663}",
            "&lt;https:\\/\\/evil.example/x>\n\n- \u{661}\u{662}\u{663}",
            0,
        ),
        // Text written right to left around a link: plain text, the address included.
        (
            "\u{5e9}\u{5dc}\u{5d5}\u{5dd}\u{200f} [\u{5de}\u{5e1}\u{5de}\u{5da}](https://evil.example/doc)\n\tx\u{200d}y",
            "\u{5e9}\u{5dc}\u{5d5}\u{5dd}\u{200f} \\[\u{5de}\u{5e1}\u{5de}\u{5da}](https:\\/\\/evil.example/doc)\n\tx\u{200d}y",
            0,
        ),
        // Without a link to the web it keeps its Markdown, its marks and joiners, line
        // breaks and tabs.
        (
            "**\u{5e9}\u{5dc}\u{5d5}\u{5dd}**\u{200f} `\u{5de}\u{5e1}\u{5de}\u{5da}` [\u{5db}\u{5d0}\u{5df}](#top) [mail](mailto:a@example.com)\n\tx\u{200d}y",
            "**\u{5e9}\u{5dc}\u{5d5}\u{5dd}**\u{200f} `\u{5de}\u{5e1}\u{5de}\u{5da}` [\u{5db}\u{5d0}\u{5df}](#top) [mail](mailto:a@example.com)\n\tx\u{200d}y",
            0,
        ),
        // What ends a paragraph inside one piece of text may not stand in a link's text.
        (
            "[a\u{2028}b](https://evil.example/) [c&#x85;d](https://evil.example/)",
            "\\[a\u{2028}b](https:\\/\\/evil.example/) \\[c&amp;#x85;d](https:\\/\\/evil.example/)",
            0,
        ),
        (
            "[a\u{1c}b](https://evil.example/)",
            "\\[a\u{1c}b](https:\\/\\/evil.example/)",
            0,
        ),
        (
            "a\u{2029}b [c](https://evil.example/)",
            "a\u{2029}b [c](https://evil.example/) (evil.example)",
            1,
        ),
        // A character reference is the character once Markdown has read it: a body
        // holding one that reorders or hides text is escaped whole, the reference showing
        // as the text it is.
        (
            "&#x202E;[sliated eeS](https://moc.buhtig.evil.example/login)",
            "&amp;#x202E;\\[sliated eeS](https:\\/\\/moc.buhtig.evil.example/login)",
            0,
        ),
        (
            "a &#8238; b &#x2067; c &#xE0041; d and *more*",
            "a &amp;#8238; b &amp;#x2067; c &amp;#xE0041; d and *more*",
            0,
        ),
        (
            "[https://evil.example/&#x202E;moc.buhtig](https://evil.example/&#x202E;moc.buhtig)",
            "\\[https:\\/\\/evil.example/&amp;#x202E;moc.buhtig](https:\\/\\/evil.example/&amp;#x202E;moc.buhtig)",
            0,
        ),
        (
            "see [the docs](https://docs.rs/x \"a &#x202e;title\")",
            "see \\[the docs](https:\\/\\/docs.rs/x \"a &amp;#x202e;title\")",
            0,
        ),
        (
            "```rust&#x202e;\ncode\n```",
            "```rust&amp;#x202e;\ncode\n```",
            0,
        ),
        // An image's alt is written by dv: the same character in it is spelled out.
        (
            "![&#x202E;x](https://evil.example/p.png) and ![a\u{202e}b][i]\n\n[i]: https://evil.example/q.png",
            "[image: \\u{202e}x] and [image: a\\u{202e}b]\n\n[i]: https://evil.example/q.png",
            0,
        ),
    ] {
        let out = sanitize_untrusted(source);
        assert_eq!(out, expected, "{source:?}");
        assert_inert(&out);
        assert_links_named(&out);
        assert_eq!(web_links(&out).len(), links, "{source:?} -> {out:?}");
    }
}

/// gpui-base's `TextView` takes a text that starts with the one its state holds as an
/// append, and parses only the last block and what was added. Read that way, a tail can
/// hold other links than the whole body the sanitiser read: here a definition above makes
/// `[d]` the link in the whole body, and without it the outer brackets are one, to another
/// site, its text ending in the host dv wrote for `[d]`. So a view's state is keyed by its
/// text (`view_key`): a body that changed is parsed whole, in a state of its own.
#[test]
fn a_body_that_grew_is_another_view_since_its_tail_alone_reads_as_another_link() {
    let first = "[d]: https://github.com/\n\nPlease re-authenticate to see the logs.";
    let grown = format!("{first}\n\n[Sign in to GitHub [d]](https://evil.example/login)");
    let (shown_first, shown_grown) = (sanitize_untrusted(first), sanitize_untrusted(&grown));
    // The whole body: `[d]` is the link, to github.com, and says so; the outer brackets
    // are no link, and the address after them is one that reads as what it opens.
    assert_eq!(
        web_links(&shown_grown)
            .into_iter()
            .map(|(text, url, _, _)| (text, url))
            .collect::<Vec<_>>(),
        [
            ("d".to_string(), "https://github.com/".to_string()),
            (
                "https://evil.example/login".to_string(),
                "https://evil.example/login".to_string()
            )
        ]
    );
    assert_links_named(&shown_grown);
    // What `TextView` would parse of it as an append: the last block and the added text.
    let tail = shown_grown
        .strip_prefix(shown_first.as_str())
        .expect("the grown body starts with the first");
    let last_block = &shown_first[shown_first.rfind("\n\n").unwrap() + 2..];
    let appended = web_links(&format!("{last_block}{tail}"));
    assert_eq!(appended.len(), 1, "{appended:?}");
    assert_eq!(appended[0].1, "https://evil.example/login");
    assert!(appended[0].0.ends_with("(github.com)"), "{appended:?}");

    // So the two texts never share a state; the same text, and only that, keeps its own.
    let id = gpui::ElementId::Name("thread/1".into());
    let key = |text: &str| super::view_key(&id, "dark", text);
    assert_ne!(key(&shown_first), key(&shown_grown));
    assert_eq!(key(&shown_grown), key(&sanitize_untrusted(&grown)));
    assert_ne!(
        key(&shown_grown),
        super::view_key(&id, "light", &shown_grown)
    );
    let other = gpui::ElementId::Name("thread/2".into());
    assert_ne!(
        key(&shown_grown),
        super::view_key(&other, "dark", &shown_grown)
    );
}

/// A body escaped coarsely holds no link to the web at all, whatever backslashes it came
/// with and however `www.` is cased; and one drawn with no Markdown holds nothing at all.
#[test]
fn the_coarse_fallback_has_no_link_to_the_web() {
    for source in [
        "WWW.evil.example and Www.evil.example and wWw.evil.example",
        "see WWW.EVIL.EXAMPLE/login",
        "[the docs](https://evil.example)",
        "\\[the docs](https://evil.example)",
        "\\\\[the docs](https://evil.example)",
        "[the docs][d]\n\n[d]: https://evil.example",
        "<https://evil.example> https://evil.example www.evil.example",
        "[a](<https://evil.example/a b> \"t\")",
    ] {
        let out = coarse_escape(source);
        assert_eq!(
            web_links(&out),
            Vec::<(String, String, String, bool)>::new(),
            "{out:?}"
        );
        assert!(super::is_link_free(&out), "{out:?}");
        let plain = super::no_markdown(source);
        assert_eq!(
            web_links(&plain),
            Vec::<(String, String, String, bool)>::new(),
            "{plain:?}"
        );
        assert_inert(&plain);
    }
    // What the last resort draws is the body's own characters.
    let source = "# *a* <b> [c](https://d.e) `f` www.g.h 1. i_j\n> k";
    let drawn = to_mdast(&super::no_markdown(source), &view_options()).unwrap();
    assert_eq!(drawn.to_string(), source);
    // The check the fallback's output passes is one a body with a link fails.
    assert!(!super::is_link_free("see WWW.evil.example"));
    assert!(!super::is_link_free("[a][b]\n\n[b]: https://evil.example"));
    assert!(super::is_link_free("[a](mailto:a@b.c) and `https://x.y`"));
}

#[test]
fn an_inline_image_becomes_an_inert_placeholder() {
    let out = sanitize_untrusted("before ![tracking pixel](https://evil.example/p.png) after");
    assert!(!out.contains("evil.example"), "the URL is gone: {out}");
    assert!(out.contains("[image: tracking pixel]"), "{out}");
    assert!(
        out.starts_with("before ") && out.ends_with(" after"),
        "{out}"
    );
    assert_inert(&out);

    let out = sanitize_untrusted("x ![](https://evil.example/p.png) y");
    assert!(!out.contains("evil.example"));
    assert!(out.contains("[image]"), "{out}");
    assert_inert(&out);
}

#[test]
fn reference_images_of_every_form_are_neutralised() {
    let out = sanitize_untrusted("see ![alt text][pix]\n\n[pix]: https://evil.example/p.png\n");
    assert!(out.contains("[image: alt text]"), "{out}");
    assert_inert(&out);
    assert_inert(&sanitize_untrusted(
        "![pix][]\n\n[pix]: https://evil.example/a.png\n",
    ));
    assert_inert(&sanitize_untrusted(
        "![pix]\n\n[pix]: https://evil.example/a.png\n",
    ));
}

#[test]
fn inline_html_is_escaped_in_any_casing() {
    let out = sanitize_untrusted("a <img src=\"https://evil.example/p.png\"> b");
    assert!(out.contains("&lt;img"), "{out}");
    assert_inert(&out);
    let upper = sanitize_untrusted("a <IMG SRC=\"https://evil.example/p.png\"> b");
    assert!(upper.contains("&lt;IMG"), "{upper}");
    assert_inert(&upper);
}

#[test]
fn an_html_block_is_escaped_with_the_image_inside_it() {
    let source = "<div>\n<img src=\"https://evil.example/p.png\">\n![x](https://evil.example/q.png)\n</div>\n";
    assert_inert(&sanitize_untrusted(source));
}

#[test]
fn images_nested_in_lists_and_links_are_neutralised() {
    let out =
        sanitize_untrusted("- first\n- has ![pix](https://evil.example/p.png) inline\n- last");
    assert!(!out.contains("evil.example"));
    assert!(out.contains("- has [image: pix] inline"), "{out}");
    assert_inert(&out);

    let out = sanitize_untrusted("[![badge](https://evil.example/b.svg)](https://ok.example)");
    assert!(!out.contains("evil.example"));
    assert_inert(&out);

    let out = sanitize_untrusted(
        "![a](https://e.example/1.png)\n\ntext\n\n![b](https://e.example/2.png)",
    );
    assert!(!out.contains("e.example"));
    assert_inert(&out);
}

#[test]
fn code_stays_verbatim() {
    for source in [
        "intro\n\n```html\n![alt](https://example.com/x.png)\n<img src=\"y.png\">\n```\n\noutro",
        "use `![alt](url)` and `<img src=x>` literally",
        "para\n\n    ![alt](https://example.com/x.png)\n",
    ] {
        assert_eq!(sanitize_untrusted(source), source);
    }
}

#[test]
fn multibyte_text_before_an_image_keeps_the_edits_aligned() {
    let out = sanitize_untrusted("héllo 🎉 ![pïx](https://evil.example/p.png) done");
    assert!(!out.contains("evil.example"), "{out}");
    assert!(out.contains("héllo 🎉 "), "{out}");
    assert!(out.ends_with(" done"), "{out}");
    assert_inert(&out);
}

#[test]
fn alt_text_cannot_reopen_syntax_through_the_placeholder() {
    let out = sanitize_untrusted("![a<b>`c`[d]](https://evil.example/p.png)");
    assert!(!out.contains("evil.example"));
    assert_inert(&out);
}

#[test]
fn the_coarse_fallback_is_fetch_proof_too() {
    assert_inert(&coarse_escape(
        "<img src=\"https://e.example/x\"> ![a](https://e.example/y)",
    ));
}

#[test]
fn a_one_line_summary_reads_as_the_rendered_text() {
    for (body, line) in [
        (
            "add a sweep or **cap** the size.\nmore",
            "add a sweep or cap the size.",
        ),
        (
            "use `limit` _here_ ~~not there~~",
            "use limit here not there",
        ),
        (
            "see [the docs](https://example.com/d) first",
            "see the docs first",
        ),
        ("```ts\nconst x = 1;\n```\nafter", "const x = 1;"),
        ("## Heading *one*\n\nbody", "Heading one"),
        ("- [ ] first item\n- second", "first item"),
        ("> quoted **bit**", "quoted bit"),
        ("---\n\nafter the rule", "after the rule"),
        (
            "![shot](https://evil.example/p.png) look",
            "[image: shot] look",
        ),
        ("line one  \nline two", "line one"),
        ("| a | b |\n|---|---|\n| 1 | 2 |", "a | b"),
        ("  \n\n", ""),
        ("plain words", "plain words"),
    ] {
        assert_eq!(super::first_line_plain(body), line, "{body:?}");
    }
}

/// DESIGN §4.5: GitHub shows a line break inside a comment's paragraph as one.
#[test]
fn a_github_comments_line_breaks_are_hard_ones() {
    let breaks = |source: &str| with_line_breaks(source);
    assert_eq!(breaks("one\ntwo\nthree"), "one  \ntwo  \nthree");
    // Between paragraphs, and at the end, there is none to make.
    assert_eq!(breaks("one\n\ntwo\n"), "one\n\ntwo\n");
    // One that is hard already stays as it is; CRLF reads as a line break.
    assert_eq!(
        breaks("one  \ntwo\\\nthree\r\nfour"),
        "one  \ntwo\\\nthree  \nfour"
    );
    // Code keeps its lines as they are.
    let code = "before\n```rust\nlet a = 1;\nlet b = 2;\n```\nafter\nmore";
    assert_eq!(
        breaks(code),
        "before  \n```rust\nlet a = 1;\nlet b = 2;\n```\nafter  \nmore"
    );
    assert_eq!(breaks("~~~\na\nb\n~~~"), "~~~\na\nb\n~~~");
    assert_eq!(breaks(""), "");
    // A fence is closed only by its own character, at least as long: a three-backtick line
    // inside a four-backtick fence is code, and so is what follows it.
    let nested = "````md\n```rust\nlet a = 1;\n```\nstill code\n````\nafter\nmore";
    assert_eq!(
        breaks(nested),
        "````md\n```rust\nlet a = 1;\n```\nstill code\n````\nafter  \nmore"
    );
    assert_eq!(
        breaks("~~~\n```\na\n~~~~\nb\nc"),
        "~~~\n```\na\n~~~~\nb  \nc"
    );
    // A closing fence takes no info string.
    assert_eq!(
        breaks("```\na\n``` x\nb\n```\nc\nd"),
        "```\na\n``` x\nb\n```\nc  \nd"
    );
    // A line that starts with a code span opens nothing: its break is a paragraph's.
    assert_eq!(
        breaks("```x``` is a span\nnext line\nlast"),
        "```x``` is a span  \nnext line  \nlast"
    );
    // Indented code after a blank line keeps its lines; an indented continuation doesn't.
    assert_eq!(
        breaks("text\n\n    code one\n    code two\nback\nagain"),
        "text\n\n    code one\n    code two\nback  \nagain"
    );
    assert_eq!(
        breaks("text\n    continued\nmore"),
        "text  \n    continued  \nmore"
    );
    // A fence inside a quote, and one indented too far to be one.
    assert_eq!(
        breaks("> said\n> ```\n> a\n> b\n> ```\n> so\n> there"),
        "> said  \n> ```\n> a\n> b\n> ```\n> so  \n> there"
    );
}

/// An address inside raw HTML is not a link once the markup around it is escaped: an
/// automatic link would run on through the escaped tags after it.
#[test]
fn an_address_inside_raw_html_is_no_link() {
    fn links(node: &Node, found: &mut Vec<String>) {
        if let Node::Link(link) = node {
            found.push(link.url.clone());
        }
        for child in node.children().into_iter().flatten() {
            links(child, found);
        }
    }
    let source = "<details><summary>raw</summary><b>bold</b> \
                  <img src=\"https://example.com/t.gif\"></details> and www.example.org</p>\n\n\
                  A link of its own: https://example.org/y.";
    let out = sanitize_untrusted(source);
    assert_inert(&out);
    let mut found = Vec::new();
    links(&parse(&out).unwrap(), &mut found);
    // The one outside the markup stays a link; none runs into an escaped tag.
    assert_eq!(found, ["https://example.org/y"]);
}

/// How a body is drawn in a window for the tests below.
#[derive(Clone, Copy)]
enum Drawn {
    /// As a card draws one: through [`super::view`].
    AsACard,
    /// Sanitised, in a `TextView` keyed by the card alone.
    KeyedByItsCard,
    /// As written, in a `TextView` of its own: what the sanitiser is there to prevent.
    AsWritten,
}

struct Body {
    text: String,
    drawn: Drawn,
}

impl gpui::Render for Body {
    fn render(
        &mut self,
        _: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        use gpui::{InteractiveElement as _, ParentElement as _, Styled as _};
        use gpui_component::text::TextView;
        let opens = |view: TextView| {
            view.on_link_click(|url, _event, _window, cx| crate::system::open_url(url, cx))
        };
        let body = match self.drawn {
            Drawn::AsACard => super::view("card", &self.text, cx),
            Drawn::KeyedByItsCard => {
                opens(TextView::markdown("card", sanitize_untrusted(&self.text)))
            }
            Drawn::AsWritten => opens(TextView::markdown("card", self.text.clone())),
        };
        gpui::div().size_full().child(
            gpui::div()
                .debug_selector(|| "body".into())
                .w(gpui::px(900.))
                .child(body),
        )
    }
}

/// The addresses a click anywhere on the body opens, each once in the order met, after
/// the body is drawn first as `first` and then as `grown` under the same card.
fn opened_by_clicks(
    cx: &mut gpui::TestAppContext,
    drawn: Drawn,
    first: &str,
    grown: &str,
) -> Vec<String> {
    use gpui::AppContext as _;
    cx.executor().allow_parking();
    cx.update(|cx| {
        crate::init(cx).expect("dv_ui initialises");
        crate::system::install(crate::system::SystemShell::scripted(), cx);
    });
    let mut body = None;
    let window = cx.open_window(gpui::size(gpui::px(1000.), gpui::px(400.)), |window, cx| {
        let view = cx.new(|_| Body {
            text: first.to_string(),
            drawn,
        });
        body = Some(view.clone());
        gpui_component::Root::new(view, window, cx)
    });
    let body = body.expect("the window built its view");
    let cx = gpui::VisualTestContext::from_window(window.into(), cx).into_mut();
    let settle = |cx: &mut gpui::VisualTestContext| {
        // The view parses off the UI thread and draws what landed.
        for _ in 0..20 {
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };
    settle(cx);
    body.update(cx, |body, cx| {
        body.text = grown.to_string();
        cx.notify();
    });
    settle(cx);
    let bounds = cx.debug_bounds("body").expect("the body drew");
    let mut y = bounds.top() + gpui::px(4.);
    while y < bounds.bottom() {
        let mut x = bounds.left() + gpui::px(3.);
        while x < bounds.right() {
            cx.simulate_click(gpui::point(x, y), gpui::Modifiers::none());
            x += gpui::px(6.);
        }
        y += gpui::px(6.);
    }
    cx.update(|_, cx| {
        let mut opened: Vec<String> = Vec::new();
        for request in crate::system::requests(cx).0 {
            if let crate::system::SystemRequest::OpenUrl(url) = request
                && !opened.contains(&url)
            {
                opened.push(url);
            }
        }
        opened
    })
}

/// A comment that grew while its card was on screen, through `TextView` itself. Keyed by
/// the card alone, the view appends: its last block then holds a link to another site
/// than the whole body's. Keyed as [`super::view`] keys it, the body is parsed whole and
/// opens what the sanitiser read there and nothing else.
const GREW_FROM: &str = "[d]: https://github.com/\n\nPlease re-authenticate to see the logs.";
const GREW_BY: &str = "\n\n[Sign in to GitHub [d]](https:\\/\\/evil.example/login) or read \
                       [the logs](https://logs.example/run)";

#[gpui::test]
fn a_view_keyed_by_its_card_alone_would_append_and_link_elsewhere(cx: &mut gpui::TestAppContext) {
    let grown = format!("{GREW_FROM}{GREW_BY}");
    assert_eq!(
        opened_by_clicks(cx, Drawn::KeyedByItsCard, GREW_FROM, &grown),
        ["https://evil.example/login", "https://logs.example/run"]
    );
}

#[gpui::test]
fn a_body_that_grew_on_screen_is_parsed_whole(cx: &mut gpui::TestAppContext) {
    let grown = format!("{GREW_FROM}{GREW_BY}");
    // The whole body's links are `[d]`, a reference, which `TextView` opens nothing for,
    // and the one to the logs: the view drew it, and nothing else opens.
    assert_eq!(
        opened_by_clicks(cx, Drawn::AsACard, GREW_FROM, &grown),
        ["https://logs.example/run"]
    );
}

/// GFM reads an address in a link's text as a link of its own, and in `TextView` a click
/// on that text opens the inner address: the text of a link to one site can open another.
/// The sanitiser takes the inner link away, so the link opens what its host says.
const NESTED: &str = "[https://evil.example/x&#9;](https://github.com/)";

#[gpui::test]
fn an_address_in_a_links_text_would_open_itself(cx: &mut gpui::TestAppContext) {
    assert_eq!(
        opened_by_clicks(cx, Drawn::AsWritten, NESTED, NESTED),
        ["https://evil.example/x&#9", "https://github.com/"]
    );
}

#[gpui::test]
fn an_address_in_a_links_text_opens_the_links_own_address(cx: &mut gpui::TestAppContext) {
    let shown = sanitize_untrusted(NESTED);
    assert!(shown.ends_with(" (github.com)"), "{shown:?}");
    assert_eq!(
        web_links(&shown)
            .into_iter()
            .map(|(_, url, _, _)| url)
            .collect::<Vec<_>>(),
        ["https://github.com/"]
    );
    assert_eq!(
        opened_by_clicks(cx, Drawn::AsACard, NESTED, NESTED),
        ["https://github.com/"]
    );
}
