//! Asset source: the Lucide icons Agentloft uses, layered over gpui-kit's
//! default component assets. Only listed icons are embedded in the binary.

use gpui_kit::{AssetSource, Result, SharedString};
use std::borrow::Cow;

gpui_kit::assets::icon_assets!(
    AppIcons,
    [
        ArrowLeft,
        ArrowRight,
        ArrowUp,
        Bug,
        Camera,
        Cookie,
        Download,
        EllipsisVertical,
        Eraser,
        ZoomIn,
        ZoomOut,
        Archive,
        Ban,
        Bot,
        Brain,
        Check,
        ChevronDown,
        ChevronUp,
        Clock,
        Cloud,
        ChevronRight,
        CircleAlert,
        CircleCheck,
        CircleStop,
        CircleX,
        Code,
        Columns2,
        Copy,
        Ellipsis,
        ExternalLink,
        Eye,
        File,
        FileBraces,
        FileCode,
        FileImage,
        FileSpreadsheet,
        FileText,
        Folder,
        FolderOpen,
        FolderPlus,
        FolderTree,
        GitBranch,
        GitMerge,
        GitPullRequest,
        Laptop,
        GitCompare,
        GitFork,
        Globe,
        House,
        Loader,
        Minus,
        MousePointerClick,
        MessageSquarePlus,
        RotateCwSquare,
        Expand,
        Smartphone,
        Tablet,
        Monitor,
        Maximize2,
        Moon,
        PanelLeft,
        Paperclip,
        PanelRight,
        Plus,
        Presentation,
        RefreshCw,
        RotateCw,
        Rows2,
        Search,
        Settings,
        ShieldCheck,
        ShieldOff,
        WandSparkles,
        FilePen,
        Hand,
        Sparkles,
        Square,
        SquarePen,
        SquareTerminal,
        Sun,
        Table,
        Trash,
        X,
    ]
);

#[derive(Clone, Copy, Debug, Default)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(svg) = brand_svg(path) {
            return Ok(Some(Cow::Borrowed(svg)));
        }
        match AppIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => gpui_kit::assets::Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut out = AppIcons.list(path)?;
        out.extend(gpui_kit::assets::Assets.list(path)?);
        Ok(out)
    }
}

/// Agent logos (Simple Icons, CC0; marks belong to their owners). OpenCode's
/// is our own simple glyph.
fn brand_svg(path: &str) -> Option<&'static [u8]> {
    Some(match path {
        "brand/claude.svg" => include_bytes!("../assets/brand/claude.svg"),
        "brand/openai.svg" => include_bytes!("../assets/brand/openai.svg"),
        "brand/gemini.svg" => include_bytes!("../assets/brand/gemini.svg"),
        "brand/opencode.svg" => include_bytes!("../assets/brand/opencode.svg"),
        _ => return None,
    })
}
