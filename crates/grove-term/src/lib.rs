//! A GPUI terminal view backed by libghostty-vt. Grove shows every
//! process's PTY output in one (you can type into it, e.g. debuggers) and
//! hosts interactive shells.
//!
//! Call [`init`] once at startup to register the key bindings, then create a
//! [`TerminalView`] with the process's output ([`TerminalFeed`]) and a
//! [`PtyHandle`] that forwards input and size changes to its PTY:
//!
//! ```ignore
//! grove_term::init(cx);
//! let terminal = cx.new(|cx| TerminalView::new(feed, pty, window, cx));
//! ```
//!
//! The view fills its parent and resizes the PTY to fit. Keys go to the
//! program only while it is focused; Cmd shortcuts other than the ones
//! bound by [`init`] (and Cmd-←/→/⌫) are left to the app.

mod element;
mod frame;
mod input;
mod metrics;
mod session;
mod settings;
mod theme;
mod view;

pub use settings::TerminalSettings;
pub use theme::{Rgb, TerminalTheme};
pub use view::{
    Clear, CopySelection, KEY_CONTEXT, Paste, ScrollPageDown, ScrollPageUp, ScrollToBottom,
    ScrollToTop, SendBacktab, SendTab, TerminalView, init,
};

use bytes::Bytes;
use futures::stream::BoxStream;

/// Where input and size changes go (the app implements it over a process PTY).
pub trait PtyHandle: Send + Sync + 'static {
    fn write(&self, data: &[u8]);
    fn resize(&self, cols: u16, rows: u16);
}

/// A process's output: what it printed so far, then everything after.
pub struct TerminalFeed {
    /// Fed first, in one go.
    pub replay: Bytes,
    /// Fed as it arrives, until the stream ends.
    pub live: BoxStream<'static, Bytes>,
}
