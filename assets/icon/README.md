# Agentloft app icon

Final icon: **"Loft floors"**, three separated mint and teal isometric panes stacked like floors of a loft (parallel agent workspaces) on a deep teal Big Sur squircle. It was chosen from the round-2 exploration (`exploration/round-2/v3.png`).

| File | Use |
|---|---|
| `icon-1024.png` | Master, 1024×1024 RGBA with a transparent ~10% margin. Regenerate everything else from this. |
| `AppIcon.icns` | macOS bundle icon (`CFBundleIconFile` = `AppIcon`), 16–512@2x |
| `AppIcon.iconset/` | Source for the `.icns` (`iconutil -c icns AppIcon.iconset`) |
| `agentloft.ico` | Windows (16, 24, 32, 48, 64, 128, 256) |
| `png/agentloft-{16,24,32,48,64,128,256,512,1024}.png` | Linux hicolor theme, docs/README, in-app use (e.g. About window, sidebar header) |
| `dev/` | The same icon in blue (hue turned 50°), for dev bundles and `cargo run`, so they can't be taken for the installed app in the Dock |

`exploration/` holds earlier concepts for reference only. Don't ship it.

## Regenerate (macOS)
```sh
cd assets/icon
for s in 16 32 128 256 512; do
  sips -z $s $s icon-1024.png --out AppIcon.iconset/icon_${s}x${s}.png
  sips -z $((s*2)) $((s*2)) icon-1024.png --out AppIcon.iconset/icon_${s}x${s}@2x.png
done
iconutil -c icns AppIcon.iconset -o AppIcon.icns
```
