//! Markdown rendering for PR bodies and comment/reply bodies (docs/backlog.md:
//! "PR body/description renders markdown as raw text ... render markdown when
//! comment bodies get it").
//!
//! Delegates entirely to `gpui_component::text::TextView` rather than a
//! hand-rolled renderer — the pinned gpui-component commit already ships a
//! full Markdown-GFM `TextView`/`TextViewState` (headers, emphasis, inline
//! code, fenced code blocks with syntax highlighting, links, lists,
//! blockquotes, tables) built on the `markdown` crate (`markdown::to_mdast`,
//! which never panics on malformed input — it returns a `Result` — so
//! pathological source degrades to a parse error the TextView swallows
//! rather than a crash).
//!
//! Theming is automatic and requires no wiring on our part: `TextView`'s
//! renderer reads colors straight from `cx.theme()` (`gpui_component::Theme`,
//! the same global `themes::apply_theme` already drives for every other
//! widget in this app), and code-block syntax colors come from
//! `cx.theme().highlight_theme`, which `Theme::apply_config` populates from
//! each bundled theme JSON's `"highlight"` section (all four bundled themes
//! carry one). Link clicks route through `cx.open_url` inside the component
//! itself — the same affordance as this app's other external links.
//!
//! Every call site keeps its own `.text_sm()`/`.text_color(...)` chain on the
//! returned `TextView` exactly as it did on the `div()` it replaces: `TextView`
//! implements `gpui::Styled` and applies the refinement as the ambient text
//! style for any run that doesn't set its own color (plain paragraph text),
//! while explicit spans (links, highlighted code) keep their theme-derived
//! colors regardless.

use gpui::{App, ElementId, SharedString};
use gpui_component::text::{TextView, TextViewStyle};

/// Build a themed Markdown [`TextView`] for `text`, keyed by `id`.
///
/// `id` must be stable and unique per rendered body (e.g. derived from a
/// comment/reply id) so `TextView`'s internal per-element state — parsed AST,
/// selection, scroll — persists correctly across re-renders instead of
/// aliasing onto an unrelated body. The current
/// [`themes::markdown_theme_epoch`](crate::themes::markdown_theme_epoch) gets
/// folded into the id on top of that: `TextView` keys its `TextViewState` off
/// the element id (`gpui_component::text::TextView::request_layout`'s
/// `window.use_keyed_state`), so an id that only changes on a theme swap
/// forces a fresh state — and thus a reparse under the new theme's
/// `highlight_theme` — instead of `TextViewState::set_text`'s same-text
/// short-circuit leaving fenced-code syntax colors baked from the old theme
/// (docs/backlog.md "Markdown code blocks may not recolor on live theme
/// swap"). These bodies are short comment/PR text, so a reparse on theme
/// swap is cheap; the id is stable across every OTHER re-render, so
/// scroll/selection state isn't churned except on the swap itself.
///
/// Every body is passed through [`sanitize_untrusted`] first (capstone
/// P2-C): `TextView` renders `![](url)` images and raw `<img>` HTML by
/// FETCHING the URL, which hands any PR participant a tracking pixel
/// (reviewer IP + view time). All bodies rendered here are untrusted —
/// remote PR/comment content obviously, but local comment bodies can be
/// pasted from anywhere too — so the one choke point sanitizes them all.
///
/// A tighter `paragraph_gap` than the component's 1rem default keeps
/// multi-paragraph bodies visually consistent with this app's already-compact
/// thread cards (`gap_2`/`py_1` throughout `workspace.rs`'s card rendering).
pub(crate) fn view(id: impl Into<ElementId>, text: impl Into<SharedString>, cx: &App) -> TextView {
    let text: SharedString = text.into();
    let sanitized: SharedString = sanitize_untrusted(text.as_ref()).into();
    let epoch = crate::themes::markdown_theme_epoch(cx);
    let keyed_id = SharedString::from(format!("{}/theme-{epoch}", id.into()));
    TextView::markdown(keyed_id, sanitized)
        .style(TextViewStyle::default().paragraph_gap(gpui::rems(0.5)))
}

/// Neutralize the two constructs that make `TextView` issue NETWORK
/// requests (or inject arbitrary markup) from untrusted Markdown, before
/// the source ever reaches the component:
///
/// - **Images** (`![alt](url)`, reference-style `![alt][ref]`): replaced
///   with an inert literal `[image: alt]` / `[image]` placeholder — the
///   URL never survives into the rendered source, so nothing can be
///   fetched.
/// - **Raw HTML** (inline `<img src=…>` in any casing, and whole HTML
///   blocks): entity-escaped (`&` → `&amp;`, `<` → `&lt;`) so it renders
///   as visible literal text instead of being interpreted.
///
/// Implemented as precise source edits over `markdown::to_mdast` span
/// offsets — the SAME parser `TextView` itself uses — rather than regexes,
/// which is what makes it code-span-aware for free: text inside fenced
/// blocks and inline code spans lives under `Code`/`InlineCode` nodes (not
/// `Image`/`Html` ones), so `` `![x](y)` `` and fences containing those
/// literal strings pass through verbatim. Anything else (links, emphasis,
/// tables, lists, ...) is untouched. If the parse fails, or a reported
/// span doesn't line up with the source (defensive — offsets are byte
/// positions today), the whole body degrades to a coarse global escape
/// instead: uglier, but still fetch-proof, which is the property that
/// matters.
pub(crate) fn sanitize_untrusted(source: &str) -> String {
    let Ok(ast) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) else {
        return coarse_escape(source);
    };
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    if !collect_unsafe_spans(&ast, source, &mut edits) {
        return coarse_escape(source);
    }
    if edits.is_empty() {
        return source.to_string();
    }
    edits.sort_by_key(|&(start, _, _)| start);
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in edits {
        // Nested/overlapping spans can't happen (we never descend into a
        // replaced node), but guard anyway rather than double-emit.
        if start < cursor {
            continue;
        }
        out.push_str(&source[cursor..start]);
        out.push_str(&replacement);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    out
}

/// Walk the mdast collecting `(start, end, replacement)` byte-span edits
/// for every image/raw-HTML node. Returns `false` if any node's reported
/// span can't be mapped onto `source` (missing position, out of range, or
/// off a char boundary) — the caller falls back to [`coarse_escape`].
fn collect_unsafe_spans(
    node: &markdown::mdast::Node,
    source: &str,
    edits: &mut Vec<(usize, usize, String)>,
) -> bool {
    use markdown::mdast::Node;
    let span_of = |node: &Node| -> Option<(usize, usize)> {
        let pos = node.position()?;
        let (start, end) = (pos.start.offset, pos.end.offset);
        // Validate against the actual source before promising an edit.
        source.get(start..end).map(|_| (start, end))
    };
    match node {
        Node::Image(img) => {
            let Some((start, end)) = span_of(node) else {
                return false;
            };
            edits.push((start, end, image_placeholder(&img.alt)));
            true
        }
        Node::ImageReference(img) => {
            let Some((start, end)) = span_of(node) else {
                return false;
            };
            edits.push((start, end, image_placeholder(&img.alt)));
            true
        }
        Node::Html(_) => {
            let Some((start, end)) = span_of(node) else {
                return false;
            };
            edits.push((start, end, coarse_escape(&source[start..end])));
            true
        }
        other => match other.children() {
            Some(children) => children
                .iter()
                .all(|child| collect_unsafe_spans(child, source, edits)),
            None => true,
        },
    }
}

/// The inert literal an image is replaced with. The alt text is kept for
/// context but stripped of every character that could re-open Markdown or
/// HTML syntax when spliced back into the source.
fn image_placeholder(alt: &str) -> String {
    let clean: String = alt
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | '(' | ')' | '<' | '>' | '`' | '!' | '&'))
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        "[image]".to_string()
    } else {
        format!("[image: {clean}]")
    }
}

/// Entity-escape HTML (`&`/`<`) AND break image syntax (`![` → `!\[`) so
/// the result can only ever render as visible literal text. Applied to a
/// raw-HTML node's span (whose inner text becomes plain Markdown again
/// once the tags are escaped — an image nested inside an HTML block must
/// not spring back to life), and as the whole-body fallback when
/// span-precise editing isn't possible — coarse (code fences get escaped
/// too) but guaranteed fetch-proof.
fn coarse_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace("![", "!\\[")
}

#[cfg(test)]
mod tests {
    // Headless parser-structure + no-panic coverage for the exact library
    // `TextView` parses with (`markdown::to_mdast`, re-exported by
    // gpui-component as `gpui_component::text::markdown_ast`'s sibling
    // parse entry point). No `gpui::App`/window needed — this is pure
    // parsing, the same call `gpui-component`'s own `format/markdown.rs`
    // makes before any rendering happens.
    use markdown::{ParseOptions, to_mdast};

    fn parse(src: &str) -> Result<markdown::mdast::Node, markdown::message::Message> {
        to_mdast(src, &ParseOptions::gfm())
    }

    #[test]
    fn plain_paragraph_parses_to_a_root_with_one_paragraph() {
        let node = parse("hello world").expect("plain text must parse");
        let markdown::mdast::Node::Root(root) = &node else {
            panic!("expected a Root node");
        };
        assert_eq!(root.children.len(), 1);
        assert!(matches!(
            root.children[0],
            markdown::mdast::Node::Paragraph(_)
        ));
    }

    #[test]
    fn header_parses_to_a_heading_node() {
        let node = parse("## Section title").expect("heading must parse");
        let markdown::mdast::Node::Root(root) = &node else {
            panic!("expected a Root node");
        };
        let markdown::mdast::Node::Heading(heading) = &root.children[0] else {
            panic!("expected a Heading node, got {:?}", root.children[0]);
        };
        assert_eq!(heading.depth, 2);
    }

    #[test]
    fn fenced_code_block_parses_to_a_code_node() {
        let node = parse("```rust\nfn main() {}\n```").expect("fenced block must parse");
        let markdown::mdast::Node::Root(root) = &node else {
            panic!("expected a Root node");
        };
        assert!(matches!(root.children[0], markdown::mdast::Node::Code(_)));
    }

    #[test]
    fn link_parses_to_a_link_node() {
        let node = parse("see [dv](https://example.com/dv)").expect("link must parse");
        let markdown::mdast::Node::Root(root) = &node else {
            panic!("expected a Root node");
        };
        let markdown::mdast::Node::Paragraph(p) = &root.children[0] else {
            panic!("expected a Paragraph node");
        };
        assert!(
            p.children
                .iter()
                .any(|c| matches!(c, markdown::mdast::Node::Link(_)))
        );
    }

    #[test]
    fn blockquote_and_list_parse_without_panicking() {
        parse("> quoted text\n\n- one\n- two\n- three").expect("blockquote + list must parse");
    }

    // --- Pathological input: must never panic, whatever the parse result. ---

    #[test]
    fn unclosed_fence_does_not_panic() {
        let _ = parse("```rust\nfn main() {\n// never closed");
    }

    #[test]
    fn deeply_nested_emphasis_does_not_panic() {
        let nested = "*".repeat(500) + "text" + &"*".repeat(500);
        let _ = parse(&nested);
    }

    #[test]
    fn a_single_huge_line_does_not_panic() {
        let huge = "a".repeat(200_000);
        let _ = parse(&huge);
    }

    #[test]
    fn link_bracket_garbage_does_not_panic() {
        let _ = parse("]()[ [[[]]] ()()( ![]( [text](");
    }

    #[test]
    fn null_bytes_and_control_chars_do_not_panic() {
        let _ = parse("hello \0 world \u{7} \u{1b}[31m fake ansi");
    }

    #[test]
    fn unicode_and_emoji_do_not_panic() {
        let _ = parse("héllo 🎉 \u{200b} zero-width \u{feff} bom-in-middle");
    }

    #[test]
    fn empty_and_whitespace_only_input_parses() {
        assert!(parse("").is_ok());
        assert!(parse("   \n\n\t  ").is_ok());
    }

    // --- sanitize_untrusted (capstone P2-C): no construct that makes
    // TextView fetch a URL may survive; code spans stay verbatim. ---------

    use super::sanitize_untrusted;

    /// The property that actually matters: after sanitizing, re-parsing the
    /// result must yield NO Image/ImageReference/Html nodes anywhere —
    /// those are exactly the nodes TextView turns into fetches/markup.
    fn assert_inert(sanitized: &str) {
        fn walk(node: &markdown::mdast::Node) {
            use markdown::mdast::Node;
            match node {
                Node::Image(_) | Node::ImageReference(_) | Node::Html(_) => {
                    panic!("sanitized output still contains a fetchable node: {node:?}")
                }
                other => {
                    if let Some(children) = other.children() {
                        children.iter().for_each(walk);
                    }
                }
            }
        }
        let ast = to_mdast(sanitized, &ParseOptions::gfm()).expect("sanitized output must parse");
        walk(&ast);
    }

    #[test]
    fn plain_markdown_passes_through_unchanged() {
        let src = "## Title\n\nsome *emphasis*, a [link](https://example.com), and\n\n- a list\n";
        assert_eq!(sanitize_untrusted(src), src);
    }

    #[test]
    fn inline_image_is_replaced_with_an_inert_placeholder() {
        let out = sanitize_untrusted("before ![tracking pixel](https://evil.example/p.png) after");
        assert!(!out.contains("evil.example"), "URL must not survive: {out}");
        assert!(out.contains("[image: tracking pixel]"), "got: {out}");
        assert!(out.starts_with("before ") && out.ends_with(" after"));
        assert_inert(&out);
    }

    #[test]
    fn image_with_empty_alt_becomes_bare_placeholder() {
        let out = sanitize_untrusted("x ![](https://evil.example/p.png) y");
        assert!(!out.contains("evil.example"));
        assert!(out.contains("[image]"), "got: {out}");
        assert_inert(&out);
    }

    #[test]
    fn reference_style_image_is_neutralized() {
        let src = "see ![alt text][pix]\n\n[pix]: https://evil.example/p.png\n";
        let out = sanitize_untrusted(src);
        assert!(out.contains("[image: alt text]"), "got: {out}");
        assert_inert(&out);
    }

    #[test]
    fn collapsed_and_shortcut_reference_images_are_neutralized() {
        let out = sanitize_untrusted("![pix][]\n\n[pix]: https://evil.example/a.png\n");
        assert_inert(&out);
        let out = sanitize_untrusted("![pix]\n\n[pix]: https://evil.example/a.png\n");
        assert_inert(&out);
    }

    #[test]
    fn inline_html_img_is_escaped_to_literal_text_any_casing() {
        let out = sanitize_untrusted("a <img src=\"https://evil.example/p.png\"> b");
        assert!(out.contains("&lt;img"), "got: {out}");
        assert_inert(&out);

        let upper = sanitize_untrusted("a <IMG SRC=\"https://evil.example/p.png\"> b");
        assert!(upper.contains("&lt;IMG"), "got: {upper}");
        assert_inert(&upper);
    }

    #[test]
    fn html_block_is_escaped_including_nested_markdown_image() {
        let src = "<div>\n<img src=\"https://evil.example/p.png\">\n![x](https://evil.example/q.png)\n</div>\n";
        let out = sanitize_untrusted(src);
        assert_inert(&out);
    }

    #[test]
    fn image_nested_in_a_list_item_is_neutralized() {
        let out =
            sanitize_untrusted("- first\n- has ![pix](https://evil.example/p.png) inline\n- last");
        assert!(!out.contains("evil.example"));
        assert!(out.contains("- has [image: pix] inline"), "got: {out}");
        assert_inert(&out);
    }

    #[test]
    fn image_inside_a_link_is_neutralized() {
        let out = sanitize_untrusted("[![badge](https://evil.example/b.svg)](https://ok.example)");
        assert!(!out.contains("evil.example"));
        assert_inert(&out);
    }

    #[test]
    fn fenced_code_block_containing_image_and_html_stays_verbatim() {
        let src = "intro\n\n```html\n![alt](https://example.com/x.png)\n<img src=\"y.png\">\n```\n\noutro";
        let out = sanitize_untrusted(src);
        assert_eq!(out, src, "code fence content must be preserved verbatim");
    }

    #[test]
    fn inline_code_span_containing_image_syntax_stays_verbatim() {
        let src = "use `![alt](url)` and `<img src=x>` literally";
        assert_eq!(sanitize_untrusted(src), src);
    }

    #[test]
    fn indented_code_block_stays_verbatim() {
        let src = "para\n\n    ![alt](https://example.com/x.png)\n";
        assert_eq!(sanitize_untrusted(src), src);
    }

    #[test]
    fn multibyte_text_before_an_image_does_not_break_span_edits() {
        // Byte-offset regression guard: non-ASCII before the image shifts
        // byte offsets away from char offsets.
        let out = sanitize_untrusted("héllo 🎉 ![pïx](https://evil.example/p.png) done");
        assert!(!out.contains("evil.example"), "got: {out}");
        assert!(out.contains("héllo 🎉 "), "got: {out}");
        assert!(out.ends_with(" done"), "got: {out}");
        assert_inert(&out);
    }

    #[test]
    fn alt_text_cannot_reopen_syntax_through_the_placeholder() {
        let out = sanitize_untrusted("![a<b>`c`[d]](https://evil.example/p.png)");
        assert!(!out.contains("evil.example"));
        assert_inert(&out);
    }

    #[test]
    fn multiple_images_across_paragraphs_all_neutralized() {
        let src = "![a](https://e.example/1.png)\n\ntext\n\n![b](https://e.example/2.png)";
        let out = sanitize_untrusted(src);
        assert!(!out.contains("e.example"));
        assert_inert(&out);
    }

    #[test]
    fn coarse_escape_fallback_is_fetch_proof_too() {
        // Drive the fallback directly (it's also the html-span
        // replacement): image syntax broken, tags escaped.
        let out =
            super::coarse_escape("<img src=\"https://e.example/x\"> ![a](https://e.example/y)");
        assert_inert(&out);
    }
}
