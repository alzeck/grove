use gpui_kit::SharedString;

use crate::theme::TerminalTheme;

/// Appearance and behaviour of a [`TerminalView`](crate::TerminalView).
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalSettings {
    /// A monospace font family.
    pub font_family: SharedString,
    /// Font size in logical pixels.
    pub font_size: f32,
    /// Line height as a multiple of `font_size`.
    pub line_height: f32,
    /// Space between the view's edges and the text, in logical pixels.
    pub padding: f32,
    /// Lines of history kept above the screen. Applies to new sessions.
    pub scrollback_lines: usize,
    /// Treat Option as Alt (sends `ESC` prefixes) instead of typing the
    /// characters macOS maps to Option combinations.
    pub option_as_alt: bool,
    /// Theme used when the window appearance is dark.
    pub dark_theme: TerminalTheme,
    /// Theme used when the window appearance is light.
    pub light_theme: TerminalTheme,
}

impl Default for TerminalSettings {
    fn default() -> Self {
        Self {
            font_family: "Menlo".into(),
            font_size: 13.0,
            line_height: 1.2,
            padding: 4.0,
            scrollback_lines: 10_000,
            option_as_alt: false,
            dark_theme: TerminalTheme::dark(),
            light_theme: TerminalTheme::light(),
        }
    }
}
