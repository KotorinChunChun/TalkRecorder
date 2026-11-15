//! Per-thread sleep inhibition and suspend/resume observation. No persistent
//! Windows power setting is changed.
//!
//! References:
//! https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-setthreadexecutionstate
//! https://learn.microsoft.com/en-us/windows/win32/api/realtimeapiset/nf-realtimeapiset-queryunbiasedinterrupttime

use anyhow::Result;
use std::{
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant, SystemTime},
};

/// Keep this guard on the recording management thread. Its non-Send marker
/// prevents restoring execution state on a different thread accidentally.
pub struct SleepGuard {
    #[cfg(windows)]
    previous: Option<windows::Win32::System::Power::EXECUTION_STATE>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl SleepGuard {
    pub fn new(enabled: bool) -> Result<Self> {
        #[cfg(windows)]
        {
            use windows::Win32::System::Power::{
                ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
            };
            let previous = if enabled {
                let state = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                anyhow::ensure!(state.0 != 0, "録音中のスリープ抑止を設定できませんでした");
                Some(state)
            } else {
                None
            };
            Ok(Self {
                previous,
                _thread_bound: PhantomData,
            })
        }
        #[cfg(not(windows))]
        {
            let _ = enabled;
            Ok(Self {
                _thread_bound: PhantomData,
            })
        }
    }
}

impl Drop for SleepGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(previous) = self.previous {
            use windows::Win32::System::Power::{ES_CONTINUOUS, SetThreadExecutionState};
            // Restore the state which this thread held before recording.
            let _ = unsafe { SetThreadExecutionState(previous | ES_CONTINUOUS) };
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct InterruptClocks {
    inclusive: u64,
    awake: u64,
}

fn suspended_between(before: InterruptClocks, after: InterruptClocks) -> Option<Duration> {
    let elapsed = after.inclusive.checked_sub(before.inclusive)?;
    let awake = after.awake.checked_sub(before.awake)?;
    Some(Duration::from_nanos(
        elapsed.saturating_sub(awake).saturating_mul(100),
    ))
}

fn interrupt_clocks() -> Option<InterruptClocks> {
    #[cfg(windows)]
    {
        use windows::Win32::System::WindowsProgramming::{
            QueryInterruptTime, QueryUnbiasedInterruptTime,
        };
        let mut awake = 0;
        if !unsafe { QueryUnbiasedInterruptTime(&mut awake) }.as_bool() {
            return None;
        }
        let inclusive = unsafe { QueryInterruptTime() };
        Some(InterruptClocks { inclusive, awake })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// On Windows compare interrupt clocks including and excluding suspended time.
/// Unlike wall time, these are not changed by the user or time synchronization.
/// API failure/non-Windows falls back to wall/monotonic observation, which can
/// conservatively report a very long scheduling stall as a resume.
pub struct ResumeDetector {
    clocks: Option<InterruptClocks>,
    instant: Instant,
    wall: SystemTime,
    threshold: Duration,
}

impl Default for ResumeDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl ResumeDetector {
    pub fn new() -> Self {
        Self::with_threshold(Duration::from_secs(2))
    }

    pub fn with_threshold(threshold: Duration) -> Self {
        Self {
            clocks: interrupt_clocks(),
            instant: Instant::now(),
            wall: SystemTime::now(),
            threshold: threshold.max(Duration::from_millis(100)),
        }
    }

    pub fn poll(&mut self) -> bool {
        let current = interrupt_clocks();
        let now = Instant::now();
        let wall = SystemTime::now();
        let detected = match (self.clocks, current) {
            (Some(before), Some(after)) => {
                suspended_between(before, after).is_some_and(|elapsed| elapsed >= self.threshold)
            }
            _ => fallback_resume(
                now.saturating_duration_since(self.instant),
                wall.duration_since(self.wall).ok(),
                self.threshold,
            ),
        };
        self.clocks = current;
        self.instant = now;
        self.wall = wall;
        detected
    }
}

fn fallback_resume(monotonic: Duration, wall: Option<Duration>, threshold: Duration) -> bool {
    wall.is_some_and(|wall| wall.saturating_sub(monotonic) >= threshold)
        || monotonic >= threshold.saturating_add(Duration::from_secs(5))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspend_clock_gap_is_distinct_from_active_stall_and_time_regression() {
        let before = InterruptClocks {
            inclusive: 100_000_000,
            awake: 90_000_000,
        };
        // A ten-minute active stall advances both clocks, so it is not sleep.
        let after = InterruptClocks {
            inclusive: 6_100_000_000,
            awake: 6_090_000_000,
        };
        assert_eq!(suspended_between(before, after), Some(Duration::ZERO));
        let resumed = InterruptClocks {
            inclusive: 700_000_000,
            awake: 100_000_000,
        };
        assert_eq!(
            suspended_between(before, resumed),
            Some(Duration::from_secs(59))
        );
        assert!(suspended_between(resumed, before).is_none());
    }

    #[test]
    fn fallback_handles_backward_clock_without_panicking() {
        let threshold = Duration::from_secs(2);
        assert!(!fallback_resume(Duration::from_millis(10), None, threshold));
        assert!(!fallback_resume(
            Duration::from_secs(1),
            Some(Duration::from_secs(1)),
            threshold
        ));
        assert!(fallback_resume(
            Duration::from_secs(1),
            Some(Duration::from_secs(11)),
            threshold
        ));
        assert!(fallback_resume(Duration::from_secs(8), None, threshold));
    }

    #[test]
    fn disabled_sleep_guard_and_short_native_clock_observation_are_safe() {
        let _guard = SleepGuard::new(false).unwrap();
        let mut detector = ResumeDetector::new();
        assert!(!detector.poll());
    }
}
