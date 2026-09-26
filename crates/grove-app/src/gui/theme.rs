//! Grove's macOS look (system colours, 13px type, SF Mono when present) and
//! the light/dark appearance setting.
//!
//! The palette lives in `theme.json` as a GPUI Kit theme set; colours it
//! doesn't name fall back to the library defaults.

use gpui_kit::component::{Theme, ThemeRegistry};
use gpui_kit::{App, Global, SharedString, Window, WindowAppearance, font};
use grove_config::Appearance;

const THEMES: &str = include_str!("theme.json");
const LIGHT: &str = "Grove Light";
const DARK: &str = "Grove Dark";
/// Preferred for paths, ports and terminals; bundled with Xcode and the SF
/// fonts download, so not on every Mac.
const SF_MONO: &str = "SF Mono";

/// The appearance chosen in settings.
#[derive(Clone, Copy, Default)]
pub struct AppearanceSetting(pub Appearance);

impl Global for AppearanceSetting {}

/// Registers the Grove themes and applies `appearance`. Call after
/// `gpui_kit::init`.
pub fn init(appearance: Appearance, cx: &mut App) {
    if let Err(e) = ThemeRegistry::global_mut(cx).load_themes_from_str(THEMES) {
        tracing::error!("can't load the Grove theme: {e:#}");
    }
    let registry = ThemeRegistry::global(cx);
    let (light, dark) = (
        registry.themes().get(LIGHT).cloned(),
        registry.themes().get(DARK).cloned(),
    );
    let theme = Theme::global_mut(cx);
    if let Some(light) = light {
        theme.light_theme = light;
    }
    if let Some(dark) = dark {
        theme.dark_theme = dark;
    }
    if has_font(SF_MONO, cx) {
        Theme::global_mut(cx).mono_font_family = SF_MONO.into();
    }
    set_appearance(appearance, cx);
}

/// Forces light or dark for the whole app, or follows macOS. Forcing goes
/// through the app's `NSAppearance`, so the window chrome, the blur behind
/// the sidebar and the terminals change with it.
pub fn set_appearance(appearance: Appearance, cx: &mut App) {
    cx.set_global(AppearanceSetting(appearance));
    cx.set_window_appearance(match appearance {
        Appearance::System => None,
        Appearance::Light => Some(WindowAppearance::Light),
        Appearance::Dark => Some(WindowAppearance::Dark),
    });
    sync(None, cx);
}

/// Matches the theme to the window's (or the app's) effective appearance.
pub fn sync(window: Option<&mut Window>, cx: &mut App) {
    Theme::sync_system_appearance(window, cx);
    // Sheets open below the unified title bar, not over its toolbar.
    Theme::global_mut(cx).sheet.margin_top = super::ui::TOOLBAR_HEIGHT;
    cx.refresh_windows();
}

pub fn appearance(cx: &App) -> Appearance {
    cx.try_global::<AppearanceSetting>()
        .map(|a| a.0)
        .unwrap_or_default()
}

/// Whether GPUI can load `family` (it panics on a missing family, so probe
/// through the fallback-aware resolver).
fn has_font(family: &str, cx: &App) -> bool {
    let family: SharedString = family.to_string().into();
    let text_system = cx.text_system();
    let id = text_system.resolve_font(&font(family.clone()));
    text_system
        .get_font_for_id(id)
        .is_some_and(|f| f.family == family)
}
