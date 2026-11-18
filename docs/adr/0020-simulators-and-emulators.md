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
  - Foldable AVDs (`hw.sensor.hinge*`, `hw.sensor.posture_list`, the cover screen as `hw.displayRegion.0.1.*`) report their fold read live from the physical model (`HINGE_ANGLE0`, `POSTURE`); posture and hinge are set over gRPC. Folded to the cover screen, the emulator streams that screen's size (each picture says so in `foldedDisplay`; `streamNotification` says the posture even while nobody watches), while touches stay in the main display's pixels.
  - Several fingers are one `TouchEvent` with an identifier per finger. Settings are read back from the device in one adb call (`settings`, `cmd uimode`, `getprop debug.layout`, `dumpsys activity`, `pm list packages -3`), so the panel shows what is set rather than what it asked for.
- **Android phones** (USB or wireless debugging, `android:@<adb serial>`, `physical`): `adb` only, nothing installed on the phone. The screen is the system's `screenrecord` H.264 (`adb exec-out`), decoded by VideoToolbox and encoded to JPEG by ImageIO on macOS, else by an installed ffmpeg, else `screencap` pictures one by one (about 1 a second). A recording keeps its size, so it is started again at the turned size when the phone turns (the display's rotation is polled on the phone every second), and, on systems whose `screenrecord` can't record without a limit, before its 3 minutes are up; the old one stops once the new one shows a picture. Touches go to a touch screen of our own, made with the system's `uinput` shell command (Android 11 and later; `input motionevent` before): `sendevent` to the phone's own touch screen is refused to adb's shell by SELinux on production builds. Keys and text through `input` (ASCII only: Android's shell has no way to type other characters or set the clipboard). A phone can't be booted or shut down; one waiting for the user to allow debugging is listed with what to do.
- **iOS**: `simctl` for the list, boot, apps and settings. The screen and touches go through the same private frameworks Simulator.app uses (CoreSimulator's display surface, SimulatorKit's HID events), in a helper process so a crash there can't take the daemon down. To review: private frameworks can change with any Xcode.

### 4. Agents

- `device_*` tools on the Agentloft MCP server: list, boot, screenshot, tap, swipe, pinch, gesture (up to five fingers along paths), type, press, apps, fold, settings (read back) and set, permission, push, shut down. Coordinates are the screenshot's pixels. Inspect, act, verify with a new screenshot, as with the browser tools.
- The same operations are a command line in agents' shells, `agent-device` (`agentloftd device …`), for agents that script or prefer a shell: the daemon puts it first on every agent's `PATH` and names the thread in `AGENTLOFT_THREAD_ID`, so the user's window shows the device the agent uses. Its screenshots are PNG files; points are pixels of the thread's last screenshot of the device, whichever of the two took it.

### The panel's tools, tabs and fingers

- A tools drawer beside the screen shows the app in front and the settings the device says it takes (`DeviceSettings::supported`): appearance, text size, contrast, reduce motion, the screen reader, accessibility frames, the network, orientation, a fake location, an app's permissions and a test push (an APNs-style alert). Each control shows what the device reads back after a change, not what was clicked; the toolbar's light/dark button too.
- One Device tab per device; only the tab showing streams. Picking a device another tab shows switches to that tab. Tabs can be renamed and are kept per thread in the window's session.
- Two fingers as Simulator.app makes them: Option-drag pinches around the screen's centre, Option+Shift-drag moves both, and a trackpad pinch over the screen is a pinch (where the platform delivers it: macOS; not the web client).
- A plugged-in phone is listed with its platform's emulators, marked as a phone: it has no Start and no power button, and one that is locked out shows why (accept the prompt on the phone).

## Consequences

- iOS works only on macOS with Xcode and a runtime; Android wherever its SDK runs. Both report what's missing rather than failing quietly.
- Simulators are heavy: booting one costs seconds and gigabytes of memory. The panel never boots one by itself: a person presses Start or an agent calls `device_boot`.
- When an agent uses a device and the panel isn't showing it, the device floats over the chat (like the browser's mini preview, but live: it can be touched and typed in), dragged by its bar and resized from its edges and corners (its height follows the device's shape); the panel's toolbar floats it too, and its bar puts it back in the panel.
- What T3 Code has that this doesn't yet: physical iPhones, devices on another computer over SSH beyond what an SSH window already gives.
- Known gaps at first merge: an emulator Android Studio started (signed JWT, no token) is listed and can be stopped but not shown; a debug daemon encodes Android pictures about 3× slower than a release one; on a phone browser the on-screen keyboard can't tell when the device has a text field focused.
