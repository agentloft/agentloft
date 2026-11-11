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
        ArrowDown,
        Bug,
        ChartPie,
        Gauge,
        Hourglass,
        Keyboard,
        RotateCcw,
        TriangleAlert,
        FileDiff,
        ScrollText,
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
        ChevronLeft,
        ChevronUp,
        ChevronsDownUp,
        ChevronsUpDown,
        CornerDownRight,
        Hash,
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
        MessageSquare,
        MessageSquareDashed,
        Palette,
        FolderGit2,
        Link,
        ListFilter,
        Github,
        Info,
        Pin,
        PinOff,
        MessageSquarePlus,
        User,
        RotateCwSquare,
        Expand,
        Smartphone,
        Tablet,
        Monitor,
        Maximize2,
        Minimize2,
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
        LogOut,
        GlobeLock,
        QrCode,
        KeyRound,
        MonitorSmartphone,
        MessageCircleQuestionMark,
        ListChecks,
        Circle,
        CircleDot,
        SquareCheck,
        Volume2,
        VolumeX,
    ]
);

// gpui-kit's own components draw these (its default bundle). Natively that
// bundle is embedded by gpui-kit; in the browser it would be downloaded icon
// by icon, so the web build embeds the same set here instead. The list is
// gpui-kit-assets 0.9.0's `default-icons.txt`: refresh it when bumping
// gpui-kit.
#[cfg(target_arch = "wasm32")]
gpui_kit::assets::icon_assets!(
    KitIcons,
    [
        ALargeSmall,
        ArrowDown,
        ArrowLeft,
        ArrowRight,
        ArrowUp,
        Asterisk,
        Ban,
        BatteryCharging,
        BatteryFull,
        BatteryLow,
        BatteryMedium,
        BatteryWarning,
        Battery,
        Bell,
        BookOpen,
        Bot,
        Building2,
        Calendar,
        CaseSensitive,
        ChartPie,
        Check,
        ChevronDown,
        ChevronLeft,
        ChevronRight,
        ChevronUp,
        ChevronsUpDown,
        CircleAlert,
        CircleCheck,
        CircleUser,
        CircleX,
        Close,
        Copy,
        Cpu,
        Dash,
        Delete,
        EllipsisVertical,
        Ellipsis,
        ExternalLink,
        EyeOff,
        Eye,
        FileText,
        File,
        FolderClosed,
        FolderOpen,
        Folder,
        Frame,
        GalleryVerticalEnd,
        Github,
        Globe,
        HardDrive,
        HeartOff,
        Heart,
        Inbox,
        Info,
        Inspector,
        LayoutDashboard,
        LoaderCircle,
        Loader,
        Map,
        Maximize,
        MemoryStick,
        Menu,
        Mic,
        Minimize,
        Minus,
        Moon,
        Network,
        Palette,
        PanelBottomOpen,
        PanelBottom,
        PanelLeftClose,
        PanelLeftOpen,
        PanelLeft,
        PanelRightClose,
        PanelRightOpen,
        PanelRight,
        Pause,
        Play,
        Plus,
        Redo2,
        Redo,
        RefreshCw,
        Replace,
        ResizeCorner,
        RotateCw,
        Search,
        Settings2,
        Settings,
        SortAscending,
        SortDescending,
        SquareTerminal,
        Square,
        StarFill,
        StarOff,
        Star,
        Sun,
        ThumbsDown,
        ThumbsUp,
        TriangleAlert,
        Undo2,
        Undo,
        User,
        WindowClose,
        WindowMaximize,
        WindowMinimize,
        WindowRestore
    ]
);

/// gpui-kit's default icons.
#[cfg(not(target_arch = "wasm32"))]
fn kit_icons() -> impl AssetSource {
    gpui_kit::assets::Assets
}

#[cfg(target_arch = "wasm32")]
fn kit_icons() -> impl AssetSource {
    KitIcons
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(svg) = brand_svg(path) {
            return Ok(Some(Cow::Borrowed(svg)));
        }
        match AppIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => kit_icons().load(path),
        }
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut out = AppIcons.list(path)?;
        out.extend(kit_icons().list(path)?);
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::assets::IconName;

    #[test]
    fn browser_tab_speaker_icons_are_bundled() {
        for icon in [IconName::Volume2, IconName::VolumeX] {
            let found = Assets.load(&icon.path()).ok().flatten();
            assert!(found.is_some(), "{} isn't bundled", icon.path());
        }
    }
}
