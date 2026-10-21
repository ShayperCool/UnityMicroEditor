//! The sound of the remote session: the Player's final mix arrives as PCM
//! samples, is brought to 48 kHz stereo and encoded as Opus, which both the
//! page's WebCodecs and the browser's WebRTC stack decode. The one real encoder
//! is the system's libopus, opened at run time like the video encoder's
//! libraries; tests substitute their own.
use std::ffi::{CStr, c_char, c_int, c_void};

/// The rate Opus is encoded at and viewers play.
pub const SAMPLE_RATE: u32 = 48_000;
/// Interleaved left and right.
pub const CHANNELS: usize = 2;
/// Samples per channel of one encoded packet: 20 ms.
pub const FRAME: usize = 960;
const BITRATE: c_int = 96_000;

/// Encodes packets of [`FRAME`] interleaved stereo samples at [`SAMPLE_RATE`].
pub trait AudioEncoder: Send {
    fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, String>;
    /// The name reported to clients.
    fn name(&self) -> &'static str;
}
/// Opens an audio encoder.
pub type AudioFactory = dyn Fn() -> Result<Box<dyn AudioEncoder>, String> + Send + Sync;

type Create = unsafe extern "C" fn(i32, c_int, c_int, *mut c_int) -> *mut c_void;
type Encode = unsafe extern "C" fn(*mut c_void, *const f32, c_int, *mut u8, i32) -> i32;
type Control = unsafe extern "C" fn(*mut c_void, c_int, ...) -> c_int;
type Destroy = unsafe extern "C" fn(*mut c_void);
type Describe = unsafe extern "C" fn(c_int) -> *const c_char;

struct Library {
    create: Create,
    encode: Encode,
    control: Control,
    destroy: Destroy,
    describe: Describe,
}

// The library stays loaded for the life of the process once found.
fn library() -> Result<&'static Library, String> {
    static LIBRARY: std::sync::OnceLock<Result<Library, String>> = std::sync::OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            // SAFETY: the name is a NUL-terminated literal; a failed load returns null.
            let handle = unsafe { libc::dlopen(c"libopus.so.0".as_ptr(), libc::RTLD_NOW) };
            if handle.is_null() {
                return Err("libopus.so.0 is not installed".into());
            }
            let symbol = |name: &CStr| {
                // SAFETY: `handle` is a loaded library and `name` is NUL-terminated.
                let found = unsafe { libc::dlsym(handle, name.as_ptr()) };
                (!found.is_null())
                    .then_some(found)
                    .ok_or_else(|| format!("libopus.so.0 lacks {}", name.to_string_lossy()))
            };
            // SAFETY: each symbol is libopus' own function of the signature named
            // by its public header.
            unsafe {
                Ok(Library {
                    create: std::mem::transmute::<*mut c_void, Create>(symbol(
                        c"opus_encoder_create",
                    )?),
                    encode: std::mem::transmute::<*mut c_void, Encode>(symbol(
                        c"opus_encode_float",
                    )?),
                    control: std::mem::transmute::<*mut c_void, Control>(symbol(
                        c"opus_encoder_ctl",
                    )?),
                    destroy: std::mem::transmute::<*mut c_void, Destroy>(symbol(
                        c"opus_encoder_destroy",
                    )?),
                    describe: std::mem::transmute::<*mut c_void, Describe>(symbol(
                        c"opus_strerror",
                    )?),
                })
            }
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn error_text(library: &Library, code: c_int) -> String {
    // SAFETY: opus_strerror returns a static NUL-terminated string for any code.
    unsafe { CStr::from_ptr((library.describe)(code)) }
        .to_string_lossy()
        .into_owned()
}

/// libopus at 96 kbit/s, tuned for the lowest delay.
pub struct Opus {
    library: &'static Library,
    state: *mut c_void,
    output: Vec<u8>,
}
// SAFETY: an encoder state is used by one caller at a time (`&mut self`) and
// is not tied to the thread that made it.
unsafe impl Send for Opus {}
impl Opus {
    pub fn open() -> Result<Opus, String> {
        const RESTRICTED_LOWDELAY: c_int = 2051;
        const SET_BITRATE: c_int = 4002;
        let library = library()?;
        let mut error = 0;
        // SAFETY: valid rate, channel count and application; `error` outlives the call.
        let state = unsafe {
            (library.create)(
                SAMPLE_RATE as i32,
                CHANNELS as c_int,
                RESTRICTED_LOWDELAY,
                &mut error,
            )
        };
        if state.is_null() || error != 0 {
            return Err(format!("opus encoder: {}", error_text(library, error)));
        }
        let encoder = Opus {
            library,
            state,
            output: vec![0; 4000],
        };
        // SAFETY: the request takes one opus_int32 argument.
        let code = unsafe { (library.control)(state, SET_BITRATE, BITRATE) };
        if code != 0 {
            return Err(format!("opus bitrate: {}", error_text(library, code)));
        }
        Ok(encoder)
    }
}
impl AudioEncoder for Opus {
    fn encode(&mut self, pcm: &[f32]) -> Result<Vec<u8>, String> {
        if pcm.len() != FRAME * CHANNELS {
            return Err("audio frame does not match the encoder's size".into());
        }
        // SAFETY: `pcm` holds FRAME samples per channel and the output buffer's
        // real size is passed.
        let size = unsafe {
            (self.library.encode)(
                self.state,
                pcm.as_ptr(),
                FRAME as c_int,
                self.output.as_mut_ptr(),
                self.output.len() as i32,
            )
        };
        if size < 0 {
            return Err(format!("opus: {}", error_text(self.library, size)));
        }
        Ok(self.output[..size as usize].to_vec())
    }
    fn name(&self) -> &'static str {
        "opus"
    }
}
impl Drop for Opus {
    fn drop(&mut self) {
        // SAFETY: `state` came from opus_encoder_create and is destroyed once.
        unsafe { (self.library.destroy)(self.state) }
    }
}

/// The audio encoder of a real session.
pub fn opus() -> Result<Box<dyn AudioEncoder>, String> {
    Opus::open().map(|encoder| Box::new(encoder) as Box<dyn AudioEncoder>)
}

/// Brings the Player's samples, at its output rate and speaker layout, to
/// stereo at [`SAMPLE_RATE`] and cuts them into packets of [`FRAME`].
#[derive(Default)]
pub struct Mixdown {
    rate: u32,
    channels: usize,
    // The last stereo sample of the previous chunk and how far past it the
    // next output sample lies, in input samples.
    previous: [f32; 2],
    position: f64,
    pending: Vec<f32>,
}
impl Mixdown {
    /// Takes one chunk of interleaved samples; returns the packets it completed.
    pub fn push(&mut self, rate: u32, channels: usize, samples: &[f32]) -> Vec<Vec<f32>> {
        if (rate, channels) != (self.rate, self.channels) {
            *self = Mixdown {
                rate,
                channels,
                ..Default::default()
            };
        }
        if rate == 0 || channels == 0 {
            return Vec::new();
        }
        let step = rate as f64 / SAMPLE_RATE as f64;
        for frame in samples.chunks_exact(channels) {
            let current = stereo(frame);
            // Output samples fall between the previous input sample and this one.
            while self.position < 1.0 {
                let t = self.position as f32;
                for (side, value) in current.iter().enumerate() {
                    let from = self.previous[side];
                    self.pending.push(from + (value - from) * t);
                }
                self.position += step;
            }
            self.position -= 1.0;
            self.previous = current;
        }
        let complete = self.pending.len() / (FRAME * CHANNELS);
        let rest = self.pending.split_off(complete * FRAME * CHANNELS);
        let packets = self
            .pending
            .as_chunks::<{ FRAME * CHANNELS }>()
            .0
            .iter()
            .map(|packet| packet.to_vec())
            .collect();
        self.pending = rest;
        packets
    }
}

// The front pair, with the centre and the surround pairs folded in at -3 dB
// (Unity's layouts put front left, front right, centre and LFE first).
fn stereo(frame: &[f32]) -> [f32; 2] {
    match frame.len() {
        1 => [frame[0], frame[0]],
        2 => [frame[0], frame[1]],
        3 => [frame[0] + 0.707 * frame[2], frame[1] + 0.707 * frame[2]],
        _ => {
            let (mut left, mut right) = (frame[0], frame[1]);
            if frame.len() >= 5 {
                left += 0.707 * frame[2];
                right += 0.707 * frame[2];
            }
            // Quad has no centre: its second pair is the rear one.
            let surround = if frame.len() == 4 { 2 } else { 4 };
            for pair in frame[surround..].as_chunks::<2>().0 {
                left += 0.707 * pair[0];
                right += 0.707 * pair[1];
            }
            [left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0)]
        }
    }
}
