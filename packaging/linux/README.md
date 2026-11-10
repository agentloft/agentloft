# Linux packaging

Linux builds are planned for M9. Until then, this directory holds the
desktop entry and the icon install layout packagers should use.

```sh
PREFIX=/usr/local
install -Dm755 target/release/agentloft  "$PREFIX/bin/agentloft"
install -Dm755 target/release/agentloftd "$PREFIX/bin/agentloftd"
install -Dm644 packaging/linux/dev.agentloft.Agentloft.desktop \
  "$PREFIX/share/applications/dev.agentloft.Agentloft.desktop"
for s in 16 24 32 48 64 128 256 512; do
  install -Dm644 "assets/icon/png/agentloft-$s.png" \
    "$PREFIX/share/icons/hicolor/${s}x${s}/apps/dev.agentloft.Agentloft.png"
done
```
