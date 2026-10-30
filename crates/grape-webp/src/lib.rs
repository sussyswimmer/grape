//! libwebp's encoder, for Grape's WebP Compress (DECISIONS D28).
//!
//! This crate holds the only `unsafe` code for images, so `grape-engines`
//! keeps `#![forbid(unsafe_code)]`. It calls libwebp 1.6.0, vendored by
//! libwebp-sys and compiled with the system's C compiler, through one
//! function: [`encode`] takes 8-bit pixels and returns libwebp's whole RIFF
//! file. There is no decoding API, so no file a user dropped ever reaches
//! libwebp (CVE-2023-4863 was in its decoder); Grape reads WebP with
//! image-webp, in Rust.
//!
//! Every `unsafe` block says why it's sound. Everything libwebp allocates is
//! freed by a drop guard, on every path, and the progress hook runs the
//! caller's closure inside `catch_unwind`, so a panic never unwinds through
//! C.

#![warn(missing_docs)]

use std::ffi::{c_int, c_void};
use std::panic::{self, AssertUnwindSafe};
use std::ptr;

use libwebp_sys as sys;

/// The longest side libwebp writes (`WEBP_MAX_DIMENSION`).
pub const MAX_SIDE: u32 = 16_383;

/// The encoder ABI of the headers the bindings come from, which libwebp's
/// `…Internal` initializers check. libwebp-sys's own `WebPConfig::new` and
/// `WebPPictureInit` pass the decoder's instead (the same number in 1.6.0,
/// but nothing promises that).
const ENCODER_ABI: c_int = sys::WEBP_ENCODER_ABI_VERSION as c_int;

/// 8-bit pixels, rows top to bottom with nothing between them.
#[derive(Clone, Copy, Debug)]
pub enum Pixels<'a> {
    /// 3 bytes a pixel.
    Rgb {
        /// `width × height × 3` bytes.
        data: &'a [u8],
        /// 1 to [`MAX_SIDE`].
        width: u32,
        /// 1 to [`MAX_SIDE`].
        height: u32,
    },
    /// 4 bytes a pixel, alpha straight (not premultiplied).
    Rgba {
        /// `width × height × 4` bytes.
        data: &'a [u8],
        /// 1 to [`MAX_SIDE`].
        width: u32,
        /// 1 to [`MAX_SIDE`].
        height: u32,
    },
}

/// Which of libwebp's two formats [`encode`] writes, and how hard it tries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// VP8, the lossy format. The pixels go straight to YUV 4:2:0 (no ARGB
    /// copy and no sharp YUV). Alpha, when any pixel isn't opaque, is kept
    /// exactly in an `ALPH` chunk (lossless, filtered); the RGB under fully
    /// transparent pixels may change.
    Lossy {
        /// libwebp's quality, 0 to 100.
        quality: f32,
        /// libwebp's method, 0 to 6 (6 is the slowest and smallest).
        method: u8,
    },
    /// VP8L, the lossless format: every pixel kept exactly, the RGB under
    /// transparent ones too (`exact`).
    Lossless {
        /// libwebp's lossless preset, 0 to 9 (9 is cwebp's `-z 9`: method
        /// 6, quality 100).
        level: u8,
    },
}

/// How [`encode`] runs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Options {
    /// The format and effort.
    pub mode: Mode,
    /// libwebp's `thread_level`: one more thread for parts of the encode
    /// (the alpha plane, half the analysis, a second lossless attempt).
    pub threads: bool,
}

/// Why [`encode`] returned no file.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// `keep_going` returned false, or panicked.
    #[error("cancelled")]
    Cancelled,
    /// A side is 0, the data isn't exactly the pixels' size, or a number in
    /// the [`Mode`] is out of its range.
    #[error("bad input")]
    BadInput,
    /// A side is over [`MAX_SIDE`].
    #[error("over 16,383 px a side")]
    TooLarge,
    /// libwebp, or the copy of its output, couldn't allocate.
    #[error("out of memory")]
    OutOfMemory,
    /// Any other failure: libwebp's `WebPEncodingError` number, or 0 when it
    /// failed without one.
    #[error("libwebp error {0}")]
    Encoder(i32),
}

/// Encodes `px` and returns libwebp's whole file (`RIFF`, `WEBP`, then
/// `VP8 ` with `VP8X` and `ALPH` before it when there's alpha, or `VP8L`).
///
/// `keep_going(percent)` is called on this thread whenever libwebp's percent
/// (0 to 100) changes; false cancels the encode. libwebp reports a lossy
/// encode about once per macroblock row, ending in the 90s (it reports 100
/// only when built without its statistics), and a lossless one coarsely.
/// Once it has returned false, or panicked (which counts as false), it isn't
/// called again.
pub fn encode(
    px: Pixels<'_>,
    opts: &Options,
    keep_going: &mut dyn FnMut(u8) -> bool,
) -> Result<Vec<u8>, Error> {
    let (data, width, height, channels) = match px {
        Pixels::Rgb {
            data,
            width,
            height,
        } => (data, width, height, 3_usize),
        Pixels::Rgba {
            data,
            width,
            height,
        } => (data, width, height, 4),
    };
    if width == 0 || height == 0 {
        return Err(Error::BadInput);
    }
    if width > MAX_SIDE || height > MAX_SIDE {
        return Err(Error::TooLarge);
    }
    // With both sides at most 16,383, none of this can overflow, and every
    // number fits a C int (a row is at most 65,532 bytes).
    let stride = width as usize * channels;
    if data.len() != stride * height as usize {
        return Err(Error::BadInput);
    }
    let config = config(opts)?;

    let mut picture = Picture::new()?;
    picture.0.use_argb = c_int::from(matches!(opts.mode, Mode::Lossless { .. }));
    picture.0.width = width as c_int;
    picture.0.height = height as c_int;
    // SAFETY: `picture` is set up and sized as `data` was checked to be:
    // `height` rows of `stride` bytes. libwebp only reads `data`, and copies
    // it (to YUV for lossy, ARGB for lossless) before returning.
    let imported = unsafe {
        if channels == 3 {
            sys::WebPPictureImportRGB(&mut picture.0, data.as_ptr(), stride as c_int)
        } else {
            sys::WebPPictureImportRGBA(&mut picture.0, data.as_ptr(), stride as c_int)
        }
    };
    if imported == 0 {
        return Err(picture.error());
    }

    let mut writer = Writer::new();
    let mut hook = Hook {
        keep_going,
        stopped: false,
    };
    picture.0.writer = Some(sys::WebPMemoryWrite);
    picture.0.custom_ptr = (&raw mut writer.0).cast::<c_void>();
    picture.0.progress_hook = Some(progress);
    picture.0.user_data = (&raw mut hook).cast::<c_void>();
    // SAFETY: `config` passed WebPValidateConfig, and `picture` holds the
    // imported pixels. Its `custom_ptr` points to `writer.0`, the
    // WebPMemoryWriter that WebPMemoryWrite expects there, and its
    // `user_data` to `hook`, which `progress` expects there. Both are locals
    // that outlive this call, and nothing else touches them until it
    // returns.
    let ok = unsafe { sys::WebPEncode(&config, &mut picture.0) };
    picture.0.custom_ptr = ptr::null_mut();
    picture.0.user_data = ptr::null_mut();
    picture.0.progress_hook = None;
    if ok == 0 {
        return Err(picture.error());
    }
    writer.bytes()
}

/// libwebp's encoder version, as (major, minor, revision), for the log.
pub fn version() -> (u8, u8, u8) {
    // SAFETY: WebPGetEncoderVersion takes nothing and returns a constant.
    let v = unsafe { sys::WebPGetEncoderVersion() };
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// libwebp's settings for `opts`, validated by libwebp.
fn config(opts: &Options) -> Result<sys::WebPConfig, Error> {
    let quality = match opts.mode {
        Mode::Lossy { quality, method } => {
            if !(0.0..=100.0).contains(&quality) || method > 6 {
                return Err(Error::BadInput);
            }
            quality
        }
        // WebPConfigLosslessPreset sets the quality.
        Mode::Lossless { level } if level <= 9 => 100.0,
        Mode::Lossless { .. } => return Err(Error::BadInput),
    };
    // SAFETY: all-zero bytes are a valid WebPConfig (C ints, floats and an
    // enum whose 0 is WEBP_HINT_DEFAULT). WebPConfigInitInternal doesn't set
    // every field (`use_delta_palette`), so it starts from zeros, as C's
    // `WebPConfig config = {0};` would.
    let mut cfg: sys::WebPConfig = unsafe { std::mem::zeroed() };
    // SAFETY: `cfg` is a live WebPConfig, and the version is the encoder ABI
    // of the headers the bindings come from.
    let init = unsafe {
        sys::WebPConfigInitInternal(
            &mut cfg,
            sys::WebPPreset::WEBP_PRESET_DEFAULT,
            quality,
            ENCODER_ABI,
        )
    };
    if init == 0 {
        return Err(Error::Encoder(0));
    }
    match opts.mode {
        Mode::Lossy { method, .. } => {
            cfg.method = c_int::from(method);
            // A lossy source is already 4:2:0, and sharp YUV would need the
            // ARGB copy.
            cfg.use_sharp_yuv = 0;
            cfg.exact = 0;
            cfg.alpha_compression = 1;
            cfg.alpha_filtering = 2;
            cfg.alpha_quality = 100;
        }
        Mode::Lossless { level } => {
            // SAFETY: `cfg` was set up above, and `level` is at most 9.
            if unsafe { sys::WebPConfigLosslessPreset(&mut cfg, c_int::from(level)) } == 0 {
                return Err(Error::BadInput);
            }
            cfg.exact = 1;
        }
    }
    cfg.thread_level = c_int::from(opts.threads);
    // SAFETY: `cfg` is set up; libwebp only reads it.
    if unsafe { sys::WebPValidateConfig(&cfg) } == 0 {
        return Err(Error::Encoder(
            sys::WebPEncodingError::VP8_ENC_ERROR_INVALID_CONFIGURATION as i32,
        ));
    }
    Ok(cfg)
}

/// A `WebPPicture` set up by `WebPPictureInitInternal`, whose buffers are
/// freed on drop.
struct Picture(sys::WebPPicture);

impl Picture {
    fn new() -> Result<Picture, Error> {
        // SAFETY: all-zero bytes are a valid WebPPicture: C ints, raw
        // pointers (null), `Option<fn>` (None) and two enums whose 0 is
        // WEBP_YUV420 and VP8_ENC_OK.
        let mut pic: sys::WebPPicture = unsafe { std::mem::zeroed() };
        // SAFETY: `pic` is a live WebPPicture, and the version is the
        // encoder ABI of the headers the bindings come from.
        if unsafe { sys::WebPPictureInitInternal(&mut pic, ENCODER_ABI) } == 0 {
            return Err(Error::Encoder(0));
        }
        Ok(Picture(pic))
    }

    /// What libwebp says went wrong.
    fn error(&self) -> Error {
        use sys::WebPEncodingError as E;
        match self.0.error_code {
            // The memory writer fails only when it can't grow its buffer.
            E::VP8_ENC_ERROR_OUT_OF_MEMORY
            | E::VP8_ENC_ERROR_BITSTREAM_OUT_OF_MEMORY
            | E::VP8_ENC_ERROR_BAD_WRITE => Error::OutOfMemory,
            E::VP8_ENC_ERROR_USER_ABORT => Error::Cancelled,
            other => Error::Encoder(other as i32),
        }
    }
}

impl Drop for Picture {
    fn drop(&mut self) {
        // SAFETY: the picture was set up by WebPPictureInitInternal, and
        // libwebp allocated whatever buffers it points to (none, if the
        // import failed or never ran). WebPPictureFree frees them and resets
        // the pointers.
        unsafe { sys::WebPPictureFree(&mut self.0) }
    }
}

/// libwebp's memory writer, whose buffer is freed on drop.
struct Writer(sys::WebPMemoryWriter);

impl Writer {
    fn new() -> Writer {
        let mut w = sys::WebPMemoryWriter {
            mem: ptr::null_mut(),
            size: 0,
            max_size: 0,
            pad: [0],
        };
        // SAFETY: `w` is a live WebPMemoryWriter; Init only stores the empty
        // state.
        unsafe { sys::WebPMemoryWriterInit(&mut w) };
        Writer(w)
    }

    /// A copy of what libwebp wrote.
    fn bytes(&self) -> Result<Vec<u8>, Error> {
        if self.0.mem.is_null() || self.0.size == 0 {
            return Err(Error::Encoder(0));
        }
        // SAFETY: WebPMemoryWrite keeps `mem` pointing to one allocation of
        // at least `size` bytes, all written, and nothing writes to it while
        // `self` is borrowed.
        let written = unsafe { std::slice::from_raw_parts(self.0.mem, self.0.size) };
        let mut out = Vec::new();
        out.try_reserve_exact(written.len())
            .map_err(|_| Error::OutOfMemory)?;
        out.extend_from_slice(written);
        Ok(out)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // SAFETY: `mem` is null or the buffer WebPMemoryWrite allocated;
        // Clear frees it and resets the writer.
        unsafe { sys::WebPMemoryWriterClear(&mut self.0) }
    }
}

/// What [`progress`] reaches through the picture's `user_data`.
struct Hook<'a> {
    keep_going: &'a mut dyn FnMut(u8) -> bool,
    /// The closure said stop, or panicked: it isn't called again.
    stopped: bool,
}

/// libwebp's `progress_hook`: 1 goes on, 0 stops the encode
/// (`VP8_ENC_ERROR_USER_ABORT`).
///
/// # Safety
///
/// `picture` must be the picture [`encode`] passed to WebPEncode, during
/// that call.
unsafe extern "C" fn progress(percent: c_int, picture: *const sys::WebPPicture) -> c_int {
    // SAFETY: libwebp calls the hook only from inside `encode`'s WebPEncode,
    // on its thread (the pictures its worker threads use have no hook), with
    // the picture `encode` passed, whose `user_data` points to that call's
    // `Hook`: alive, and borrowed by nothing else until WebPEncode returns.
    let hook = unsafe { &mut *(*picture).user_data.cast::<Hook<'_>>() };
    if hook.stopped {
        return 0;
    }
    let percent = u8::try_from(percent.clamp(0, 100)).unwrap_or(100);
    let go = panic::catch_unwind(AssertUnwindSafe(|| (hook.keep_going)(percent)));
    if matches!(go, Ok(true)) {
        1
    } else {
        hook.stopped = true;
        0
    }
}

#[cfg(test)]
mod tests;
