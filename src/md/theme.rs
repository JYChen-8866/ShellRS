//! ShellRS theme values consumed by the Waku-derived Markdown renderer.
//!
//! The renderer was written against Waku's private `Theme` type. This adapter
//! keeps that seam, but resolves every color from GPUI Component's semantic
//! theme instead of introducing a second palette.

use gpui_kit::{Hsla, component};

/// The subset of a theme used by Markdown paint.
#[derive(Clone, Copy)]
pub struct Theme {
    pub text: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,
    pub text_ghost: Hsla,
    pub border: Hsla,
    pub inset: Hsla,
    pub overlay: Hsla,
    pub code_text: Hsla,
    pub code_wash: Hsla,
    pub selection: Hsla,
    pub accent: Hsla,
    pub accent_foreground: Hsla,
    pub success: Hsla,
    pub danger: Hsla,
    pub is_dark: bool,
}

impl Theme {
    /// Project the active GPUI Component theme into the renderer's paint roles.
    pub fn from_component(theme: &component::Theme) -> Self {
        Self {
            text: theme.foreground,
            text_secondary: theme.muted_foreground,
            text_tertiary: theme.muted_foreground.opacity(0.82),
            text_ghost: theme.muted_foreground.opacity(0.62),
            border: theme.border,
            inset: theme.muted,
            overlay: theme.accent,
            code_text: theme.primary,
            code_wash: theme.muted,
            selection: theme.selection,
            accent: theme.primary,
            accent_foreground: theme.primary_foreground,
            success: theme.success,
            danger: theme.danger,
            is_dark: theme.mode.is_dark(),
        }
    }

    #[cfg(test)]
    pub fn dark() -> Self {
        let mut theme = component::Theme::default();
        theme.mode = component::theme::ThemeMode::Dark;
        theme.colors.background = gpui_kit::black();
        theme.colors.foreground = gpui_kit::white();
        theme.colors.muted_foreground = gpui_kit::white().opacity(0.65);
        Self::from_component(&theme)
    }

    #[cfg(test)]
    pub fn light() -> Self {
        Self::from_component(&component::Theme::default())
    }
}
