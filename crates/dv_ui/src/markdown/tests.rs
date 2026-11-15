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

/// After sanitising, nothing `TextView` would fetch or render as markup is left.
fn assert_inert(sanitized: &str) {
    if let Some(node) = fetchable(sanitized) {
        panic!("sanitised output still has a fetchable node: {node}\nin {sanitized:?}");
    }
}

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
    let source = "## Title\n\nsome *emphasis*, a [link](https://example.com), and\n\n- a list\n";
    assert_eq!(sanitize_untrusted(source), source);
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
