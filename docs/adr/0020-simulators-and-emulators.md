# ADR-0020: iOS simulators and Android emulators, streamed like the browser

- **Status:** Proposed. Built on 2026-11-17 at the product owner's request ("Similar to t3code I want emulator rendering… iOS and Android"); the choices marked "to review" are theirs to revisit.
- **Date:** 2026-11-17

## Context

- Agents build mobile apps too. To see what they made, a person runs Xcode's Simulator or Android Studio's emulator in windows of their own, beside Agentloft, and an agent can't look at either.
- T3 Code has a Device panel: a live iOS simulator or Android emulator beside the thread, touch and keys forwarded, Home/Back/rotate in a toolbar, and `device_*` tools for agents. We take the idea, not its code.
- The daemon already streams pages of its own headless browser to every viewer, desktop, browser or SSH window, as acknowledged JPEG pictures ([ADR-0017](0017-streamed-browser.md)). A simulator's screen is the same kind of thing: pictures out, input back.

## Decision

**The daemon runs simulators and emulators headless and streams their screens; a Device panel shows one, and agents drive the same devices.**

### 1. Devices belong to the computer, not a thread

- A device is named `ios:<simulator UDID>` or `android:<AVD name>` (`DeviceId`), stable across boots. Any window may watch any device; closing a panel never stops one. The thread whose agent used a device last is remembered (`DeviceInfo.agent`) and its windows are told (`DeviceDriven`) so they can show it.
- Only a device someone watches streams. A screen nobody watches is kept two minutes for an agent's next step, then let go (the device keeps running).

### 2. Wire

- `Topic::Devices`: the list, read again every few seconds while anyone subscribes. `DeviceSupport` says what a platform lacks (no Xcode, no SDK, no virtual device) so the panel can say what to install.
- `Topic::Device(id)`: state and pictures (`DeviceFrame`), acknowledged with the same `PageFrameAck` and paced by the same window as pages.
- Input is in fractions of the picture (`DeviceInput`), whichever way the device is turned, so a viewer needs no knowledge of the screen's pixels.
- Requests: list, boot, shut down, input, capture (PNG), apps (install, launch, terminate, uninstall, open a link) and settings (appearance, location).
- A browser session may use devices only where it may use the daemon's browser (same permission: both show and run things on the daemon's computer). Installing an app (a path on that computer) is refused to browser sessions.

### 3. Backends

- One `Backend` per platform in `loft-core/src/devices/` and a `Screen` per watched device; the manager in `devices/mod.rs` numbers pictures and fans them out. `fake.rs` is a phone that needs neither SDK (`AGENTLOFT_FAKE_DEVICES=1`), for tests and for trying the panel.
- **Android**: the emulator's own gRPC control service (streamed screenshots, touch, keys, rotation), started headless (`-no-window`). `adb` for apps and settings.
- **iOS**: `simctl` for the list, boot, apps and settings. The screen and touches go through the same private frameworks Simulator.app uses (CoreSimulator's display surface, SimulatorKit's HID events), in a helper process so a crash there can't take the daemon down. To review: private frameworks can change with any Xcode.

### 4. Agents

- `device_*` tools on the Agentloft MCP server: list, boot, screenshot, tap, swipe, type, press, apps, shut down. Coordinates are the screenshot's pixels. Inspect, act, verify with a new screenshot, as with the browser tools.

### 5. The 3D model and folding (added 2026-11-17)

- The panel can show the device as a 3D model you turn: a phone, tablet, book-style foldable or flip phone, chosen from the platform, name, screen proportions and `DeviceInfo.fold`, with the live picture on its screen (the main one, split at the hinge, or the cover while `on_cover`). GPUI has no 3D, so `loft-raster` draws it in software, into a picture the panel's size at the window's scale, only when the picture, the view or the fold changes and at most about 30 times a second. Each screen pixel remembers the point of the device's picture it shows, so touches on the turned screen land exactly; a drag elsewhere turns the model, the wheel or a pinch off the device zooms, a pinch over a foldable folds it.
- A foldable gets its postures and a hinge slider in 2D and 3D (`DeviceSetting::Posture`, `Hinge`); the model's hinge follows the device's. Turned so the cover faces you while it is open, a fold-in that can close is closed (the cover becomes the live screen) and the view turns so the cover stays facing you. The floating card stays 2D.
- To review: the model's direction when the device turns on its side is assumed counter-clockwise (the panel doesn't read the device's orientation), so after Rotate Right the model's camera bump is on the wrong side (the picture is upright and touches are right either way).

## Consequences

- iOS works only on macOS with Xcode and a runtime; Android wherever its SDK runs. Both report what's missing rather than failing quietly.
- Simulators are heavy: booting one costs seconds and gigabytes of memory. The panel never boots one by itself: a person presses Start or an agent calls `device_boot`.
- When an agent uses a device and the panel isn't showing it, the device floats over the chat (like the browser's mini preview, but live: it can be touched and typed in), dragged by its bar; the panel's toolbar floats it too, and its bar puts it back in the panel.
- What T3 Code has that this doesn't yet: resizing the floating device, accessibility overlays, physical devices, devices on another computer over SSH beyond what an SSH window already gives.
- Known gaps at first merge: one finger only (no pinch); an emulator Android Studio started (signed JWT, no token) is listed and can be stopped but not shown; a debug daemon encodes Android pictures about 3× slower than a release one; the panel's light/dark toggle shows what it last sent, not the device's own setting; on a phone browser the on-screen keyboard can't tell when the device has a text field focused.
