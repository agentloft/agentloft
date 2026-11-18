//! What accessibility sees on a simulator's screen: the frame of every
//! element of the app in front, for the accessibility frames overlay.
//!
//! Found by reading macOS's AccessibilityPlatformTranslation framework and
//! CoreSimulator (their Objective-C classes and protocols), not from
//! anyone's code:
//!
//! - `AXPTranslator` (`+sharedInstance`) turns the device's accessibility
//!   into macOS accessibility elements (`AXPMacPlatformElement`). It asks
//!   its `bridgeTokenDelegate` (protocol `AXPTranslationTokenDelegateHelper`)
//!   for a block per token that carries each `AXPTranslatorRequest` to the
//!   device and returns its `AXPTranslatorResponse`, once
//!   `supportsDelegateTokens` is on.
//! - `-[SimDevice sendAccessibilityRequestAsync:completionQueue:
//!   completionHandler:]` carries a request to the device; the token is the
//!   device's `accessibilityPlatformTranslationToken`.
//! - `-[AXPTranslator frontmostApplicationWithDisplayId:bridgeDelegateToken:]`
//!   is the app in front; `macPlatformElementFromTranslation:` makes it an
//!   element, whose `accessibilityFrame` (points of the device's screen, as
//!   the delegate's `…ConvertPlatformFrameToSystem…` leaves them) and
//!   `AXChildren` attribute walk the tree.

use super::sim::{Device, Shared, guarded};
use anyhow::{Context, Result};
use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, NSObject};
use objc2::{AllocAnyThread, DefinedClass, class, define_class, msg_send};
use objc2_foundation::{NSRect, NSString};
use std::ffi::CStr;
use std::sync::mpsc;
use std::time::Duration;

const TRANSLATION: &str = "/System/Library/PrivateFrameworks/AccessibilityPlatformTranslation.framework/AccessibilityPlatformTranslation";

/// How long the device may take over one request.
const ANSWER_TIME: Duration = Duration::from_secs(3);
/// The tree is cut off here (a long list has hundreds of rows).
const MOST_ELEMENTS: usize = 600;
const DEEPEST: usize = 40;

/// A frame, in points of the screen as the interface shows it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Frame {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// `id (^)(id)`: the bridge's block, a request in and a response out.
struct RequestBlock;

// SAFETY: `@16@?0@8` is `id (^)(id)` on a 64-bit target, which these are.
unsafe impl block2::ManualBlockEncoding for RequestBlock {
    type Arguments = (*mut AnyObject,);
    type Return = *mut AnyObject;
    const ENCODING_CSTR: &'static CStr = c"@16@?0@8";
}

type Callback = RcBlock<dyn Fn(*mut AnyObject) -> *mut AnyObject>;

pub struct BridgeIvars {
    callback: Callback,
}

define_class!(
    // SAFETY: `NSObject` has no subclassing requirements; the methods are
    // those of `AXPTranslationTokenDelegateHelper`, with its types.
    #[unsafe(super(NSObject))]
    #[name = "AgentloftSimulatorAccessibilityBridge"]
    #[ivars = BridgeIvars]
    pub struct Bridge;

    impl Bridge {
        #[unsafe(method(accessibilityTranslationDelegateBridgeCallbackWithToken:))]
        fn callback(
            &self,
            _token: *mut AnyObject,
        ) -> *mut block2::Block<dyn Fn(*mut AnyObject) -> *mut AnyObject> {
            RcBlock::as_ptr(&self.ivars().callback)
        }

        #[unsafe(method(accessibilityTranslationConvertPlatformFrameToSystem:withToken:))]
        fn convert(&self, rect: NSRect, _token: *mut AnyObject) -> NSRect {
            rect
        }

        #[unsafe(method(accessibilityTranslationRootParentWithToken:))]
        fn root_parent(&self, _token: *mut AnyObject) -> *mut AnyObject {
            std::ptr::null_mut()
        }
    }
);

/// The device's accessibility, through the translator. Made and used on one
/// thread.
pub struct Accessibility {
    translator: Retained<AnyObject>,
    token: Retained<NSString>,
    // The translator holds its delegate weakly.
    _bridge: Retained<Bridge>,
}

impl Accessibility {
    pub fn new(device: &Device) -> Result<Self> {
        super::sim::open_library(TRANSLATION)
            .context("this Mac has no AccessibilityPlatformTranslation")?;
        guarded("Opening the simulator's accessibility", || {
            let class = AnyClass::get(c"AXPTranslator")
                .context("AccessibilityPlatformTranslation has no AXPTranslator")?;
            let sim = Shared(device.sim_device());
            // SAFETY: `-[SimDevice accessibilityPlatformTranslationToken]`.
            let token: Option<Retained<NSString>> =
                unsafe { msg_send![&*sim.0, accessibilityPlatformTranslationToken] };
            let token = token.context("the simulator has no accessibility token")?;
            let queue = Shared(device.queue());
            let callback: Callback = RcBlock::with_encoding::<_, _, _, RequestBlock>(
                move |request: *mut AnyObject| -> *mut AnyObject {
                    match ask(&sim.0, queue.0, request) {
                        Some(response) => Retained::autorelease_ptr(response),
                        // SAFETY: `+[AXPTranslatorResponse emptyResponse]`.
                        None => unsafe {
                            let empty: Option<Retained<AnyObject>> =
                                msg_send![class!(AXPTranslatorResponse), emptyResponse];
                            empty.map_or(std::ptr::null_mut(), Retained::autorelease_ptr)
                        },
                    }
                },
            );
            let bridge = Bridge::alloc().set_ivars(BridgeIvars { callback });
            // SAFETY: NSObject's `init` on a fresh instance.
            let bridge: Retained<Bridge> = unsafe { msg_send![super(bridge), init] };
            // SAFETY: the translator's shared instance, its delegate and
            // switch (see the module's documentation).
            let translator: Retained<AnyObject> = unsafe {
                let translator: Option<Retained<AnyObject>> = msg_send![class, sharedInstance];
                let translator = translator.context("no AXPTranslator")?;
                let _: () = msg_send![&*translator, setBridgeTokenDelegate: &*bridge];
                let _: () = msg_send![&*translator, setSupportsDelegateTokens: true];
                translator
            };
            Ok(Self {
                translator,
                token,
                _bridge: bridge,
            })
        })?
    }

    /// The frames of the elements of the app in front; `None` while there
    /// is none to read (a device that just started, an app on its way).
    pub fn frames(&self) -> Result<Option<Vec<Frame>>> {
        guarded("Reading the simulator's accessibility", || {
            autoreleasepool(|_| {
                // SAFETY: the translator's methods for the app in front and
                // its element (see the module's documentation).
                let root: Option<Retained<AnyObject>> = unsafe {
                    let app: Option<Retained<AnyObject>> = msg_send![
                        &*self.translator,
                        frontmostApplicationWithDisplayId: 0u32,
                        bridgeDelegateToken: &*self.token
                    ];
                    let app = app?;
                    msg_send![&*self.translator, macPlatformElementFromTranslation: &*app]
                };
                let mut frames = Vec::new();
                walk(&*root?, 0, &mut frames);
                Some(frames)
            })
        })
    }
}

/// Carry `request` to the device and wait for its answer.
fn ask(
    device: &AnyObject,
    queue: *mut AnyObject,
    request: *mut AnyObject,
) -> Option<Retained<AnyObject>> {
    if request.is_null() {
        return None;
    }
    let (tx, rx) = mpsc::channel::<Shared<Option<Retained<AnyObject>>>>();
    let done = super::sim::block(move |response: *mut AnyObject| {
        // SAFETY: the response CoreSimulator passed, retained for us.
        let response = unsafe { Retained::retain(response) };
        let _ = tx.send(Shared(response));
    });
    // SAFETY: `-[SimDevice sendAccessibilityRequestAsync:completionQueue:
    // completionHandler:]`, with a request the translator made.
    unsafe {
        let _: () = msg_send![
            device,
            sendAccessibilityRequestAsync: request,
            completionQueue: queue,
            completionHandler: &*done
        ];
    }
    rx.recv_timeout(ANSWER_TIME).ok().and_then(|r| r.0)
}

fn walk(element: &AnyObject, depth: usize, frames: &mut Vec<Frame>) {
    if frames.len() >= MOST_ELEMENTS || depth > DEEPEST {
        return;
    }
    // SAFETY: NSAccessibility's `accessibilityFrame`, which every
    // AXPMacPlatformElement has.
    let rect: NSRect = unsafe { msg_send![element, accessibilityFrame] };
    if depth > 0 && rect.size.width > 0.0 && rect.size.height > 0.0 {
        frames.push(Frame {
            x: rect.origin.x,
            y: rect.origin.y,
            width: rect.size.width,
            height: rect.size.height,
        });
    }
    let name = objc2_foundation::ns_string!("AXChildren");
    // SAFETY: `accessibilityAttributeValue:`, an NSArray of elements or nil.
    let children: Option<Retained<AnyObject>> =
        unsafe { msg_send![element, accessibilityAttributeValue: name] };
    let Some(children) = children else { return };
    // SAFETY: NSArray.
    let count: usize = unsafe { msg_send![&*children, count] };
    for i in 0..count {
        // SAFETY: NSArray, within its count.
        let child: Retained<AnyObject> = unsafe { msg_send![&*children, objectAtIndex: i] };
        walk(&child, depth + 1, frames);
    }
}
