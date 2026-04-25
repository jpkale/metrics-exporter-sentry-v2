//! Background flush-task scheduling. Two implementations gated on features:
//! `tokio` (default) and `blocking` (a dedicated OS thread). When both are
//! enabled simultaneously, `tokio` wins (a runtime context is required at
//! install time; otherwise the recorder still works but loses time-based
//! flushing).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Handle to the background flush task. Drops the underlying task on drop.
pub struct FlushTaskHandle {
    stop: Arc<AtomicBool>,
    /// Only used by the `blocking` implementation. Always `None` for `tokio`.
    join: Option<std::thread::JoinHandle<()>>,
}

impl FlushTaskHandle {
    /// Signal the background task to stop. Best-effort; the task wakes on its
    /// next interval tick.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for FlushTaskHandle {
    fn drop(&mut self) {
        self.stop();
        if let Some(j) = self.join.take() {
            // Don't block forever if the thread is mid-sleep — joining is
            // best-effort. We at least signaled stop; on the next wakeup
            // the thread exits.
            let _ = j.join();
        }
    }
}

/// Spawn a flush task. The closure `flush` is invoked every `interval`. If no
/// async runtime / thread can be started the returned handle is a no-op.
#[cfg(feature = "tokio")]
pub fn spawn_flush_task<F>(interval: Duration, flush: F) -> FlushTaskHandle
where
    F: Fn() + Send + Sync + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let stop_inner = stop.clone();
        handle.spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // The first tick fires immediately; skip it so we don't
            // double-flush right after install.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if stop_inner.load(Ordering::SeqCst) {
                    break;
                }
                flush();
            }
        });
    } else {
        tracing::warn!(
            "metrics-exporter-sentry-v2: no tokio runtime active at install time; \
             time-based flush disabled (size-based flush still active)"
        );
    }

    FlushTaskHandle { stop, join: None }
}

#[cfg(all(feature = "blocking", not(feature = "tokio")))]
pub fn spawn_flush_task<F>(interval: Duration, flush: F) -> FlushTaskHandle
where
    F: Fn() + Send + Sync + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let stop_inner = stop.clone();
    let join = std::thread::Builder::new()
        .name("metrics-exporter-sentry-v2-flush".into())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                if stop_inner.load(Ordering::SeqCst) {
                    break;
                }
                flush();
            }
        })
        .ok();
    FlushTaskHandle { stop, join }
}

#[cfg(not(any(feature = "tokio", feature = "blocking")))]
pub fn spawn_flush_task<F>(_interval: Duration, _flush: F) -> FlushTaskHandle
where
    F: Fn() + Send + Sync + 'static,
{
    FlushTaskHandle {
        stop: Arc::new(AtomicBool::new(false)),
        join: None,
    }
}
