//! H.264 decoded by macOS itself (VideoToolbox, in hardware) and its
//! pictures encoded as JPEG by ImageIO: neither depends on how this crate
//! was optimised, so a debug daemon streams as fast as a release one.
//!
//! Only the few C functions needed are declared here, by hand.

#![allow(unsafe_code, non_upper_case_globals, non_snake_case)]

use anyhow::{anyhow, bail};
use std::ffi::{c_char, c_void};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};

type OSStatus = i32;
type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFStringRef = *const c_void;
type CFMutableDataRef = *mut c_void;
type CMFormatDescriptionRef = *const c_void;
type CMBlockBufferRef = *mut c_void;
type CMSampleBufferRef = *mut c_void;
type CVImageBufferRef = *mut c_void;
type CGImageRef = *mut c_void;
type CGImageDestinationRef = *mut c_void;
type VTDecompressionSessionRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

#[repr(C)]
struct CMVideoDimensions {
    width: i32,
    height: i32,
}

type OutputCallback = extern "C" fn(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: OSStatus,
    info: u32,
    image: CVImageBufferRef,
    pts: CMTime,
    duration: CMTime,
);

#[repr(C)]
struct OutputCallbackRecord {
    callback: OutputCallback,
    refcon: *mut c_void,
}

/// `CFDictionaryKeyCallBacks` / `ValueCallBacks`: only their address is used.
#[repr(C)]
struct CallBacks {
    _words: [usize; 6],
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: CallBacks;
    static kCFTypeDictionaryValueCallBacks: CallBacks;
    fn CFDictionaryCreate(
        allocator: CFAllocatorRef,
        keys: *const *const c_void,
        values: *const *const c_void,
        count: isize,
        key_callbacks: *const CallBacks,
        value_callbacks: *const CallBacks,
    ) -> CFDictionaryRef;
    fn CFNumberCreate(allocator: CFAllocatorRef, kind: isize, value: *const c_void) -> CFTypeRef;
    fn CFStringCreateWithCString(
        allocator: CFAllocatorRef,
        text: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFDataCreateMutable(allocator: CFAllocatorRef, capacity: isize) -> CFMutableDataRef;
    fn CFDataGetLength(data: CFMutableDataRef) -> isize;
    fn CFDataGetBytePtr(data: CFMutableDataRef) -> *const u8;
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: CFAllocatorRef,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        nal_header_length: i32,
        out: *mut CMFormatDescriptionRef,
    ) -> OSStatus;
    fn CMVideoFormatDescriptionGetDimensions(desc: CMFormatDescriptionRef) -> CMVideoDimensions;
    fn CMBlockBufferCreateWithMemoryBlock(
        allocator: CFAllocatorRef,
        memory: *mut c_void,
        length: usize,
        block_allocator: CFAllocatorRef,
        custom_source: *const c_void,
        offset: usize,
        data_length: usize,
        flags: u32,
        out: *mut CMBlockBufferRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(
        source: *const c_void,
        dest: CMBlockBufferRef,
        offset: usize,
        length: usize,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFAllocatorRef,
        data: CMBlockBufferRef,
        format: CMFormatDescriptionRef,
        samples: isize,
        timings: isize,
        timing: *const c_void,
        sizes_count: isize,
        sizes: *const usize,
        out: *mut CMSampleBufferRef,
    ) -> OSStatus;
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: CFStringRef;
    static kCVPixelBufferWidthKey: CFStringRef;
    static kCVPixelBufferHeightKey: CFStringRef;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: CFAllocatorRef,
        format: CMFormatDescriptionRef,
        decoder_spec: CFDictionaryRef,
        image_attributes: CFDictionaryRef,
        callback: *const OutputCallbackRecord,
        out: *mut VTDecompressionSessionRef,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: VTDecompressionSessionRef,
        sample: CMSampleBufferRef,
        flags: u32,
        frame_refcon: *mut c_void,
        info_out: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: VTDecompressionSessionRef);
    fn VTCreateCGImageFromCVPixelBuffer(
        pixels: CVImageBufferRef,
        options: CFDictionaryRef,
        out: *mut CGImageRef,
    ) -> OSStatus;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    static kCGImageDestinationLossyCompressionQuality: CFStringRef;
    fn CGImageDestinationCreateWithData(
        data: CFMutableDataRef,
        kind: CFStringRef,
        count: usize,
        options: CFDictionaryRef,
    ) -> CGImageDestinationRef;
    fn CGImageDestinationAddImage(
        dest: CGImageDestinationRef,
        image: CGImageRef,
        properties: CFDictionaryRef,
    );
    fn CGImageDestinationFinalize(dest: CGImageDestinationRef) -> bool;
}

const kCFNumberSInt32Type: isize = 3;
const kCFNumberFloat64Type: isize = 6;
const kCFStringEncodingUTF8: u32 = 0x0800_0100;
const kCVPixelFormatType_32BGRA: i32 = 0x4247_5241;

/// A Core Foundation object we own one reference to.
struct Owned(CFTypeRef);

impl Owned {
    fn new(cf: CFTypeRef) -> Option<Self> {
        (!cf.is_null()).then_some(Self(cf))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: we hold one reference, released once.
        unsafe { CFRelease(self.0) }
    }
}

fn number_i32(v: i32) -> Option<Owned> {
    // SAFETY: a CFNumber from a valid i32.
    Owned::new(unsafe { CFNumberCreate(null(), kCFNumberSInt32Type, (&raw const v).cast()) })
}

fn number_f64(v: f64) -> Option<Owned> {
    // SAFETY: a CFNumber from a valid f64.
    Owned::new(unsafe { CFNumberCreate(null(), kCFNumberFloat64Type, (&raw const v).cast()) })
}

fn dictionary(pairs: &[(CFStringRef, &Owned)]) -> Option<Owned> {
    let keys: Vec<*const c_void> = pairs.iter().map(|(k, _)| *k).collect();
    let values: Vec<*const c_void> = pairs.iter().map(|(_, v)| v.0).collect();
    // SAFETY: keys and values are valid CF objects; the dictionary retains
    // them.
    Owned::new(unsafe {
        CFDictionaryCreate(
            null(),
            keys.as_ptr(),
            values.as_ptr(),
            pairs.len() as isize,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    })
}

/// A decoded picture (a `CVPixelBuffer`), held until encoded.
pub struct Pixels(Owned);

// SAFETY: a CVPixelBuffer the decoder has finished with is only read, and
// Core Foundation's reference counting is thread-safe.
unsafe impl Send for Pixels {}
// SAFETY: as above: nothing writes to it.
unsafe impl Sync for Pixels {}

impl Pixels {
    /// The picture as a JPEG, and its size.
    pub fn jpeg(&self, quality: f64) -> anyhow::Result<(Vec<u8>, u32, u32)> {
        let mut image: CGImageRef = null_mut();
        // SAFETY: a valid pixel buffer; `image` receives a new reference.
        let status =
            unsafe { VTCreateCGImageFromCVPixelBuffer(self.0.0 as _, null(), &raw mut image) };
        if status != 0 || image.is_null() {
            bail!("VideoToolbox couldn't make an image ({status})");
        }
        let image = Owned(image as _);
        // SAFETY: plain Core Foundation calls on valid objects; each object
        // made here is released by its `Owned`.
        unsafe {
            let data = Owned::new(CFDataCreateMutable(null(), 0) as _)
                .ok_or_else(|| anyhow!("no memory for a JPEG"))?;
            let kind = Owned::new(CFStringCreateWithCString(
                null(),
                c"public.jpeg".as_ptr(),
                kCFStringEncodingUTF8,
            ))
            .ok_or_else(|| anyhow!("no JPEG type"))?;
            let dest =
                Owned::new(CGImageDestinationCreateWithData(data.0 as _, kind.0, 1, null()) as _)
                    .ok_or_else(|| anyhow!("ImageIO can't write JPEG"))?;
            let quality = number_f64(quality).ok_or_else(|| anyhow!("no number"))?;
            let props = dictionary(&[(kCGImageDestinationLossyCompressionQuality, &quality)])
                .ok_or_else(|| anyhow!("no dictionary"))?;
            CGImageDestinationAddImage(dest.0 as _, image.0 as _, props.0);
            if !CGImageDestinationFinalize(dest.0 as _) {
                bail!("ImageIO couldn't write the JPEG");
            }
            let len = CFDataGetLength(data.0 as _).max(0) as usize;
            let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(data.0 as _), len).to_vec();
            let (w, h) = super::jpeg_size(&bytes).unwrap_or((0, 0));
            Ok((bytes, w, h))
        }
    }
}

/// Where the decoder's callback leaves each picture.
#[derive(Default)]
struct Out {
    latest: Mutex<Option<Pixels>>,
    failures: std::sync::atomic::AtomicU32,
}

extern "C" fn on_picture(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: OSStatus,
    _info: u32,
    image: CVImageBufferRef,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: `refcon` is the `Out` the session was made with, kept alive
    // by the `Decoder` until the session is invalidated.
    let out = unsafe { &*(refcon as *const Out) };
    if status != 0 || image.is_null() {
        out.failures
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }
    // SAFETY: the callback doesn't own `image`: retain it to keep it.
    let pixels = Pixels(Owned(unsafe { CFRetain(image as _) }));
    *out.latest.lock().unwrap_or_else(|e| e.into_inner()) = Some(pixels);
}

/// A VideoToolbox decompression session for one stream's parameter sets.
pub struct Decoder {
    session: VTDecompressionSessionRef,
    format: Owned,
    out: Arc<Out>,
}

// SAFETY: the session is used from one thread at a time (`&mut self`).
unsafe impl Send for Decoder {}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: the session is valid; invalidating it waits for its
        // callbacks, after which `out` may go.
        unsafe {
            VTDecompressionSessionInvalidate(self.session);
            CFRelease(self.session as _);
        }
    }
}

impl Decoder {
    /// A decoder for the stream `sps`/`pps` describe, giving pictures at
    /// most `max` pixels on their long side (scaled by VideoToolbox).
    pub fn new(sps: &[u8], pps: &[u8], max: u32) -> anyhow::Result<Self> {
        let sets = [sps.as_ptr(), pps.as_ptr()];
        let sizes = [sps.len(), pps.len()];
        let mut format: CMFormatDescriptionRef = null();
        // SAFETY: pointers to two valid byte slices and their lengths.
        let status = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                null(),
                2,
                sets.as_ptr(),
                sizes.as_ptr(),
                4,
                &raw mut format,
            )
        };
        if status != 0 || format.is_null() {
            bail!("the phone's video has parameters macOS doesn't take ({status})");
        }
        let format = Owned(format);
        // SAFETY: a valid video format description.
        let dims = unsafe { CMVideoFormatDescriptionGetDimensions(format.0) };
        let (sw, sh) = (dims.width.max(1) as u32, dims.height.max(1) as u32);
        let (width, height) = super::fit(sw, sh, max);
        let pixel_format =
            number_i32(kCVPixelFormatType_32BGRA).ok_or_else(|| anyhow!("no number"))?;
        let w = number_i32(width as i32).ok_or_else(|| anyhow!("no number"))?;
        let h = number_i32(height as i32).ok_or_else(|| anyhow!("no number"))?;
        // SAFETY: reading the framework's constant keys.
        let attributes = unsafe {
            dictionary(&[
                (kCVPixelBufferPixelFormatTypeKey, &pixel_format),
                (kCVPixelBufferWidthKey, &w),
                (kCVPixelBufferHeightKey, &h),
            ])
        }
        .ok_or_else(|| anyhow!("no dictionary"))?;
        let out = Arc::new(Out::default());
        let record = OutputCallbackRecord {
            callback: on_picture,
            refcon: Arc::as_ptr(&out) as *mut c_void,
        };
        let mut session: VTDecompressionSessionRef = null_mut();
        // SAFETY: valid format and attributes; `out` outlives the session
        // (dropped after it is invalidated).
        let status = unsafe {
            VTDecompressionSessionCreate(
                null(),
                format.0,
                null(),
                attributes.0,
                &raw const record,
                &raw mut session,
            )
        };
        if status != 0 || session.is_null() {
            bail!("VideoToolbox can't decode the phone's video ({status})");
        }
        Ok(Self {
            session,
            format,
            out,
        })
    }

    /// Decode one picture's slices; returns it when the decoder gave one.
    pub fn decode(&mut self, nals: &[Vec<u8>]) -> anyhow::Result<Option<Pixels>> {
        // Length-prefixed (AVCC), as the format description says.
        let mut sample = Vec::with_capacity(nals.iter().map(|n| n.len() + 4).sum());
        for nal in nals {
            sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            sample.extend_from_slice(nal);
        }
        let len = sample.len();
        // SAFETY: Core Media copies `sample` into a block of its own; the
        // buffers made here are released before returning. Decoding is
        // synchronous (no flags): the callback has run when it returns.
        unsafe {
            let mut block: CMBlockBufferRef = null_mut();
            let status = CMBlockBufferCreateWithMemoryBlock(
                null(),
                null_mut(),
                len,
                null(),
                null(),
                0,
                len,
                0,
                &raw mut block,
            );
            if status != 0 || block.is_null() {
                bail!("no memory for a picture ({status})");
            }
            let block = Owned(block as _);
            let status =
                CMBlockBufferReplaceDataBytes(sample.as_ptr().cast(), block.0 as _, 0, len);
            if status != 0 {
                bail!("couldn't copy a picture ({status})");
            }
            let mut buffer: CMSampleBufferRef = null_mut();
            let status = CMSampleBufferCreateReady(
                null(),
                block.0 as _,
                self.format.0,
                1,
                0,
                null(),
                1,
                &raw const len,
                &raw mut buffer,
            );
            if status != 0 || buffer.is_null() {
                bail!("couldn't make a sample ({status})");
            }
            let buffer = Owned(buffer as _);
            let mut info = 0u32;
            let status = VTDecompressionSessionDecodeFrame(
                self.session,
                buffer.0 as _,
                0,
                null_mut(),
                &raw mut info,
            );
            if status != 0 {
                bail!("VideoToolbox couldn't decode a picture ({status})");
            }
        }
        let failed = self
            .out
            .failures
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        if failed > 0 {
            bail!("VideoToolbox couldn't decode a picture");
        }
        Ok(self
            .out
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take())
    }
}
