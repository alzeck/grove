//! Opens a window running your shell in a `TerminalView`.
//!
//! ```sh
//! cargo run -p grove-term --example terminal_demo
//! ```

use std::{io::Read, io::Write, sync::Arc, thread};

use bytes::Bytes;
use futures::{StreamExt as _, channel::mpsc};
use gpui_kit::{
    App, AppContext as _, Bounds, Focusable as _, KeyBinding, WindowBounds, WindowOptions, px, size,
};
use grove_term::{PtyHandle, TerminalFeed, TerminalView};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

gpui_kit::actions!(terminal_demo, [Quit]);

struct Pty {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
}

impl PtyHandle for Pty {
    fn write(&self, data: &[u8]) {
        let mut writer = self.writer.lock();
        if let Err(err) = writer.write_all(data).and_then(|()| writer.flush()) {
            eprintln!("writing to pty: {err}");
        }
    }

    fn resize(&self, cols: u16, rows: u16) {
        let size = PtySize {
            rows,
            cols,
            ..PtySize::default()
        };
        if let Err(err) = self.master.lock().resize(size) {
            eprintln!("resizing pty: {err}");
        }
    }
}

/// Starts the login shell and returns its output stream and PTY handle.
fn spawn_shell() -> anyhow::Result<(TerminalFeed, Arc<Pty>)> {
    let pair = native_pty_system().openpty(PtySize::default())?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut command = CommandBuilder::new(shell);
    command.arg("-l");
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    if let Some(home) = std::env::var_os("HOME") {
        command.cwd(home);
    }
    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let (tx, rx) = mpsc::unbounded::<Bytes>();
    thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx
                        .unbounded_send(Bytes::copy_from_slice(&buf[..n]))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = child.wait();
    });

    let feed = TerminalFeed {
        replay: Bytes::new(),
        live: rx.boxed(),
    };
    let pty = Arc::new(Pty {
        master: Mutex::new(pair.master),
        writer: Mutex::new(writer),
    });
    Ok((feed, pty))
}

fn main() -> anyhow::Result<()> {
    let (feed, pty) = spawn_shell()?;
    gpui_kit::application().run(move |cx: &mut App| {
        grove_term::init(cx);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_action(|_: &Quit, cx| cx.quit());

        let bounds = Bounds::centered(None, size(px(900.0), px(600.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..WindowOptions::default()
        };
        cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| TerminalView::new(feed, pty, window, cx));
            window.focus(&view.focus_handle(cx), cx);
            view
        })
        .expect("failed to open window");
        cx.activate(true);
    });
    Ok(())
}
