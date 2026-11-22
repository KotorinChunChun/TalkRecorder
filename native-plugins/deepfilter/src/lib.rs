//! Optional DeepFilterNet3 host ABI v1. Model loading happens only in create().
//! Every process call is an independent mono window; a pristine inference state is
//! cloned without loading the weights again. STFT and lookahead delay are flushed
//! and removed so the host's sample positions and tail are preserved.

use df::tract::{DfParams, DfTract, RuntimeParams};
use std::{
    ffi::{c_char, c_void, CStr},
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    ptr, slice,
};
use tract_core::ndarray::{ArrayView2, ArrayViewMut2};

const RATE: u32 = 48_000;
const MAX_SAMPLES: usize = RATE as usize * 30;

struct Plugin {
    pristine: DfTract,
}

impl Plugin {
    fn load(configuration: &str) -> Result<Self, String> {
        let configuration: serde_json::Value = serde_json::from_str(configuration)
            .map_err(|error| format!("Invalid DeepFilterNet configuration: {error}"))?;
        let model = configuration
            .get("model")
            .and_then(|model| model.as_str())
            .ok_or("DeepFilterNet requires resource 'model'")?;
        let model = PathBuf::from(model);
        if !model.is_absolute() || !model.is_file() {
            return Err("DeepFilterNet model must be an existing absolute file path".into());
        }
        let parameters = DfParams::new(model).map_err(|error| error.to_string())?;
        // Limit extreme attenuation to make the default usable for conversation;
        // this remains an enhancement model, not a guarantee against voice loss.
        let runtime = RuntimeParams::default().with_atten_lim(35.0);
        let pristine = DfTract::new(parameters, &runtime).map_err(|error| error.to_string())?;
        if pristine.sr != RATE as usize || pristine.ch != 1 {
            return Err("DeepFilterNet plugin requires a 48 kHz mono model".into());
        }
        if pristine.hop_size == 0 || pristine.fft_size < pristine.hop_size {
            return Err("DeepFilterNet model has invalid frame dimensions".into());
        }
        Ok(Self { pristine })
    }

    fn delay(&self) -> usize {
        // This is the compensation used by upstream's libDF/src/bin/enhance_wav.rs.
        self.pristine.fft_size - self.pristine.hop_size
            + self.pristine.lookahead * self.pristine.hop_size
    }

    fn process(&self, input: &[f32], rate: u32) -> Result<Vec<f32>, String> {
        if rate != RATE {
            return Err("DeepFilterNet supports 48000 Hz only".into());
        }
        if input.len() > MAX_SAMPLES || input.iter().any(|sample| !sample.is_finite()) {
            return Err("DeepFilterNet input is too long or contains non-finite samples".into());
        }
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let delay = self.delay();
        let hop = self.pristine.hop_size;
        let padded_frames = (input.len() + delay).div_ceil(hop) * hop;
        let mut padded = vec![0.0; padded_frames];
        padded[..input.len()].copy_from_slice(input);
        let mut enhanced = vec![0.0; padded_frames];
        let mut model = self.pristine.clone();
        for (source, destination) in padded.chunks_exact(hop).zip(enhanced.chunks_exact_mut(hop)) {
            model
                .process(
                    ArrayView2::from_shape((1, hop), source).map_err(|error| error.to_string())?,
                    ArrayViewMut2::from_shape((1, hop), destination)
                        .map_err(|error| error.to_string())?,
                )
                .map_err(|error| error.to_string())?;
        }
        let output = enhanced[delay..delay + input.len()].to_vec();
        if output.iter().any(|sample| !sample.is_finite()) {
            return Err("DeepFilterNet produced non-finite samples".into());
        }
        Ok(output)
    }
}

unsafe fn write_error(error: *mut c_char, capacity: usize, message: &str) {
    if error.is_null() || capacity == 0 {
        return;
    }
    let bytes = message.as_bytes();
    let count = bytes.len().min(capacity - 1);
    // The host owns a writable error buffer with at least capacity bytes.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), error.cast::<u8>(), count);
        *error.add(count) = 0;
    }
}

#[no_mangle]
pub extern "C" fn tr_denoise_api_version() -> u32 {
    1
}

/// # Safety
/// configuration is a valid NUL-terminated UTF-8 string; error has capacity bytes.
#[no_mangle]
pub unsafe extern "C" fn tr_denoise_create(
    configuration: *const c_char,
    error: *mut c_char,
    error_capacity: usize,
) -> *mut c_void {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if configuration.is_null() {
            return Err("DeepFilterNet configuration is null".into());
        }
        let configuration = unsafe { CStr::from_ptr(configuration) }
            .to_str()
            .map_err(|_| "DeepFilterNet configuration is not UTF-8".to_string())?;
        if configuration.len() > 65_536 {
            return Err("DeepFilterNet configuration exceeds 64 KiB".into());
        }
        Plugin::load(configuration).map(|plugin| Box::into_raw(Box::new(plugin)).cast::<c_void>())
    }));
    match result {
        Ok(Ok(plugin)) => plugin,
        Ok(Err(message)) => {
            unsafe { write_error(error, error_capacity, &message) };
            ptr::null_mut()
        }
        Err(_) => {
            unsafe {
                write_error(
                    error,
                    error_capacity,
                    "DeepFilterNet initialization panicked",
                )
            };
            ptr::null_mut()
        }
    }
}

/// # Safety
/// state was created by this DLL; input/output have samples elements, may overlap,
/// and are not used concurrently. error has error_capacity writable bytes.
#[no_mangle]
pub unsafe extern "C" fn tr_denoise_process(
    state: *mut c_void,
    input: *const f32,
    samples: usize,
    sample_rate: u32,
    output: *mut f32,
    error: *mut c_char,
    error_capacity: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<(), String> {
        if state.is_null() || (samples > 0 && (input.is_null() || output.is_null())) {
            return Err("DeepFilterNet received a null state or audio pointer".into());
        }
        if samples > MAX_SAMPLES {
            return Err("DeepFilterNet input exceeds 30 seconds".into());
        }
        let input = if samples == 0 {
            &[]
        } else {
            unsafe { slice::from_raw_parts(input, samples) }
        };
        let plugin = unsafe { &*state.cast::<Plugin>() };
        let enhanced = plugin.process(input, sample_rate)?;
        if samples > 0 {
            unsafe { ptr::copy(enhanced.as_ptr(), output, samples) };
        }
        Ok(())
    }));
    match result {
        Ok(Ok(())) => 0,
        Ok(Err(message)) => {
            unsafe { write_error(error, error_capacity, &message) };
            1
        }
        Err(_) => {
            unsafe { write_error(error, error_capacity, "DeepFilterNet processing panicked") };
            2
        }
    }
}

/// # Safety
/// state is null or a live, unique pointer returned by this DLL's create function.
#[no_mangle]
pub unsafe extern "C" fn tr_denoise_destroy(state: *mut c_void) {
    if !state.is_null() {
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
            drop(Box::from_raw(state.cast::<Plugin>()));
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_model() -> Plugin {
        let model = std::env::var("TALKRECORDER_DEEPFILTER_MODEL")
            .expect("Set TALKRECORDER_DEEPFILTER_MODEL to the prepared fixed model");
        Plugin::load(&serde_json::json!({"model":model}).to_string()).unwrap()
    }

    #[test]
    fn null_and_missing_configuration_are_errors() {
        let mut error = [0i8; 256];
        assert!(
            unsafe { tr_denoise_create(ptr::null(), error.as_mut_ptr(), error.len()) }.is_null()
        );
        assert!(Plugin::load("{}").is_err());
        assert_eq!(tr_denoise_api_version(), 1);
    }

    #[test]
    #[ignore = "Requires prepared fixed DeepFilterNet3 model"]
    fn arbitrary_lengths_keep_count_finite_values_and_independent_state() {
        let plugin = load_model();
        assert_eq!(plugin.delay(), 1_440);
        for length in [0, 1, 479, 480, 481, 48_000] {
            let input: Vec<f32> = (0..length)
                .map(|index| 0.03 * (index as f32 * 0.073).sin())
                .collect();
            let output = plugin.process(&input, RATE).unwrap();
            assert_eq!(output.len(), input.len());
            assert!(output.iter().all(|value| value.is_finite()));
            assert_eq!(output, plugin.process(&input, RATE).unwrap());
        }
        assert!(plugin.process(&[0.0], 44_100).is_err());
        assert!(plugin.process(&[f32::NAN], RATE).is_err());
    }

    #[test]
    #[ignore = "Requires prepared fixed DeepFilterNet3 model and public WAV fixture"]
    fn speech_position_is_preserved_after_delay_compensation() {
        let fixture = std::env::var("TALKRECORDER_SPEECH_FIXTURE").unwrap();
        let mut reader = hound::WavReader::open(fixture).unwrap();
        let specification = reader.spec();
        assert_eq!(specification.channels, 1);
        assert_eq!(specification.sample_format, hound::SampleFormat::Int);
        let pcm: Vec<f32> = reader
            .samples::<i16>()
            .map(|sample| sample.unwrap() as f32 / 32_768.0)
            .collect();
        let original: Vec<f32> = (0..3 * RATE as usize)
            .map(|index| {
                let position =
                    (index as f64 / RATE as f64 + 2.0) * specification.sample_rate as f64;
                let low = position.floor() as usize;
                let fraction = (position - low as f64) as f32;
                pcm[low] * (1.0 - fraction) + pcm[low + 1] * fraction
            })
            .collect();
        let plugin = load_model();
        let enhanced = plugin.process(&original, RATE).unwrap();
        let begin = RATE as usize / 2;
        let end = original.len() - begin;
        let score = |lag: isize| {
            let mut dot = 0.0f64;
            let mut left = 0.0f64;
            let mut right = 0.0f64;
            for index in (begin..end).step_by(6) {
                let a = original[index] as f64;
                let b = enhanced[(index as isize + lag) as usize] as f64;
                dot += a * b;
                left += a * a;
                right += b * b;
            }
            dot / (left * right).sqrt()
        };
        let (lag, ncc) = (-4_800..=4_800)
            .step_by(6)
            .map(|lag| (lag, score(lag)))
            .max_by(|left, right| left.1.total_cmp(&right.1))
            .unwrap();
        eprintln!("DeepFilterNet speech NCC={ncc:.6}, residual lag={} frames ({:.3}ms), compensated delay={} frames", lag, lag as f64 / 48.0, plugin.delay());
        assert!(ncc > 0.6, "Speech must match, measured NCC {ncc}");
        assert!(lag.abs() <= 48, "Residual lag must stay within 1ms: {lag}");
    }

    #[test]
    #[ignore = "Requires release DLL and prepared fixed model"]
    fn actual_dll_abi_processes_audio_and_reports_invalid_input() {
        use libloading::{Library, Symbol};
        use std::ffi::CString;
        type Create = unsafe extern "C" fn(*const c_char, *mut c_char, usize) -> *mut c_void;
        type Process = unsafe extern "C" fn(
            *mut c_void,
            *const f32,
            usize,
            u32,
            *mut f32,
            *mut c_char,
            usize,
        ) -> i32;
        let library_path = std::env::var("TALKRECORDER_DEEPFILTER_DLL").unwrap();
        let model = std::env::var("TALKRECORDER_DEEPFILTER_MODEL").unwrap();
        let configuration = CString::new(serde_json::json!({"model":model}).to_string()).unwrap();
        unsafe {
            let library = Library::new(library_path).unwrap();
            let version: Symbol<unsafe extern "C" fn() -> u32> =
                library.get(b"tr_denoise_api_version\0").unwrap();
            let create: Symbol<Create> = library.get(b"tr_denoise_create\0").unwrap();
            let process: Symbol<Process> = library.get(b"tr_denoise_process\0").unwrap();
            let destroy: Symbol<unsafe extern "C" fn(*mut c_void)> =
                library.get(b"tr_denoise_destroy\0").unwrap();
            assert_eq!(version(), 1);
            let mut error = [0i8; 2048];
            let initialization_started = std::time::Instant::now();
            let state = create(configuration.as_ptr(), error.as_mut_ptr(), error.len());
            assert!(
                !state.is_null(),
                "{}",
                CStr::from_ptr(error.as_ptr()).to_string_lossy()
            );
            let initialization_ms = initialization_started.elapsed().as_secs_f64() * 1_000.0;
            let input: Vec<f32> = (0..48_137)
                .map(|index| (index as f32 * 0.073).sin() * 0.05)
                .collect();
            let mut output = vec![f32::NAN; input.len()];
            let processing_started = std::time::Instant::now();
            assert_eq!(
                process(
                    state,
                    input.as_ptr(),
                    input.len(),
                    RATE,
                    output.as_mut_ptr(),
                    error.as_mut_ptr(),
                    error.len()
                ),
                0
            );
            let processing_seconds = processing_started.elapsed().as_secs_f64();
            eprintln!("DeepFilterNet DLL init={initialization_ms:.1}ms, process={:.1}ms for {} samples, RTF={:.4}", processing_seconds * 1_000.0, input.len(), processing_seconds / (input.len() as f64 / RATE as f64));
            assert!(output.iter().all(|sample| sample.is_finite()));
            assert_ne!(
                process(
                    state,
                    ptr::null(),
                    1,
                    RATE,
                    output.as_mut_ptr(),
                    error.as_mut_ptr(),
                    error.len()
                ),
                0
            );
            assert!(!CStr::from_ptr(error.as_ptr()).to_bytes().is_empty());
            destroy(state);
            destroy(ptr::null_mut());
        }
    }
}
