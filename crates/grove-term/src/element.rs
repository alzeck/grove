use gpui_kit::{
    App, BorderStyle, Bounds, CursorStyle, DispatchPhase, Element, ElementId, Entity,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent, ShapedLine,
    Style, TextAlign, Window, fill, outline, relative,
};

use crate::view::TerminalView;

/// Everything needed to paint one frame, prepared by the view.
pub(crate) struct PaintData {
    pub background: Hsla,
    /// Cell backgrounds and selection, painted under the text.
    pub rects: Vec<(Bounds<Pixels>, Hsla)>,
    pub lines: Vec<(Point<Pixels>, ShapedLine)>,
    pub cursor: Option<CursorPaint>,
    pub scrollbar: Option<(Bounds<Pixels>, Hsla)>,
    pub line_height: Pixels,
}

pub(crate) struct CursorPaint {
    pub bounds: Bounds<Pixels>,
    pub color: Hsla,
    pub hollow: bool,
    /// The character under a block cursor, in the cell's background colour.
    pub text: Option<(Point<Pixels>, ShapedLine)>,
}

/// Paints a [`TerminalView`]'s grid and routes mouse input to it. The grid
/// is sized to the element's bounds during prepaint.
pub(crate) struct TerminalElement {
    view: Entity<TerminalView>,
}

impl TerminalElement {
    pub fn new(view: Entity<TerminalView>) -> Self {
        Self { view }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

pub(crate) struct Prepainted {
    hitbox: Hitbox,
    paint: PaintData,
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let paint = self
            .view
            .update(cx, |view, _| view.prepaint(bounds, window));
        Prepainted { hitbox, paint }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepainted: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let hitbox = &prepainted.hitbox;
        let data = &prepainted.paint;
        window.set_cursor_style(CursorStyle::IBeam, hitbox);
        self.register_mouse_listeners(hitbox, window);

        window.with_content_mask(Some(gpui_kit::ContentMask { bounds }), |window| {
            window.paint_quad(fill(bounds, data.background));
            for (rect, color) in &data.rects {
                window.paint_quad(fill(*rect, *color));
            }
            for (origin, line) in &data.lines {
                paint_line(line, *origin, data.line_height, window, cx);
            }
            if let Some(cursor) = &data.cursor {
                if cursor.hollow {
                    window.paint_quad(outline(cursor.bounds, cursor.color, BorderStyle::Solid));
                } else {
                    window.paint_quad(fill(cursor.bounds, cursor.color));
                }
                if let Some((origin, line)) = &cursor.text {
                    paint_line(line, *origin, data.line_height, window, cx);
                }
            }
            if let Some((rect, color)) = data.scrollbar {
                window.paint_quad(fill(rect, color).corner_radii(rect.size.width / 2.0));
            }
        });
    }
}

impl TerminalElement {
    fn register_mouse_listeners(&self, hitbox: &Hitbox, window: &mut Window) {
        window.on_mouse_event({
            let view = self.view.clone();
            let hitbox = hitbox.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                    view.update(cx, |view, cx| view.mouse_down(event, window, cx));
                    cx.stop_propagation();
                }
            }
        });
        // Moves and releases are handled outside the bounds too, so a drag
        // that leaves the terminal keeps selecting.
        window.on_mouse_event({
            let view = self.view.clone();
            let hitbox = hitbox.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    let hovered = hitbox.is_hovered(window);
                    view.update(cx, |view, cx| view.mouse_move(event, hovered, cx));
                }
            }
        });
        window.on_mouse_event({
            let view = self.view.clone();
            move |event: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Bubble {
                    view.update(cx, |view, cx| view.mouse_up(event, cx));
                }
            }
        });
        window.on_mouse_event({
            let view = self.view.clone();
            let hitbox = hitbox.clone();
            move |event: &ScrollWheelEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble && hitbox.should_handle_scroll(window) {
                    view.update(cx, |view, cx| view.scroll_wheel(event, cx));
                    cx.stop_propagation();
                }
            }
        });
    }
}

fn paint_line(
    line: &ShapedLine,
    origin: Point<Pixels>,
    line_height: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    if let Err(err) = line.paint(origin, line_height, TextAlign::Left, None, window, cx) {
        tracing::warn!("painting terminal text: {err}");
    }
}
