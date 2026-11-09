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
        Archive,
        Ban,
        Bot,
        Brain,
        Check,
        ChevronDown,
        ChevronRight,
        Clock,
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
        Laptop,
        GitCompare,
        GitFork,
        Globe,
        Loader,
        Minus,
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
        Sparkles,
        Square,
        SquarePen,
        SquareTerminal,
        Sun,
        Table,
        X,
    ]
);

#[derive(Clone, Copy, Debug, Default)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
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
