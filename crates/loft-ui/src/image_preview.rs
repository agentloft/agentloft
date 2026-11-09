//! Images anywhere in the app: click for a large preview, right-click to
//! copy, save or open them.

use crate::theme::Palette;
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::{Sizable as _, h_flex};
use gpui_kit::{
    App, ClipboardItem, Context, Image, ObjectFit, ParentElement as _, Styled as _,
    StyledImage as _, Window, div, img, px,
};
use std::path::PathBuf;
use std::sync::Arc;

/// An image to show: already in memory, or a file on disk.
#[derive(Clone)]
pub enum Pic {
    Loaded { image: Arc<Image>, name: String },
    File { path: PathBuf, name: String },
}

impl Pic {
    /// A file, titled by its file name.
    pub fn file(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".into());
        Pic::File { path, name }
    }

    pub fn name(&self) -> String {
        match self {
            Pic::Loaded { name, .. } | Pic::File { name, .. } => name.clone(),
        }
    }

    fn bytes(&self) -> Option<Vec<u8>> {
        match self {
            Pic::Loaded { image, .. } => Some(image.bytes().to_vec()),
            Pic::File { path, .. } => std::fs::read(path).ok(),
        }
    }

    fn image(&self) -> Option<Arc<Image>> {
        match self {
            Pic::Loaded { image, .. } => Some(image.clone()),
            Pic::File { path, .. } => {
                let format = crate::attachments::format_for_path(path)?;
                Some(Arc::new(Image::from_bytes(
                    format,
                    std::fs::read(path).ok()?,
                )))
            }
        }
    }

    fn element(&self) -> gpui_kit::Img {
        match self {
            Pic::Loaded { image, .. } => img(image.clone()),
            Pic::File { path, .. } => img(path.clone()),
        }
    }
}

/// Put the image on the clipboard.
pub fn copy(pic: &Pic, cx: &mut App) {
    if let Some(image) = pic.image() {
        cx.write_to_clipboard(ClipboardItem::new_image(&image));
    }
}

/// Ask where to save a copy, then write it.
pub fn save_as(pic: Pic, cx: &mut App) {
    let dir = std::env::home_dir()
        .map(|h| h.join("Downloads"))
        .filter(|d| d.is_dir())
        .or_else(std::env::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));
    let rx = cx.prompt_for_new_path(&dir, Some(&pic.name()));
    cx.spawn(async move |_| {
        if let Ok(Ok(Some(path))) = rx.await
            && let Some(bytes) = pic.bytes()
        {
            let _ = std::fs::write(path, bytes);
        }
    })
    .detach();
}

/// The right-click menu for an image.
pub fn menu(
    pic: Pic,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    move |menu, _, _| {
        let (a, b, c) = (pic.clone(), pic.clone(), pic.clone());
        let mut menu = menu
            .item(
                PopupMenuItem::new("Open Preview")
                    .icon(IconName::Maximize2)
                    .on_click(move |_, window, cx| open(a.clone(), window, cx)),
            )
            .item(
                PopupMenuItem::new("Copy Image")
                    .icon(IconName::Copy)
                    .on_click(move |_, _, cx| copy(&b, cx)),
            )
            .item(
                PopupMenuItem::new("Save Image As…")
                    .icon(IconName::ArrowDown)
                    .on_click(move |_, _, cx| save_as(c.clone(), cx)),
            );
        if let Pic::File { path, .. } = &pic {
            let path = path.clone();
            menu = menu.separator().item(
                PopupMenuItem::new("Open in Default App")
                    .icon(IconName::ExternalLink)
                    .on_click(move |_, _, cx| cx.open_with_system(&path)),
            );
        }
        menu
    }
}

/// The image, large, in a dialog.
pub fn open(pic: Pic, window: &mut Window, cx: &mut App) {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::component::button::{Button, ButtonVariants as _};
    use gpui_kit::component::dialog::{DialogClose, DialogFooter};
    let viewport = window.viewport_size();
    let width = (f32::from(viewport.width) * 0.8).clamp(420., 1200.);
    let height = (f32::from(viewport.height) * 0.68).max(240.);
    window.open_dialog(cx, move |dialog, _, cx| {
        let p = Palette::of(cx);
        let (copy_pic, save_pic) = (pic.clone(), pic.clone());
        dialog
            .w(px(width))
            .title(pic.name())
            .child(
                div()
                    .h(px(height))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(10.))
                    .bg(p.muted)
                    .overflow_hidden()
                    .child(
                        pic.element()
                            .max_w_full()
                            .max_h_full()
                            .object_fit(ObjectFit::Contain),
                    ),
            )
            .footer(
                DialogFooter::new().child(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("image-copy")
                                .cursor_pointer()
                                .small()
                                .label("Copy")
                                .icon(IconName::Copy)
                                .ghost()
                                .on_click(move |_, _, cx| copy(&copy_pic, cx)),
                        )
                        .child(
                            Button::new("image-save")
                                .cursor_pointer()
                                .small()
                                .label("Save As…")
                                .icon(IconName::ArrowDown)
                                .ghost()
                                .on_click(move |_, _, cx| save_as(save_pic.clone(), cx)),
                        )
                        .child(
                            DialogClose::new().child(
                                Button::new("image-close")
                                    .cursor_pointer()
                                    .small()
                                    .label("Close")
                                    .outline(),
                            ),
                        ),
                ),
            )
    });
}
