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
//! **A link says where it goes** (DESIGN §4.5). A link's text is its author's: "the docs",
//! or another site's address. So the sanitiser writes the host a click opens after every
//! link to the web, "the docs (docs.rs)", whatever the link's text is, its own address
//! included ([`link_host`]). The host is an aid to the reader, drawn inside text a
//! stranger wrote: it stands directly after its link because nothing in a body that holds
//! a link draws out of the order it is written in. A body with a link to the web and any
//! character that runs right to left ([`runs_right_to_left`]) is drawn without Markdown,
//! so without links; the explicit controls are spelled out in every body.
//!
//! **Read as the view reads it.** The sanitiser parses a body with the options `TextView`
//! parses with ([`view_options`]: GFM with inline and block math, gpui-base's
//! `MarkdownExtensions::parse_options` with nothing registered), edits the spans that parse
//! finds, and then reads what it wrote the way `TextView` will ([`is_inert`]): parsed with
//! those options again, and each math node's source once more without math, since `TextView`
//! shows math nobody claims as the prose between its dollars (gpui-base's
//! `flatten_unclaimed_math`). An edit can change what the text around it means (escaping a
//! tag frees a backtick inside it; escaping an HTML block ends it early), so only the second
//! reading says whether an image or a tag is left, a link that doesn't say where it
//! goes, or a character that reorders or hides text (one written as a character reference
//! is such a character only once Markdown has read it). When one is, the whole body is
//! escaped coarsely ([`coarse_escape`]), which no parse can read an image, a tag or a link
//! to the web out of.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{Hash as _, Hasher as _};

use gpui::{App, ElementId, SharedString};
use gpui_component::text::{TextView, TextViewStyle};
use markdown::ParseOptions;
use markdown::mdast::Node;

use crate::theme::ActiveTheme as _;

/// A Markdown body as a [`TextView`] keyed by `id`, which must be stable and unique per body
/// (a thread or reply id), so the view's parsed state follows the body across renders. The
/// current light/dark appearance is part of the key, so a mode change parses again and fenced
/// code takes the new palette; and so is the text itself ([`view_key`]).
pub fn view(id: impl Into<ElementId>, text: &str, cx: &App) -> TextView {
    let sanitized: SharedString = sanitize_untrusted(text).into();
    let keyed = view_key(&id.into(), cx.dv_theme().key(), &sanitized);
    TextView::markdown(keyed, sanitized)
        .style(TextViewStyle::default().paragraph_gap(gpui::rems(0.5)))
        .on_link_click(|url, _event, _window, cx| crate::system::open_url(url, cx))
}

/// The key of the view's state for the sanitised text `sanitized` under `id` and the theme
/// `theme`. The text is part of it (its hash), so a body that changed is a new state, parsed
/// whole. A `TextView` handed a text that starts with the one its state holds takes it as
/// an append and parses only the last block and what was added (gpui-base's
/// `TextViewState::set_text`), in a context of its own: a link definition earlier in the
/// body doesn't exist there, so the tail can read as other links than the whole body the
/// sanitiser read and checked (a comment edited on GitHub to end in `[text [d]](address)`,
/// `[d]` defined above). No state ever sees two texts.
fn view_key(id: &ElementId, theme: impl std::fmt::Display, sanitized: &str) -> SharedString {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    sanitized.hash(&mut hasher);
    SharedString::from(format!("{id}/{theme}/{:016x}", hasher.finish()))
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
/// shows it as. Every link to the web gains the host it opens after it ([`link_host`]);
/// an address in a link's text becomes plain text there, and a hard break in it a line
/// break that draws as a space. Everything else (emphasis, tables, lists) passes through.
/// The result is then read as `TextView` will read it ([`is_inert`]); when the parse
/// fails, a span doesn't map onto the source, or that reading still finds an image, a tag,
/// a link that doesn't name its host, or a link in a body with text that runs right to
/// left, the whole body is escaped coarsely instead: uglier, and
/// fetch-proof and link-free. That too is read as `TextView` will read it, and should it
/// hold a link to the web or anything but inert text all the same, the body is drawn with
/// no Markdown at all ([`no_markdown`]).
///
/// Before any of it, the characters that reorder or hide text are spelled out
/// ([`spell_out`]), in prose and in code alike. One written as a character reference
/// (`&#x202E;`) is such a character only in what Markdown reads out of the body, which is
/// where [`is_inert`] looks for it: a body holding one is escaped coarsely, and the
/// reference shows as the text it is.
pub fn sanitize_untrusted(source: &str) -> String {
    let source = spell_out(source);
    edited(&source, &view_options())
        .filter(|sanitized| is_inert(sanitized))
        .or_else(|| Some(coarse_escape(&source)).filter(|escaped| is_link_free(escaped)))
        .unwrap_or_else(|| no_markdown(&source))
}

/// Whether `c` is an explicit bidirectional control (an embedding, an override or an
/// isolate, U+202A–U+202E and U+2066–U+2069) or a tag character (U+E0000–U+E007F): what a
/// body may not hold as a character ([`spell_out`], [`is_inert`]).
fn reorders_or_hides(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{E0000}'..='\u{E007F}')
}

/// Whether `c` can draw a line in another order than it is written without any explicit
/// control: a letter of a script written right to left or one of its numbers (the blocks
/// from Hebrew to Arabic Extended, their presentation forms, the right-to-left scripts of
/// the supplementary planes) or a right-to-left mark. Without one of these, and with the
/// explicit controls spelled out ([`spell_out`]), every piece `TextView` shapes on its own
/// (a line, a code span, the text beside it) draws left to right in the order it is
/// written. A body that holds one may hold no link to the web ([`is_inert`]).
fn runs_right_to_left(c: char) -> bool {
    matches!(
        c,
        '\u{0590}'..='\u{08FF}'
            | '\u{200F}'
            | '\u{FB1D}'..='\u{FDFF}'
            | '\u{FE70}'..='\u{FEFF}'
            | '\u{10800}'..='\u{10FFF}'
            | '\u{1E800}'..='\u{1EFFF}'
    )
}

/// Whether `c` ends a paragraph for the bidirectional algorithm inside one piece of text
/// (U+001C–U+001E, U+0085, U+2028, U+2029): a link's text is one line and holds none.
fn ends_a_paragraph(c: char) -> bool {
    matches!(
        c,
        '\u{001C}'..='\u{001E}' | '\u{0085}' | '\u{2028}' | '\u{2029}'
    )
}

/// `source` with every explicit bidirectional control (the embeddings, overrides and
/// isolates, U+202A–U+202E and U+2066–U+2069) and every tag character (U+E0000–U+E007F)
/// spelled out as the CLI spells them (`\u{202e}`, PLAN §10.3): an override would draw a
/// line, and the host dv writes after a link in it, in another order than it is written,
/// and a tag hides text. Line breaks and tabs stay, as do the marks scripts written right
/// to left need (LRM, RLM, the joiners): Hebrew or Arabic reads as it is written.
fn spell_out(source: &str) -> Cow<'_, str> {
    if !source.contains(reorders_or_hides) {
        return Cow::Borrowed(source);
    }
    let mut out = String::with_capacity(source.len() + 16);
    for c in source.chars() {
        if reorders_or_hides(c) {
            // Writing to a `String` can't fail.
            let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
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
    if !collect_unsafe_spans(&ast, source, &definitions(&ast), false, &mut edits) {
        return None;
    }
    if edits.is_empty() {
        return Some(source.to_string());
    }
    edits.sort_by_key(|&(start, _, _)| start);
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in edits {
        // Spans never nest (a replaced node isn't descended into; what goes after a link is
        // written at its end, after the edits inside it); skip an overlap rather than emit
        // text twice.
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
/// or renders as markup, no link that hides where it goes, and nothing that reorders or
/// hides text: no `Image`, `ImageReference` or `Html` node in a parse with
/// [`view_options`], nor in a math node's source read as prose; after every link to the
/// web, its host ([`link_host`]); in a link's text no link, no hard break and nothing
/// that ends a paragraph; in what the parse read out of the body ([`read_out`]) no
/// character [`spell_out`] spells out, which only a character reference can have put
/// there; and, when the body holds a link to the web, no character that runs right to
/// left anywhere in it, written or read out ([`runs_right_to_left`]): the host stands
/// directly after its link only where the text draws in the order it is written. Text
/// that doesn't parse is not inert.
fn is_inert(text: &str) -> bool {
    /// What a reading found beside what fails it at once.
    #[derive(Default)]
    struct Found {
        web_link: bool,
        right_to_left: bool,
    }
    fn inert_under(
        node: &Node,
        source: &str,
        definitions: &HashMap<String, String>,
        in_link: bool,
        found: &mut Found,
    ) -> bool {
        let read = read_out(node);
        let holds = |is: fn(char) -> bool| read.iter().any(|text| text.contains(is));
        if holds(reorders_or_hides) || (in_link && holds(ends_a_paragraph)) {
            return false;
        }
        found.right_to_left |= holds(runs_right_to_left);
        let children = |node: &Node, in_link: bool, found: &mut Found| {
            node.children()
                .into_iter()
                .flatten()
                .all(|child| inert_under(child, source, definitions, in_link, found))
        };
        match node {
            Node::Image(_) | Node::ImageReference(_) | Node::Html(_) => false,
            Node::InlineMath(_) | Node::Math(_) => node
                .position()
                .and_then(|position| source.get(position.start.offset..position.end.offset))
                .is_some_and(|literal| {
                    markdown::to_mdast(literal, &prose_options()).is_ok_and(|prose| {
                        inert_under(&prose, literal, &self::definitions(&prose), in_link, found)
                    })
                }),
            // A link in another link's text opens its own address under the outer link's
            // host, and a hard break there puts the host lines below the link's start
            // (`collect_unsafe_spans` leaves neither).
            Node::Link(_) | Node::LinkReference(_) | Node::Break(_) if in_link => false,
            Node::Link(_) | Node::LinkReference(_) => {
                let named = match link_host(node, definitions) {
                    Some(host) => {
                        found.web_link = true;
                        node.position()
                            .and_then(|position| source.get(position.end.offset..))
                            .is_some_and(|after| after.starts_with(&host_suffix(&host)))
                    }
                    None => true,
                };
                named && children(node, true, found)
            }
            other => children(other, in_link, found),
        }
    }
    let mut found = Found::default();
    markdown::to_mdast(text, &view_options())
        .is_ok_and(|ast| inert_under(&ast, text, &definitions(&ast), false, &mut found))
        && !(found.web_link && (found.right_to_left || text.contains(runs_right_to_left)))
}

/// What the parse read out of `node` itself, character references decoded: its text when
/// it is text (`Text`, code, math, raw HTML), and what it carries beside its children (a
/// link's address and title, a definition's, a reference's label, an image's alternative
/// text, a code block's language and the rest of its fence's line).
fn read_out(node: &Node) -> Vec<&str> {
    fn all<'a>(texts: &[Option<&'a str>]) -> Vec<&'a str> {
        texts.iter().flatten().copied().collect()
    }
    match node {
        Node::Text(text) => vec![&text.value],
        Node::InlineCode(code) => vec![&code.value],
        Node::InlineMath(math) => vec![&math.value],
        Node::Html(html) => vec![&html.value],
        Node::Code(code) => all(&[
            Some(&code.value),
            code.lang.as_deref(),
            code.meta.as_deref(),
        ]),
        Node::Math(math) => all(&[Some(&math.value), math.meta.as_deref()]),
        Node::Link(link) => all(&[Some(&link.url), link.title.as_deref()]),
        Node::Definition(definition) => all(&[
            Some(&definition.url),
            definition.title.as_deref(),
            definition.label.as_deref(),
            Some(&definition.identifier),
        ]),
        Node::LinkReference(reference) => {
            all(&[reference.label.as_deref(), Some(&reference.identifier)])
        }
        Node::Image(image) => all(&[Some(&image.alt), Some(&image.url), image.title.as_deref()]),
        Node::ImageReference(image) => all(&[
            Some(&image.alt),
            image.label.as_deref(),
            Some(&image.identifier),
        ]),
        Node::FootnoteDefinition(note) => all(&[note.label.as_deref(), Some(&note.identifier)]),
        Node::FootnoteReference(note) => all(&[note.label.as_deref(), Some(&note.identifier)]),
        _ => Vec::new(),
    }
}

/// Where each link reference under `node` goes: its definitions' destinations by their
/// identifier, the first of each as Markdown reads them.
fn definitions(node: &Node) -> HashMap<String, String> {
    fn collect(node: &Node, found: &mut HashMap<String, String>) {
        if let Node::Definition(definition) = node {
            found
                .entry(definition.identifier.clone())
                .or_insert_with(|| definition.url.clone());
        }
        for child in node.children().into_iter().flatten() {
            collect(child, found);
        }
    }
    let mut found = HashMap::new();
    collect(node, &mut found);
    found
}

/// The host to write after `link` (a `Link` or a `LinkReference`, whose destination
/// `definitions` has) so it says where it goes (module docs): the host of the web address
/// a click opens, as [`shown_host`] writes it. Every link to the web has one, a link
/// written as its own address too: what stands beside a link, or what its text reads
/// as, can make any address read as another's, so no link is trusted to name itself.
/// `None` only for a link that opens nothing (`crate::system::open_url` opens web
/// addresses only).
///
/// Only a link that isn't inside another's text is asked. GFM reads an address in a link's
/// text as a link too (`[https://docs.rs/x](https://docs.rs/x)` is two), and in
/// `TextView` a click on that text opens the inner address, whatever the outer link's
/// host says. So the inner one is taken away ([`collect_unsafe_spans`]): its text stays,
/// as text of the outer link, which is then the only thing a click there opens.
fn link_host(link: &Node, definitions: &HashMap<String, String>) -> Option<String> {
    let url = match link {
        Node::Link(link) => link.url.as_str(),
        Node::LinkReference(reference) => definitions.get(&reference.identifier)?.as_str(),
        _ => return None,
    };
    let url = url.trim();
    crate::system::is_web_url(url).then(|| shown_host(url))
}

/// What follows a link whose host is `host`: the host in brackets, as plain text.
fn host_suffix(host: &str) -> String {
    format!(" ({host})")
}

/// The host of the web address `url` as text that is plain in Markdown: lowercase letters,
/// digits, `.`, `-` and an IPv6 literal's `:`, without the login before it or the port
/// after it. Any other character is spelled out (`{U+0430}`), so a host can't pass for
/// another by a letter that looks like one of its own, nor open any syntax; `www.` is kept
/// from reading as an address of its own.
fn shown_host(url: &str) -> String {
    let address = url.split_once(':').map_or("", |(_, rest)| rest);
    let address = address.strip_prefix("//").unwrap_or(address);
    let authority = address
        .split(['/', '\\', '?', '#'])
        .next()
        .unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => match host.rsplit_once(':') {
            Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
            _ => host,
        },
    };
    let mut shown = String::with_capacity(host.len());
    for c in host.chars() {
        match c {
            'a'..='z' | '0'..='9' | '.' | '-' | ':' => shown.push(c),
            'A'..='Z' => shown.push(c.to_ascii_lowercase()),
            // Writing to a `String` can't fail.
            c => {
                let _ = write!(shown, "{{U+{:04X}}}", u32::from(c));
            }
        }
    }
    if shown.is_empty() {
        return "no host".to_string();
    }
    break_www(&shown)
}

/// `text` with every `www.`, in any case, kept from starting an address of its own (`www\.`):
/// GFM takes `www.`, `WWW.` and every mix of the two for the start of a link.
fn break_www(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 4);
    let mut rest = text;
    while let Some(at) = rest
        .as_bytes()
        .windows(4)
        .position(|window| window.eq_ignore_ascii_case(b"www."))
    {
        out.push_str(&rest[..at + 3]);
        out.push_str("\\.");
        rest = &rest[at + 4..];
    }
    out.push_str(rest);
    out
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
/// shows it as; inside a link's text (`in_link`), for every link, its source as text that
/// is no link, and for every hard break, a line break that draws as a space; and, after
/// every link to the web, the host that says where it goes ([`link_host`], with
/// `definitions` for a reference's destination), written after the edits inside the link.
/// `false` when a node's span can't be mapped onto `source` (no position, out of range,
/// off a char boundary), which makes the caller escape the whole body.
fn collect_unsafe_spans(
    node: &Node,
    source: &str,
    definitions: &HashMap<String, String>,
    in_link: bool,
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
        // An address GFM reads as a link inside another link's text (`link_host`): its
        // source with every punctuation character escaped reads as the same text and as
        // no link.
        Node::Link(_) | Node::LinkReference(_) if in_link => edit(node, &no_markdown),
        // A link's text is one line, so its host is on the line the link starts on: a hard
        // break in it keeps its line ending alone, which Markdown folds into a space.
        Node::Break(_) if in_link => edit(node, &|written| match written.find(['\r', '\n']) {
            Some(line_end) => written[line_end..].to_string(),
            None => " ".to_string(),
        }),
        other => {
            let is_link = matches!(other, Node::Link(_) | Node::LinkReference(_));
            let inside = other.children().into_iter().flatten().all(|child| {
                collect_unsafe_spans(child, source, definitions, in_link || is_link, edits)
            });
            // The host goes after the edits inside the link.
            match (link_host(other, definitions), span_of(other)) {
                (Some(host), Some((_, end))) if inside => {
                    edits.push((end, end, host_suffix(&host)));
                    true
                }
                (Some(_), None) => false,
                _ => inside,
            }
        }
    }
}

/// The inert literal an image becomes: its alt text, what reorders or hides text spelled
/// out ([`spell_out`]) and stripped of every character that could reopen Markdown or HTML
/// syntax once spliced back into the source.
fn image_placeholder(alt: &str) -> String {
    // The alt as Markdown read it: a character reference in it is the character by now.
    let clean: String = spell_out(alt)
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

/// Escapes HTML (`&`, `<`) and every opening bracket (`[` to `\[`, after each backslash is
/// made a literal one, `\\`, so none written before a bracket undoes the escape), so the
/// result can only render as literal text: for a raw-HTML node's span (an image inside an
/// HTML block must not come back to life once the tags are escaped) and for a whole body
/// when span edits aren't possible or didn't hold. No `<` and no bracket that opens anything
/// is left in it, and without one no parser, whatever its options and however it pairs the
/// rest, reads a tag, an image or a bracketed link out of it: that is what makes it the
/// fallback. A bare address in it is kept from becoming a link (`://` to `:\/\/`, `www.` in
/// any case to `www\.`): an automatic link runs on through the escaped markup after it (`&lt;/details>`),
/// which would then show as written and be part of what a click opens. So nothing in it is
/// a link to the web at all, and none can hide where it goes. Code in it shows the
/// backslashes it gained.
fn coarse_escape(text: &str) -> String {
    break_www(
        &text
            .replace('\\', "\\\\")
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('[', "\\[")
            .replace("://", ":\\/\\/"),
    )
}

/// Whether `text`, read as `TextView` reads a body, is inert ([`is_inert`]) and holds no
/// link to the web at all: what the coarse escape's output must be.
fn is_link_free(text: &str) -> bool {
    fn free_under(node: &Node, source: &str, definitions: &HashMap<String, String>) -> bool {
        let url = match node {
            Node::Link(link) => Some(link.url.as_str()),
            Node::LinkReference(reference) => {
                definitions.get(&reference.identifier).map(String::as_str)
            }
            Node::InlineMath(_) | Node::Math(_) => {
                return node
                    .position()
                    .and_then(|position| source.get(position.start.offset..position.end.offset))
                    .is_some_and(|literal| {
                        markdown::to_mdast(literal, &prose_options()).is_ok_and(|prose| {
                            free_under(&prose, literal, &self::definitions(&prose))
                        })
                    });
            }
            _ => None,
        };
        !url.is_some_and(|url| crate::system::is_web_url(url.trim()))
            && node
                .children()
                .into_iter()
                .flatten()
                .all(|child| free_under(child, source, definitions))
    }
    is_inert(text)
        && markdown::to_mdast(text, &view_options())
            .is_ok_and(|ast| free_under(&ast, text, &definitions(&ast)))
}

/// `text` as Markdown that is no Markdown: every ASCII punctuation character escaped, which
/// CommonMark reads as that character and nothing else, so no emphasis, link, image, tag,
/// address, heading, list or code span is left to read out of it. What a body is drawn as
/// when even its coarse escape reads as something ([`sanitize_untrusted`]): its words,
/// and its line breaks as Markdown folds them.
fn no_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests;
