//! Video encoders of the remote session. The one real implementation drives
//! the NVIDIA driver's hardware encoder through the C source in
//! `rust/native/nvenc`; tests substitute their own.
use std::{
    ffi::{CStr, c_char, c_int},
    ptr,
};

/// One encoded frame: a complete H.264 Annex-B access unit.
pub struct Encoded {
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// Encodes frames of one fixed size, each as soon as it is given.
pub trait Encoder: Send {
    /// `pixels` holds `width * height` tightly packed R,G,B,A values, top row first.
    fn encode(
        &mut self,
        pixels: &[u8],
        force_keyframe: bool,
        timestamp: u64,
    ) -> Result<Encoded, String>;
    /// The name reported to clients.
    fn name(&self) -> &'static str;
}

pub struct Parameters {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}
/// Opens an encoder for frames of one size.
pub type Factory = dyn Fn(&Parameters) -> Result<Box<dyn Encoder>, String> + Send + Sync;

#[repr(C)]
struct Raw {
    _opaque: [u8; 0],
}
unsafe extern "C" {
    fn ume_nvenc_open(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        device: c_int,
        result: *mut *mut Raw,
        error: *mut c_char,
        capacity: usize,
    ) -> c_int;
    fn ume_nvenc_encode(
        state: *mut Raw,
        pixels: *const u8,
        force_idr: c_int,
        timestamp: u64,
        output: *mut u8,
        output_capacity: usize,
        keyframe: *mut c_int,
        error: *mut c_char,
        capacity: usize,
    ) -> i64;
    fn ume_nvenc_close(state: *mut Raw);
}

fn message(buffer: &[c_char; 512]) -> String {
    // SAFETY: the C side writes a NUL-terminated string within the buffer, which starts zeroed.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// NVENC H.264 on the GPU selected by `UNITY_MICRO_EDITOR_NVENC_DEVICE` (CUDA ordinal, default 0).
pub struct Nvenc {
    raw: *mut Raw,
    pixels: usize,
    output: Vec<u8>,
}
// SAFETY: the encoder makes its CUDA context current around every call, so it
// is not tied to a thread; `&mut self` keeps the calls sequential.
unsafe impl Send for Nvenc {}
impl Nvenc {
    pub fn open(parameters: &Parameters) -> Result<Nvenc, String> {
        let device = std::env::var("UNITY_MICRO_EDITOR_NVENC_DEVICE")
            .ok()
            .and_then(|value| value.parse::<c_int>().ok())
            .unwrap_or(0);
        let mut raw = ptr::null_mut();
        let mut error = [0 as c_char; 512];
        // SAFETY: `raw` and `error` outlive the call and `capacity` is the buffer's size.
        let status = unsafe {
            ume_nvenc_open(
                parameters.width,
                parameters.height,
                parameters.fps,
                parameters.bitrate,
                device,
                &mut raw,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status != 0 || raw.is_null() {
            return Err(message(&error));
        }
        let pixels = parameters.width as usize * parameters.height as usize * 4;
        Ok(Nvenc {
            raw,
            pixels,
            output: vec![0; pixels.max(1 << 20)],
        })
    }
}
impl Encoder for Nvenc {
    fn encode(
        &mut self,
        pixels: &[u8],
        force_keyframe: bool,
        timestamp: u64,
    ) -> Result<Encoded, String> {
        if pixels.len() != self.pixels {
            return Err("frame does not match the encoder's size".into());
        }
        let mut keyframe: c_int = 0;
        let mut error = [0 as c_char; 512];
        // SAFETY: `pixels` holds exactly the frame the encoder was opened for,
        // and the output and error buffers are passed with their real sizes.
        let size = unsafe {
            ume_nvenc_encode(
                self.raw,
                pixels.as_ptr(),
                force_keyframe as c_int,
                timestamp,
                self.output.as_mut_ptr(),
                self.output.len(),
                &mut keyframe,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if size < 0 {
            return Err(message(&error));
        }
        Ok(Encoded {
            data: self.output[..size as usize].to_vec(),
            keyframe: keyframe != 0,
        })
    }
    fn name(&self) -> &'static str {
        "nvenc_h264"
    }
}
impl Drop for Nvenc {
    fn drop(&mut self) {
        // SAFETY: `raw` came from `ume_nvenc_open` and is closed once.
        unsafe { ume_nvenc_close(self.raw) }
    }
}

/// The encoder of a real session.
pub fn hardware(parameters: &Parameters) -> Result<Box<dyn Encoder>, String> {
    Nvenc::open(parameters).map(|encoder| Box::new(encoder) as Box<dyn Encoder>)
}

/// The WebCodecs codec string (`avc1.PPCCLL`) of the stream's sequence
/// parameter set, `None` when the access unit carries none.
pub fn codec_string(access_unit: &[u8]) -> Option<String> {
    let mut index = 0;
    while index + 3 < access_unit.len() {
        if access_unit[index..index + 3] != [0, 0, 1] {
            index += 1;
            continue;
        }
        let unit = &access_unit[index + 3..];
        if unit.len() >= 4 && unit[0] & 0x1f == 7 {
            return Some(format!(
                "avc1.{:02x}{:02x}{:02x}",
                unit[1], unit[2], unit[3]
            ));
        }
        index += 3;
    }
    None
}
