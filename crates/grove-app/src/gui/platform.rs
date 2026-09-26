//! macOS specifics that GPUI doesn't cover.

/// Shows or hides the Dock icon. Grove lives in the menu bar; it only needs
/// a Dock icon (and an app menu) while its window is open.
pub fn set_dock_icon_visible(visible: bool) {
    #[cfg(target_os = "macos")]
    {
        use objc2::MainThreadMarker;
        use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
        let Some(mtm) = MainThreadMarker::new() else {
            tracing::warn!("set_dock_icon_visible called off the main thread");
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        if visible {
            set_app_icon(&app);
        }
        let policy = if visible {
            NSApplicationActivationPolicy::Regular
        } else {
            NSApplicationActivationPolicy::Accessory
        };
        app.setActivationPolicy(policy);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = visible;
}

/// Inside Grove.app the bundle's icon (Assets.car) is used. A bare binary
/// (`cargo run`) would get the generic one, so set ours explicitly.
#[cfg(target_os = "macos")]
fn set_app_icon(app: &objc2_app_kit::NSApplication) {
    use objc2::AllocAnyThread as _;
    use objc2_app_kit::NSImage;
    use objc2_foundation::NSData;

    let bundled = std::env::current_exe()
        .map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false);
    if bundled {
        return;
    }
    let png = include_bytes!("../../assets/dock-icon.png");
    let data = NSData::with_bytes(png);
    if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
        unsafe { app.setApplicationIconImage(Some(&image)) };
    }
}

#[cfg(target_os = "macos")]
pub use glass::SidebarGlass;

/// No Liquid Glass off macOS; the sidebar uses its tinted fallback.
#[cfg(not(target_os = "macos"))]
pub struct SidebarGlass;

#[cfg(not(target_os = "macos"))]
impl SidebarGlass {
    pub fn install(_: &gpui_kit::Window) -> Option<Self> {
        None
    }
    pub fn set_frame(&self, _: gpui_kit::Bounds<gpui_kit::Pixels>, _: gpui_kit::Pixels) {}
    pub fn set_visible(&self, _: bool) {}
    pub fn set_tint(&self, _: gpui_kit::Hsla) {}
}

#[cfg(target_os = "macos")]
mod glass {
    use gpui_kit::Hsla;
    use gpui_kit::{Bounds, Pixels, Window};
    use objc2::rc::Retained;
    use objc2::runtime::AnyClass;
    use objc2::{MainThreadMarker, MainThreadOnly as _};
    use objc2_app_kit::{
        NSAutoresizingMaskOptions, NSColor, NSGlassEffectView, NSGlassEffectViewStyle, NSView,
        NSWindowOrderingMode,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::cell::Cell;

    /// The macOS 27 sidebar: an edge-to-edge Liquid Glass pane
    /// (`NSGlassEffectView`, macOS 26+) under GPUI's view, spanning the
    /// sidebar column from the window's top to bottom edge. GPUI leaves that
    /// column transparent and reports its bounds every frame through
    /// [`Self::set_frame`]. It sits above the window's behind-window blur.
    pub struct SidebarGlass {
        view: Retained<NSGlassEffectView>,
        frame: Cell<Option<NSRect>>,
        tint: Cell<Option<Hsla>>,
    }

    impl SidebarGlass {
        /// `None` before macOS 26, or with `GROVE_NO_GLASS` set (to try the
        /// fallback).
        pub fn install(window: &Window) -> Option<Self> {
            if std::env::var_os("GROVE_NO_GLASS").is_some() {
                return None;
            }
            AnyClass::get(c"NSGlassEffectView")?;
            let mtm = MainThreadMarker::new()?;
            let RawWindowHandle::AppKit(handle) =
                HasWindowHandle::window_handle(window).ok()?.as_raw()
            else {
                return None;
            };
            // SAFETY: GPUI's AppKit handle points at its live content NSView,
            // and we're on the main thread.
            let gpui_view: &NSView = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
            let container = unsafe { gpui_view.superview() }?;

            let view = NSGlassEffectView::initWithFrame(
                NSGlassEffectView::alloc(mtm),
                NSRect::new(NSPoint::new(0., 0.), NSSize::new(0., 0.)),
            );
            view.setStyle(NSGlassEffectViewStyle::Regular);
            // Edge to edge: only the window's own corners are rounded.
            view.setCornerRadius(0.);
            // Pinned to the left edge with a fixed width; grows with the
            // window's height, so live resizing stays smooth between frames.
            view.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewHeightSizable
                    | NSAutoresizingMaskOptions::ViewMaxXMargin,
            );
            container.addSubview_positioned_relativeTo(
                &view,
                NSWindowOrderingMode::Below,
                Some(gpui_view),
            );
            tracing::info!("sidebar uses Liquid Glass");
            Some(Self {
                view,
                frame: Cell::new(None),
                tint: Cell::new(None),
            })
        }

        /// Moves the pane to `bounds` (GPUI coordinates: top-left origin in
        /// the window's content area of height `height`).
        pub fn set_frame(&self, bounds: Bounds<Pixels>, height: Pixels) {
            let frame = NSRect::new(
                NSPoint::new(
                    f64::from(f32::from(bounds.origin.x)),
                    f64::from(f32::from(height - bounds.origin.y - bounds.size.height)),
                ),
                NSSize::new(
                    f64::from(f32::from(bounds.size.width)),
                    f64::from(f32::from(bounds.size.height)),
                ),
            );
            if self.frame.get() != Some(frame) {
                self.frame.set(Some(frame));
                self.view.setFrame(frame);
            }
        }

        pub fn set_visible(&self, visible: bool) {
            self.view.setHidden(!visible);
        }

        /// The window server's number for the window, for
        /// `screencapture -l` (dev screenshots).
        #[cfg(feature = "debug-screenshots")]
        pub fn window_number(window: &Window) -> Option<isize> {
            let RawWindowHandle::AppKit(handle) =
                HasWindowHandle::window_handle(window).ok()?.as_raw()
            else {
                return None;
            };
            // SAFETY: as in `install`.
            let view: &NSView = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
            Some(view.window()?.windowNumber())
        }

        /// Tints the glass so it reads as light or dark with the app's
        /// appearance whatever the desktop behind it looks like.
        pub fn set_tint(&self, color: Hsla) {
            if self.tint.get() == Some(color) {
                return;
            }
            self.tint.set(Some(color));
            let rgba = color.to_rgb();
            let color = NSColor::colorWithSRGBRed_green_blue_alpha(
                rgba.r.into(),
                rgba.g.into(),
                rgba.b.into(),
                rgba.a.into(),
            );
            self.view.setTintColor(Some(&color));
        }
    }

    impl Drop for SidebarGlass {
        fn drop(&mut self) {
            self.view.removeFromSuperview();
        }
    }
}
