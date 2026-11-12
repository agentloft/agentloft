//! Composer image attachments: from the clipboard (⌘V), dropped files or
//! the paperclip picker. Sent with the message; the daemon stores them and
//! the agent receives them natively (Claude image blocks, Codex
//! `localImage`).

use crate::press::ClaimsPress as _;
use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, Sizable as _, h_flex};
use gpui_kit::{
    AnyElement, ClipboardEntry, ClipboardItem, Image, ImageFormat, InteractiveElement as _,
    IntoElement, ObjectFit, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, div, img, px,
};
use loft_protocol::Attachment;
use std::path::Path;
use std::sync::Arc;

/// Per-message limit (the daemon enforces the same).
pub const MAX_IMAGES: usize = 10;
const MAX_BYTES: usize = 20 * 1024 * 1024;

#[derive(Clone)]
pub struct PendingImage {
    pub name: String,
    pub mime: &'static str,
    pub data: Arc<Vec<u8>>,
    pub preview: Arc<Image>,
}

impl PendingImage {
    fn new(name: String, format: ImageFormat, data: Vec<u8>) -> Option<Self> {
        let mime = mime_for(format)?;
        if data.len() > MAX_BYTES {
            return None;
        }
        let data = Arc::new(data);
        Some(Self {
            name,
            mime,
            preview: Arc::new(Image::from_bytes(format, data.to_vec())),
            data,
        })
    }

    /// An image the daemon stored (a queued message's), read back.
    pub fn stored(name: String, path: &Path, data: Vec<u8>) -> Option<Self> {
        Self::new(name, format_for_path(path)?, data)
    }

    /// A PNG made by the app (e.g. a page screenshot).
    pub fn png(name: &str, data: Vec<u8>) -> Option<Self> {
        Self::new(name.to_owned(), ImageFormat::Png, data)
    }

    pub fn to_attachment(&self) -> Attachment {
        Attachment {
            name: self.name.clone(),
            mime: self.mime.to_owned(),
            data: self.data.to_vec().into(),
        }
    }
}

fn mime_for(format: ImageFormat) -> Option<&'static str> {
    Some(match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::Webp => "image/webp",
        _ => return None,
    })
}

pub(crate) fn format_for_path(path: &Path) -> Option<ImageFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => ImageFormat::Png,
        "jpg" | "jpeg" => ImageFormat::Jpeg,
        "gif" => ImageFormat::Gif,
        "webp" => ImageFormat::Webp,
        _ => return None,
    })
}

/// Read image files (others are skipped). Blocking; run off the UI thread.
pub fn read_paths(paths: &[std::path::PathBuf]) -> Vec<PendingImage> {
    paths
        .iter()
        .filter_map(|path| {
            let format = format_for_path(path)?;
            let data = std::fs::read(path).ok()?;
            let name = path.file_name()?.to_string_lossy().into_owned();
            PendingImage::new(name, format, data)
        })
        .collect()
}

/// Images on the clipboard (screenshots, copied images), if any.
pub fn from_clipboard(item: &ClipboardItem) -> Vec<PendingImage> {
    item.entries()
        .iter()
        .filter_map(|entry| match entry {
            ClipboardEntry::Image(image) => {
                let format = image.format();
                let ext = match format {
                    ImageFormat::Jpeg => "jpg",
                    ImageFormat::Gif => "gif",
                    ImageFormat::Webp => "webp",
                    _ => "png",
                };
                PendingImage::new(format!("pasted.{ext}"), format, image.bytes().to_vec())
            }
            _ => None,
        })
        .collect()
}

/// Thumbnails above the composer text, each removable.
pub fn strip(
    images: &[PendingImage],
    p: &Palette,
    on_remove: impl Fn(usize, &mut gpui_kit::Window, &mut gpui_kit::App) + Clone + 'static,
) -> Option<AnyElement> {
    if images.is_empty() {
        return None;
    }
    let mut row = h_flex().gap_2().px_2().pt_1().flex_wrap();
    for (ix, image) in images.iter().enumerate() {
        use gpui_kit::component::menu::ContextMenuExt as _;
        let on_remove = on_remove.clone();
        let pic = crate::image_preview::Pic::Loaded {
            image: image.preview.clone(),
            name: image.name.clone(),
        };
        let open_pic = pic.clone();
        row = row.child(
            div()
                .id(SharedString::from(format!("attachment-{ix}")))
                .cursor_pointer()
                .on_click(move |_, window, cx| {
                    crate::image_preview::open(open_pic.clone(), window, cx)
                })
                .size(px(56.))
                .rounded(px(10.))
                .overflow_hidden()
                .border_1()
                .border_color(p.border)
                .group("attachment")
                .child(
                    img(image.preview.clone())
                        .size_full()
                        .object_fit(ObjectFit::Cover),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("attachment-x-{ix}")))
                        .absolute()
                        .top(px(3.))
                        .right(px(3.))
                        .size(px(18.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(gpui_kit::black().opacity(0.6))
                        .text_color(gpui_kit::white())
                        .cursor_pointer()
                        .child(Icon::new(IconName::X).xsmall())
                        .claims_press()
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            on_remove(ix, window, cx)
                        }),
                )
                .tooltip({
                    let name = image.name.clone();
                    move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(name.clone()).build(window, cx)
                    }
                })
                .context_menu(crate::image_preview::menu(pic)),
        );
    }
    Some(row.into_any_element())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_supported_images_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("shot.PNG");
        std::fs::write(&png, [0x89, b'P', b'N', b'G']).unwrap();
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "hi").unwrap();
        let read = read_paths(&[png, txt]);
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].name, "shot.PNG");
        assert_eq!(read[0].mime, "image/png");
    }
}
