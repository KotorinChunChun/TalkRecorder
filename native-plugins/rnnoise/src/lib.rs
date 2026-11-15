//! RNNoise (nnnoiseless 0.5.2) adapter for TalkRecorder's denoiser ABI v1.
//!
//! Each call is an independent mono window. The host supplies preceding context.
//! RNNoise's first 480-sample output is its STFT warm-up delay. We discard that
//! output and process one zero-padded frame after the input to recover the tail.

use nnnoiseless::{DenoiseState, RnnModel};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    ptr, slice,
};

const RATE: u32 = 48_000;
const FRAME: usize = DenoiseState::FRAME_SIZE;
const MAX_SAMPLES: usize = RATE as usize * 60;
const PCM_SCALE: f32 = 32768.0;

struct Plugin {
    // Default RNNoise weights are compiled into nnnoiseless and shared by each
    // fresh DenoiseState, so process calls do not read model files or the network.
    model: RnnModel,
}

impl Plugin {
    fn new(config_json: &str) -> Result<Self, String> {
        let config: serde_json::Value = serde_json::from_str(config_json)
            .map_err(|e| format!("Invalid plugin configuration: {e}"))?;
        if !config.as_object().is_some_and(|object| object.is_empty()) {
            return Err(
                "RNNoise uses its built-in model; resources must be an empty object".into(),
            );
        }
        Ok(Self {
            model: RnnModel::default(),
        })
    }

    fn process(&self, input: &[f32], sample_rate: u32) -> Result<Vec<f32>, String> {
        if sample_rate != RATE {
            return Err("RNNoise requires 48000 Hz mono audio".into());
        }
        if input.len() > MAX_SAMPLES {
            return Err("The audio window exceeds 60 seconds".into());
        }
        if input.iter().any(|value| !value.is_finite()) {
            return Err("The input contains non-finite samples".into());
        }
        if input.is_empty() {
            return Ok(Vec::new());
        }
        let mut denoiser = DenoiseState::with_model(&self.model);
        let mut in_frame = [0.0; FRAME];
        let mut out_frame = [0.0; FRAME];
        let frame_count = input.len().div_ceil(FRAME);
        let mut aligned = Vec::with_capacity(frame_count * FRAME);
        // The final iteration feeds zero lookahead to synthesize the last real
        // frame. It is required even when input.len() is exactly divisible by 480.
        for frame_index in 0..=frame_count {
            in_frame.fill(0.0);
            let start = frame_index * FRAME;
            if start < input.len() {
                let end = (start + FRAME).min(input.len());
                for (out, &sample) in in_frame.iter_mut().zip(&input[start..end]) {
                    *out = sample.clamp(-1.0, 1.0) * PCM_SCALE;
                }
            }
            denoiser.process_frame(&mut out_frame, &in_frame);
            if frame_index != 0 {
                aligned.extend(
                    out_frame
                        .iter()
                        .map(|sample| (sample / PCM_SCALE).clamp(-1.0, 1.0)),
                );
            }
        }
        aligned.truncate(input.len());
        if aligned.len() != input.len() || aligned.iter().any(|value| !value.is_finite()) {
            return Err("RNNoise produced an invalid sample count or value".into());
        }
        Ok(aligned)
    }
}

unsafe fn write_error(buffer: *mut c_char, capacity: usize, message: &str) {
    if buffer.is_null() || capacity == 0 {
        return;
    }
    let count = message.len().min(capacity - 1);
    unsafe {
        ptr::copy_nonoverlapping(message.as_ptr(), buffer.cast::<u8>(), count);
        *buffer.add(count) = 0;
    }
}

fn failure_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        format!("RNNoise panicked: {message}")
    } else if let Some(message) = panic.downcast_ref::<String>() {
        format!("RNNoise panicked: {message}")
    } else {
        "RNNoise panicked".into()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tr_denoise_api_version() -> u32 {
    1
}

/// # Safety
/// config_json is a valid NUL-terminated UTF-8 string, and error points to
/// error_capacity writable bytes (or is NULL). Destroy the returned handle once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tr_denoise_create(
    config_json: *const c_char,
    error: *mut c_char,
    error_capacity: usize,
) -> *mut c_void {
    unsafe { write_error(error, error_capacity, "") };
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<Plugin, String> {
        if config_json.is_null() {
            return Err("Plugin configuration is NULL".into());
        }
        let config = unsafe { CStr::from_ptr(config_json) }
            .to_str()
            .map_err(|_| "Plugin configuration is not UTF-8".to_owned())?;
        if config.len() > 64 * 1024 {
            return Err("Plugin configuration exceeds 64 KiB".into());
        }
        Plugin::new(config)
    }));
    match result {
        Ok(Ok(plugin)) => Box::into_raw(Box::new(plugin)).cast(),
        Ok(Err(message)) => {
            unsafe { write_error(error, error_capacity, &message) };
            ptr::null_mut()
        }
        Err(panic) => {
            unsafe { write_error(error, error_capacity, &failure_message(panic)) };
            ptr::null_mut()
        }
    }
}

/// # Safety
/// state is a live handle from tr_denoise_create. input/output point to samples
/// readable/writable aligned f32 values. NULL audio pointers are allowed at zero.
/// error points to error_capacity writable bytes (or is NULL).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tr_denoise_process(
    state: *mut c_void,
    input: *const f32,
    samples: usize,
    sample_rate: u32,
    output: *mut f32,
    error: *mut c_char,
    error_capacity: usize,
) -> c_int {
    unsafe { write_error(error, error_capacity, "") };
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<(), String> {
        if state.is_null() {
            return Err("Plugin handle is NULL".into());
        }
        if samples > MAX_SAMPLES {
            return Err("The audio window exceeds 60 seconds".into());
        }
        if samples != 0
            && (input.is_null()
                || output.is_null()
                || !(input as usize).is_multiple_of(align_of::<f32>())
                || !(output as usize).is_multiple_of(align_of::<f32>()))
        {
            return Err("Invalid audio pointer".into());
        }
        let input_slice = if samples == 0 {
            &[]
        } else {
            unsafe { slice::from_raw_parts(input, samples) }
        };
        let plugin = unsafe { &*state.cast::<Plugin>() };
        let processed = plugin.process(input_slice, sample_rate)?;
        if samples != 0 {
            unsafe { ptr::copy_nonoverlapping(processed.as_ptr(), output, samples) };
        }
        Ok(())
    }));
    match result {
        Ok(Ok(())) => 0,
        Ok(Err(message)) => {
            unsafe { write_error(error, error_capacity, &message) };
            1
        }
        Err(panic) => {
            unsafe { write_error(error, error_capacity, &failure_message(panic)) };
            2
        }
    }
}

/// # Safety
/// state is NULL or a live handle returned by tr_denoise_create, not yet freed.
#[unsafe(no_mangle)]
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
    use std::ffi::CString;

    #[test]
    fn arbitrary_window_lengths_and_tail_keep_exact_finite_samples() {
        let plugin = Plugin::new("{}").unwrap();
        for length in [0, 1, 479, 480, 481, 959, 960, 961, 48_000, 48_001] {
            let input: Vec<f32> = (0..length).map(|i| 0.1 * (i as f32 * 0.07).sin()).collect();
            let output = plugin.process(&input, RATE).unwrap();
            assert_eq!(input.len(), output.len());
            assert!(
                output
                    .iter()
                    .all(|value| value.is_finite() && value.abs() <= 1.0)
            );
        }
    }

    #[test]
    fn calls_are_independent_and_do_not_leak_channel_history() {
        let plugin = Plugin::new("{}").unwrap();
        let input: Vec<f32> = (0..4_800).map(|i| 0.15 * (i as f32 * 0.05).sin()).collect();
        let first = plugin.process(&input, RATE).unwrap();
        let _ = plugin.process(&vec![0.2; 4_800], RATE).unwrap();
        assert_eq!(first, plugin.process(&input, RATE).unwrap());
        assert_eq!(plugin.process(&[0.0; 481], RATE).unwrap(), vec![0.0; 481]);
    }

    #[test]
    fn rejects_wrong_rate_nonfinite_and_invalid_resources() {
        let plugin = Plugin::new("{}").unwrap();
        assert!(plugin.process(&[0.0], 44_100).is_err());
        assert!(plugin.process(&[f32::NAN], RATE).is_err());
        assert!(plugin.process(&[f32::INFINITY], RATE).is_err());
        assert!(Plugin::new("[]").is_err());
        assert!(Plugin::new("{\"model\":\"external\"}").is_err());
    }

    #[test]
    fn abi_reports_errors_and_does_not_modify_output_on_failure() {
        let config = CString::new("{}").unwrap();
        let mut error = [0i8; 128];
        unsafe {
            let handle = tr_denoise_create(config.as_ptr(), error.as_mut_ptr(), error.len());
            assert!(!handle.is_null());
            let mut output = [0.75];
            let status = tr_denoise_process(
                handle,
                [f32::NAN].as_ptr(),
                1,
                RATE,
                output.as_mut_ptr(),
                error.as_mut_ptr(),
                error.len(),
            );
            assert_ne!(status, 0);
            assert_eq!(output, [0.75]);
            assert!(!CStr::from_ptr(error.as_ptr()).to_bytes().is_empty());
            assert_eq!(
                tr_denoise_process(
                    handle,
                    ptr::null(),
                    0,
                    RATE,
                    ptr::null_mut(),
                    error.as_mut_ptr(),
                    error.len()
                ),
                0
            );
            assert_ne!(
                tr_denoise_process(
                    handle,
                    ptr::null(),
                    MAX_SAMPLES + 1,
                    RATE,
                    ptr::null_mut(),
                    error.as_mut_ptr(),
                    error.len()
                ),
                0
            );
            tr_denoise_destroy(handle);
            tr_denoise_destroy(ptr::null_mut());
            assert!(tr_denoise_create(ptr::null(), error.as_mut_ptr(), error.len()).is_null());
        }
    }

    #[test]
    fn stft_delay_is_compensated_and_last_frame_is_flushed() {
        let plugin = Plugin::new("{}").unwrap();
        let mut input = vec![0.0; 9_600];
        // Repeated multi-harmonic voiced bursts make the network retain signal;
        // measure the whole waveform lag, rather than only equal vector lengths.
        for (i, value) in input.iter_mut().enumerate().take(8_640).skip(960) {
            let envelope = (std::f32::consts::PI * (i - 960) as f32 / 7_680.0)
                .sin()
                .powi(2);
            *value = envelope * (0.2 * (i as f32 * 0.026).sin() + 0.07 * (i as f32 * 0.052).sin());
        }
        let aligned = plugin.process(&input, RATE).unwrap();
        let best_lag = (-(FRAME as isize)..=FRAME as isize)
            .max_by(|&a, &b| {
                correlation(&input, &aligned, a).total_cmp(&correlation(&input, &aligned, b))
            })
            .unwrap();
        // High-pass/pitch filtering can change waveform phase by a few samples;
        // this still rejects an uncompensated 480-sample frame delay.
        assert!(best_lag.abs() <= 16, "remaining delay: {best_lag} samples");
        let mut tail_input = vec![0.0; 4_800];
        tail_input[4_799] = 0.5;
        let tail = plugin.process(&tail_input, RATE).unwrap();
        assert!(
            tail.iter()
                .rev()
                .take(FRAME)
                .any(|value| value.abs() > 1e-7)
        );
    }

    fn correlation(a: &[f32], b: &[f32], lag: isize) -> f64 {
        let mut xy = 0.0;
        let mut xx = 0.0;
        let mut yy = 0.0;
        for (i, &sample) in a.iter().enumerate() {
            let j = i as isize + lag;
            if j >= 0 && (j as usize) < b.len() {
                let x = f64::from(sample);
                let y = f64::from(b[j as usize]);
                xy += x * y;
                xx += x * x;
                yy += y * y;
            }
        }
        xy / (xx * yy).sqrt().max(f64::MIN_POSITIVE)
    }
}
