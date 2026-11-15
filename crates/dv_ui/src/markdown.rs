//! Markdown for comment, reply and summary bodies (DESIGN §4.1 thread card), rendered by
//! gpui-component's `TextView` over a sanitised source (PLAN §15).
//!
//! Every body is untrusted: an agent wrote it, or the reviewer pasted it from anywhere, and a
//! GitHub thread's body comes from any PR participant. `TextView` hands the URL of an image
//! (`![](url)`, raw `<img>`) to gpui's image loader, which fetches it with the app's HTTP
//! client: with one installed, that would hand whoever wrote the body a tracking pixel (the
//! reviewer's address and the time they looked). It also renders raw HTML (formatting tags,
//! links, tables, images). [`sanitize_untrusted`] removes both before the source reaches it;
//! [`view`] is the one way a view renders a body, so nothing skips it. Links open through
//! [`crate::system::open_url`], which opens web addresses only and only records them under
//! automation.
//!
//! **Read as the view reads it.** The sanitiser parses a body with the options `TextView`
//! parses with ([`view_options`]: GFM with inline and block math, gpui-base's
//! `MarkdownExtensions::parse_options` with nothing registered), edits the spans that parse
//! finds, and then reads what it wrote the way `TextView` will ([`is_inert`]): parsed with
//! those options again, and each math node's source once more without math, since `TextView`
//! shows math nobody claims as the prose between its dollars (gpui-base's
//! `flatten_unclaimed_math`). An edit can change what the text around it means (escaping a
//! tag frees a backtick inside it; escaping an HTML block ends it early), so only the second
//! reading says whether an image or a tag is left. When one is, the whole body is escaped
//! coarsely ([`coarse_escape`]), which no parse can read an image or a tag out of.

use gpui::{App, ElementId, SharedString};
use gpui_component::text::{TextView, TextViewStyle};
use markdown::ParseOptions;
use markdown::mdast::Node;

use crate::theme::ActiveTheme as _;

/// A Markdown body as a [`TextView`] keyed by `id`, which must be stable and unique per body
/// (a thread or reply id), so the view's parsed state follows the body across renders. The
/// current light/dark appearance is part of the key, so a mode change parses again and fenced
/// code takes the new palette.
pub fn view(id: impl Into<ElementId>, text: &str, cx: &App) -> TextView {
    let sanitized: SharedString = sanitize_untrusted(text).into();
    let keyed = SharedString::from(format!("{}/{}", id.into(), cx.dv_theme().key()));
    TextView::markdown(keyed, sanitized)
        .style(TextViewStyle::default().paragraph_gap(gpui::rems(0.5)))
        .on_link_click(|url, _event, _window, cx| crate::system::open_url(url, cx))
}

/// [`view`] whose links go to `on_link` instead of [`crate::system::open_url`]: the hover
/// card's, where a link to a file opens it in dv (PLAN §18.1). `on_link` must still send any
/// other link through `open_url`.
pub fn view_with_links(
    id: impl Into<ElementId>,
    text: &str,
    on_link: impl Fn(&str, &mut App) + Send + Sync + 'static,
    cx: &App,
) -> TextView {
    view(id, text, cx).on_link_click(move |url, _event, _window, cx| on_link(url, cx))
}

/// `source` with every line break inside a paragraph made a hard one, as GitHub shows a
/// comment (its comments break where they were typed; Markdown elsewhere folds such a break
/// into a space): two spaces go at the end of each line that has text and is followed by
/// another that has, unless it already ends in a hard break. Where a line break means
/// nothing (after a heading, a table's row, a list's item), a hard one means nothing either.
///
/// Code keeps its lines as they are. A fenced block is told as CommonMark tells one
/// ([`fence_run`]): opened by three or more backticks or tildes after at most three spaces
/// (and any `>` of a quote), closed only by a run of the same character at least as long;
/// an indented block is a line indented four spaces or a tab after a blank line, and the
/// indented lines that follow it. An indented line deeper in a list reads as code here and
/// keeps its break folded. A code span that runs over a line break gains the two spaces
/// inside it, which a code span shows as one space anyway.
pub fn with_line_breaks(source: &str) -> String {
    let mut out = String::with_capacity(source.len() + 16);
    // The fence that is open: its character and how long it is.
    let mut fence: Option<(char, usize)> = None;
    let (mut after_blank, mut indented) = (true, false);
    let mut lines = source.split('\n').peekable();
    while let Some(line) = lines.next() {
        let text = line.trim_end_matches('\r');
        let open = fence;
        // A backtick fence's info string holds no backtick: a line that starts with a code
        // span opens nothing.
        let run = fence_run(text).filter(|(character, _, info)| {
            open.is_some() || *character != '`' || !info.contains('`')
        });
        match (open, run) {
            (None, Some((character, length, _))) => fence = Some((character, length)),
            (Some((opened, least)), Some((character, length, info)))
                if opened == character && length >= least && info.trim().is_empty() =>
            {
                fence = None;
            }
            _ => {}
        }
        // Inside a fenced block, or the line that opens or closes one.
        let fenced = open.is_some() || fence.is_some();
        let blank = text.trim().is_empty();
        indented = !blank
            && fence.is_none()
            && (after_blank || indented)
            && (text.starts_with("    ") || text.starts_with('\t'));
        after_blank = blank;
        out.push_str(text);
        let next_has_text = lines.peek().is_some_and(|next| !next.trim().is_empty());
        let code = fenced || indented;
        if !code && !blank && next_has_text && !text.ends_with("  ") && !text.ends_with('\\') {
            out.push_str("  ");
        }
        if lines.peek().is_some() {
            out.push('\n');
        }
    }
    out
}

/// The run of backticks or tildes `line` starts with, when it can open or close a fenced
/// code block: three or more of one character after at most three spaces of indent, past
/// the `>` of any quote it is in. With it, the character, the run's length and what follows
/// it on the line (an opening fence's info string).
fn fence_run(line: &str) -> Option<(char, usize, &str)> {
    let mut rest = line;
    loop {
        let indent = rest.len() - rest.trim_start_matches(' ').len();
        if indent > 3 {
            return None;
        }
        rest = &rest[indent..];
        match rest.strip_prefix('>') {
            Some(quoted) => rest = quoted.strip_prefix(' ').unwrap_or(quoted),
            None => break,
        }
    }
    let character = rest.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let length = rest.len() - rest.trim_start_matches(character).len();
    (length >= 3).then(|| (character, length, &rest[length..]))
}

/// Neutralises the constructs that make `TextView` fetch anything or render markup from an
/// untrusted body, before the source reaches it:
///
/// - **Images** (`![alt](url)` and reference-style `![alt][ref]`) become an inert literal
///   `[image: alt]` (or `[image]`), so no URL survives to be fetched.
/// - **Raw HTML** (inline tags in any casing, and whole HTML blocks) is entity-escaped (`&` to
///   `&amp;`, `<` to `&lt;`) and shows as literal text.
///
/// The edits are made at the spans `markdown::to_mdast` reports, the parser `TextView` uses
/// itself, with the options it uses ([`view_options`]), so code is left alone for free: text
/// in fenced and indented blocks and inline code spans lives under `Code`/`InlineCode` nodes,
/// never `Image` or `Html` ones. A math node's source is sanitised as the prose `TextView`
/// shows it as. Everything else (links, emphasis, tables, lists) passes through. The result
/// is then read as `TextView` will read it ([`is_inert`]); when the parse fails, a span
/// doesn't map onto the source, or that reading still finds an image or a tag, the whole body
/// is escaped coarsely instead: uglier, and fetch-proof under any parse.
pub fn sanitize_untrusted(source: &str) -> String {
    edited(source, &view_options())
        .filter(|sanitized| is_inert(sanitized))
        .unwrap_or_else(|| coarse_escape(source))
}

/// The options `TextView` parses a body with: gpui-base's
/// `MarkdownExtensions::parse_options` with no extension registered, as [`view`] builds it.
/// GFM, with inline and block math on.
fn view_options() -> ParseOptions {
    let mut options = ParseOptions::gfm();
    options.constructs.math_text = true;
    options.constructs.math_flow = true;
    options
}

/// The options `TextView` reads a math node's source with when no plugin claims it
/// (gpui-base's `flatten_unclaimed_math`): [`view_options`] without math.
fn prose_options() -> ParseOptions {
    ParseOptions::gfm()
}

/// `source` with every span a parse with `options` finds an image or raw HTML in replaced
/// ([`collect_unsafe_spans`]); `None` when it doesn't parse or a span doesn't map onto it.
fn edited(source: &str, options: &ParseOptions) -> Option<String> {
    let ast = markdown::to_mdast(source, options).ok()?;
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    if !collect_unsafe_spans(&ast, source, &mut edits) {
        return None;
    }
    if edits.is_empty() {
        return Some(source.to_string());
    }
    edits.sort_by_key(|&(start, _, _)| start);
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in edits {
        // Spans never nest (a replaced node isn't descended into); skip an overlap rather than
        // emit text twice.
        if start < cursor {
            continue;
        }
        out.push_str(&source[cursor..start]);
        out.push_str(&replacement);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    Some(out)
}

/// Whether `text`, read as `TextView` reads a body (module docs), holds nothing it fetches
/// or renders as markup: no `Image`, `ImageReference` or `Html` node in a parse with
/// [`view_options`], nor in a math node's source read as prose. Text that doesn't parse is
/// not inert.
fn is_inert(text: &str) -> bool {
    fn inert_under(node: &Node, source: &str) -> bool {
        match node {
            Node::Image(_) | Node::ImageReference(_) | Node::Html(_) => false,
            Node::InlineMath(_) | Node::Math(_) => node
                .position()
                .and_then(|position| source.get(position.start.offset..position.end.offset))
                .is_some_and(|literal| {
                    markdown::to_mdast(literal, &prose_options())
                        .is_ok_and(|prose| inert_under(&prose, literal))
                }),
            other => other
                .children()
                .into_iter()
                .flatten()
                .all(|child| inert_under(child, source)),
        }
    }
    markdown::to_mdast(text, &view_options()).is_ok_and(|ast| inert_under(&ast, text))
}

/// The first line of a Markdown body as plain text, for one-line summaries (the comments list,
/// the sheet's comments, a collapsed card; DESIGN §4.1 "the first line of the comment"): the
/// first line with text of the first block that has any, its inline markup dropped (emphasis,
/// code spans and links read as their text; an image as `[image: alt]`), a fenced block by its
/// first line of code rather than its fence, a heading without its `#`s, a list item without
/// its marker. Trimmed; empty for a body with no text. A body that doesn't parse falls back to
/// its first non-empty line as written.
pub fn first_line_plain(source: &str) -> String {
    let parsed = markdown::to_mdast(source, &ParseOptions::gfm());
    match parsed {
        Ok(ast) => block_line(&ast).unwrap_or_default(),
        Err(_) => first_text_line(source).unwrap_or_default(),
    }
}

/// The first line with text of the first block under `node` that has one.
fn block_line(node: &markdown::mdast::Node) -> Option<String> {
    use markdown::mdast::Node;
    match node {
        Node::Code(code) => first_text_line(&code.value),
        Node::Math(math) => first_text_line(&math.value),
        Node::Html(html) => first_text_line(&html.value),
        Node::Paragraph(_) | Node::Heading(_) => {
            let mut text = String::new();
            inline_text(node, &mut text);
            first_text_line(&text)
        }
        Node::TableRow(row) => {
            let cells: Vec<String> = row
                .children
                .iter()
                .map(|cell| {
                    let mut text = String::new();
                    inline_text(cell, &mut text);
                    text.split_whitespace().collect::<Vec<_>>().join(" ")
                })
                .collect();
            first_text_line(&cells.join(" | "))
        }
        Node::ThematicBreak(_) | Node::Definition(_) => None,
        other => other.children()?.iter().find_map(block_line),
    }
}

/// Appends the text of the inline content under `node`, without its markup; a hard break is a
/// newline.
fn inline_text(node: &markdown::mdast::Node, out: &mut String) {
    use markdown::mdast::Node;
    match node {
        Node::Text(text) => out.push_str(&text.value),
        Node::InlineCode(code) => out.push_str(&code.value),
        Node::InlineMath(math) => out.push_str(&math.value),
        Node::Html(html) => out.push_str(&html.value),
        Node::Break(_) => out.push('\n'),
        Node::Image(image) => out.push_str(&image_placeholder(&image.alt)),
        Node::ImageReference(image) => out.push_str(&image_placeholder(&image.alt)),
        other => {
            for child in other.children().into_iter().flatten() {
                inline_text(child, out);
            }
        }
    }
}

/// The first line of `text` with anything on it, trimmed.
fn first_text_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// Collects a `(start, end, replacement)` byte-span edit for every image and raw-HTML node
/// under `node`, and for every math node, whose source is sanitised as the prose `TextView`
/// shows it as. `false` when a node's span can't be mapped onto `source` (no position, out
/// of range, off a char boundary), which makes the caller escape the whole body.
fn collect_unsafe_spans(
    node: &Node,
    source: &str,
    edits: &mut Vec<(usize, usize, String)>,
) -> bool {
    let span_of = |node: &Node| -> Option<(usize, usize)> {
        let position = node.position()?;
        let (start, end) = (position.start.offset, position.end.offset);
        source.get(start..end).map(|_| (start, end))
    };
    let mut edit = |node: &Node, replacement: &dyn Fn(&str) -> String| match span_of(node) {
        Some((start, end)) => {
            edits.push((start, end, replacement(&source[start..end])));
            true
        }
        None => false,
    };
    match node {
        Node::Image(image) => edit(node, &|_| image_placeholder(&image.alt)),
        Node::ImageReference(image) => edit(node, &|_| image_placeholder(&image.alt)),
        Node::Html(_) => edit(node, &coarse_escape),
        Node::InlineMath(_) | Node::Math(_) => edit(node, &|math| {
            edited(math, &prose_options()).unwrap_or_else(|| coarse_escape(math))
        }),
        other => match other.children() {
            Some(children) => children
                .iter()
                .all(|child| collect_unsafe_spans(child, source, edits)),
            None => true,
        },
    }
}

/// The inert literal an image becomes: its alt text, stripped of every character that could
/// reopen Markdown or HTML syntax once spliced back into the source.
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

/// Escapes HTML (`&`, `<`) and breaks image syntax (`![` to `!\[`), so the result can only
/// render as literal text: for a raw-HTML node's span (an image inside an HTML block must not
/// come back to life once the tags are escaped) and for a whole body when span edits aren't
/// possible or didn't hold. No `<` and no `![` is left in it, and without one no parser,
/// whatever its options and however it pairs the rest, reads a tag or an image out of it:
/// that is what makes it the fallback. A bare address in it is kept from becoming a link (`://` to `:\/\/`, `www.` to
/// `www\.`): an automatic link runs on through the escaped markup after it (`&lt;/details>`),
/// which would then show as written and be part of what a click opens.
fn coarse_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace("![", "!\\[")
        .replace("://", ":\\/\\/")
        .replace("www.", "www\\.")
}

#[cfg(test)]
mod tests;
