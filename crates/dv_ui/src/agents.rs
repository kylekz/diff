//! How an agent shows beside what it wrote (PLAN §2.5, DESIGN §4.1): the model it said it runs
//! on, in the model's own words ("Claude Fable 5.2"), with its family's mark — Claude's,
//! OpenAI's, Gemini's, Kimi's, DeepSeek's, Z.ai's for GLM, Qwen's — in the family's colour.
//!
//! The family is read from the model's name ([`ModelFamily::detect`]); an agent that named no
//! model shows by its own name, with the mark its name tells (`claude-code`, `codex`,
//! `gemini-cli`) or the bot glyph. Nothing here is stored: a review keeps the name the agent
//! gave, and a family dv doesn't know yet shows the bot glyph until it does.

use dv_core::review::{Author, AuthorKind};
use gpui::{Div, Hsla, IntoElement, ParentElement as _, Pixels, Styled as _, div};

use crate::components::icon;
use crate::icons::DvIcon;
use crate::theme::DvTheme;

/// A family of models with a mark of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModelFamily {
    Claude,
    Gpt,
    Gemini,
    Kimi,
    DeepSeek,
    Glm,
    Qwen,
}

impl ModelFamily {
    pub const ALL: [ModelFamily; 7] = [
        Self::Claude,
        Self::Gpt,
        Self::Gemini,
        Self::Kimi,
        Self::DeepSeek,
        Self::Glm,
        Self::Qwen,
    ];

    /// The family `name` (a model's, or an agent's) belongs to, ignoring case. A family's own
    /// name, its maker's or its coding agent's anywhere in it decides ("Claude Fable 5.2",
    /// "gpt-6.1-sol", "moonshot/kimi-k3"); the names that are also plain words count only as
    /// whole words ("Opus 5.5", "o4-mini", never "octopus").
    pub fn detect(name: &str) -> Option<Self> {
        let name = name.to_lowercase();
        let words: Vec<&str> = name
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .collect();
        Self::ALL.into_iter().find(|family| {
            let (anywhere, whole) = family.names();
            anywhere.iter().any(|part| name.contains(part))
                || words.iter().any(|word| whole.contains(word))
                || (*family == Self::Gpt && words.iter().any(|word| is_o_series(word)))
        })
    }

    /// The names that tell the family: those that do anywhere in a name, and those that do
    /// only as a whole word.
    fn names(self) -> (&'static [&'static str], &'static [&'static str]) {
        match self {
            Self::Claude => (
                &["claude", "anthropic"],
                &["opus", "sonnet", "haiku", "fable", "mythos"],
            ),
            Self::Gpt => (&["gpt", "openai", "codex"], &[]),
            Self::Gemini => (&["gemini"], &[]),
            Self::Kimi => (&["kimi", "moonshot"], &[]),
            Self::DeepSeek => (&["deepseek"], &[]),
            Self::Glm => (&["glm", "zhipu"], &["zai"]),
            Self::Qwen => (&["qwen", "tongyi"], &["qwq"]),
        }
    }

    /// `"claude"`, `"gpt"`, `"gemini"`, `"kimi"`, `"deepseek"`, `"glm"` or `"qwen"`, for
    /// automation state.
    pub fn key(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Gpt => "gpt",
            Self::Gemini => "gemini",
            Self::Kimi => "kimi",
            Self::DeepSeek => "deepseek",
            Self::Glm => "glm",
            Self::Qwen => "qwen",
        }
    }

    /// The family's mark.
    pub fn icon(self) -> DvIcon {
        match self {
            Self::Claude => DvIcon::ModelClaude,
            Self::Gpt => DvIcon::ModelOpenAi,
            Self::Gemini => DvIcon::ModelGemini,
            Self::Kimi => DvIcon::ModelKimi,
            Self::DeepSeek => DvIcon::ModelDeepSeek,
            Self::Glm => DvIcon::ModelZai,
            Self::Qwen => DvIcon::ModelQwen,
        }
    }

    /// The colour its mark is drawn in (DESIGN §1.6): the family's own, or `fg` for a mark
    /// that is black or white.
    pub fn color(self, theme: &DvTheme) -> Hsla {
        match self {
            Self::Claude => theme.models.claude,
            Self::Gemini => theme.models.gemini,
            Self::DeepSeek => theme.models.deepseek,
            Self::Qwen => theme.models.qwen,
            Self::Gpt | Self::Kimi | Self::Glm => theme.colors.fg,
        }
    }
}

/// OpenAI's reasoning models: `o1`, `o3`, `o4` (as a whole word, so `o4-mini` too).
fn is_o_series(word: &str) -> bool {
    word.strip_prefix('o')
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// The name `author` shows by: the model an agent said it runs on, else the author's own.
pub fn shown_name(author: &Author) -> &str {
    author.model.as_deref().unwrap_or(&author.name)
}

/// The family whose mark an agent shows: its model's, else (no model named) the one its own
/// name tells. `None` for anyone but an agent, and for an agent whose model dv doesn't know.
pub fn family(author: &Author) -> Option<ModelFamily> {
    if author.kind != AuthorKind::Agent {
        return None;
    }
    ModelFamily::detect(author.model.as_deref().unwrap_or(&author.name))
}

/// The glyph that stands for an agent, and its colour: its family's mark in the family's
/// colour, or the bot glyph in `blue` for a family dv doesn't know (and for an agent an event
/// doesn't name).
fn glyph_of(author: Option<&Author>, theme: &DvTheme) -> (DvIcon, Hsla) {
    match author.and_then(family) {
        Some(family) => (family.icon(), family.color(theme)),
        None => (DvIcon::Bot, theme.colors.blue),
    }
}

/// An agent's glyph on its own, `size` square, where a line speaks for the agent (the ready
/// banner, a thread's note).
pub fn glyph(author: Option<&Author>, size: Pixels, theme: &DvTheme) -> impl IntoElement {
    let (glyph, color) = glyph_of(author, theme);
    icon(glyph, size, color)
}

/// An agent's avatar: a `size` square holding its glyph on a soft fill of the glyph's colour
/// (`blueSoft` under the bot glyph).
pub fn mark(author: &Author, size: Pixels, glyph: Pixels, theme: &DvTheme) -> Div {
    let (glyph_icon, color) = glyph_of(Some(author), theme);
    let fill = match glyph_icon {
        DvIcon::Bot => theme.colors.blue_soft,
        _ => color.opacity(0.14),
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(size)
        .bg(fill)
        .child(icon(glyph_icon, glyph, color))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_models_name_tells_its_family() {
        for (name, family) in [
            ("Claude Fable 5.2", ModelFamily::Claude),
            ("claude-opus-5-5", ModelFamily::Claude),
            ("Opus 5.5", ModelFamily::Claude),
            ("GPT 6.1 Sol", ModelFamily::Gpt),
            ("gpt-6.1-codex", ModelFamily::Gpt),
            ("o4-mini", ModelFamily::Gpt),
            ("Gemini 4 Argon", ModelFamily::Gemini),
            ("Kimi K3", ModelFamily::Kimi),
            ("moonshot-v2", ModelFamily::Kimi),
            ("DeepSeek V4", ModelFamily::DeepSeek),
            ("deepseek-reasoner", ModelFamily::DeepSeek),
            ("GLM 5", ModelFamily::Glm),
            ("zai/glm-5-air", ModelFamily::Glm),
            ("Qwen 4 Max", ModelFamily::Qwen),
            ("qwq-plus", ModelFamily::Qwen),
        ] {
            assert_eq!(ModelFamily::detect(name), Some(family), "{name}");
        }
        for name in ["Llama 5", "octopus", "o", "ozone3", "grok-5", ""] {
            assert_eq!(ModelFamily::detect(name), None, "{name}");
        }
    }

    #[test]
    fn an_agent_shows_by_its_model_else_by_its_name() {
        let named = Author::agent("claude-code").with_model("GLM 5");
        assert_eq!(shown_name(&named), "GLM 5");
        assert_eq!(family(&named), Some(ModelFamily::Glm));

        // Without a model, the agent's own name, and the mark that name tells.
        let plain = Author::agent("claude-code");
        assert_eq!(shown_name(&plain), "claude-code");
        assert_eq!(family(&plain), Some(ModelFamily::Claude));
        assert_eq!(family(&Author::agent("codex")), Some(ModelFamily::Gpt));
        assert_eq!(family(&Author::agent("cursor")), None);

        // A model dv doesn't know shows by its name, with the bot glyph.
        let unknown = Author::agent("codex").with_model("Llama 5");
        assert_eq!(shown_name(&unknown), "Llama 5");
        assert_eq!(family(&unknown), None);

        // Only agents have a family, whatever a person is called.
        assert_eq!(family(&Author::reviewer("Claude")), None);
        assert_eq!(family(&Author::github("gemini-bot")), None);
    }

    #[test]
    fn every_family_has_a_mark_of_its_own() {
        let icons: std::collections::HashSet<_> = ModelFamily::ALL
            .iter()
            .map(|family| family.icon())
            .collect();
        assert_eq!(icons.len(), ModelFamily::ALL.len());
    }
}
