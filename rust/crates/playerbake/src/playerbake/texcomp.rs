//! Block compression as the importer does it: DXT1 and DXT5 by the Intel ISPC
//! Texture Compressor (`CompressBlocksBC1`, `CompressBlocksBC3`), which the
//! Editor ships as `libispc_texcomp.so` and calls on the image as it stores
//! it (8-bit RGBA, rows from the bottom), DXT1 with its alpha set opaque.
//! The library of the installation is loaded, so the blocks are its own.
use anyhow::{Context as _, Result, bail};
use std::{
    ffi::{CString, c_void},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

#[repr(C)]
struct Surface {
    ptr: *const u8,
    width: i32,
    height: i32,
    stride: i32,
}

type Compress = unsafe extern "C" fn(*const Surface, *mut u8);

struct Library {
    bc1: Compress,
    bc3: Compress,
}

// SAFETY: the library's entry points keep no state between calls.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

static EDITOR: Mutex<Option<PathBuf>> = Mutex::new(None);
static LIBRARY: OnceLock<Result<Library, String>> = OnceLock::new();

/// The Unity installation (its `Editor` directory) whose compressor is used.
pub fn use_editor(editor: &Path) {
    *EDITOR.lock().unwrap() = Some(editor.to_path_buf());
}

fn library() -> Result<&'static Library> {
    let loaded = LIBRARY.get_or_init(|| {
        let editor = EDITOR
            .lock()
            .unwrap()
            .clone()
            .ok_or("no Unity installation to take the texture compressor from")?;
        let path = editor.join("libispc_texcomp.so");
        let name = CString::new(path.to_string_lossy().as_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: a shared library of the installation, looked up by name.
        unsafe {
            let handle = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if handle.is_null() {
                return Err(format!("cannot load {}", path.display()));
            }
            let symbol = |name: &str| -> Result<Compress, String> {
                let c = CString::new(name).unwrap();
                let address: *mut c_void = libc::dlsym(handle, c.as_ptr());
                if address.is_null() {
                    return Err(format!("{} has no {name}", path.display()));
                }
                Ok(std::mem::transmute::<*mut c_void, Compress>(address))
            };
            Ok(Library {
                bc1: symbol("CompressBlocksBC1")?,
                bc3: symbol("CompressBlocksBC3")?,
            })
        }
    });
    loaded.as_ref().map_err(|error| anyhow::anyhow!("{error}"))
}

/// The DXT1 (`alpha` false) or DXT5 blocks of an image whose sides are
/// multiples of four.
pub fn compress(rgba: &[u8], width: usize, height: usize, alpha: bool) -> Result<Vec<u8>> {
    if !width.is_multiple_of(4) || !height.is_multiple_of(4) || rgba.len() != width * height * 4 {
        bail!("a block-compressed image is a whole number of blocks");
    }
    let library = library().context("the texture compressor")?;
    let mut pixels = rgba.to_vec();
    if !alpha {
        for pixel in pixels.chunks_mut(4) {
            pixel[3] = 255;
        }
    }
    let block = if alpha { 16 } else { 8 };
    let mut out = vec![0u8; width / 4 * (height / 4) * block];
    let surface = Surface {
        ptr: pixels.as_ptr(),
        width: width as i32,
        height: height as i32,
        stride: width as i32 * 4,
    };
    // SAFETY: the surface covers `pixels`, and `out` holds every block.
    unsafe {
        let compress = if alpha { library.bc3 } else { library.bc1 };
        compress(&surface, out.as_mut_ptr());
    }
    Ok(out)
}
