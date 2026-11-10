//! UniFFI surface for `landline-core`.
//!
//! This crate adapts core types for UniFFI and holds no logic. Anything that
//! decides something belongs in the core.

use std::sync::Arc;
use std::time::Duration;

uniffi::setup_scaffolding!();

/// Version of the Rust core.
#[uniffi::export]
pub fn core_version() -> String {
    landline_core::version().to_owned()
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum LandlineError {
    #[error(transparent)]
    Core(#[from] landline_core::CoreError),
}

/// Implemented in Swift. Methods are called on core threads.
#[uniffi::export(callback_interface)]
pub trait CoreDelegate: Send + Sync {
    fn heartbeat(&self, count: u64);
}

struct DelegateAdapter(Box<dyn CoreDelegate>);

impl landline_core::CoreDelegate for DelegateAdapter {
    fn heartbeat(&self, count: u64) {
        self.0.heartbeat(count);
    }
}

#[derive(uniffi::Object)]
pub struct LandlineCore {
    inner: landline_core::Core,
}

#[uniffi::export]
impl LandlineCore {
    #[uniffi::constructor]
    pub fn new(delegate: Box<dyn CoreDelegate>) -> Result<Arc<Self>, LandlineError> {
        let delegate = Arc::new(DelegateAdapter(delegate));
        let inner = landline_core::Core::new(landline_core::Config::default(), delegate)?;
        Ok(Arc::new(Self { inner }))
    }

    pub fn start(&self) {
        self.inner.start();
    }

    pub fn stop(&self) {
        self.inner.stop();
    }

    pub async fn echo_after(&self, text: String, delay_ms: u64) -> Result<String, LandlineError> {
        let delay = Duration::from_millis(delay_ms);
        Ok(self.inner.echo_after(text, delay).await?)
    }
}
