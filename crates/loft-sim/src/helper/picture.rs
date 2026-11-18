//! Pictures of the framebuffer: read from its IOSurface, scaled and turned
//! upright with vImage, encoded with ImageIO (JPEG for the stream, PNG for
//! captures).

use super::ax::Frame;
use crate::geometry::Turn;
use anyhow::{Result, bail};
use std::ffi::c_void;

#[repr(C)]
struct VImageBuffer {
    data: *mut c_void,
    height: usize,
    width: usize,
    row_bytes: usize,
}

type CFTypeRef = *const c_void;

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceLock(surface: *const c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: *const c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceGetBaseAddress(surface: *const c_void) -> *mut c_void;
    fn IOSurfaceGetWidth(surface: *const c_void) -> usize;
    fn IOSurfaceGetHeight(surface: *const c_void) -> usize;
    fn IOSurfaceGetBytesPerRow(surface: *const c_void) -> usize;
    fn IOSurfaceGetPixelFormat(surface: *const c_void) -> u32;
}

#[link(name = "Accelerate", kind = "framework")]
unsafe extern "C" {
    fn vImageScale_ARGB8888(
        src: *const VImageBuffer,
        dest: *const VImageBuffer,
        temp: *mut c_void,
        flags: u32,
    ) -> isize;
    fn vImageRotate90_ARGB8888(
        src: *const VImageBuffer,
        dest: *const VImageBuffer,
        rotation: u8,
        back_color: *const u8,
        flags: u32,
    ) -> isize;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: CFTypeRef);
    fn CFDataCreateMutable(allocator: CFTypeRef, capacity: isize) -> CFTypeRef;
    fn CFDataGetLength(data: CFTypeRef) -> isize;
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFNumberCreate(allocator: CFTypeRef, kind: isize, value: *const c_void) -> CFTypeRef;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGColorSpaceCreateWithName(name: CFTypeRef) -> CFTypeRef;
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: CFTypeRef,
        info: u32,
    ) -> CFTypeRef;
    fn CGBitmapContextCreateImage(context: CFTypeRef) -> CFTypeRef;
    static kCGColorSpaceSRGB: CFTypeRef;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    fn CGImageDestinationCreateWithData(
        data: CFTypeRef,
        kind: CFTypeRef,
        count: usize,
        options: CFTypeRef,
    ) -> CFTypeRef;
    fn CGImageDestinationAddImage(destination: CFTypeRef, image: CFTypeRef, properties: CFTypeRef);
    fn CGImageDestinationFinalize(destination: CFTypeRef) -> bool;
    static kCGImageDestinationLossyCompressionQuality: CFTypeRef;
}

const READ_ONLY: u32 = 1;
const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
/// `kCGImageAlphaNoneSkipFirst | kCGBitmapByteOrder32Little`: BGRA in memory.
const BGRX_INFO: u32 = 6 | (2 << 12);
const CF_NUMBER_DOUBLE: isize = 13;

/// A picture in memory, BGRA, upright.
pub struct Pixels {
    pub data: Vec<u8>,
    pub width: usize,
    pub height: usize,
}

/// Owns a CoreFoundation object.
struct Cf(CFTypeRef);

impl Drop for Cf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: an object we created (a +1 reference).
            unsafe { CFRelease(self.0) }
        }
    }
}

/// The surface's size in pixels.
pub fn surface_size(surface: *const c_void) -> (usize, usize) {
    // SAFETY: an IOSurface the caller holds.
    unsafe { (IOSurfaceGetWidth(surface), IOSurfaceGetHeight(surface)) }
}

/// Read `surface` at `width` × `height` (its own size or smaller, as built)
/// and turn it upright for `turn`.
pub fn read(surface: *const c_void, width: usize, height: usize, turn: Turn) -> Result<Pixels> {
    // SAFETY: an IOSurface the caller holds; it is locked while read.
    let scaled = unsafe {
        if IOSurfaceGetPixelFormat(surface) != BGRA {
            bail!("the simulator's screen isn't BGRA");
        }
        if IOSurfaceLock(surface, READ_ONLY, std::ptr::null_mut()) != 0 {
            bail!("the simulator's screen couldn't be read");
        }
        let src = VImageBuffer {
            data: IOSurfaceGetBaseAddress(surface),
            height: IOSurfaceGetHeight(surface),
            width: IOSurfaceGetWidth(surface),
            row_bytes: IOSurfaceGetBytesPerRow(surface),
        };
        let mut data = vec![0u8; width * height * 4];
        let result = if (src.width, src.height) == (width, height) {
            for row in 0..height {
                std::ptr::copy_nonoverlapping(
                    (src.data as *const u8).add(row * src.row_bytes),
                    data.as_mut_ptr().add(row * width * 4),
                    width * 4,
                );
            }
            0
        } else {
            let dest = VImageBuffer {
                data: data.as_mut_ptr().cast(),
                height,
                width,
                row_bytes: width * 4,
            };
            vImageScale_ARGB8888(&src, &dest, std::ptr::null_mut(), 0)
        };
        IOSurfaceUnlock(surface, READ_ONLY, std::ptr::null_mut());
        if result != 0 {
            bail!("vImage couldn't scale the screen ({result})");
        }
        Pixels {
            data,
            width,
            height,
        }
    };
    turn_upright(scaled, turn)
}

fn turn_upright(pixels: Pixels, turn: Turn) -> Result<Pixels> {
    let quarters = turn.clockwise_quarters();
    if quarters == 0 {
        return Ok(pixels);
    }
    let (width, height) = if quarters % 2 == 1 {
        (pixels.height, pixels.width)
    } else {
        (pixels.width, pixels.height)
    };
    let mut data = vec![0u8; width * height * 4];
    let src = VImageBuffer {
        data: pixels.data.as_ptr() as *mut c_void,
        height: pixels.height,
        width: pixels.width,
        row_bytes: pixels.width * 4,
    };
    let dest = VImageBuffer {
        data: data.as_mut_ptr().cast(),
        height,
        width,
        row_bytes: width * 4,
    };
    // vImage counts clockwise turns as 3 (90°), 2 (180°), 1 (270°).
    let rotation = match quarters {
        1 => 3,
        2 => 2,
        _ => 1,
    };
    let black = [0u8; 4];
    // SAFETY: both buffers are as large as they say.
    let result = unsafe { vImageRotate90_ARGB8888(&src, &dest, rotation, black.as_ptr(), 0) };
    if result != 0 {
        bail!("vImage couldn't turn the screen ({result})");
    }
    Ok(Pixels {
        data,
        width,
        height,
    })
}

/// Outline `frames` (points, `factor` pixels each) in the picture, a point
/// wide, in a pink that shows on light and dark screens alike.
pub fn outline(pixels: &mut Pixels, frames: &[Frame], factor: f64) {
    const BGRA: [u8; 4] = [0x8c, 0x2d, 0xff, 0xff];
    let width = pixels.width as i64;
    let height = pixels.height as i64;
    let thick = (factor * 1.0).round().max(1.0) as i64;
    let mut fill = |x0: i64, y0: i64, x1: i64, y1: i64| {
        let (x0, x1) = (x0.clamp(0, width), x1.clamp(0, width));
        let (y0, y1) = (y0.clamp(0, height), y1.clamp(0, height));
        for y in y0..y1 {
            let row = (y * width) as usize * 4;
            for x in x0..x1 {
                let at = row + x as usize * 4;
                pixels.data[at..at + 4].copy_from_slice(&BGRA);
            }
        }
    };
    for frame in frames {
        let x0 = (frame.x * factor).round() as i64;
        let y0 = (frame.y * factor).round() as i64;
        let x1 = ((frame.x + frame.width) * factor).round() as i64;
        let y1 = ((frame.y + frame.height) * factor).round() as i64;
        fill(x0, y0, x1, y0 + thick);
        fill(x0, y1 - thick, x1, y1);
        fill(x0, y0, x0 + thick, y1);
        fill(x1 - thick, y0, x1, y1);
    }
}

/// Encode as JPEG (`quality` 0–1) or, with `None`, PNG.
pub fn encode(pixels: &mut Pixels, quality: Option<f64>) -> Result<Vec<u8>> {
    // SAFETY: CoreGraphics and ImageIO over a buffer we own, every object
    // released by `Cf`.
    unsafe {
        let space = Cf(CGColorSpaceCreateWithName(kCGColorSpaceSRGB));
        let context = Cf(CGBitmapContextCreate(
            pixels.data.as_mut_ptr().cast(),
            pixels.width,
            pixels.height,
            8,
            pixels.width * 4,
            space.0,
            BGRX_INFO,
        ));
        if context.0.is_null() {
            bail!("CoreGraphics couldn't take the picture");
        }
        let image = Cf(CGBitmapContextCreateImage(context.0));
        let out = Cf(CFDataCreateMutable(std::ptr::null(), 0));
        let kind = match quality {
            Some(_) => objc2_foundation::ns_string!("public.jpeg"),
            None => objc2_foundation::ns_string!("public.png"),
        };
        let kind_ref: CFTypeRef = (kind as *const objc2_foundation::NSString).cast();
        let destination = Cf(CGImageDestinationCreateWithData(
            out.0,
            kind_ref,
            1,
            std::ptr::null(),
        ));
        if destination.0.is_null() || image.0.is_null() {
            bail!("ImageIO couldn't encode the picture");
        }
        let options = quality.map(|q| {
            let number = Cf(CFNumberCreate(
                std::ptr::null(),
                CF_NUMBER_DOUBLE,
                (&q as *const f64).cast(),
            ));
            let keys = [kCGImageDestinationLossyCompressionQuality];
            let values = [number.0];
            Cf(CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                1,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            ))
        });
        CGImageDestinationAddImage(
            destination.0,
            image.0,
            options.as_ref().map_or(std::ptr::null(), |o| o.0),
        );
        if !CGImageDestinationFinalize(destination.0) {
            bail!("ImageIO couldn't encode the picture");
        }
        let len = CFDataGetLength(out.0) as usize;
        Ok(std::slice::from_raw_parts(CFDataGetBytePtr(out.0), len).to_vec())
    }
}
