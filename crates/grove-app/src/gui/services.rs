//! App-wide handles: the core and its runtime, the main window, and helpers
//! for running core operations from the UI.

use super::workspace::Workspace;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::{AnyWindowHandle, App, AsyncApp, Global, SharedString, Task, WeakEntity, Window};
use grove_core::{Core, NoticeLevel};
use std::future::Future;

/// The core and the tokio runtime it runs on.
#[derive(Clone)]
pub struct Services {
    pub core: Core,
    pub rt: tokio::runtime::Handle,
}

impl Global for Services {}

impl Services {
    pub fn get(cx: &App) -> Services {
        cx.global::<Services>().clone()
    }
}

/// The main window, while it is open.
///
/// The handle is untyped on purpose: updating through a `WindowHandle<Root>`
/// leases the `Root` entity, and opening a dialog or notification from
/// inside that update would update `Root` again and panic.
#[derive(Default)]
pub struct MainWindow {
    pub handle: Option<AnyWindowHandle>,
    pub workspace: Option<WeakEntity<Workspace>>,
}

impl Global for MainWindow {}

impl MainWindow {
    pub fn handle(cx: &App) -> Option<AnyWindowHandle> {
        cx.try_global::<MainWindow>().and_then(|w| w.handle)
    }

    /// Runs `f` with the main window's workspace, if the window is open.
    pub fn with_workspace(
        cx: &mut App,
        f: impl FnOnce(&mut Workspace, &mut Window, &mut gpui_kit::Context<Workspace>) + 'static,
    ) -> bool {
        let Some(main) = cx.try_global::<MainWindow>() else {
            return false;
        };
        let (Some(handle), Some(workspace)) = (main.handle, main.workspace.clone()) else {
            return false;
        };
        handle
            .update(cx, |_, window, cx| {
                workspace.update(cx, |ws, cx| f(ws, window, cx)).is_ok()
            })
            .unwrap_or(false)
    }
}

/// Shows a notification in the main window (only logged when it's closed).
pub fn notify(cx: &mut App, level: NoticeLevel, message: impl Into<SharedString>) {
    let message = message.into();
    let Some(handle) = MainWindow::handle(cx) else {
        return;
    };
    let note = match level {
        NoticeLevel::Info => Notification::info(message),
        NoticeLevel::Warning => Notification::warning(message),
        NoticeLevel::Error => Notification::error(message).autohide(false),
    };
    let _ = handle.update(cx, |_, window, cx| window.push_notification(note, cx));
}

/// Runs a core operation on the runtime. Failures show up as an error
/// notification prefixed with `what`; the task resolves to `None` then.
pub fn run_op<T, Fut>(
    cx: &mut App,
    what: impl Into<SharedString>,
    op: impl FnOnce(Core) -> Fut,
) -> Task<Option<T>>
where
    T: Send + 'static,
    Fut: Future<Output = grove_core::Result<T>> + Send + 'static,
{
    let services = Services::get(cx);
    let job = services.rt.spawn(op(services.core.clone()));
    let what = what.into();
    cx.spawn(async move |cx: &mut AsyncApp| match job.await {
        Ok(Ok(value)) => Some(value),
        Ok(Err(e)) => {
            cx.update(|cx| notify(cx, NoticeLevel::Error, format!("{what}: {e}")));
            None
        }
        Err(e) => {
            tracing::error!("{what}: task failed: {e}");
            cx.update(|cx| notify(cx, NoticeLevel::Error, format!("{what}: {e}")));
            None
        }
    })
}

/// Runs a future on the core's runtime and returns its result (or the
/// panic/cancellation as an error string) without notifying.
pub fn run_bg<T, Fut>(cx: &mut App, fut: Fut) -> Task<Result<T, String>>
where
    T: Send + 'static,
    Fut: Future<Output = T> + Send + 'static,
{
    let job = Services::get(cx).rt.spawn(fut);
    cx.spawn(async move |_: &mut AsyncApp| job.await.map_err(|e| e.to_string()))
}
