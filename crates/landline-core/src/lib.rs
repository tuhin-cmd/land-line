//! Landline core: the KDE Connect protocol, transport, and plugins.
//!
//! The core owns the protocol and all network state, and never calls a macOS
//! API. It runs its own tokio runtime, so callers need no async runtime of
//! their own: synchronous methods return immediately, and async methods can be
//! awaited from any executor (including Swift's, through UniFFI).
//!
//! Milestone 0 only proves the shape of that boundary. [`Core::echo_after`] and
//! [`CoreDelegate::heartbeat`] are placeholders that exercise an async call and
//! a delegate callback end to end. Real features replace them from M1 onward.

#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio::task::JoinHandle;
use tracing::debug;

/// Version of the core, as set in the workspace manifest.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("could not start the core runtime: {0}")]
    Runtime(#[source] std::io::Error),
    #[error("a core task stopped before it finished")]
    TaskStopped,
}

/// Receives events from the core. Methods are called on core threads, so
/// implementations hop to whatever thread they need.
pub trait CoreDelegate: Send + Sync + 'static {
    /// Called once per heartbeat interval while the core is started.
    /// `count` starts at 1 and restarts after each [`Core::start`].
    fn heartbeat(&self, count: u64);
}

#[derive(Debug, Clone)]
pub struct Config {
    pub heartbeat_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(1),
        }
    }
}

pub struct Core {
    config: Config,
    delegate: Arc<dyn CoreDelegate>,
    runtime: Runtime,
    heartbeat: Mutex<Option<JoinHandle<()>>>,
}

impl Core {
    pub fn new(config: Config, delegate: Arc<dyn CoreDelegate>) -> Result<Self, CoreError> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("landline-core")
            .enable_all()
            .build()
            .map_err(CoreError::Runtime)?;
        Ok(Self {
            config,
            delegate,
            runtime,
            heartbeat: Mutex::new(None),
        })
    }

    /// Starts background work. Does nothing if already started.
    pub fn start(&self) {
        let mut heartbeat = self
            .heartbeat
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if heartbeat.is_some() {
            return;
        }
        let delegate = Arc::clone(&self.delegate);
        let period = self.config.heartbeat_interval;
        *heartbeat = Some(self.runtime.spawn(async move {
            let mut ticks = tokio::time::interval(period);
            ticks.tick().await; // the first tick completes immediately
            let mut count = 0u64;
            loop {
                ticks.tick().await;
                count += 1;
                delegate.heartbeat(count);
            }
        }));
        debug!("core started");
    }

    /// Stops background work. Does nothing if not started.
    pub fn stop(&self) {
        let task = self
            .heartbeat
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            task.abort();
            debug!("core stopped");
        }
    }

    /// Returns `text` after `delay`. The work runs on the core runtime, and the
    /// returned future can be awaited from any executor.
    pub async fn echo_after(&self, text: String, delay: Duration) -> Result<String, CoreError> {
        self.runtime
            .spawn(async move {
                tokio::time::sleep(delay).await;
                text
            })
            .await
            .map_err(|_| CoreError::TaskStopped)
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    struct Recorder(mpsc::Sender<u64>);

    impl CoreDelegate for Recorder {
        fn heartbeat(&self, count: u64) {
            let _ = self.0.send(count);
        }
    }

    fn core() -> (Core, mpsc::Receiver<u64>) {
        let (tx, rx) = mpsc::channel();
        let config = Config {
            heartbeat_interval: Duration::from_millis(10),
        };
        (Core::new(config, Arc::new(Recorder(tx))).unwrap(), rx)
    }

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn version_matches_manifest() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn heartbeat_counts_from_one() {
        let (core, rx) = core();
        core.start();
        assert_eq!(rx.recv_timeout(WAIT), Ok(1));
        assert_eq!(rx.recv_timeout(WAIT), Ok(2));
    }

    #[test]
    fn start_twice_runs_one_heartbeat() {
        let (core, rx) = core();
        core.start();
        core.start();
        let first: Vec<u64> = (0..4).map(|_| rx.recv_timeout(WAIT).unwrap()).collect();
        assert_eq!(first, [1, 2, 3, 4]);
    }

    #[test]
    fn stop_ends_heartbeat_and_restart_counts_again() {
        let (core, rx) = core();
        core.start();
        rx.recv_timeout(WAIT).unwrap();
        core.stop();
        // Let a heartbeat that was already running finish, then drain.
        std::thread::sleep(Duration::from_millis(50));
        while rx.try_recv().is_ok() {}
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());

        core.start();
        assert_eq!(rx.recv_timeout(WAIT), Ok(1));
    }

    // Awaited from a runtime that is not the core's, as Swift will do.
    #[tokio::test(flavor = "current_thread")]
    async fn echo_after_resolves_on_a_foreign_executor() {
        let (core, _rx) = core();
        let echoed = core.echo_after("hello".into(), Duration::from_millis(5));
        assert_eq!(echoed.await.unwrap(), "hello");
        // A runtime must not be dropped from async context.
        tokio::task::spawn_blocking(move || drop(core))
            .await
            .unwrap();
    }
}
