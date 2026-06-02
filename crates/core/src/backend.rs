//! The backend contract.
//!
//! Each mode implements [`Backend`], turning a target string into a [`Document`].
//! The dispatcher (in the binary) holds one concrete backend per mode and calls
//! the matching one; adding a mode means adding a `Backend` impl plus a route arm.

use anyhow::Result;

use crate::content::Document;

/// A rendering backend for one [`Mode`](crate::intent::Mode).
///
/// `fetch` is async because most backends do network IO. The trait is used for
/// static dispatch (each backend is a distinct concrete type), so async-fn-in-trait
/// is exactly the right tool here.
#[allow(async_fn_in_trait)]
pub trait Backend {
    /// A short human-readable name, e.g. `"text"`, for status display.
    fn name(&self) -> &'static str;

    /// Resolve `target` (a URL or query) into a renderable document.
    async fn fetch(&self, target: &str) -> Result<Document>;
}
