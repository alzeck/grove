use std::{
    future::Future,
    hash::{DefaultHasher, Hash, Hasher},
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
};

use futures::{FutureExt as _, StreamExt as _};
use gpui_kit::{
    App, Bounds, ClipboardItem, Context, FocusHandle, Focusable, Font, FontFeatures, FontStyle,
    FontWeight, Hsla, KeyBinding, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent, ShapedLine,
    SharedString, StrikethroughStyle, Subscription, Task, TextRun, TouchPhase, UnderlineStyle,
    Window, WindowAppearance, div, point, prelude::*, px, size,
};
use libghostty_vt::{key, mouse, terminal::ScrollViewport};

use crate::{
    PtyHandle, TerminalFeed, TerminalSettings,
    element::{CursorPaint, PaintData, TerminalElement},
    frame::{CursorShape, Frame, TextStyle, UnderlineKind},
    input::{map_keystroke, text_editing_shortcut},
    metrics::{CellSize, cell_at, grid_size},
    session::{MousePosition, Session},
    theme::{Rgb, TerminalTheme},
};

gpui_kit::actions!(
    grove_terminal,
    [
        /// Copy the selection to the clipboard.
        CopySelection,
        /// Paste the clipboard as terminal input.
        Paste,
        /// Clear the scrollback and screen.
        Clear,
        /// Send Tab to the program (instead of moving focus).
        SendTab,
        /// Send Shift-Tab to the program (instead of moving focus).
        SendBacktab,
        ScrollPageUp,
        ScrollPageDown,
        ScrollToTop,
        ScrollToBottom,
    ]
);

/// Key context of a focused [`TerminalView`], for key bindings.
pub const KEY_CONTEXT: &str = "GroveTerminal";

/// Registers the terminal's key bindings. Call once at startup.
pub fn init(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("cmd-c", CopySelection, context),
        KeyBinding::new("cmd-v", Paste, context),
        KeyBinding::new("cmd-k", Clear, context),
        // Bound so they beat focus-navigation bindings further up the tree.
        KeyBinding::new("tab", SendTab, context),
        KeyBinding::new("shift-tab", SendBacktab, context),
        KeyBinding::new("cmd-pageup", ScrollPageUp, context),
        KeyBinding::new("cmd-pagedown", ScrollPageDown, context),
        KeyBinding::new("cmd-home", ScrollToTop, context),
        KeyBinding::new("cmd-end", ScrollToBottom, context),
    ]);
}

/// Output read in one go before the screen is updated.
const MAX_BATCH: usize = 256 * 1024;
const SCROLLBAR_WIDTH: f32 = 6.0;

/// A terminal showing a process's output and sending it keyboard input.
pub struct TerminalView {
    focus_handle: FocusHandle,
    settings: TerminalSettings,
    fonts: Fonts,
    metrics: Option<Metrics>,
    session: Session,
    pty: Arc<dyn PtyHandle>,
    /// The grid size last sent to the PTY.
    pty_size: Option<(u16, u16)>,
    read_only: bool,
    dark: bool,
    frame: Frame,
    layout: Option<GridLayout>,
    drag: Option<Drag>,
    /// Scroll distance not yet turned into whole lines.
    scroll_px: f32,
    _feed: Task<()>,
    _subscriptions: [Subscription; 3],
}

#[derive(Clone, Copy, Debug)]
struct Metrics {
    cell: CellSize,
    font_size: Pixels,
}

/// Where the grid was last laid out, for mapping mouse positions.
#[derive(Clone, Copy, Debug)]
struct GridLayout {
    origin: Point<Pixels>,
    cell: CellSize,
    cols: u16,
    rows: u16,
}

#[derive(Clone, Copy, Debug)]
enum Drag {
    /// Selecting text from the cell where the button went down.
    Selecting { col: u16, row: u16, moved: bool },
    /// Reporting a button press to a program that tracks the mouse.
    Reporting(mouse::Button),
}

struct Fonts {
    regular: Font,
    bold: Font,
    italic: Font,
    bold_italic: Font,
}

impl Fonts {
    fn new(family: &SharedString) -> Self {
        let font = |weight, style| Font {
            family: family.clone(),
            // One glyph per cell: ligatures would break the grid.
            features: FontFeatures::disable_ligatures(),
            fallbacks: None,
            weight,
            style,
        };
        Self {
            regular: font(FontWeight::NORMAL, FontStyle::Normal),
            bold: font(FontWeight::BOLD, FontStyle::Normal),
            italic: font(FontWeight::NORMAL, FontStyle::Italic),
            bold_italic: font(FontWeight::BOLD, FontStyle::Italic),
        }
    }

    fn get(&self, bold: bool, italic: bool) -> &Font {
        match (bold, italic) {
            (false, false) => &self.regular,
            (true, false) => &self.bold,
            (false, true) => &self.italic,
            (true, true) => &self.bold_italic,
        }
    }
}

fn is_dark(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

fn new_session(
    settings: &TerminalSettings,
    size: (u16, u16),
    pty: Arc<dyn PtyHandle>,
    dark: bool,
) -> Session {
    let theme = if dark {
        &settings.dark_theme
    } else {
        &settings.light_theme
    };
    Session::new(size.0, size.1, settings.scrollback_lines, pty, theme, dark)
        .expect("libghostty failed to allocate a terminal")
}

impl TerminalView {
    pub fn new(
        feed: TerminalFeed,
        pty: Arc<dyn PtyHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_settings(feed, pty, TerminalSettings::default(), window, cx)
    }

    pub fn with_settings(
        feed: TerminalFeed,
        pty: Arc<dyn PtyHandle>,
        settings: TerminalSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let dark = is_dark(window.appearance());
        let subscriptions = [
            cx.observe_window_appearance(window, |this, window, cx| {
                this.set_dark(is_dark(window.appearance()), cx);
            }),
            cx.on_focus(&focus_handle, window, |this, _, cx| {
                this.focus_changed(true, cx);
            }),
            cx.on_blur(&focus_handle, window, |this, _, cx| {
                this.focus_changed(false, cx);
            }),
        ];
        Self {
            session: new_session(&settings, (80, 24), pty.clone(), dark),
            fonts: Fonts::new(&settings.font_family),
            focus_handle,
            settings,
            metrics: None,
            pty,
            pty_size: None,
            read_only: false,
            dark,
            frame: Frame::default(),
            layout: None,
            drag: None,
            scroll_px: 0.0,
            _feed: spawn_feed(feed, cx),
            _subscriptions: subscriptions,
        }
    }

    /// Process restarted: reset terminal state and start over with a new feed.
    pub fn replace_feed(
        &mut self,
        feed: TerminalFeed,
        pty: Arc<dyn PtyHandle>,
        cx: &mut Context<Self>,
    ) {
        self.session = new_session(&self.settings, self.session.size(), pty.clone(), self.dark);
        self.pty = pty;
        self.pty_size = None;
        self.drag = None;
        self._feed = spawn_feed(feed, cx);
        cx.notify();
    }

    /// Clears the scrollback and the screen.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if let Err(err) = self.session.clear() {
            tracing::warn!("clearing terminal: {err}");
        }
        cx.notify();
    }

    /// Ignores keyboard, paste and mouse input, e.g. once the process exited.
    pub fn set_read_only(&mut self, read_only: bool, cx: &mut Context<Self>) {
        self.read_only = read_only;
        if read_only {
            self.drag = None;
        }
        cx.notify();
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn settings(&self) -> &TerminalSettings {
        &self.settings
    }

    /// Applies new settings. `scrollback_lines` takes effect on the next
    /// [`replace_feed`](Self::replace_feed).
    pub fn set_settings(&mut self, settings: TerminalSettings, cx: &mut Context<Self>) {
        self.fonts = Fonts::new(&settings.font_family);
        self.metrics = None;
        self.settings = settings;
        self.apply_theme();
        cx.notify();
    }

    fn theme(&self) -> &TerminalTheme {
        if self.dark {
            &self.settings.dark_theme
        } else {
            &self.settings.light_theme
        }
    }

    fn apply_theme(&mut self) {
        let theme = self.theme().clone();
        if let Err(err) = self.session.set_theme(&theme, self.dark) {
            tracing::warn!("setting terminal colours: {err}");
        }
    }

    fn set_dark(&mut self, dark: bool, cx: &mut Context<Self>) {
        if self.dark != dark {
            self.dark = dark;
            self.apply_theme();
            cx.notify();
        }
    }

    fn focus_changed(&mut self, focused: bool, cx: &mut Context<Self>) {
        if !self.read_only
            && let Err(err) = self.session.focus_changed(focused)
        {
            tracing::warn!("reporting focus: {err}");
        }
        cx.notify();
    }

    fn write_output(&mut self, data: &[u8], cx: &mut Context<Self>) {
        self.session.feed(data);
        cx.notify();
    }

    // ---- Keyboard ------------------------------------------------------------

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let action = if event.is_held {
            key::Action::Repeat
        } else {
            key::Action::Press
        };
        if self.send_keystroke(&event.keystroke, action, cx) {
            cx.stop_propagation();
        }
    }

    fn on_key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        // Only produces bytes when the program asked for key release events.
        if self.send_keystroke(&event.keystroke, key::Action::Release, cx) {
            cx.stop_propagation();
        }
    }

    /// Returns whether the keystroke became terminal input.
    fn send_keystroke(
        &mut self,
        keystroke: &Keystroke,
        action: key::Action,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.read_only {
            return false;
        }
        if let Some(bytes) = text_editing_shortcut(keystroke) {
            if action == key::Action::Release {
                return false;
            }
            self.pty.write(bytes);
            self.after_input(cx);
            return true;
        }
        // Other Cmd shortcuts belong to the app.
        if keystroke.modifiers.platform {
            return false;
        }
        let option_as_alt = self.settings.option_as_alt;
        let input = map_keystroke(keystroke, option_as_alt);
        match self.session.send_key(input, action, option_as_alt) {
            Ok(true) => {
                if action != key::Action::Release {
                    self.after_input(cx);
                }
                true
            }
            Ok(false) => false,
            Err(err) => {
                tracing::warn!("encoding {keystroke}: {err}");
                false
            }
        }
    }

    fn after_input(&mut self, cx: &mut Context<Self>) {
        self.session.scroll(ScrollViewport::Bottom);
        let _ = self.session.clear_selection();
        cx.notify();
    }

    fn send_tab(&mut self, shift: bool, cx: &mut Context<Self>) {
        let keystroke = Keystroke {
            modifiers: Modifiers {
                shift,
                ..Modifiers::default()
            },
            key: "tab".into(),
            key_char: None,
        };
        if !self.send_keystroke(&keystroke, key::Action::Press, cx) {
            cx.propagate();
        }
    }

    fn copy_selection(&mut self, _: &CopySelection, _: &mut Window, cx: &mut Context<Self>) {
        match self.session.selection_text() {
            Ok(Some(text)) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
            Ok(None) => cx.propagate(),
            Err(err) => tracing::warn!("copying selection: {err}"),
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if self.read_only {
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        if let Err(err) = self.session.paste(&text) {
            tracing::warn!("pasting: {err}");
        }
        self.after_input(cx);
    }

    fn scroll_by(&mut self, scroll: ScrollViewport, cx: &mut Context<Self>) {
        self.session.scroll(scroll);
        cx.notify();
    }

    fn page(&self) -> isize {
        self.session.size().1.saturating_sub(1).max(1) as isize
    }

    // ---- Mouse -----------------------------------------------------------

    fn grid_cell(&self, position: Point<Pixels>) -> Option<(u16, u16, i32)> {
        let layout = self.layout?;
        let at = position - layout.origin;
        Some(cell_at(
            f32::from(at.x),
            f32::from(at.y),
            layout.cell,
            layout.cols,
            layout.rows,
        ))
    }

    /// The position in the units the mouse encoder expects. Cells are
    /// scaled to whole pixels so fractional cell widths don't drift.
    fn mouse_position(&self, position: Point<Pixels>) -> Option<MousePosition> {
        let layout = self.layout?;
        let at = position - layout.origin;
        let cell_width = layout.cell.width.round().max(1.0);
        let cell_height = layout.cell.height.round().max(1.0);
        Some(MousePosition {
            x: f32::from(at.x) / layout.cell.width * cell_width,
            y: f32::from(at.y) / layout.cell.height * cell_height,
            size: mouse::EncoderSize {
                screen_width: (f32::from(layout.cols) * cell_width) as u32,
                screen_height: (f32::from(layout.rows) * cell_height) as u32,
                cell_width: cell_width as u32,
                cell_height: cell_height as u32,
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            },
        })
    }

    /// Mouse events go to the program when it tracks the mouse, unless Shift
    /// is held to select text instead.
    fn reports_mouse(&self, modifiers: &Modifiers) -> bool {
        !self.read_only && !modifiers.shift && self.session.mouse_tracking()
    }

    pub(crate) fn mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let Some((col, row, _)) = self.grid_cell(event.position) else {
            return;
        };
        if self.reports_mouse(&event.modifiers) {
            if let Some(button) = mouse_button(event.button)
                && let Some(at) = self.mouse_position(event.position)
            {
                let mods = key_mods(&event.modifiers);
                if let Err(err) =
                    self.session
                        .send_mouse(mouse::Action::Press, Some(button), mods, at, true)
                {
                    tracing::warn!("reporting mouse: {err}");
                }
                self.drag = Some(Drag::Reporting(button));
            }
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        if let Err(err) = self.session.begin_selection(col, row, event.click_count) {
            tracing::warn!("selecting: {err}");
        }
        self.drag = Some(Drag::Selecting {
            col,
            row,
            moved: false,
        });
        cx.notify();
    }

    pub(crate) fn mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        match self.drag {
            Some(Drag::Selecting { col, row, moved }) => {
                if event.pressed_button != Some(MouseButton::Left) {
                    // The release happened where we couldn't see it.
                    self.drag = None;
                    self.session.end_selection();
                    return;
                }
                let Some((to_col, to_row, overflow)) = self.grid_cell(event.position) else {
                    return;
                };
                if overflow != 0 {
                    // Dragging past the top or bottom scrolls.
                    self.session
                        .scroll(ScrollViewport::Delta(overflow as isize));
                } else if !moved && (to_col, to_row) == (col, row) {
                    return;
                }
                self.drag = Some(Drag::Selecting {
                    col,
                    row,
                    moved: true,
                });
                if let Err(err) = self.session.extend_selection(to_col, to_row) {
                    tracing::warn!("selecting: {err}");
                }
                cx.notify();
            }
            Some(Drag::Reporting(button)) => self.report_motion(event, Some(button)),
            None if hovered && self.reports_mouse(&event.modifiers) => {
                self.report_motion(event, None);
            }
            None => {}
        }
    }

    fn report_motion(&mut self, event: &MouseMoveEvent, button: Option<mouse::Button>) {
        let Some(at) = self.mouse_position(event.position) else {
            return;
        };
        let mods = key_mods(&event.modifiers);
        // The encoder drops motion the program didn't ask for.
        if let Err(err) =
            self.session
                .send_mouse(mouse::Action::Motion, button, mods, at, button.is_some())
        {
            tracing::warn!("reporting mouse: {err}");
        }
    }

    pub(crate) fn mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        match self.drag.take() {
            Some(Drag::Selecting { .. }) => {
                self.session.end_selection();
                cx.notify();
            }
            Some(Drag::Reporting(button)) => {
                if let Some(at) = self.mouse_position(event.position) {
                    let mods = key_mods(&event.modifiers);
                    if let Err(err) = self.session.send_mouse(
                        mouse::Action::Release,
                        Some(button),
                        mods,
                        at,
                        false,
                    ) {
                        tracing::warn!("reporting mouse: {err}");
                    }
                }
            }
            None => {}
        }
    }

    pub(crate) fn scroll_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let (Some(layout), Some(at)) = (self.layout, self.mouse_position(event.position)) else {
            return;
        };
        if event.touch_phase == TouchPhase::Started {
            self.scroll_px = 0.0;
        }
        let line_height = layout.cell.height;
        self.scroll_px += f32::from(event.delta.pixel_delta(px(line_height)).y);
        let lines = (self.scroll_px / line_height).trunc();
        if lines == 0.0 {
            return;
        }
        self.scroll_px -= lines * line_height;
        let mods = key_mods(&event.modifiers);
        let to_program = !self.read_only && !event.modifiers.shift;
        if let Err(err) = self.session.wheel(lines as i32, mods, at, to_program) {
            tracing::warn!("scrolling: {err}");
        }
        cx.notify();
    }

    // ---- Layout and painting ---------------------------------------------

    fn metrics(&mut self, window: &Window) -> Metrics {
        if let Some(metrics) = self.metrics {
            return metrics;
        }
        let font_size = px(self.settings.font_size.max(1.0));
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&self.fonts.regular);
        let width = text_system
            .advance(font_id, font_size, 'm')
            .map(|advance| f32::from(advance.width))
            .unwrap_or(f32::from(font_size) * 0.6);
        let metrics = Metrics {
            cell: CellSize {
                width: width.max(1.0),
                height: (f32::from(font_size) * self.settings.line_height.max(1.0)).round(),
            },
            font_size,
        };
        self.metrics = Some(metrics);
        metrics
    }

    /// Fits the grid to `bounds`, reads the screen and prepares everything
    /// the element paints.
    pub(crate) fn prepaint(&mut self, bounds: Bounds<Pixels>, window: &mut Window) -> PaintData {
        let metrics = self.metrics(window);
        let padding = self.settings.padding.max(0.0);
        let origin = bounds.origin + point(px(padding), px(padding));
        let (cols, rows) = grid_size(
            f32::from(bounds.size.width) - 2.0 * padding,
            f32::from(bounds.size.height) - 2.0 * padding,
            metrics.cell,
        );

        let scale = window.scale_factor();
        let cell_px = (
            (metrics.cell.width * scale).round() as u32,
            (metrics.cell.height * scale).round() as u32,
        );
        if let Err(err) = self.session.resize(cols, rows, cell_px.0, cell_px.1) {
            tracing::warn!("resizing terminal: {err}");
        }
        let (cols, rows) = self.session.size();
        if self.pty_size != Some((cols, rows)) {
            self.pty.resize(cols, rows);
            self.pty_size = Some((cols, rows));
        }
        if let Err(err) = self.session.build_frame(&mut self.frame) {
            tracing::warn!("reading terminal: {err}");
        }
        self.layout = Some(GridLayout {
            origin,
            cell: metrics.cell,
            cols,
            rows,
        });

        let focused = self.focus_handle.is_focused(window);
        self.paint_data(bounds, origin, metrics, focused, window)
    }

    fn paint_data(
        &self,
        bounds: Bounds<Pixels>,
        origin: Point<Pixels>,
        metrics: Metrics,
        focused: bool,
        window: &Window,
    ) -> PaintData {
        let cell = metrics.cell;
        let at = |col: u16, row: u16| {
            point(
                origin.x + px(f32::from(col) * cell.width),
                origin.y + px(f32::from(row) * cell.height),
            )
        };
        // Snapped outwards so adjacent backgrounds leave no seams.
        let cells = |col: u16, row: u16, count: u16| {
            let start = at(col, row);
            Bounds::new(
                point(start.x.floor(), start.y),
                size(px(f32::from(count) * cell.width).ceil(), px(cell.height)),
            )
        };

        let frame = &self.frame;
        let selection = self.theme().selection.to_hsla();
        let mut rects = Vec::new();
        let mut lines = Vec::new();
        for (y, row) in frame.visible_rows().iter().enumerate() {
            let y = y as u16;
            for span in &row.backgrounds {
                rects.push((cells(span.col, y, span.cells), span.color.to_hsla()));
            }
            if let Some((start, end)) = row.selection {
                rects.push((cells(start, y, end.saturating_sub(start) + 1), selection));
            }
            for run in &row.runs {
                let line = self.shape(row.run_text(run), &run.style, run.style.fg, metrics, window);
                lines.push((at(run.col, y), line));
            }
        }

        // Output of a finished process has no live cursor.
        let cursor = frame.cursor.filter(|_| !self.read_only).map(|c| {
            let width: u16 = if c.wide { 2 } else { 1 };
            let origin = at(c.col, c.row);
            let block = Bounds::new(
                origin,
                size(px(f32::from(width) * cell.width), px(cell.height)),
            );
            let shape = if focused {
                c.shape
            } else {
                CursorShape::HollowBlock
            };
            let (bounds, text) = match shape {
                CursorShape::Block => {
                    let text = (!frame.cursor_text.is_empty()).then(|| {
                        let line =
                            self.shape(&frame.cursor_text, &c.style, c.text_color, metrics, window);
                        (origin, line)
                    });
                    (block, text)
                }
                CursorShape::HollowBlock => (block, None),
                CursorShape::Bar => (Bounds::new(origin, size(px(2.0), px(cell.height))), None),
                CursorShape::Underline => (
                    Bounds::new(
                        point(origin.x, origin.y + px(cell.height - 2.0)),
                        size(block.size.width, px(2.0)),
                    ),
                    None,
                ),
            };
            CursorPaint {
                bounds,
                color: c.color.to_hsla(),
                hollow: shape == CursorShape::HollowBlock,
                text,
            }
        });

        let scrollbar = frame
            .scrollbar
            .filter(|s| !s.at_bottom() && s.total > 0)
            .map(|s| {
                let track = f32::from(frame.row_count as u16) * cell.height;
                let thumb =
                    (s.len as f32 / s.total as f32 * track).clamp(16.0_f32.min(track), track);
                let scrollable = s.total.saturating_sub(s.len).max(1) as f32;
                let top = s.offset as f32 / scrollable * (track - thumb);
                let color: Hsla = frame.foreground.to_hsla().opacity(0.35);
                (
                    Bounds::new(
                        point(
                            bounds.origin.x + bounds.size.width - px(SCROLLBAR_WIDTH + 1.0),
                            origin.y + px(top),
                        ),
                        size(px(SCROLLBAR_WIDTH), px(thumb)),
                    ),
                    color,
                )
            });

        PaintData {
            background: frame.background.to_hsla(),
            rects,
            lines,
            cursor,
            scrollbar,
            line_height: px(cell.height),
        }
    }

    fn shape(
        &self,
        text: &str,
        style: &TextStyle,
        color: Rgb,
        metrics: Metrics,
        window: &Window,
    ) -> ShapedLine {
        let color = color.to_hsla();
        let run = TextRun {
            len: text.len(),
            font: self.fonts.get(style.bold, style.italic).clone(),
            color,
            background_color: None,
            underline: (style.underline != UnderlineKind::None).then(|| UnderlineStyle {
                thickness: px(1.0),
                color: Some(style.underline_color.map_or(color, Rgb::to_hsla)),
                wavy: style.underline == UnderlineKind::Curly,
            }),
            strikethrough: style.strikethrough.then_some(StrikethroughStyle {
                thickness: px(1.0),
                color: Some(color),
            }),
        };
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        // Keyed by hash so unchanged lines are neither copied nor reshaped.
        window.text_system().shape_line_by_hash(
            hasher.finish(),
            text.len(),
            metrics.font_size,
            &[run],
            Some(px(metrics.cell.width)),
            || SharedString::from(text.to_owned()),
        )
    }
}

fn mouse_button(button: MouseButton) -> Option<mouse::Button> {
    match button {
        MouseButton::Left => Some(mouse::Button::Left),
        MouseButton::Right => Some(mouse::Button::Right),
        MouseButton::Middle => Some(mouse::Button::Middle),
        MouseButton::Navigate(_) => None,
    }
}

fn key_mods(modifiers: &Modifiers) -> key::Mods {
    let mut mods = key::Mods::empty();
    mods.set(key::Mods::SHIFT, modifiers.shift);
    mods.set(key::Mods::ALT, modifiers.alt);
    mods.set(key::Mods::CTRL, modifiers.control);
    mods.set(key::Mods::SUPER, modifiers.platform);
    mods
}

/// Feeds the replay, then live output. Chunks that are already waiting are
/// fed together so a burst of output costs one repaint.
fn spawn_feed(feed: TerminalFeed, cx: &mut Context<TerminalView>) -> Task<()> {
    let TerminalFeed { replay, mut live } = feed;
    cx.spawn(async move |this, cx| {
        if !replay.is_empty()
            && this
                .update(cx, |view, cx| view.write_output(&replay, cx))
                .is_err()
        {
            return;
        }
        let mut batch = Vec::new();
        while let Some(first) = live.next().await {
            let mut ended = false;
            batch.clear();
            while batch.len() + first.len() < MAX_BATCH {
                match live.next().now_or_never() {
                    Some(Some(more)) => {
                        if batch.is_empty() {
                            batch.extend_from_slice(&first);
                        }
                        batch.extend_from_slice(&more);
                    }
                    Some(None) => {
                        ended = true;
                        break;
                    }
                    None => break,
                }
            }
            let data = if batch.is_empty() {
                &first[..]
            } else {
                &batch[..]
            };
            if this
                .update(cx, |view, cx| view.write_output(data, cx))
                .is_err()
                || ended
            {
                return;
            }
            // Let the window paint between bursts.
            YieldNow(false).await;
        }
    })
}

struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_hidden()
            .on_key_down(cx.listener(Self::on_key_down))
            .on_key_up(cx.listener(Self::on_key_up))
            .on_action(cx.listener(Self::copy_selection))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(|this, _: &Clear, _, cx| this.clear(cx)))
            .on_action(cx.listener(|this, _: &SendTab, _, cx| this.send_tab(false, cx)))
            .on_action(cx.listener(|this, _: &SendBacktab, _, cx| this.send_tab(true, cx)))
            .on_action(cx.listener(|this, _: &ScrollPageUp, _, cx| {
                this.scroll_by(ScrollViewport::Delta(-this.page()), cx);
            }))
            .on_action(cx.listener(|this, _: &ScrollPageDown, _, cx| {
                this.scroll_by(ScrollViewport::Delta(this.page()), cx);
            }))
            .on_action(cx.listener(|this, _: &ScrollToTop, _, cx| {
                this.scroll_by(ScrollViewport::Top, cx);
            }))
            .on_action(cx.listener(|this, _: &ScrollToBottom, _, cx| {
                this.scroll_by(ScrollViewport::Bottom, cx);
            }))
            .child(TerminalElement::new(cx.entity()))
    }
}
