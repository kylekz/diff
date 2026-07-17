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

use gpui::{ElementId, SharedString};
use gpui_component::text::{TextView, TextViewStyle};

/// Build a themed Markdown [`TextView`] for `text`, keyed by `id`.
///
/// `id` must be stable and unique per rendered body (e.g. derived from a
/// comment/reply id) so `TextView`'s internal per-element state — parsed AST,
/// selection, scroll — persists correctly across re-renders instead of
/// aliasing onto an unrelated body.
///
/// A tighter `paragraph_gap` than the component's 1rem default keeps
/// multi-paragraph bodies visually consistent with this app's already-compact
/// thread cards (`gap_2`/`py_1` throughout `workspace.rs`'s card rendering).
pub(crate) fn view(id: impl Into<ElementId>, text: impl Into<SharedString>) -> TextView {
    TextView::markdown(id, text).style(TextViewStyle::default().paragraph_gap(gpui::rems(0.5)))
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
}
