//! A GPUI-free snapshot of the visible screen: per-row text runs (adjacent
//! cells with the same style batched together), background spans, selection
//! and cursor. Built from libghostty's render state, painted by the element.

use std::ops::Range;

use libghostty_vt::{
    RenderState, Terminal,
    render::{CellIterator, CursorVisualStyle, Dirty, RowIterator},
    screen::CellWide,
    style::{RgbColor, Style, StyleColor, Underline},
};

use crate::theme::Rgb;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum UnderlineKind {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

impl From<Underline> for UnderlineKind {
    fn from(u: Underline) -> Self {
        match u {
            Underline::Single => UnderlineKind::Single,
            Underline::Double => UnderlineKind::Double,
            Underline::Curly => UnderlineKind::Curly,
            Underline::Dotted => UnderlineKind::Dotted,
            Underline::Dashed => UnderlineKind::Dashed,
            _ => UnderlineKind::None,
        }
    }
}

/// How the text of a cell is drawn. Background is handled separately.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextStyle {
    pub fg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: UnderlineKind,
    pub underline_color: Option<Rgb>,
    pub strikethrough: bool,
}

impl TextStyle {
    fn has_decoration(&self) -> bool {
        self.underline != UnderlineKind::None || self.strikethrough
    }
}

/// Adjacent cells of one row drawn with the same style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextRun {
    pub col: u16,
    pub cells: u16,
    /// Byte range into [`FrameRow::text`].
    pub text: Range<usize>,
    pub style: TextStyle,
    /// Whether more cells may be appended. Glyphs are forced onto the cell
    /// grid one per cell, so a wide glyph must end its run.
    open: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BgSpan {
    pub col: u16,
    pub cells: u16,
    pub color: Rgb,
}

#[derive(Debug, Default)]
pub(crate) struct FrameRow {
    pub text: String,
    pub runs: Vec<TextRun>,
    pub backgrounds: Vec<BgSpan>,
    /// Selected columns, inclusive.
    pub selection: Option<(u16, u16)>,
}

impl FrameRow {
    pub fn clear(&mut self) {
        self.text.clear();
        self.runs.clear();
        self.backgrounds.clear();
        self.selection = None;
    }

    pub fn run_text(&self, run: &TextRun) -> &str {
        &self.text[run.text.clone()]
    }

    /// Adds a non-default background, merging with the previous span when
    /// adjacent and the same colour.
    pub fn push_background(&mut self, col: u16, cells: u16, color: Rgb) {
        if let Some(last) = self.backgrounds.last_mut()
            && last.color == color
            && last.col + last.cells == col
        {
            last.cells += cells;
            return;
        }
        self.backgrounds.push(BgSpan { col, cells, color });
    }

    /// Adds one cell's grapheme. Cells must be pushed left to right. Blank
    /// cells in between are skipped by the caller and filled with spaces
    /// when the run continues past them.
    pub fn push_cell(&mut self, col: u16, cells: u16, grapheme: &[char], style: TextStyle) {
        if let Some(last) = self.runs.last_mut()
            && last.open
            && last.style == style
            && col >= last.col + last.cells
        {
            let gap = col - (last.col + last.cells);
            // Bridging a gap with spaces is invisible unless the run is
            // decorated, in which case the gap would get underlined.
            if gap == 0 || !style.has_decoration() {
                self.text.extend(std::iter::repeat_n(' ', usize::from(gap)));
                self.text.extend(grapheme.iter().copied());
                last.cells += gap + cells;
                last.text.end = self.text.len();
                last.open = cells == 1;
                return;
            }
        }
        let start = self.text.len();
        self.text.extend(grapheme.iter().copied());
        self.runs.push(TextRun {
            col,
            cells,
            text: start..self.text.len(),
            style,
            open: cells == 1,
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorShape {
    Block,
    HollowBlock,
    Bar,
    Underline,
}

impl From<CursorVisualStyle> for CursorShape {
    fn from(style: CursorVisualStyle) -> Self {
        match style {
            CursorVisualStyle::Bar => CursorShape::Bar,
            CursorVisualStyle::Underline => CursorShape::Underline,
            CursorVisualStyle::BlockHollow => CursorShape::HollowBlock,
            _ => CursorShape::Block,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameCursor {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
    pub color: Rgb,
    /// Two cells wide (on a wide character).
    pub wide: bool,
    /// Style of the character under the cursor, drawn in `text_color` on a
    /// block cursor. The text itself is in [`Frame::cursor_text`].
    pub style: TextStyle,
    pub text_color: Rgb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Scrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

impl Scrollbar {
    pub fn at_bottom(&self) -> bool {
        self.offset + self.len >= self.total
    }
}

#[derive(Debug, Default)]
pub(crate) struct Frame {
    pub cols: u16,
    /// Only the first `row_count` rows are valid; the rest are kept to reuse
    /// their allocations.
    pub rows: Vec<FrameRow>,
    pub row_count: usize,
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Option<FrameCursor>,
    pub cursor_text: String,
    pub scrollbar: Option<Scrollbar>,
}

impl Frame {
    pub fn visible_rows(&self) -> &[FrameRow] {
        &self.rows[..self.row_count]
    }

    fn reset(&mut self, rows: usize) {
        if self.rows.len() < rows {
            self.rows.resize_with(rows, FrameRow::default);
        }
        for row in &mut self.rows[..rows] {
            row.clear();
        }
        self.row_count = rows;
        self.cursor = None;
        self.cursor_text.clear();
        self.scrollbar = None;
    }
}

/// Foreground and background after applying inverse, faint and invisible.
/// `None` background means the terminal default.
pub(crate) fn resolve_colors(
    fg: Option<Rgb>,
    bg: Option<Rgb>,
    style: &Style,
    default_fg: Rgb,
    default_bg: Rgb,
) -> (Rgb, Option<Rgb>) {
    let (mut fg, bg) = if style.inverse {
        (bg.unwrap_or(default_bg), Some(fg.unwrap_or(default_fg)))
    } else {
        (fg.unwrap_or(default_fg), bg)
    };
    if style.faint {
        fg = fg.mix(bg.unwrap_or(default_bg), 0.5);
    }
    (fg, bg)
}

fn resolve_style_color(color: StyleColor, palette: &[RgbColor; 256]) -> Option<Rgb> {
    match color {
        StyleColor::None => None,
        StyleColor::Palette(index) => Some(palette[usize::from(index.0)].into()),
        StyleColor::Rgb(rgb) => Some(rgb.into()),
    }
}

/// Reads the terminal's viewport into a [`Frame`], reusing its buffers.
pub(crate) struct FrameBuilder {
    render_state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    grapheme: Vec<char>,
    default_style: Style,
}

impl FrameBuilder {
    pub fn new() -> Result<Self, libghostty_vt::Error> {
        Ok(Self {
            render_state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            grapheme: Vec::with_capacity(8),
            default_style: Style::default(),
        })
    }

    pub fn build(
        &mut self,
        terminal: &Terminal<'static, 'static>,
        frame: &mut Frame,
    ) -> Result<(), libghostty_vt::Error> {
        let snapshot = self.render_state.update(terminal)?;
        let colors = snapshot.colors()?;
        let default_fg = Rgb::from(colors.foreground);
        let default_bg = Rgb::from(colors.background);
        let row_count = usize::from(snapshot.rows()?);

        frame.reset(row_count);
        frame.cols = snapshot.cols()?;
        frame.foreground = default_fg;
        frame.background = default_bg;

        let cursor = if snapshot.cursor_visible()? {
            snapshot.cursor_viewport()?.map(|c| {
                // On the right half of a wide character: draw on the character.
                let col = if c.at_wide_tail {
                    c.x.saturating_sub(1)
                } else {
                    c.x
                };
                (col, c.y)
            })
        } else {
            None
        };
        let cursor_shape = CursorShape::from(snapshot.cursor_visual_style()?);
        let cursor_color = colors.cursor.map_or(default_fg, Rgb::from);

        let mut rows = self.rows.update(&snapshot)?;
        let mut y: u16 = 0;
        while let Some(row) = rows.next() {
            let Some(out) = frame.rows.get_mut(usize::from(y)) else {
                break;
            };
            out.selection = row.selection()?.map(|s| (s.start_x, s.end_x));

            let mut cells = self.cells.update(row)?;
            let mut x: u16 = 0;
            while let Some(cell) = cells.next() {
                let col = x;
                x += 1;

                let styled = cell.has_styling()?;
                let style = if styled {
                    cell.style()?
                } else {
                    self.default_style
                };
                let (fg, bg) = resolve_colors(
                    cell.fg_color()?.map(Rgb::from),
                    cell.bg_color()?.map(Rgb::from),
                    &style,
                    default_fg,
                    default_bg,
                );
                if let Some(bg) = bg {
                    out.push_background(col, 1, bg);
                }

                let wide = cell.raw_cell()?.wide()?;
                if matches!(wide, CellWide::SpacerTail | CellWide::SpacerHead) {
                    continue;
                }
                let width = if wide == CellWide::Wide { 2 } else { 1 };
                let text_style = TextStyle {
                    fg,
                    bold: style.bold,
                    italic: style.italic,
                    underline: style.underline.into(),
                    underline_color: resolve_style_color(style.underline_color, &colors.palette),
                    strikethrough: style.strikethrough,
                };

                let len = if style.invisible {
                    0
                } else {
                    cell.graphemes_len()?
                };
                if len > 0 {
                    self.grapheme.clear();
                    self.grapheme.resize(len, '\0');
                    cell.graphemes_buf(&mut self.grapheme)?;
                }

                if cursor == Some((col, y)) {
                    frame.cursor = Some(FrameCursor {
                        col,
                        row: y,
                        shape: cursor_shape,
                        color: cursor_color,
                        wide: width == 2,
                        style: text_style,
                        text_color: bg.unwrap_or(default_bg),
                    });
                    frame
                        .cursor_text
                        .extend(self.grapheme[..len].iter().copied());
                }

                if len > 0 {
                    out.push_cell(col, width, &self.grapheme, text_style);
                } else if text_style.has_decoration() && !style.invisible {
                    out.push_cell(col, 1, &[' '], text_style);
                }
            }
            row.set_dirty(false)?;
            y += 1;
        }
        snapshot.set_dirty(Dirty::Clean)?;

        frame.scrollbar = terminal.scrollbar().ok().map(|s| Scrollbar {
            total: s.total,
            offset: s.offset,
            len: s.len,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use libghostty_vt::TerminalOptions;

    use super::*;

    fn plain(fg: u32) -> TextStyle {
        TextStyle {
            fg: Rgb::hex(fg),
            ..TextStyle::default()
        }
    }

    fn texts(row: &FrameRow) -> Vec<(u16, u16, &str)> {
        row.runs
            .iter()
            .map(|r| (r.col, r.cells, row.run_text(r)))
            .collect()
    }

    #[test]
    fn batches_adjacent_cells_with_same_style() {
        let mut row = FrameRow::default();
        let white = plain(0xffffff);
        let red = plain(0xff0000);
        for (col, c) in "ab".chars().enumerate() {
            row.push_cell(col as u16, 1, &[c], white);
        }
        row.push_cell(2, 1, &['c'], red);
        row.push_cell(3, 1, &['d'], white);
        assert_eq!(texts(&row), [(0, 2, "ab"), (2, 1, "c"), (3, 1, "d")]);
    }

    #[test]
    fn bridges_blank_gaps_with_spaces() {
        let mut row = FrameRow::default();
        let white = plain(0xffffff);
        row.push_cell(0, 1, &['a'], white);
        row.push_cell(3, 1, &['b'], white);
        assert_eq!(texts(&row), [(0, 4, "a  b")]);

        // Underlined runs must not underline the gap.
        let mut row = FrameRow::default();
        let underlined = TextStyle {
            underline: UnderlineKind::Single,
            ..white
        };
        row.push_cell(0, 1, &['a'], underlined);
        row.push_cell(2, 1, &['b'], underlined);
        assert_eq!(texts(&row), [(0, 1, "a"), (2, 1, "b")]);
    }

    #[test]
    fn wide_characters_end_their_run() {
        let mut row = FrameRow::default();
        let white = plain(0xffffff);
        row.push_cell(0, 1, &['a'], white);
        row.push_cell(1, 2, &['中'], white);
        row.push_cell(3, 1, &['b'], white);
        assert_eq!(texts(&row), [(0, 3, "a中"), (3, 1, "b")]);
    }

    #[test]
    fn combining_marks_stay_in_the_cell() {
        let mut row = FrameRow::default();
        let white = plain(0xffffff);
        row.push_cell(0, 1, &['e', '\u{301}'], white);
        row.push_cell(1, 1, &['x'], white);
        assert_eq!(texts(&row), [(0, 2, "e\u{301}x")]);
    }

    #[test]
    fn merges_background_spans() {
        let mut row = FrameRow::default();
        let blue = Rgb::hex(0x0000ff);
        row.push_background(0, 1, blue);
        row.push_background(1, 1, blue);
        row.push_background(3, 1, blue);
        row.push_background(4, 1, Rgb::hex(0x00ff00));
        assert_eq!(
            row.backgrounds,
            [
                BgSpan {
                    col: 0,
                    cells: 2,
                    color: blue
                },
                BgSpan {
                    col: 3,
                    cells: 1,
                    color: blue
                },
                BgSpan {
                    col: 4,
                    cells: 1,
                    color: Rgb::hex(0x00ff00)
                },
            ]
        );
    }

    #[test]
    fn resolves_inverse_and_faint() {
        let fg = Rgb::hex(0xffffff);
        let bg = Rgb::hex(0x000000);
        let mut style = Style::default();
        assert_eq!(resolve_colors(None, None, &style, fg, bg), (fg, None));

        style.inverse = true;
        assert_eq!(resolve_colors(None, None, &style, fg, bg), (bg, Some(fg)));
        let red = Rgb::hex(0xff0000);
        assert_eq!(
            resolve_colors(Some(red), None, &style, fg, bg),
            (bg, Some(red))
        );

        style.inverse = false;
        style.faint = true;
        assert_eq!(
            resolve_colors(None, None, &style, fg, bg),
            (Rgb::hex(0x808080), None)
        );
    }

    fn terminal(cols: u16, rows: u16) -> Terminal<'static, 'static> {
        Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: 100,
        })
        .unwrap()
    }

    #[test]
    fn reads_styled_text_from_ghostty() {
        let mut term = terminal(20, 3);
        let theme = crate::theme::TerminalTheme::dark();
        term.set_default_fg_color(Some(theme.foreground.into()))
            .unwrap()
            .set_default_bg_color(Some(theme.background.into()))
            .unwrap()
            .set_default_color_palette(Some(theme.ghostty_palette()))
            .unwrap();
        term.vt_write(b"\x1b[1;31mred\x1b[0m plain");

        let mut builder = FrameBuilder::new().unwrap();
        let mut frame = Frame::default();
        builder.build(&term, &mut frame).unwrap();

        assert_eq!(frame.cols, 20);
        assert_eq!(frame.visible_rows().len(), 3);
        assert_eq!(frame.foreground, theme.foreground);
        assert_eq!(frame.background, theme.background);

        let row = &frame.visible_rows()[0];
        assert_eq!(texts(row), [(0, 3, "red"), (3, 6, " plain")]);
        let red = &row.runs[0].style;
        assert!(red.bold);
        assert!(!red.italic);
        assert_eq!(red.fg, theme.ansi[1]);
        let plain = &row.runs[1].style;
        assert!(!plain.bold);
        assert_eq!(plain.fg, theme.foreground);
        assert!(row.backgrounds.is_empty());
        assert!(frame.visible_rows()[1].runs.is_empty());

        let cursor = frame.cursor.expect("cursor visible");
        assert_eq!((cursor.col, cursor.row), (9, 0));
        assert_eq!(cursor.shape, CursorShape::Block);
        assert_eq!(frame.cursor_text, "");
    }

    #[test]
    fn reads_colors_decorations_and_wide_chars() {
        let mut term = terminal(20, 2);
        term.vt_write(b"\x1b[38;2;1;2;3;48;5;21;4;9mA\x1b[0m\x1b[7mB\x1b[0m\xe4\xb8\xadC");
        term.vt_write(b"\x1b[2;1H\x1b[8mhidden\x1b[0m\x1b[3mi");

        let mut builder = FrameBuilder::new().unwrap();
        let mut frame = Frame::default();
        builder.build(&term, &mut frame).unwrap();
        let (fg, bg) = (frame.foreground, frame.background);

        let row = &frame.visible_rows()[0];
        assert_eq!(
            texts(row),
            [(0, 1, "A"), (1, 1, "B"), (2, 2, "中"), (4, 1, "C")]
        );
        let a = &row.runs[0].style;
        assert_eq!(a.fg, Rgb::hex(0x010203));
        assert_eq!(a.underline, UnderlineKind::Single);
        assert!(a.strikethrough);
        // Inverse B: default colours swapped.
        assert_eq!(row.runs[1].style.fg, bg);
        assert_eq!(
            row.backgrounds,
            [
                BgSpan {
                    col: 0,
                    cells: 1,
                    color: crate::theme::xterm_color(21)
                },
                BgSpan {
                    col: 1,
                    cells: 1,
                    color: fg
                },
            ]
        );

        let row = &frame.visible_rows()[1];
        assert_eq!(texts(row), [(6, 1, "i")]);
        assert!(row.runs[0].style.italic);
    }

    #[test]
    fn reports_selection_per_row() {
        use libghostty_vt::selection::Selection;
        use libghostty_vt::terminal::{Point, PointCoordinate};

        let mut term = terminal(10, 3);
        term.vt_write(b"hello\r\nworld");
        let start = term
            .grid_ref(Point::Viewport(PointCoordinate { x: 2, y: 0 }))
            .unwrap();
        let end = term
            .grid_ref(Point::Viewport(PointCoordinate { x: 1, y: 1 }))
            .unwrap();
        term.set_selection(Some(&Selection::new(start, end, false)))
            .unwrap();

        let mut builder = FrameBuilder::new().unwrap();
        let mut frame = Frame::default();
        builder.build(&term, &mut frame).unwrap();
        let rows = frame.visible_rows();
        assert_eq!(rows[0].selection, Some((2, 9)));
        assert_eq!(rows[1].selection, Some((0, 1)));
        assert_eq!(rows[2].selection, None);
    }
}
