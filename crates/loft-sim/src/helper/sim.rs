//! Xcode's private simulator frameworks, as this helper uses them.
//!
//! Found by reading Xcode 26's frameworks (their Objective-C classes and
//! protocols, and the C functions SimulatorKit exports), not from anyone's
//! code:
//!
//! - CoreSimulator's `SimServiceContext` gives the device set and a
//!   `SimDevice` by UDID. A device's `io` lists its IO ports; the main
//!   display's port descriptor (`SimDisplayIOSurfaceRenderable`, with
//!   `screenProperties.screenID == 1`) has the framebuffer as an IOSurface
//!   (`framebufferSurface`), a callback for every change to it
//!   (`registerCallbackWithUUID:damageRectanglesCallback:`), one for a new
//!   surface (`…ioSurfacesChangeCallback:`), and how its interface is turned
//!   (`screenProperties.uiOrientation`).
//! - SimulatorKit's `SimDeviceLegacyHIDClient` sends HID messages to the
//!   device, made by its exported `IndigoHIDMessageFor…` functions:
//!   `MouseNSEvent(point, second point, target, NSEvent type, size, edge)`
//!   with the point as a fraction of `size` and target `0x32` (the main
//!   screen's digitizer). Given a second point (Simulator.app's
//!   Option-drag pinch), the message carries a second finger: after a
//!   `0x20`-byte header (its byte `0x1c` counts the events) come digitizer
//!   events of `0xa0` bytes, one for the hand and one per finger. A
//!   finger's event has its index and identity at `+0x10` (1 and 2 for the
//!   first finger, 2 and 3 for the second), what changed at `+0x18`, its
//!   point (two doubles, fractions) at `+0x1c`, and whether it touches at
//!   `+0x44` and `+0x48`. More fingers are more such events, made from the
//!   second's; `Button(source, direction, target)` with target
//!   `0x33` and source 0 Home, 1 Lock; `KeyboardArbitrary(usage,
//!   direction)`; `HIDArbitrary(target, page, usage, direction)`. Direction
//!   is 1 down, 2 up. The functions return a `calloc`ed message the client
//!   frees.
//! - Turning the device is a GraphicsServices event (type 50, the
//!   `UIDeviceOrientation` as its 4-byte payload) sent as a Mach message to
//!   the device's `PurpleWorkspacePort` (`-[SimDevice lookup:error:]`), as
//!   Simulator.app's Rotate Left and Right do.
//!
//! What isn't used: `SimScreen`'s `registerScreenCallbacksWithUUID:…`
//! (Simulator.app's newer frame callback) shut the device down when this
//! helper registered for it, so pictures follow the damage callback instead.

use anyhow::{Context, Result, anyhow, bail};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::{class, msg_send};
use objc2_foundation::NSString;
use std::ffi::{CStr, CString, c_void};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

const CORE_SIMULATOR: &str =
    "/Library/Developer/PrivateFrameworks/CoreSimulator.framework/CoreSimulator";
/// SimulatorKit, relative to the developer directory: Xcode 27 moved it
/// from the developer directory's own frameworks to Xcode's shared ones.
const SIMULATOR_KIT: &[&str] = &[
    "../SharedFrameworks/SimulatorKit.framework/SimulatorKit",
    "Library/PrivateFrameworks/SimulatorKit.framework/SimulatorKit",
];

/// `SimDevice.state` when the device is running.
pub const BOOTED: u64 = 3;

/// The main screen's digitizer, and its buttons.
const TOUCH_TARGET: u32 = 0x32;
const BUTTON_TARGET: u32 = 0x33;
pub const DOWN: u32 = 1;
pub const UP: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

type MouseFn =
    unsafe extern "C" fn(*const CGPoint, *const CGPoint, u32, u64, CGSize, u64) -> *mut c_void;
type ButtonFn = unsafe extern "C" fn(u32, u32, u32) -> *mut c_void;
type KeyboardFn = unsafe extern "C" fn(u32, u32) -> *mut c_void;
type ArbitraryFn = unsafe extern "C" fn(u32, u32, u32, u32) -> *mut c_void;

/// The type of every block given to CoreSimulator: one object, no result.
/// Its proxies send blocks to the service, which needs their signature.
struct OneObject;

// SAFETY: `v16@?0@8` is `void (^)(id)` on a 64-bit target, which these are.
unsafe impl block2::ManualBlockEncoding for OneObject {
    type Arguments = (*mut AnyObject,);
    type Return = ();
    const ENCODING_CSTR: &'static CStr = c"v16@?0@8";
}

pub fn block(f: impl Fn(*mut AnyObject) + 'static) -> RcBlock<dyn Fn(*mut AnyObject)> {
    RcBlock::with_encoding::<_, _, _, OneObject>(f)
}

unsafe extern "C" {
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut AnyObject;
    fn mach_msg_send(msg: *mut c_void) -> i32;
}

/// Run `f`, turning an Objective-C exception (a selector this Xcode doesn't
/// have, say) into an error instead of ending the process.
pub fn guarded<R>(what: &str, f: impl FnOnce() -> R) -> Result<R> {
    objc2::exception::catch(AssertUnwindSafe(f)).map_err(|e| {
        anyhow!(
            "{what} failed: {}",
            e.map_or("an unknown exception".to_string(), |e| format!("{e:?}"))
        )
    })
}

fn nsstring(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn describe(error: *mut AnyObject) -> String {
    if error.is_null() {
        return "no reason given".into();
    }
    // SAFETY: a non-null `NSError **` out-parameter is an NSError.
    let text: Option<Retained<NSString>> = unsafe { msg_send![error, localizedDescription] };
    text.map_or_else(|| "no reason given".into(), |t| t.to_string())
}

fn responds(object: &AnyObject, selector: &CStr) -> bool {
    let sel = objc2::runtime::Sel::register(selector);
    // SAFETY: `respondsToSelector:` is NSObject's and takes any selector.
    let yes: Bool = unsafe { msg_send![object, respondsToSelector: sel] };
    yes.as_bool()
}

pub fn open_library(path: &str) -> Result<*mut c_void> {
    let c = CString::new(path)?;
    // SAFETY: a path, RTLD_NOW; the library stays loaded for the process.
    let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW) };
    if handle.is_null() {
        // SAFETY: dlerror's string is valid until the next dl call.
        let why = unsafe { CStr::from_ptr(libc::dlerror()) }
            .to_string_lossy()
            .into_owned();
        bail!("couldn't load {path}: {why}");
    }
    Ok(handle)
}

fn symbol(handle: *mut c_void, name: &str) -> Result<*mut c_void> {
    let c = CString::new(name)?;
    // SAFETY: a handle from dlopen and a symbol name.
    let f = unsafe { libc::dlsym(handle, c.as_ptr()) };
    if f.is_null() {
        bail!("SimulatorKit has no {name} in this version of Xcode");
    }
    Ok(f)
}

/// Objective-C objects shared between the helper's threads. The ones held
/// here are CoreSimulator's proxies for objects of its service, made to be
/// called from any thread; the HID client's sends are serialized.
pub struct Shared<T>(pub T);
// SAFETY: see above.
unsafe impl<T> Send for Shared<T> {}
// SAFETY: see above.
unsafe impl<T> Sync for Shared<T> {}

/// The device, its main display and its HID client.
pub struct Device {
    device: Retained<AnyObject>,
    display: Retained<AnyObject>,
    hid: Retained<AnyObject>,
    queue: *mut AnyObject,
    mouse: MouseFn,
    button: ButtonFn,
    keyboard: KeyboardFn,
    arbitrary: ArbitraryFn,
    send_lock: Mutex<()>,
    workspace_port: Mutex<Option<u32>>,
    pub scale: f32,
}

impl Device {
    /// Find a running device by UDID, in the default set or in `set`.
    pub fn open(udid: &str, set: Option<&Path>, developer_dir: &Path) -> Result<Self> {
        open_library(CORE_SIMULATOR).context("Xcode's CoreSimulator isn't installed")?;
        let kit = SIMULATOR_KIT
            .iter()
            .map(|path| developer_dir.join(path))
            .find(|path| path.exists())
            .context("this Xcode has no SimulatorKit")?;
        let kit =
            open_library(&kit.to_string_lossy()).context("this Xcode's SimulatorKit won't load")?;
        // SAFETY: the exported functions, with the signatures read from
        // this Xcode's SimulatorKit (see the module's documentation).
        let (mouse, button, keyboard, arbitrary) = unsafe {
            (
                std::mem::transmute::<*mut c_void, MouseFn>(symbol(
                    kit,
                    "IndigoHIDMessageForMouseNSEvent",
                )?),
                std::mem::transmute::<*mut c_void, ButtonFn>(symbol(
                    kit,
                    "IndigoHIDMessageForButton",
                )?),
                std::mem::transmute::<*mut c_void, KeyboardFn>(symbol(
                    kit,
                    "IndigoHIDMessageForKeyboardArbitrary",
                )?),
                std::mem::transmute::<*mut c_void, ArbitraryFn>(symbol(
                    kit,
                    "IndigoHIDMessageForHIDArbitrary",
                )?),
            )
        };
        guarded("Opening the simulator", || {
            Self::find(udid, set, developer_dir)
        })??
        .with_functions(mouse, button, keyboard, arbitrary)
    }

    fn find(udid: &str, set: Option<&Path>, developer_dir: &Path) -> Result<Parts> {
        let context_class = AnyClass::get(c"SimServiceContext")
            .context("CoreSimulator has no SimServiceContext")?;
        let mut error: *mut AnyObject = std::ptr::null_mut();
        let dir = nsstring(&developer_dir.to_string_lossy());
        // SAFETY: `+[SimServiceContext sharedServiceContextForDeveloperDir:error:]`.
        let context: Option<Retained<AnyObject>> = unsafe {
            msg_send![context_class, sharedServiceContextForDeveloperDir: &*dir, error: &mut error]
        };
        let context =
            context.ok_or_else(|| anyhow!("CoreSimulator didn't start: {}", describe(error)))?;
        let devices: Option<Retained<AnyObject>> = match set {
            Some(path) => {
                let path = nsstring(&path.to_string_lossy());
                // SAFETY: `-[SimServiceContext deviceSetWithPath:error:]`.
                unsafe { msg_send![&*context, deviceSetWithPath: &*path, error: &mut error] }
            }
            // SAFETY: `-[SimServiceContext defaultDeviceSetWithError:]`.
            None => unsafe { msg_send![&*context, defaultDeviceSetWithError: &mut error] },
        };
        let devices = devices.ok_or_else(|| anyhow!("no device set: {}", describe(error)))?;
        // SAFETY: `-[SimDeviceSet devicesByUDID]`, an NSDictionary keyed by NSUUID.
        let by_udid: Retained<AnyObject> = unsafe { msg_send![&*devices, devicesByUDID] };
        let uuid_string = nsstring(udid);
        // SAFETY: NSUUID's initializer; nil for a malformed string.
        let uuid: Option<Retained<AnyObject>> = unsafe {
            msg_send![msg_send![class!(NSUUID), alloc], initWithUUIDString: &*uuid_string]
        };
        let uuid = uuid.with_context(|| format!("{udid} isn't a UDID"))?;
        // SAFETY: NSDictionary lookup.
        let device: Option<Retained<AnyObject>> =
            unsafe { msg_send![&*by_udid, objectForKey: &*uuid] };
        let device = device.with_context(|| format!("no simulator {udid}"))?;
        // SAFETY: `-[SimDevice state]`.
        let state: u64 = unsafe { msg_send![&*device, state] };
        if state != BOOTED {
            bail!("the simulator isn't running");
        }

        // The main display among the device's IO ports.
        // SAFETY: `-[SimDevice io]` and `-[SimDeviceIOClient ioPorts]`.
        let io: Retained<AnyObject> = unsafe { msg_send![&*device, io] };
        let ports: Retained<AnyObject> = unsafe { msg_send![&*io, ioPorts] };
        // SAFETY: NSArray.
        let count: usize = unsafe { msg_send![&*ports, count] };
        let mut display = None;
        for i in 0..count {
            // SAFETY: NSArray, and the port's `descriptor`.
            let port: Retained<AnyObject> = unsafe { msg_send![&*ports, objectAtIndex: i] };
            let descriptor: Option<Retained<AnyObject>> = unsafe { msg_send![&*port, descriptor] };
            let Some(descriptor) = descriptor else {
                continue;
            };
            if !responds(&descriptor, c"framebufferSurface")
                || !responds(&descriptor, c"screenProperties")
            {
                continue;
            }
            if screen_id(&descriptor) == Some(1) {
                display = Some(descriptor);
                break;
            }
        }
        let display = display.context("the simulator has no main display")?;

        // SAFETY: `-[SimDevice deviceType]` and `-[SimDeviceType mainScreenScale]`.
        let device_type: Option<Retained<AnyObject>> = unsafe { msg_send![&*device, deviceType] };
        let scale = device_type
            .filter(|t| responds(t, c"mainScreenScale"))
            .map(|t| unsafe { msg_send![&*t, mainScreenScale] })
            .unwrap_or(1.0f32);

        let hid_class = AnyClass::get(c"_TtC12SimulatorKit24SimDeviceLegacyHIDClient")
            .context("SimulatorKit has no SimDeviceLegacyHIDClient")?;
        // SAFETY: `-[SimDeviceLegacyHIDClient initWithDevice:error:]`.
        let hid: Option<Retained<AnyObject>> = unsafe {
            msg_send![msg_send![hid_class, alloc], initWithDevice: &*device, error: &mut error]
        };
        let hid = hid.ok_or_else(|| anyhow!("no input for the simulator: {}", describe(error)))?;
        Ok(Parts {
            device,
            display,
            hid,
            scale,
        })
    }

    /// The `SimDevice`.
    pub fn sim_device(&self) -> Retained<AnyObject> {
        self.device.clone()
    }

    /// The queue CoreSimulator answers on.
    pub fn queue(&self) -> *mut AnyObject {
        self.queue
    }

    pub fn state(&self) -> Result<u64> {
        // SAFETY: `-[SimDevice state]`.
        guarded("Reading the simulator's state", || unsafe {
            msg_send![&*self.device, state]
        })
    }

    /// The framebuffer as it is now (a new one after the surfaces change).
    pub fn surface(&self) -> Result<Shared<Retained<AnyObject>>> {
        // SAFETY: `framebufferSurface`, an IOSurface.
        let surface: Option<Retained<AnyObject>> = guarded("Reading the screen", || unsafe {
            msg_send![&*self.display, framebufferSurface]
        })?;
        surface
            .map(Shared)
            .context("the simulator's screen has no framebuffer yet")
    }

    /// How the frontmost interface is turned (`UIInterfaceOrientation`).
    pub fn interface_orientation(&self) -> Result<u32> {
        guarded("Reading the screen's orientation", || {
            // SAFETY: `screenProperties.uiOrientation`, checked to exist.
            let properties: Option<Retained<AnyObject>> =
                unsafe { msg_send![&*self.display, screenProperties] };
            match properties {
                Some(p) if responds(&p, c"uiOrientation") => unsafe {
                    msg_send![&*p, uiOrientation]
                },
                _ => 1u32,
            }
        })
    }

    /// Call `changed` whenever the screen changes, and `surfaces` when the
    /// framebuffer is replaced. Returns what [`Device::stop_watching`] takes.
    pub fn watch(
        &self,
        changed: impl Fn() + 'static,
        surfaces: impl Fn() + 'static,
    ) -> Result<(Retained<AnyObject>, Retained<AnyObject>)> {
        guarded("Following the screen", || {
            // SAFETY: NSUUID, and the display's two callbacks, whose blocks
            // take one object (the damaged rectangles; the new surfaces).
            unsafe {
                let damage_id: Retained<AnyObject> = msg_send![class!(NSUUID), UUID];
                let surfaces_id: Retained<AnyObject> = msg_send![class!(NSUUID), UUID];
                let on_damage = block(move |_rects| changed());
                let on_surfaces = block(move |_surfaces| surfaces());
                let _: () = msg_send![
                    &*self.display,
                    registerCallbackWithUUID: &*damage_id,
                    damageRectanglesCallback: &*on_damage
                ];
                let _: () = msg_send![
                    &*self.display,
                    registerCallbackWithUUID: &*surfaces_id,
                    ioSurfacesChangeCallback: &*on_surfaces
                ];
                (damage_id, surfaces_id)
            }
        })
    }

    pub fn stop_watching(&self, ids: &(Retained<AnyObject>, Retained<AnyObject>)) {
        let _ = guarded("Leaving the screen", || {
            // SAFETY: the unregistering halves of the calls in `watch`.
            unsafe {
                let _: () =
                    msg_send![&*self.display, unregisterDamageRectanglesCallbackWithUUID: &*ids.0];
                let _: () =
                    msg_send![&*self.display, unregisterIOSurfacesChangeCallbackWithUUID: &*ids.1];
            }
        });
    }

    /// Send a message made by one of SimulatorKit's functions and wait until
    /// the device has it.
    fn send(&self, message: *mut c_void) -> Result<()> {
        if message.is_null() {
            bail!("SimulatorKit made no message");
        }
        let _serial = self.send_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, rx) = mpsc::channel::<Option<String>>();
        let done = block(move |error: *mut AnyObject| {
            let _ = tx.send((!error.is_null()).then(|| describe(error)));
        });
        guarded("Sending input", || {
            // SAFETY: `sendWithMessage:freeWhenDone:completionQueue:completion:`;
            // the message was calloc'ed by SimulatorKit and is freed by it.
            unsafe {
                let _: () = msg_send![
                    &*self.hid,
                    sendWithMessage: message,
                    freeWhenDone: Bool::YES,
                    completionQueue: self.queue,
                    completion: &*done
                ];
            }
        })?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(None) => Ok(()),
            Ok(Some(why)) => bail!("the simulator refused input: {why}"),
            Err(_) => bail!("the simulator didn't take the input"),
        }
    }

    /// A finger on the screen as built, at a fraction of it. `kind` is an
    /// `NSEventType`: 1 down, 6 dragged, 2 up. A drag less than 16 ms after
    /// the last message is dropped by SimulatorKit: `Ok(false)`.
    pub fn touch(&self, kind: u64, x: f64, y: f64) -> Result<bool> {
        self.fingers(kind, &[(x, y)])
    }

    /// Fingers that go down, move and lift together (a pinch, a two-finger
    /// turn), as [`Device::touch`].
    pub fn fingers(&self, kind: u64, points: &[(f64, f64)]) -> Result<bool> {
        let Some(&(x, y)) = points.first() else {
            bail!("a touch needs a finger");
        };
        let point = CGPoint { x, y };
        let second = points.get(1).map(|&(x, y)| CGPoint { x, y });
        let size = CGSize {
            width: 1.0,
            height: 1.0,
        };
        let second_ptr = second
            .as_ref()
            .map_or(std::ptr::null(), |p| p as *const CGPoint);
        // SAFETY: see `MouseFn`; the second point, when there is one, lives
        // until the call returns. Edge 0: an ordinary touch (other edges
        // are swipes from the screen's edges).
        let message = unsafe { (self.mouse)(&point, second_ptr, TOUCH_TARGET, kind, size, 0) };
        if message.is_null() {
            return Ok(false);
        }
        let message = if points.len() > 2 {
            more_fingers(message, &points[2..])?
        } else {
            message
        };
        self.send(message).map(|()| true)
    }

    /// Press or release a hardware button (0 Home, 1 Lock).
    pub fn button(&self, source: u32, direction: u32) -> Result<()> {
        // SAFETY: see `ButtonFn`.
        self.send(unsafe { (self.button)(source, direction, BUTTON_TARGET) })
    }

    /// Press or release a key of the keyboard page.
    pub fn key(&self, usage: u32, direction: u32) -> Result<()> {
        // SAFETY: see `KeyboardFn`.
        self.send(unsafe { (self.keyboard)(usage, direction) })
    }

    /// Press or release any HID usage, such as the consumer page's volume.
    pub fn usage(&self, page: u32, usage: u32, direction: u32) -> Result<()> {
        // SAFETY: see `ArbitraryFn`.
        self.send(unsafe { (self.arbitrary)(BUTTON_TARGET, page, usage, direction) })
    }

    /// Turn the device (`UIDeviceOrientation`), as Simulator.app's Rotate
    /// Left and Right do.
    pub fn hold(&self, orientation: u32) -> Result<()> {
        let port = {
            let mut cached = self
                .workspace_port
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            match *cached {
                Some(port) => port,
                None => {
                    let name = nsstring("PurpleWorkspacePort");
                    let mut error: *mut AnyObject = std::ptr::null_mut();
                    // SAFETY: `-[SimDevice lookup:error:]`, a send right or 0.
                    let port: u32 = guarded("Finding the simulator's workspace", || unsafe {
                        msg_send![&*self.device, lookup: &*name, error: &mut error]
                    })?;
                    if port == 0 {
                        bail!("the simulator can't be turned: {}", describe(error));
                    }
                    *cached = Some(port);
                    port
                }
            }
        };
        let mut message = [0u8; 0x70];
        let mut put = |offset: usize, value: u32| {
            message[offset..offset + 4].copy_from_slice(&value.to_ne_bytes())
        };
        // mach_msg_header_t: copy-send to the port, id 123.
        put(0x00, 0x13);
        put(0x04, (4 + 0x6b) & !3);
        put(0x08, port);
        put(0x14, 123);
        // The event: device orientation changed, from the host; four bytes of
        // data, the orientation.
        put(0x18, 50 | 0x2_0000);
        put(0x48, 4);
        put(0x4c, orientation);
        // SAFETY: a complete Mach message with a send right we hold.
        let result = unsafe { mach_msg_send(message.as_mut_ptr().cast()) };
        if result != 0 {
            *self
                .workspace_port
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
            bail!("the simulator didn't take the turn (Mach error {result:#x})");
        }
        Ok(())
    }
}

/// The layout of a touch message (see the module's documentation).
const HEADER: usize = 0x20;
const EVENT: usize = 0xa0;
const EVENTS_AT: usize = 0x1c;
const FINGER_ID: usize = 0x10;
const FINGER_POINT: usize = 0x1c;
/// The most fingers a touch takes (two hands').
pub const MOST_FINGERS: usize = 10;

/// A two-finger message from SimulatorKit with `more` fingers after its
/// two, each made from the second's event, moved to its own point. Frees
/// `message` and returns a new one, `calloc`ed as SimulatorKit's are.
fn more_fingers(message: *mut c_void, more: &[(f64, f64)]) -> Result<*mut c_void> {
    let old = message.cast::<u8>();
    // SAFETY: SimulatorKit's message, read within its header.
    let events = unsafe { *old.add(EVENTS_AT) } as usize;
    if events != 3 {
        // SAFETY: the message SimulatorKit made, freed once.
        unsafe { libc::free(message) };
        bail!("SimulatorKit's touch has {events} events, not a hand and two fingers");
    }
    let fingers = 2 + more.len();
    let size = HEADER + (fingers + 1) * EVENT;
    // SAFETY: a fresh buffer, filled within its size from the old message's
    // `HEADER + 3 * EVENT` bytes; the old one is freed once.
    unsafe {
        let new = libc::calloc(1, size).cast::<u8>();
        if new.is_null() {
            libc::free(message);
            bail!("no memory for a touch");
        }
        std::ptr::copy_nonoverlapping(old, new, HEADER + 3 * EVENT);
        libc::free(message);
        *new.add(EVENTS_AT) = (fingers + 1) as u8;
        let second = new.add(HEADER + 2 * EVENT);
        for (i, &(x, y)) in more.iter().enumerate() {
            let finger = 2 + i;
            let event = new.add(HEADER + (finger + 1) * EVENT);
            std::ptr::copy_nonoverlapping(second, event, EVENT);
            let id = [(finger + 1) as u32, (finger + 2) as u32];
            std::ptr::copy_nonoverlapping(id.as_ptr().cast::<u8>(), event.add(FINGER_ID), 8);
            let point = [x, y];
            std::ptr::copy_nonoverlapping(point.as_ptr().cast::<u8>(), event.add(FINGER_POINT), 16);
        }
        Ok(new.cast())
    }
}

struct Parts {
    device: Retained<AnyObject>,
    display: Retained<AnyObject>,
    hid: Retained<AnyObject>,
    scale: f32,
}

impl Parts {
    fn with_functions(
        self,
        mouse: MouseFn,
        button: ButtonFn,
        keyboard: KeyboardFn,
        arbitrary: ArbitraryFn,
    ) -> Result<Device> {
        // SAFETY: a global queue; it lives for the process.
        let queue = unsafe { dispatch_get_global_queue(0x21, 0) };
        Ok(Device {
            device: self.device,
            display: self.display,
            hid: self.hid,
            queue,
            mouse,
            button,
            keyboard,
            arbitrary,
            send_lock: Mutex::new(()),
            workspace_port: Mutex::new(None),
            scale: self.scale,
        })
    }
}

fn screen_id(descriptor: &AnyObject) -> Option<u32> {
    // SAFETY: `screenProperties.screenID`, checked to exist.
    let properties: Option<Retained<AnyObject>> =
        unsafe { msg_send![descriptor, screenProperties] };
    let properties = properties?;
    responds(&properties, c"screenID").then(|| unsafe { msg_send![&*properties, screenID] })
}
