//! UI integration tests: a real `TerminalView` in a headless window.
//!
//! The test platform's text system gives Menlo-like metrics at 13px: cells
//! are 7.8 × 16 px, so a 640 × 480 window (4px padding) holds 81 × 29 cells.

use std::sync::Arc;

use bytes::Bytes;
use futures::{StreamExt as _, channel::mpsc};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AppContext as _, Focusable as _, Keystroke, TestAppContext, WindowHandle, point, px, size,
};
use grove_term::{CopySelection, PtyHandle, TerminalFeed, TerminalView};
use parking_lot::Mutex;

#[derive(Default)]
struct RecordingPty {
    written: Mutex<Vec<u8>>,
    sizes: Mutex<Vec<(u16, u16)>>,
}

impl PtyHandle for RecordingPty {
    fn write(&self, data: &[u8]) {
        self.written.lock().extend_from_slice(data);
    }

    fn resize(&self, cols: u16, rows: u16) {
        self.sizes.lock().push((cols, rows));
    }
}

impl RecordingPty {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.written.lock())
    }
}

type Output = mpsc::UnboundedSender<Bytes>;

fn feed(replay: &'static [u8]) -> (TerminalFeed, Output) {
    let (tx, rx) = mpsc::unbounded();
    let feed = TerminalFeed {
        replay: Bytes::from_static(replay),
        live: rx.boxed(),
    };
    (feed, tx)
}

fn open(
    cx: &mut TestAppContext,
    replay: &'static [u8],
) -> (WindowHandle<TerminalView>, Arc<RecordingPty>, Output) {
    cx.update(grove_term::init);
    let pty = Arc::new(RecordingPty::default());
    let (feed, output) = feed(replay);
    let handle = cx.open_window(size(px(640.0), px(480.0)), {
        let pty = pty.clone();
        |window, cx| TerminalView::new(feed, pty, window, cx)
    });
    handle
        .update(cx, |view, window, cx| {
            window.focus(&view.focus_handle(cx), cx);
        })
        .unwrap();
    cx.run_until_parked();
    render(cx, &handle);
    (handle, pty, output)
}

fn render(cx: &mut TestAppContext, handle: &WindowHandle<TerminalView>) {
    cx.update_window((*handle).into(), |_, window, cx| window.render_frame(cx))
        .unwrap();
}

fn press(cx: &mut TestAppContext, handle: &WindowHandle<TerminalView>, keys: &[&str]) {
    cx.update_window((*handle).into(), |_, window, cx| {
        for key in keys {
            window.press(key, cx);
        }
    })
    .unwrap();
}

#[gpui_kit::test]
fn sizes_the_pty_to_the_window(cx: &mut TestAppContext) {
    let (_, pty, _) = open(cx, b"");
    assert_eq!(*pty.sizes.lock(), [(81, 29)]);
}

#[gpui_kit::test]
fn types_into_the_pty(cx: &mut TestAppContext) {
    let (handle, pty, _) = open(cx, b"");
    press(
        cx,
        &handle,
        &["l", "s", "space", "shift-a", "tab", "enter", "ctrl-c", "up"],
    );
    assert_eq!(pty.take(), b"ls A\t\r\x03\x1b[A");

    // Option-Left moves a word back, as in other macOS terminals.
    press(cx, &handle, &["alt-left", "cmd-left"]);
    assert_eq!(pty.take(), b"\x1bb\x01");
}

#[gpui_kit::test]
fn leaves_app_shortcuts_alone(cx: &mut TestAppContext) {
    let (handle, pty, _) = open(cx, b"");
    let handled = cx
        .update_window(handle.into(), |_, window, cx| {
            window.dispatch_keystroke(Keystroke::parse("cmd-w").unwrap(), cx)
        })
        .unwrap();
    assert!(!handled);
    assert!(pty.take().is_empty());
}

#[gpui_kit::test]
fn feeds_output_and_answers_queries(cx: &mut TestAppContext) {
    let (handle, pty, output) = open(cx, b"replayed\r\n\x1b[c");
    assert_eq!(pty.take(), b"\x1b[?62;22c");

    output.unbounded_send(Bytes::from_static(b"live ")).unwrap();
    output
        .unbounded_send(Bytes::from_static(b"output\x1b[6n"))
        .unwrap();
    cx.run_until_parked();
    render(cx, &handle);
    // The cursor is after "live output" on the second line.
    assert_eq!(pty.take(), b"\x1b[2;12R");
}

#[gpui_kit::test]
fn copies_a_dragged_selection(cx: &mut TestAppContext) {
    let (handle, _, output) = open(cx, b"");
    output
        .unbounded_send(Bytes::from_static(b"hello world\r\nsecond line"))
        .unwrap();
    cx.run_until_parked();

    // Cell centres: 4px padding, 7.8 × 16 cells.
    let cell = |col: f32, row: f32| point(px(4.0 + 7.8 * col + 3.9), px(4.0 + 16.0 * row + 8.0));
    cx.update_window(handle.into(), |_, window, cx| {
        window.drag(cell(6.0, 0.0), cell(5.0, 1.0), cx);
    })
    .unwrap();
    cx.dispatch_action(handle.into(), CopySelection);
    let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(copied.as_deref(), Some("world\nsecond"));
}

#[gpui_kit::test]
fn read_only_ignores_input(cx: &mut TestAppContext) {
    let (handle, pty, _) = open(cx, b"");
    handle
        .update(cx, |view, _, cx| view.set_read_only(true, cx))
        .unwrap();
    press(cx, &handle, &["a", "enter", "tab"]);
    assert!(pty.take().is_empty());
}

#[gpui_kit::test]
fn replace_feed_switches_pty(cx: &mut TestAppContext) {
    let (handle, old_pty, old_output) = open(cx, b"old");
    let new_pty = Arc::new(RecordingPty::default());
    let (new_feed, new_output) = feed(b"\x1b[c");
    handle
        .update(cx, |view, _, cx| {
            view.replace_feed(new_feed, new_pty.clone(), cx);
        })
        .unwrap();
    cx.run_until_parked();
    render(cx, &handle);
    assert_eq!(new_pty.take(), b"\x1b[?62;22c");
    assert_eq!(*new_pty.sizes.lock(), [(81, 29)]);

    // The old feed is dropped; its output goes nowhere.
    assert!(
        old_output
            .unbounded_send(Bytes::from_static(b"\x1b[c"))
            .is_err()
    );
    press(cx, &handle, &["x"]);
    assert_eq!(new_pty.take(), b"x");
    assert!(old_pty.take().is_empty());
    drop(new_output);
}
