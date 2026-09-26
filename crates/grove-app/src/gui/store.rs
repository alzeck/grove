//! The latest core [`Snapshot`], kept current from core events.

use super::services::{Services, notify};
use gpui_kit::{App, AppContext as _, AsyncApp, Context, Entity, Global};
use grove_core::{CoreEvent, NoticeLevel, Snapshot};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch};

/// Bursts of `Changed` events within this window cause one re-read.
const COALESCE: Duration = Duration::from_millis(50);

pub struct Store {
    snapshot: Arc<Snapshot>,
}

impl Store {
    pub fn snapshot(&self) -> Arc<Snapshot> {
        self.snapshot.clone()
    }

    fn set(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        self.snapshot = Arc::new(snapshot);
        cx.notify();
    }
}

/// The app's store entity.
pub struct AppStore(pub Entity<Store>);

impl Global for AppStore {}

impl AppStore {
    pub fn entity(cx: &App) -> Entity<Store> {
        cx.global::<AppStore>().0.clone()
    }

    pub fn snapshot(cx: &App) -> Arc<Snapshot> {
        cx.global::<AppStore>().0.read(cx).snapshot()
    }
}

/// Creates the store and starts forwarding core events: `Changed` re-reads
/// the snapshot, `Notice` becomes a notification.
pub fn start(cx: &mut App) -> Entity<Store> {
    let services = Services::get(cx);
    let core = services.core.clone();
    // Subscribe before the first read so nothing slips between them.
    let mut events = core.subscribe();
    let store = cx.new(|_| Store {
        snapshot: Arc::new(core.snapshot()),
    });

    let (changed_tx, mut changed_rx) = watch::channel(0u64);
    let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<(NoticeLevel, String)>();
    services.rt.spawn(async move {
        loop {
            match events.recv().await {
                Ok(CoreEvent::Changed) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    changed_tx.send_modify(|v| *v = v.wrapping_add(1));
                }
                Ok(CoreEvent::Notice { level, message }) => {
                    let _ = notice_tx.send((level, message));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let weak = store.downgrade();
    cx.spawn(async move |cx: &mut AsyncApp| {
        while changed_rx.changed().await.is_ok() {
            let snapshot = core.snapshot();
            if weak
                .update(cx, |store, cx| store.set(snapshot, cx))
                .is_err()
            {
                break;
            }
            cx.background_executor().timer(COALESCE).await;
        }
    })
    .detach();

    cx.spawn(async move |cx: &mut AsyncApp| {
        while let Some((level, message)) = notice_rx.recv().await {
            cx.update(|cx| notify(cx, level, message));
        }
    })
    .detach();

    cx.set_global(AppStore(store.clone()));
    store
}
