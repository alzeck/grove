//! The libghostty terminal plus everything that talks to it: effects,
//! input encoding, selection, scrolling. Knows nothing about GPUI.

use std::{cell::Cell, fmt::Write as _, rc::Rc, sync::Arc};

use libghostty_vt::{
    Error, Terminal, TerminalOptions,
    fmt::Format,
    focus,
    key::{self, OptionAsAlt},
    mouse, paste,
    screen::{Screen, TrackedGridRef},
    selection::{FormatOptions, SelectLineOptions, SelectWordOptions, Selection},
    terminal::{
        ColorScheme, ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType, Mode,
        Point, PointCoordinate, PrimaryDeviceAttributes, ScrollViewport, SecondaryDeviceAttributes,
        SizeReportSize, TertiaryDeviceAttributes,
    },
};

use crate::{
    PtyHandle,
    frame::{Frame, FrameBuilder},
    input::KeyInput,
    theme::TerminalTheme,
};

/// DA1 `CSI ? 62 ; 22 c` (VT220 with ANSI colour), like Ghostty.
const DEVICE_ATTRIBUTES: DeviceAttributes = DeviceAttributes {
    primary: PrimaryDeviceAttributes::new(
        ConformanceLevel::VT220,
        &[DeviceAttributeFeature::ANSI_COLOR],
    ),
    secondary: SecondaryDeviceAttributes {
        device_type: DeviceType::VT220,
        firmware_version: 1,
        rom_cartridge: 0,
    },
    tertiary: TertiaryDeviceAttributes { unit_id: 0 },
};

/// State the effect callbacks read while the terminal parses output.
struct Shared {
    size: Cell<SizeReportSize>,
    dark: Cell<bool>,
}

/// Where a mouse event happened, for the mouse encoder.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MousePosition {
    /// Relative to the grid origin, in logical pixels.
    pub x: f32,
    pub y: f32,
    pub size: mouse::EncoderSize,
}

pub(crate) struct Session {
    // Dropped before the terminal it points into.
    anchor: Option<TrackedGridRef>,
    terminal: Terminal<'static, 'static>,
    pty: Arc<dyn PtyHandle>,
    shared: Rc<Shared>,
    frames: FrameBuilder,
    key_encoder: key::Encoder<'static>,
    key_event: key::Event<'static>,
    mouse_encoder: mouse::Encoder<'static>,
    mouse_event: mouse::Event<'static>,
    out: Vec<u8>,
    cols: u16,
    rows: u16,
    cell_px: (u32, u32),
}

impl Session {
    pub fn new(
        cols: u16,
        rows: u16,
        scrollback: usize,
        pty: Arc<dyn PtyHandle>,
        theme: &TerminalTheme,
        dark: bool,
    ) -> Result<Self, Error> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut terminal = Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: scrollback,
        })?;
        let shared = Rc::new(Shared {
            size: Cell::new(SizeReportSize {
                rows,
                columns: cols,
                cell_width: 0,
                cell_height: 0,
            }),
            dark: Cell::new(dark),
        });

        terminal
            .on_pty_write({
                let pty = pty.clone();
                move |_, data| pty.write(data)
            })?
            .on_size({
                let shared = shared.clone();
                move |_| Some(shared.size.get())
            })?
            .on_color_scheme({
                let shared = shared.clone();
                move |_| {
                    Some(if shared.dark.get() {
                        ColorScheme::Dark
                    } else {
                        ColorScheme::Light
                    })
                }
            })?
            .on_device_attributes(|_| Some(DEVICE_ATTRIBUTES))?
            .on_xtversion(|_| Some("grove"))?;

        let mut session = Self {
            anchor: None,
            terminal,
            pty,
            shared,
            frames: FrameBuilder::new()?,
            key_encoder: key::Encoder::new()?,
            key_event: key::Event::new()?,
            mouse_encoder: mouse::Encoder::new()?,
            mouse_event: mouse::Event::new()?,
            out: Vec::with_capacity(64),
            cols,
            rows,
            cell_px: (0, 0),
        };
        session.set_theme(theme, dark)?;
        Ok(session)
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Feeds process output to the terminal.
    pub fn feed(&mut self, data: &[u8]) {
        self.terminal.vt_write(data);
    }

    pub fn set_theme(&mut self, theme: &TerminalTheme, dark: bool) -> Result<(), Error> {
        self.terminal
            .set_default_fg_color(Some(theme.foreground.into()))?
            .set_default_bg_color(Some(theme.background.into()))?
            .set_default_cursor_color(Some(theme.cursor.into()))?
            .set_default_color_palette(Some(theme.ghostty_palette()))?;
        if self.shared.dark.replace(dark) != dark
            && self.terminal.mode(Mode::COLOR_SCHEME_REPORT)?
        {
            let scheme = if dark {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            };
            let mut buf = [0u8; 16];
            let len = scheme.encode_report(&mut buf)?;
            self.pty.write(&buf[..len]);
        }
        Ok(())
    }

    /// Returns whether the grid size changed.
    pub fn resize(
        &mut self,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> Result<bool, Error> {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let cell_px = (cell_width_px, cell_height_px);
        if (cols, rows) == (self.cols, self.rows) && cell_px == self.cell_px {
            return Ok(false);
        }
        self.terminal
            .resize(cols, rows, cell_width_px, cell_height_px)?;
        self.shared.size.set(SizeReportSize {
            rows,
            columns: cols,
            cell_width: cell_width_px,
            cell_height: cell_height_px,
        });
        let grid_changed = (cols, rows) != (self.cols, self.rows);
        self.cols = cols;
        self.rows = rows;
        self.cell_px = cell_px;
        Ok(grid_changed)
    }

    pub fn build_frame(&mut self, frame: &mut Frame) -> Result<(), Error> {
        self.frames.build(&self.terminal, frame)
    }

    /// Encodes a key event and writes it to the PTY. Returns whether the key
    /// produced any input.
    pub fn send_key(
        &mut self,
        input: KeyInput<'_>,
        action: key::Action,
        option_as_alt: bool,
    ) -> Result<bool, Error> {
        self.key_event
            .set_action(action)
            .set_key(input.key)
            .set_mods(input.mods)
            .set_consumed_mods(input.consumed)
            .set_unshifted_codepoint(input.unshifted)
            .set_utf8(input.text);
        self.key_encoder
            .set_options_from_terminal(&self.terminal)
            .set_macos_option_as_alt(if option_as_alt {
                OptionAsAlt::True
            } else {
                OptionAsAlt::False
            });
        self.out.clear();
        self.key_encoder
            .encode_to_vec(&self.key_event, &mut self.out)?;
        Ok(self.write_out())
    }

    /// Writes pasted text, bracketed when the program asked for it.
    pub fn paste(&mut self, text: &str) -> Result<(), Error> {
        let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE)?;
        let mut data = text.as_bytes().to_vec();
        // Encoding only replaces bytes and adds the two 6-byte brackets.
        self.out.clear();
        self.out.resize(data.len() + 16, 0);
        let len = paste::encode(&mut data, bracketed, &mut self.out)?;
        self.out.truncate(len);
        self.write_out();
        Ok(())
    }

    pub fn focus_changed(&mut self, focused: bool) -> Result<(), Error> {
        if self.terminal.mode(Mode::FOCUS_EVENT)? {
            let event = if focused {
                focus::Event::Gained
            } else {
                focus::Event::Lost
            };
            let mut buf = [0u8; 8];
            let len = event.encode(&mut buf)?;
            self.pty.write(&buf[..len]);
        }
        Ok(())
    }

    fn write_out(&mut self) -> bool {
        if self.out.is_empty() {
            return false;
        }
        self.pty.write(&self.out);
        true
    }

    /// Clears scrollback and screen, keeping the cursor's line (usually the
    /// prompt) at the top. Full-screen programs own the alternate screen, so
    /// it is left alone.
    pub fn clear(&mut self) -> Result<(), Error> {
        self.clear_selection()?;
        if self.terminal.active_screen()? == Screen::Alternate {
            return Ok(());
        }
        let x = self.terminal.cursor_x()?;
        let y = self.terminal.cursor_y()?;
        let mut seq = String::with_capacity(32);
        if y > 0 {
            // Scroll up so the cursor's line becomes the first.
            let _ = write!(seq, "\x1b[{y}S");
        }
        if self.rows > 1 {
            seq.push_str("\x1b[2;1H\x1b[J");
        }
        let _ = write!(seq, "\x1b[1;{}H\x1b[3J", x + 1);
        self.terminal.vt_write(seq.as_bytes());
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
        Ok(())
    }

    pub fn scroll(&mut self, scroll: ScrollViewport) {
        self.terminal.scroll_viewport(scroll);
    }

    pub fn is_alternate_screen(&self) -> bool {
        self.terminal
            .active_screen()
            .is_ok_and(|s| s == Screen::Alternate)
    }

    // ---- Selection -------------------------------------------------------

    fn viewport_point(col: u16, row: u16) -> Point {
        Point::Viewport(PointCoordinate {
            x: col,
            y: u32::from(row),
        })
    }

    /// Starts a selection at a viewport cell. A single click only anchors
    /// it; double and triple clicks select the word or line.
    pub fn begin_selection(&mut self, col: u16, row: u16, clicks: usize) -> Result<(), Error> {
        self.clear_selection()?;
        let point = Self::viewport_point(col, row);
        match clicks {
            0 | 1 => self.anchor = Some(self.terminal.track_grid_ref(point)?),
            2 => {
                let at = self.terminal.grid_ref(point)?;
                let word = self.terminal.select_word(SelectWordOptions::new(at))?;
                self.terminal.set_selection(word.as_ref())?;
            }
            _ => {
                let at = self.terminal.grid_ref(point)?;
                let line = self.terminal.select_line(SelectLineOptions::new(at))?;
                self.terminal.set_selection(line.as_ref())?;
            }
        }
        Ok(())
    }

    /// Selects from the anchor to a viewport cell (inclusive).
    pub fn extend_selection(&mut self, col: u16, row: u16) -> Result<(), Error> {
        let Some(anchor) = &self.anchor else {
            return Ok(());
        };
        let Some(start) = anchor.snapshot(&self.terminal)? else {
            return Ok(());
        };
        let end = self.terminal.grid_ref(Self::viewport_point(col, row))?;
        self.terminal
            .set_selection(Some(&Selection::new(start, end, false)))?;
        Ok(())
    }

    pub fn end_selection(&mut self) {
        self.anchor = None;
    }

    pub fn clear_selection(&mut self) -> Result<(), Error> {
        self.anchor = None;
        self.terminal.set_selection(None)?;
        Ok(())
    }

    /// The selected text, with soft-wrapped lines joined and trailing
    /// whitespace trimmed.
    pub fn selection_text(&self) -> Result<Option<String>, Error> {
        let options = FormatOptions::new()
            .with_emit_format(Format::Plain)
            .with_unwrap(true)
            .with_trim(true);
        let bytes = self.terminal.format_selection_alloc(None, options)?;
        Ok(bytes
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .filter(|s| !s.is_empty()))
    }

    // ---- Mouse reporting ---------------------------------------------------

    pub fn mouse_tracking(&self) -> bool {
        self.terminal.is_mouse_tracking().unwrap_or(false)
    }

    /// Reports a mouse event to the program (when it enabled mouse tracking).
    pub fn send_mouse(
        &mut self,
        action: mouse::Action,
        button: Option<mouse::Button>,
        mods: key::Mods,
        at: MousePosition,
        any_button_pressed: bool,
    ) -> Result<(), Error> {
        self.mouse_event
            .set_action(action)
            .set_button(button)
            .set_mods(mods)
            .set_position(mouse::Position { x: at.x, y: at.y });
        self.mouse_encoder
            .set_options_from_terminal(&self.terminal)
            .set_size(at.size)
            .set_any_button_pressed(any_button_pressed)
            .set_track_last_cell(true);
        self.out.clear();
        self.mouse_encoder
            .encode_to_vec(&self.mouse_event, &mut self.out)?;
        self.write_out();
        Ok(())
    }

    /// Scrolls by `lines` (positive towards history). With `to_program`,
    /// goes to the program as wheel buttons when it tracks the mouse, or as
    /// arrow keys on the alternate screen (mode 1007). Otherwise scrolls the
    /// viewport.
    pub fn wheel(
        &mut self,
        lines: i32,
        mods: key::Mods,
        at: MousePosition,
        to_program: bool,
    ) -> Result<(), Error> {
        if lines == 0 {
            return Ok(());
        }
        let count = lines.unsigned_abs().min(64);
        if to_program && self.mouse_tracking() {
            let button = if lines > 0 {
                mouse::Button::Four
            } else {
                mouse::Button::Five
            };
            // Wheel "buttons" are only ever pressed, as in xterm and Ghostty.
            for _ in 0..count {
                self.send_mouse(mouse::Action::Press, Some(button), mods, at, false)?;
            }
        } else if to_program
            && self.is_alternate_screen()
            && self.terminal.mode(Mode::ALT_SCROLL)?
        {
            let key = if lines > 0 {
                key::Key::ArrowUp
            } else {
                key::Key::ArrowDown
            };
            let input = KeyInput {
                key,
                unshifted: '\0',
                mods: key::Mods::empty(),
                consumed: key::Mods::empty(),
                text: None,
            };
            for _ in 0..count {
                self.send_key(input, key::Action::Press, false)?;
            }
        } else {
            self.scroll(ScrollViewport::Delta(-(lines as isize)));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::*;
    use crate::input::map_keystroke;
    use gpui_kit::{Keystroke, Modifiers};

    #[derive(Default)]
    struct RecordingPty {
        written: Mutex<Vec<u8>>,
    }

    impl PtyHandle for RecordingPty {
        fn write(&self, data: &[u8]) {
            self.written.lock().extend_from_slice(data);
        }
        fn resize(&self, _cols: u16, _rows: u16) {}
    }

    impl RecordingPty {
        fn take(&self) -> Vec<u8> {
            std::mem::take(&mut *self.written.lock())
        }
    }

    fn session(cols: u16, rows: u16) -> (Session, Arc<RecordingPty>) {
        let pty = Arc::new(RecordingPty::default());
        let session =
            Session::new(cols, rows, 100, pty.clone(), &TerminalTheme::dark(), true).unwrap();
        (session, pty)
    }

    fn press(session: &mut Session, key: &str, key_char: Option<&str>, modifiers: Modifiers) {
        let keystroke = Keystroke {
            modifiers,
            key: key.into(),
            key_char: key_char.map(Into::into),
        };
        session
            .send_key(map_keystroke(&keystroke, false), key::Action::Press, false)
            .unwrap();
    }

    fn row_text(session: &mut Session, row: usize) -> String {
        let mut frame = Frame::default();
        session.build_frame(&mut frame).unwrap();
        let row = &frame.visible_rows()[row];
        let mut text = String::new();
        for run in &row.runs {
            while text.chars().count() < usize::from(run.col) {
                text.push(' ');
            }
            text.push_str(row.run_text(run));
        }
        text
    }

    #[test]
    fn answers_queries_through_the_pty() {
        let (mut session, pty) = session(80, 24);
        session.feed(b"\x1b[c");
        assert_eq!(pty.take(), b"\x1b[?62;22c");
        session.feed(b"\x1b[6n");
        assert_eq!(pty.take(), b"\x1b[1;1R");
    }

    #[test]
    fn encodes_keys() {
        let (mut session, pty) = session(80, 24);
        let none = Modifiers::default();
        press(&mut session, "a", Some("a"), none);
        press(
            &mut session,
            "c",
            None,
            Modifiers {
                control: true,
                ..none
            },
        );
        press(&mut session, "enter", Some("\n"), none);
        press(&mut session, "backspace", None, none);
        press(&mut session, "up", None, none);
        press(
            &mut session,
            "tab",
            None,
            Modifiers {
                shift: true,
                ..none
            },
        );
        assert_eq!(pty.take(), b"a\x03\r\x7f\x1b[A\x1b[Z");

        // Application cursor keys (DECCKM).
        session.feed(b"\x1b[?1h");
        press(&mut session, "up", None, none);
        assert_eq!(pty.take(), b"\x1bOA");
    }

    #[test]
    fn pastes_with_brackets_when_enabled() {
        let (mut session, pty) = session(80, 24);
        session.paste("ls\nexit").unwrap();
        assert_eq!(pty.take(), b"ls\rexit");
        session.feed(b"\x1b[?2004h");
        session.paste("echo hi").unwrap();
        assert_eq!(pty.take(), b"\x1b[200~echo hi\x1b[201~");
    }

    #[test]
    fn resize_reports_changes() {
        let (mut session, _) = session(80, 24);
        assert!(session.resize(100, 30, 8, 16).unwrap());
        assert!(!session.resize(100, 30, 8, 16).unwrap());
        assert!(!session.resize(100, 30, 9, 18).unwrap());
        assert_eq!(session.size(), (100, 30));
        assert!(session.resize(0, 0, 8, 16).unwrap());
        assert_eq!(session.size(), (1, 1));
    }

    #[test]
    fn clear_keeps_the_cursor_line() {
        let (mut session, _) = session(20, 5);
        session.feed(b"one\r\ntwo\r\n$ ls");
        session.clear().unwrap();
        assert_eq!(row_text(&mut session, 0), "$ ls");
        assert_eq!(row_text(&mut session, 1), "");
        let mut frame = Frame::default();
        session.build_frame(&mut frame).unwrap();
        let cursor = frame.cursor.unwrap();
        assert_eq!((cursor.col, cursor.row), (4, 0));
        assert!(frame.scrollbar.unwrap().total <= 5);
    }

    #[test]
    fn selects_and_copies_text() {
        let (mut session, _) = session(20, 5);
        session.feed(b"hello world\r\nsecond line");
        assert_eq!(session.selection_text().unwrap(), None);

        session.begin_selection(6, 0, 1).unwrap();
        session.extend_selection(5, 1).unwrap();
        assert_eq!(
            session.selection_text().unwrap().as_deref(),
            Some("world\nsecond")
        );

        // Output arriving meanwhile doesn't lose the anchor.
        session.feed(b"\r\nmore");
        session.extend_selection(10, 0).unwrap();
        assert_eq!(session.selection_text().unwrap().as_deref(), Some("world"));

        session.begin_selection(1, 1, 2).unwrap();
        assert_eq!(session.selection_text().unwrap().as_deref(), Some("second"));

        session.clear_selection().unwrap();
        assert_eq!(session.selection_text().unwrap(), None);
    }

    #[test]
    fn wheel_scrolls_history_or_reports_to_the_program() {
        let (mut session, pty) = session(10, 3);
        for i in 0..10 {
            session.feed(format!("line {i}\r\n").as_bytes());
        }
        let at = MousePosition {
            x: 1.0,
            y: 1.0,
            size: mouse::EncoderSize {
                screen_width: 80,
                screen_height: 48,
                cell_width: 8,
                cell_height: 16,
                padding_top: 0,
                padding_bottom: 0,
                padding_right: 0,
                padding_left: 0,
            },
        };
        session.wheel(2, key::Mods::empty(), at, true).unwrap();
        let mut frame = Frame::default();
        session.build_frame(&mut frame).unwrap();
        assert!(!frame.scrollbar.unwrap().at_bottom());
        assert!(pty.take().is_empty());

        session.scroll(ScrollViewport::Bottom);
        session.feed(b"\x1b[?1000h\x1b[?1006h");
        session.wheel(1, key::Mods::empty(), at, true).unwrap();
        assert_eq!(pty.take(), b"\x1b[<64;1;1M");

        session.feed(b"\x1b[?1000l\x1b[?1049h");
        session.wheel(-1, key::Mods::empty(), at, true).unwrap();
        assert_eq!(pty.take(), b"\x1b[B");
    }
}
