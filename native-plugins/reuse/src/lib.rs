//! Optional RE-USE adapter. All runtime/model paths are supplied by the host.
use anyhow::{Context, Result, bail, ensure};
use libloading::Library;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
};

type Ptr = *mut c_void;
type Free = unsafe extern "C" fn(Ptr);
type Callback = unsafe extern "C" fn(*const f32, usize, Ptr) -> c_int;
const MAX_SAMPLES: usize = 48_000 * 40;

struct Owned {
    pointer: Ptr,
    free: Free,
}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { (self.free)(self.pointer) };
    }
}

struct Native {
    model_load: unsafe extern "C" fn(*const c_char, Ptr, *mut Ptr) -> c_int,
    model_free: Free,
    params_new: unsafe extern "C" fn(*mut Ptr) -> c_int,
    params_free: Free,
    params_device: unsafe extern "C" fn(Ptr, *const c_char) -> c_int,
    params_threads: unsafe extern "C" fn(Ptr, c_int) -> c_int,
    request_new: unsafe extern "C" fn(Ptr, *mut Ptr) -> c_int,
    request_free: Free,
    set_audio: unsafe extern "C" fn(Ptr, *const f32, usize, c_int) -> c_int,
    set_float: unsafe extern "C" fn(Ptr, c_int, f64) -> c_int,
    enhance: unsafe extern "C" fn(Ptr, Callback, Ptr) -> c_int,
    last_error: unsafe extern "C" fn() -> *const c_char,
    _library: Library,
}

impl Native {
    fn load(path: &Path) -> Result<Self> {
        ensure!(
            path.is_absolute() && path.is_file(),
            "speech runtime is missing"
        );
        unsafe {
            #[cfg(windows)]
            let library: Library =
                libloading::os::windows::Library::load_with_flags(path, 0x0000_0100 | 0x0000_0800)?
                    .into();
            #[cfg(not(windows))]
            let library = Library::new(path)?;
            let major =
                library.get::<unsafe extern "C" fn() -> c_int>(b"speech_api_version_major\0")?;
            let minor =
                library.get::<unsafe extern "C" fn() -> c_int>(b"speech_api_version_minor\0")?;
            ensure!(
                major() == 4 && minor() >= 1,
                "speech.cpp API 4.1 (major 4) is required"
            );
            macro_rules! symbol {
                ($name:literal) => {
                    *library.get(concat!($name, "\0").as_bytes())?
                };
            }
            Ok(Self {
                model_load: symbol!("speech_model_load"),
                model_free: symbol!("speech_model_free"),
                params_new: symbol!("speech_load_params_new"),
                params_free: symbol!("speech_load_params_free"),
                params_device: symbol!("speech_load_params_set_device"),
                params_threads: symbol!("speech_load_params_set_threads"),
                request_new: symbol!("speech_request_new"),
                request_free: symbol!("speech_request_free"),
                set_audio: symbol!("speech_request_set_audio"),
                set_float: symbol!("speech_request_set_float"),
                enhance: symbol!("speech_enhance"),
                last_error: symbol!("speech_last_error"),
                _library: library,
            })
        }
    }

    fn check(&self, status: c_int) -> Result<()> {
        if status != 0 {
            let pointer = unsafe { (self.last_error)() };
            let detail = if pointer.is_null() {
                "unknown".into()
            } else {
                unsafe { CStr::from_ptr(pointer) }.to_string_lossy()
            };
            bail!("speech.cpp error {status}: {detail}");
        }
        Ok(())
    }
}

struct State {
    // Model must be released before its library.
    model: Owned,
    chunk_secs: f64,
    native: Native,
}

impl State {
    fn load(json: &str) -> Result<Self> {
        let config: serde_json::Value = serde_json::from_str(json)?;
        let runtime = config["runtime"]
            .as_str()
            .context("runtime resource is required")?;
        let model = config["model"]
            .as_str()
            .context("model resource is required")?;
        ensure!(
            Path::new(model).is_absolute() && Path::new(model).is_file(),
            "model resource is missing"
        );
        let chunk_secs = config
            .get("chunk_secs")
            .map_or(Some(5.0), |value| value.as_f64())
            .context("invalid chunk_secs")?;
        ensure!(
            chunk_secs.is_finite() && (0.05..=30.0).contains(&chunk_secs),
            "chunk_secs must be 0.05..30"
        );
        let native = Native::load(Path::new(runtime))?;
        let mut params = std::ptr::null_mut();
        native.check(unsafe { (native.params_new)(&mut params) })?;
        ensure!(!params.is_null(), "empty load parameters");
        let params = Owned {
            pointer: params,
            free: native.params_free,
        };
        let device = config.get("device").and_then(|value| value.as_str());
        if let Some(device) = device {
            let device = CString::new(device)?;
            native.check(unsafe { (native.params_device)(params.pointer, device.as_ptr()) })?;
        }
        native.check(unsafe { (native.params_threads)(params.pointer, 4) })?;
        let model_path = CString::new(model)?;
        let mut pointer = std::ptr::null_mut();
        native.check(unsafe {
            (native.model_load)(model_path.as_ptr(), params.pointer, &mut pointer)
        })?;
        ensure!(!pointer.is_null(), "empty model");
        Ok(Self {
            model: Owned {
                pointer,
                free: native.model_free,
            },
            chunk_secs,
            native,
        })
    }

    fn process(&mut self, input: &[f32], rate: u32) -> Result<Vec<f32>> {
        ensure!(rate == 48_000, "sample rate must be 48000");
        ensure!(
            input.len() <= MAX_SAMPLES && input.iter().all(|x| x.is_finite()),
            "invalid input"
        );
        if input.is_empty() {
            return Ok(Vec::new());
        }
        // Very short final fragments still need enough samples for the STFT.
        // The native enhancement preserves positions; this padding adds no prefix.
        let mut padded = input.to_vec();
        padded.resize(input.len().max(4096), 0.0);
        let mut pointer = std::ptr::null_mut();
        self.native
            .check(unsafe { (self.native.request_new)(self.model.pointer, &mut pointer) })?;
        ensure!(!pointer.is_null(), "empty request");
        let request = Owned {
            pointer,
            free: self.native.request_free,
        };
        self.native.check(unsafe {
            (self.native.set_audio)(
                request.pointer,
                padded.as_ptr(),
                padded.len(),
                rate as c_int,
            )
        })?;
        self.native
            .check(unsafe { (self.native.set_float)(request.pointer, 63, self.chunk_secs) })?;
        let mut enhanced = Vec::<f32>::with_capacity(padded.len());
        self.native.check(unsafe {
            (self.native.enhance)(
                request.pointer,
                collect,
                (&mut enhanced as *mut Vec<f32>).cast(),
            )
        })?;
        ensure!(
            enhanced.len() == padded.len() && enhanced.iter().all(|x| x.is_finite()),
            "enhancement returned invalid samples"
        );
        enhanced.truncate(input.len());
        Ok(enhanced)
    }
}

unsafe extern "C" fn collect(samples: *const f32, count: usize, user: Ptr) -> c_int {
    if samples.is_null() || user.is_null() || count > MAX_SAMPLES {
        return 1;
    }
    let output = unsafe { &mut *user.cast::<Vec<f32>>() };
    if output.len().saturating_add(count) > MAX_SAMPLES {
        return 1;
    }
    output.extend_from_slice(unsafe { std::slice::from_raw_parts(samples, count) });
    0
}

unsafe fn write_error(buffer: *mut c_char, capacity: usize, text: &str) {
    if buffer.is_null() || capacity == 0 {
        return;
    }
    let amount = text.len().min(capacity - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), buffer.cast(), amount);
        *buffer.add(amount) = 0;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tr_denoise_api_version() -> u32 {
    1
}

/// # Safety
/// config and error must reference valid buffers with the documented ABI sizes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tr_denoise_create(
    config: *const c_char,
    error: *mut c_char,
    capacity: usize,
) -> Ptr {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<State> {
        ensure!(!config.is_null(), "config is null");
        let json = unsafe { CStr::from_ptr(config) }.to_str()?;
        ensure!(json.len() <= 65_536, "config is too large");
        State::load(json)
    }));
    match result {
        Ok(Ok(state)) => Box::into_raw(Box::new(state)).cast(),
        Ok(Err(failure)) => {
            unsafe { write_error(error, capacity, &format!("{failure:#}")) };
            std::ptr::null_mut()
        }
        Err(_) => {
            unsafe { write_error(error, capacity, "plugin initialization panicked") };
            std::ptr::null_mut()
        }
    }
}

/// # Safety
/// state must be a live plugin handle, and audio buffers must contain samples floats.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tr_denoise_process(
    state: Ptr,
    input: *const f32,
    samples: usize,
    rate: u32,
    output: *mut f32,
    error: *mut c_char,
    capacity: usize,
) -> c_int {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
        ensure!(
            !state.is_null() && samples <= MAX_SAMPLES,
            "invalid state or length"
        );
        ensure!(
            samples == 0 || (!input.is_null() && !output.is_null()),
            "audio is null"
        );
        let input = if samples == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(input, samples) }
        };
        let enhanced = unsafe { &mut *state.cast::<State>() }.process(input, rate)?;
        if samples != 0 {
            unsafe { std::ptr::copy_nonoverlapping(enhanced.as_ptr(), output, samples) };
        }
        Ok(())
    }));
    match result {
        Ok(Ok(())) => 0,
        Ok(Err(failure)) => {
            unsafe { write_error(error, capacity, &format!("{failure:#}")) };
            1
        }
        Err(_) => {
            unsafe { write_error(error, capacity, "plugin processing panicked") };
            2
        }
    }
}

/// # Safety
/// state must be null or a live handle, passed here exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tr_denoise_destroy(state: Ptr) {
    if !state.is_null() {
        drop(unsafe { Box::from_raw(state.cast::<State>()) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn abi_rejects_missing_config_and_invalid_handle() {
        let mut error = [0i8; 256];
        assert_eq!(tr_denoise_api_version(), 1);
        assert!(
            unsafe { tr_denoise_create(c"{}".as_ptr(), error.as_mut_ptr(), error.len()) }.is_null()
        );
        assert_ne!(
            unsafe {
                tr_denoise_process(
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    0,
                    48_000,
                    std::ptr::null_mut(),
                    error.as_mut_ptr(),
                    error.len(),
                )
            },
            0
        );
        unsafe { tr_denoise_destroy(std::ptr::null_mut()) };
    }
}
