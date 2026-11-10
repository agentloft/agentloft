//! Installed apps' own icons (as Finder shows them), for "Open in …".

use gpui_kit::{Image, ImageFormat};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Pixel size the icons are drawn at (shown at 16–20 points, Retina).
#[cfg(target_os = "macos")]
const PIXELS: f64 = 64.;

thread_local! {
    static CACHE: RefCell<HashMap<PathBuf, Option<Arc<Image>>>> = RefCell::new(HashMap::new());
}

/// The icon of the app bundle at `app` (cached; main thread).
pub fn icon(app: &Path) -> Option<Arc<Image>> {
    CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(app.to_path_buf())
            .or_insert_with(|| {
                png(app).map(|png| Arc::new(Image::from_bytes(ImageFormat::Png, png)))
            })
            .clone()
    })
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn png(app: &Path) -> Option<Vec<u8>> {
    use objc2::AllocAnyThread as _;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSWorkspace};
    use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};
    let path = NSString::from_str(app.to_str()?);
    // SAFETY: AppKit calls on the main thread (callers render UI there);
    // every object is retained for the duration.
    unsafe {
        let image = NSWorkspace::sharedWorkspace().iconForFile(&path);
        let mut rect = NSRect::new(NSPoint::new(0., 0.), NSSize::new(PIXELS, PIXELS));
        let cg = image.CGImageForProposedRect_context_hints(&mut rect, None, None)?;
        let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), &cg);
        let data = rep
            .representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())?;
        Some(data.to_vec())
    }
}

#[cfg(not(target_os = "macos"))]
fn png(_app: &Path) -> Option<Vec<u8>> {
    None
}
