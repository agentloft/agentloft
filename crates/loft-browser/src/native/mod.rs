//! The platform side of a view's page ([`crate::ViewHost`]). macOS embeds a
//! WKWebView (through wry) as a child of GPUI's content NSView; other
//! platforms get a stub whose constructor fails.

#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // objc2 calls into WebKit/AppKit; each block has a SAFETY note.
mod host_macos;
#[cfg(target_os = "macos")]
pub(crate) use host_macos::NativeHost;

#[cfg(not(target_os = "macos"))]
mod host_stub;
#[cfg(not(target_os = "macos"))]
pub(crate) use host_stub::NativeHost;
