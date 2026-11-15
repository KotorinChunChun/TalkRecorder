//! Conservative recovery of a stream clock when an endpoint reports another epoch.
//!
//! Arrival observations include unknown transport latency. A recovered clock is
//! therefore a stable receiver-side time line, not a measurement of acoustic or
//! one-way network delay. Missing audio is never inferred to be silence.
use std::collections::VecDeque;

use serde::Serialize;

const TICKS_PER_SECOND: f64 = 10_000_000.0;
// A capture timestamp precedes retrieval; a small future tolerance accommodates
// endpoint scheduling jitter, not a second epoch or a many-second driver error.
const MAX_NATIVE_PAST_SKEW: u64 = 10_000_000;
const MAX_NATIVE_FUTURE_SKEW: u64 = 500_000;
const WARMUP_SECONDS: f64 = 5.0;
const BIN_SECONDS: f64 = 0.5;
const MAX_BINS: usize = 64;
const MIN_BINS: usize = 8;
const MIN_PACKETS: usize = 30;
const MAX_DRIFT_PPM: f64 = 5_000.0;
const MAX_ENVELOPE_SPREAD_MS: f64 = 40.0;
const MAX_ARRIVAL_GAP: u64 = 2 * 10_000_000;
const MAX_LATE_PACKET_MS: f64 = 200.0;
const PHASE_SLEW_SECONDS: f64 = 5.0;
const MAX_PHASE_SLEW_PPM: f64 = 1_000.0;
const RATE_SLEW_PPM_PER_SECOND: f64 = 500.0;
const MAX_REJECTED_LEARNING_SECONDS: f64 = 5.0;

#[derive(Debug, Clone, Copy)]
pub struct ClockObservation {
    pub device_frame: u64,
    pub frames: u32,
    pub sample_rate: u32,
    pub raw_qpc_100ns: u64,
    pub received_qpc_100ns: u64,
    pub native_timestamp_valid: bool,
    pub discontinuity: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockMode {
    Native,
    Learning,
    Recovered,
    Unstable,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ClockEstimate {
    pub qpc_100ns: u64,
    pub valid: bool,
    pub mode: ClockMode,
    /// Recovered packet start minus its native timestamp. This is not latency.
    pub offset_ms: f64,
    /// Fitted/candidate envelope spread, or the observed late-packet deviation
    /// when that window rejects a packet. Excludes unknown fixed latency.
    pub uncertainty_ms: f64,
    /// (Estimated sample rate / nominal sample rate - 1) * 1,000,000.
    pub drift_ppm: f64,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct EnvelopePoint {
    bin: u64,
    frame_end: u64,
    received: u64,
}

#[derive(Debug, Clone, Copy)]
struct Fit {
    frame_origin: u64,
    received_origin: u64,
    intercept: f64,
    ticks_per_frame: f64,
    spread_ms: f64,
}

#[derive(Debug, Clone, Copy)]
enum FitRejectionReason {
    TooFewBins,
    TooFewSeparatedPoints,
    InvalidSlope,
    DriftOutsideBound,
    InvalidSpread,
    SpreadOutsideBound,
    PacketLate,
    PacketAhead,
}

#[derive(Debug, Clone, Copy)]
struct FitRejection {
    reason: FitRejectionReason,
    drift_ppm: f64,
    spread_ms: f64,
}

impl FitRejection {
    fn estimate(self, o: ClockObservation, restarted: bool) -> ClockEstimate {
        let reason = match (self.reason, restarted) {
            (FitRejectionReason::TooFewBins, _) => "too few receiver clock envelope bins",
            (FitRejectionReason::TooFewSeparatedPoints, _) => {
                "too few separated clock observations"
            }
            (FitRejectionReason::InvalidSlope, false) => "receiver clock fit slope is invalid",
            (FitRejectionReason::InvalidSlope, true) => {
                "receiver clock fit slope is invalid; restarted learning"
            }
            (FitRejectionReason::DriftOutsideBound, false) => {
                "receiver clock fit drift exceeds allowed bound"
            }
            (FitRejectionReason::DriftOutsideBound, true) => {
                "receiver clock fit drift exceeds allowed bound; restarted learning"
            }
            (FitRejectionReason::InvalidSpread, false) => {
                "receiver clock envelope spread is invalid"
            }
            (FitRejectionReason::InvalidSpread, true) => {
                "receiver clock envelope spread is invalid; restarted learning"
            }
            (FitRejectionReason::SpreadOutsideBound, false) => {
                "receiver clock envelope spread exceeds allowed bound"
            }
            (FitRejectionReason::SpreadOutsideBound, true) => {
                "receiver clock envelope spread exceeds allowed bound; restarted learning"
            }
            (FitRejectionReason::PacketLate, _) => {
                "packet too late for recovered clock window; restarted learning"
            }
            (FitRejectionReason::PacketAhead, _) => {
                "packet ahead of recovered clock window; restarted learning"
            }
        };
        ClockEstimate {
            drift_ppm: if self.drift_ppm.is_finite() {
                self.drift_ppm
            } else {
                0.0
            },
            uncertainty_ms: if self.spread_ms.is_finite() {
                self.spread_ms.max(0.0)
            } else {
                0.0
            },
            ..unknown(o, ClockMode::Unstable, reason)
        }
    }
}

impl Fit {
    fn at(self, frame: u64) -> f64 {
        self.received_origin as f64
            + self.intercept
            + (i128::from(frame) - i128::from(self.frame_origin)) as f64 * self.ticks_per_frame
    }
}

#[derive(Debug, Clone, Copy)]
struct Model {
    frame: u64,
    qpc: f64,
    ticks_per_frame: f64,
}

#[derive(Debug, Default)]
pub struct ClockRecovery {
    previous: Option<ClockObservation>,
    last_native: Option<u64>,
    frame_origin: u64,
    received_origin: u64,
    envelopes: VecDeque<EnvelopePoint>,
    packet_count: usize,
    fit: Option<Fit>,
    model: Option<Model>,
    last_rejection: Option<FitRejection>,
    rejected_since: Option<u64>,
}

impl ClockRecovery {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    fn clear_recovery(&mut self) {
        self.envelopes.clear();
        self.packet_count = 0;
        self.fit = None;
        self.model = None;
        self.last_rejection = None;
        self.rejected_since = None;
    }

    fn restart_learning_at(&mut self, o: ClockObservation) {
        // Keep previous device continuity and last_native: a fresh fit must not
        // make a reset or a regressing native timestamp appear trustworthy.
        self.clear_recovery();
        self.frame_origin = o.device_frame;
        self.received_origin = o.received_qpc_100ns;
        self.packet_count = 1;
        self.envelopes.push_back(EnvelopePoint {
            bin: (f64::from(o.frames) / (f64::from(o.sample_rate) * BIN_SECONDS)) as u64,
            frame_end: o.device_frame + u64::from(o.frames),
            received: o.received_qpc_100ns,
        });
    }

    pub fn observe(&mut self, observation: ClockObservation) -> ClockEstimate {
        let o = observation;
        if !(8_000..=384_000).contains(&o.sample_rate)
            || o.frames == 0
            || o.frames > o.sample_rate
            || o.received_qpc_100ns == 0
            || o.device_frame.checked_add(u64::from(o.frames)).is_none()
        {
            self.clear_recovery();
            self.previous = None;
            return unknown(o, ClockMode::Unstable, "invalid clock observation");
        }

        let frame_end = o.device_frame + u64::from(o.frames);
        let mut broken = o.discontinuity;
        let mut reason = "device discontinuity";
        let mut receive_fault = false;
        if let Some(previous) = self.previous {
            if o.received_qpc_100ns < previous.received_qpc_100ns {
                broken = true;
                receive_fault = true;
                reason = "receiver clock regressed";
            } else if o.received_qpc_100ns - previous.received_qpc_100ns > MAX_ARRIVAL_GAP {
                broken = true;
                receive_fault = true;
                reason = "packet arrival gap";
            } else if o.sample_rate != previous.sample_rate {
                broken = true;
                reason = "sample rate changed";
            } else if o.device_frame != previous.device_frame + u64::from(previous.frames) {
                broken = true;
                reason = "device frame gap or reset";
            }
        }
        self.previous = Some(o);
        if broken {
            self.clear_recovery();
        }

        let native_ok = o.native_timestamp_valid
            && o.raw_qpc_100ns != 0
            && o.received_qpc_100ns.saturating_sub(o.raw_qpc_100ns) <= MAX_NATIVE_PAST_SKEW
            && o.raw_qpc_100ns.saturating_sub(o.received_qpc_100ns) <= MAX_NATIVE_FUTURE_SKEW
            && self
                .last_native
                .is_none_or(|previous| o.raw_qpc_100ns >= previous);
        if native_ok && !receive_fault && self.model.is_none() {
            self.last_native = Some(o.raw_qpc_100ns);
            self.clear_recovery();
            return ClockEstimate {
                qpc_100ns: o.raw_qpc_100ns,
                valid: !broken,
                mode: ClockMode::Native,
                offset_ms: 0.0,
                uncertainty_ms: 0.0,
                drift_ppm: 0.0,
                reason: if broken { reason } else { "native clock" },
            };
        }

        if self.envelopes.is_empty() {
            self.frame_origin = o.device_frame;
            self.received_origin = o.received_qpc_100ns;
        }
        let nominal = TICKS_PER_SECOND / f64::from(o.sample_rate);
        self.packet_count = self.packet_count.saturating_add(1);
        let bin = ((frame_end - self.frame_origin) as f64
            / (f64::from(o.sample_rate) * BIN_SECONDS)) as u64;
        let point = EnvelopePoint {
            bin,
            frame_end,
            received: o.received_qpc_100ns,
        };
        let new_bin = self.envelopes.back().is_none_or(|last| last.bin != bin);
        if let Some(last) = self.envelopes.back_mut().filter(|last| last.bin == bin) {
            let residual = |point: EnvelopePoint| {
                (i128::from(point.received) - i128::from(self.received_origin)) as f64
                    - (point.frame_end - self.frame_origin) as f64 * nominal
            };
            if residual(point) < residual(*last) {
                *last = point;
            }
        } else {
            self.envelopes.push_back(point);
            while self.envelopes.len() > MAX_BINS {
                self.envelopes.pop_front();
            }
        }

        let warm = (frame_end - self.frame_origin) as f64 / f64::from(o.sample_rate)
            >= WARMUP_SECONDS
            && (o.received_qpc_100ns - self.received_origin) as f64 / TICKS_PER_SECOND
                >= WARMUP_SECONDS - BIN_SECONDS
            && self.packet_count >= MIN_PACKETS;
        if !warm || self.envelopes.len() < MIN_BINS + 1 {
            return unknown(
                o,
                if broken {
                    ClockMode::Unstable
                } else {
                    ClockMode::Learning
                },
                if broken {
                    reason
                } else {
                    "learning receiver clock envelope"
                },
            );
        }
        if new_bin || (self.fit.is_none() && self.last_rejection.is_none()) {
            let candidate = fit_envelope(
                &self.envelopes,
                self.frame_origin,
                self.received_origin,
                nominal,
            );
            match candidate {
                Ok(fit) => {
                    self.fit = Some(fit);
                    self.last_rejection = None;
                    self.rejected_since = None;
                }
                Err(rejection) => {
                    self.fit = None;
                    let rejected_since = *self.rejected_since.get_or_insert(o.received_qpc_100ns);
                    let restart = self.model.is_some()
                        || o.received_qpc_100ns.saturating_sub(rejected_since) as f64
                            >= MAX_REJECTED_LEARNING_SECONDS * TICKS_PER_SECOND;
                    if restart {
                        // Do not relax drift/jitter bounds. An old anomalous bin
                        // cannot veto indefinitely: start a fresh full warmup,
                        // with every packet Unknown until that new fit passes.
                        self.restart_learning_at(o);
                    } else {
                        self.last_rejection = Some(rejection);
                    }
                    return rejection.estimate(o, restart);
                }
            }
        }
        if let Some(rejection) = self.last_rejection {
            return rejection.estimate(o, false);
        }
        let fit = self.fit.expect("fit is available after warmup");
        if broken {
            return unknown(o, ClockMode::Unstable, reason);
        }
        if self.model.is_none() {
            self.model = Some(Model {
                frame: o.device_frame,
                qpc: fit.at(o.device_frame),
                ticks_per_frame: fit.ticks_per_frame,
            });
        }
        let model = self.model.as_mut().expect("model initialized");
        let frames_elapsed = o.device_frame.saturating_sub(model.frame);
        let qpc = model.qpc + frames_elapsed as f64 * model.ticks_per_frame;

        if native_ok {
            self.last_native = Some(o.raw_qpc_100ns);
            // Changing epoch by even a small backwards step could overwrite a
            // retained packet. Hand native control back only at sample precision.
            if (qpc - o.raw_qpc_100ns as f64).abs() <= nominal / 2.0 {
                self.clear_recovery();
                return ClockEstimate {
                    qpc_100ns: o.raw_qpc_100ns,
                    valid: true,
                    mode: ClockMode::Native,
                    offset_ms: 0.0,
                    uncertainty_ms: 0.0,
                    drift_ppm: 0.0,
                    reason: "native clock recovered",
                };
            }
        }
        let target = if native_ok {
            o.raw_qpc_100ns as f64
        } else {
            fit.at(o.device_frame)
        };
        let phase = ((target - qpc) / (f64::from(o.sample_rate) * PHASE_SLEW_SECONDS)).clamp(
            -nominal * MAX_PHASE_SLEW_PPM / 1_000_000.0,
            nominal * MAX_PHASE_SLEW_PPM / 1_000_000.0,
        );
        let target_step = fit.ticks_per_frame + phase;
        let max_step_change = nominal * RATE_SLEW_PPM_PER_SECOND / 1_000_000.0
            * (frames_elapsed as f64 / f64::from(o.sample_rate));
        model.ticks_per_frame +=
            (target_step - model.ticks_per_frame).clamp(-max_step_change, max_step_change);
        model.frame = o.device_frame;
        model.qpc = qpc;
        let packet_end = qpc + f64::from(o.frames) * model.ticks_per_frame;
        let late_ms = (o.received_qpc_100ns as f64 - packet_end) / 10_000.0;
        if !(-MAX_ENVELOPE_SPREAD_MS..=MAX_LATE_PACKET_MS).contains(&late_ms) {
            let rejection = FitRejection {
                reason: if late_ms > MAX_LATE_PACKET_MS {
                    FitRejectionReason::PacketLate
                } else {
                    FitRejectionReason::PacketAhead
                },
                drift_ppm: (nominal / model.ticks_per_frame - 1.0) * 1_000_000.0,
                spread_ms: late_ms.abs().max(fit.spread_ms),
            };
            self.restart_learning_at(o);
            return rejection.estimate(o, true);
        }
        ClockEstimate {
            qpc_100ns: qpc.max(0.0).round().min(u64::MAX as f64) as u64,
            valid: true,
            mode: ClockMode::Recovered,
            offset_ms: (qpc - o.raw_qpc_100ns as f64) / 10_000.0,
            uncertainty_ms: fit.spread_ms,
            drift_ppm: (nominal / model.ticks_per_frame - 1.0) * 1_000_000.0,
            reason: "stable receiver clock; fixed transport delay unmeasured",
        }
    }
}

fn unknown(o: ClockObservation, mode: ClockMode, reason: &'static str) -> ClockEstimate {
    ClockEstimate {
        qpc_100ns: o.raw_qpc_100ns,
        valid: false,
        mode,
        offset_ms: 0.0,
        uncertainty_ms: 0.0,
        drift_ppm: 0.0,
        reason,
    }
}

fn fit_envelope(
    points: &VecDeque<EnvelopePoint>,
    frame_origin: u64,
    received_origin: u64,
    nominal: f64,
) -> Result<Fit, FitRejection> {
    // Exclude the current half-second bin: its minimum has not been observed yet.
    let points: Vec<_> = points.iter().take(points.len().saturating_sub(1)).collect();
    if points.len() < MIN_BINS {
        return Err(FitRejection {
            reason: FitRejectionReason::TooFewBins,
            drift_ppm: 0.0,
            spread_ms: 0.0,
        });
    }
    let xy: Vec<_> = points
        .iter()
        .map(|point| {
            (
                (point.frame_end - frame_origin) as f64,
                (i128::from(point.received) - i128::from(received_origin)) as f64,
            )
        })
        .collect();
    let mut slopes = Vec::new();
    for (index, &(x, y)) in xy.iter().enumerate() {
        for &(next_x, next_y) in &xy[index + 1..] {
            if (next_x - x) * nominal >= 2.0 * TICKS_PER_SECOND {
                slopes.push((next_y - y) / (next_x - x));
            }
        }
    }
    if slopes.is_empty() {
        return Err(FitRejection {
            reason: FitRejectionReason::TooFewSeparatedPoints,
            drift_ppm: 0.0,
            spread_ms: 0.0,
        });
    }
    slopes.sort_by(f64::total_cmp);
    let step = quantile(&slopes, 0.5);
    if !step.is_finite() || step <= 0.0 {
        return Err(FitRejection {
            reason: FitRejectionReason::InvalidSlope,
            drift_ppm: 0.0,
            spread_ms: 0.0,
        });
    }
    let mut residuals: Vec<_> = xy.iter().map(|&(x, y)| y - x * step).collect();
    residuals.sort_by(f64::total_cmp);
    let intercept = quantile(&residuals, 0.1);
    let spread_ms = (quantile(&residuals, 0.95) - quantile(&residuals, 0.05)) / 10_000.0;
    let drift_ppm = (nominal / step - 1.0) * 1_000_000.0;
    if !spread_ms.is_finite() {
        return Err(FitRejection {
            reason: FitRejectionReason::InvalidSpread,
            drift_ppm,
            spread_ms: 0.0,
        });
    }
    if drift_ppm.abs() > MAX_DRIFT_PPM {
        return Err(FitRejection {
            reason: FitRejectionReason::DriftOutsideBound,
            drift_ppm,
            spread_ms,
        });
    }
    if spread_ms > MAX_ENVELOPE_SPREAD_MS {
        return Err(FitRejection {
            reason: FitRejectionReason::SpreadOutsideBound,
            drift_ppm,
            spread_ms,
        });
    }
    Ok(Fit {
        frame_origin,
        received_origin,
        intercept,
        ticks_per_frame: step,
        spread_ms,
    })
}

fn quantile(sorted: &[f64], fraction: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * fraction).round() as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPOCH: u64 = 21_000_000_000_000;

    fn remote(packet: u64, ppm: f64) -> ClockObservation {
        let frame = packet * 480;
        let ticks_per_frame = TICKS_PER_SECOND / (48_000.0 * (1.0 + ppm / 1_000_000.0));
        let jitter = (packet % 10) * 5_000;
        ClockObservation {
            device_frame: frame,
            frames: 480,
            sample_rate: 48_000,
            raw_qpc_100ns: 100_000_000 + (frame as f64 * ticks_per_frame) as u64,
            received_qpc_100ns: EPOCH + ((frame + 480) as f64 * ticks_per_frame) as u64 + jitter,
            native_timestamp_valid: false,
            discontinuity: false,
        }
    }

    #[test]
    fn native_clock_is_preserved_without_training() {
        let mut clock = ClockRecovery::new();
        for packet in 0..1000 {
            let mut observation = remote(packet, 0.0);
            observation.raw_qpc_100ns = EPOCH + packet * 100_000;
            observation.native_timestamp_valid = true;
            let estimate = clock.observe(observation);
            assert!(estimate.valid);
            assert_eq!(estimate.mode, ClockMode::Native);
            assert_eq!(estimate.qpc_100ns, observation.raw_qpc_100ns);
        }
    }

    #[test]
    fn another_epoch_and_timestamp_error_recover_from_continuous_frames() {
        let mut clock = ClockRecovery::new();
        let mut previous = 0;
        let mut recovered = 0;
        for packet in 0..3000 {
            let observation = remote(packet, 0.0);
            let estimate = clock.observe(observation);
            if estimate.valid {
                assert_eq!(estimate.mode, ClockMode::Recovered);
                assert!(estimate.qpc_100ns > previous);
                assert!(estimate.qpc_100ns.abs_diff(EPOCH + packet * 100_000) < 100_000);
                assert!(estimate.uncertainty_ms <= MAX_ENVELOPE_SPREAD_MS);
                previous = estimate.qpc_100ns;
                recovered += 1;
            } else {
                assert!(
                    packet <= 550,
                    "recovery lost at packet {packet}: {estimate:?}"
                );
            }
        }
        assert!(recovered > 2400);
    }

    #[test]
    fn burst_delivery_uses_frame_clock_instead_of_packet_arrival_jitter() {
        let mut clock = ClockRecovery::new();
        let mut previous = None;
        for packet in 0..3000 {
            let mut observation = remote(packet, 0.0);
            observation.received_qpc_100ns = EPOCH + (packet / 10 + 1) * 1_000_000;
            let estimate = clock.observe(observation);
            if estimate.valid {
                if let Some(last) = previous {
                    assert!(estimate.qpc_100ns.abs_diff(last + 100_000) <= 2);
                }
                previous = Some(estimate.qpc_100ns);
            }
        }
        assert!(previous.is_some());
    }

    #[test]
    fn long_positive_and_negative_drift_stays_bounded() {
        for ppm in [-100.0, 100.0] {
            let mut clock = ClockRecovery::new();
            let mut last = None;
            for packet in 0..60_000 {
                let observation = remote(packet, ppm);
                let estimate = clock.observe(observation);
                if packet > 600 {
                    assert!(estimate.valid, "{ppm}: packet {packet}: {estimate:?}");
                    let expected =
                        EPOCH + (packet as f64 * 100_000.0 / (1.0 + ppm / 1_000_000.0)) as u64;
                    assert!(estimate.qpc_100ns.abs_diff(expected) < 100_000);
                    if let Some(previous) = last {
                        assert!(estimate.qpc_100ns > previous);
                    }
                    last = Some(estimate.qpc_100ns);
                }
            }
            assert!(last.is_some());
        }
    }

    #[test]
    fn gaps_resets_clock_regression_and_discontinuity_require_new_warmup() {
        for kind in 0..5 {
            let mut clock = ClockRecovery::new();
            for packet in 0..800 {
                clock.observe(remote(packet, 0.0));
            }
            let mut observation = remote(800, 0.0);
            match kind {
                0 => observation.device_frame += 480,
                1 => observation.device_frame = 0,
                2 => observation.received_qpc_100ns -= 20_000_000,
                3 => observation.received_qpc_100ns += 30_000_000,
                _ => observation.discontinuity = true,
            }
            assert!(!clock.observe(observation).valid);
            assert!(!clock.observe(remote(801, 0.0)).valid);
        }
    }

    #[test]
    fn native_clock_can_return_without_backwards_timestamp() {
        let mut clock = ClockRecovery::new();
        for packet in 0..800 {
            clock.observe(remote(packet, 0.0));
        }
        let mut previous = 0;
        let mut native = false;
        for packet in 800..2000 {
            let mut observation = remote(packet, 0.0);
            observation.raw_qpc_100ns = EPOCH + packet * 100_000;
            observation.native_timestamp_valid = true;
            let estimate = clock.observe(observation);
            assert!(estimate.valid);
            assert!(estimate.qpc_100ns > previous);
            previous = estimate.qpc_100ns;
            native |= estimate.mode == ClockMode::Native;
        }
        assert!(native);
    }

    #[test]
    fn plausible_epoch_but_seconds_wrong_is_not_accepted_as_native() {
        for offset in [-50_000_000i128, 90_000_000] {
            let mut clock = ClockRecovery::new();
            let mut recovered = false;
            for packet in 0..1000 {
                let mut observation = remote(packet, 0.0);
                observation.raw_qpc_100ns = (i128::from(EPOCH + packet * 100_000) + offset) as u64;
                observation.native_timestamp_valid = true;
                let estimate = clock.observe(observation);
                assert_ne!(estimate.mode, ClockMode::Native);
                if estimate.valid {
                    assert_eq!(estimate.mode, ClockMode::Recovered);
                    assert!(estimate.qpc_100ns.abs_diff(EPOCH + packet * 100_000) < 100_000);
                    recovered = true;
                }
            }
            assert!(recovered);
        }
    }

    #[test]
    fn native_clock_never_hides_the_current_device_gap_or_reset() {
        for reset in [false, true] {
            let mut clock = ClockRecovery::new();
            for packet in 0..2 {
                let mut observation = remote(packet, 0.0);
                observation.raw_qpc_100ns = EPOCH + packet * 100_000;
                observation.native_timestamp_valid = true;
                assert!(clock.observe(observation).valid);
            }
            let mut changed = remote(2, 0.0);
            changed.device_frame = if reset { 0 } else { 1440 };
            changed.raw_qpc_100ns = EPOCH + 200_000;
            changed.native_timestamp_valid = true;
            let estimate = clock.observe(changed);
            assert!(!estimate.valid);
            let mut next = changed;
            next.device_frame += 480;
            next.raw_qpc_100ns += 100_000;
            next.received_qpc_100ns += 100_000;
            assert!(clock.observe(next).valid);
        }
    }

    #[test]
    fn large_unstable_delivery_does_not_acquire_a_recovered_clock() {
        let mut clock = ClockRecovery::new();
        let mut unstable = false;
        for packet in 0..3000 {
            let mut observation = remote(packet, 0.0);
            // Monotonic reception in 700 ms batches; this cannot satisfy the
            // 40 ms lower-envelope spread and 200 ms accepted delivery bound.
            observation.received_qpc_100ns = EPOCH + (packet / 70 + 1) * 7_000_000;
            let estimate = clock.observe(observation);
            assert!(!estimate.valid);
            unstable |= estimate.mode == ClockMode::Unstable;
        }
        assert!(unstable);
    }

    #[test]
    fn calibrated_fast_clock_produces_complete_blocks_in_production_aligner() {
        use crate::{capture::CaptureEvent, sync_audio::Aligner};
        let mut clock = ClockRecovery::new();
        let mut aligner = Aligner::new(EPOCH, vec![1], 30.0).unwrap();
        let mut next = 0;
        let mut valid_after_training = 0;
        for packet in 0..2000 {
            let observation = remote(packet, 100.0);
            let estimate = clock.observe(observation);
            aligner
                .ingest(CaptureEvent::Packet {
                    track: 0,
                    sample_rate: observation.sample_rate,
                    channels: 1,
                    device_frame: observation.device_frame,
                    qpc_100ns: estimate.qpc_100ns,
                    samples: vec![0.25; observation.frames as usize],
                    discontinuity: false,
                    timestamp_valid: estimate.valid,
                })
                .unwrap();
            let elapsed = (observation.received_qpc_100ns - EPOCH) as f64 / TICKS_PER_SECOND;
            while elapsed >= next as f64 / 48_000.0 + 1.3 {
                let block = aligner.take(next, 48_000).unwrap();
                if next >= 10 * 48_000 && block.valid[0] {
                    valid_after_training += 1;
                }
                next += 48_000;
            }
        }
        assert!(
            valid_after_training >= 5,
            "calibrated clock never escaped provisional placement: {valid_after_training}"
        );
    }

    #[test]
    fn rejected_fit_reports_its_actual_drift_and_spread() {
        let nominal = TICKS_PER_SECOND / 48_000.0;
        for drift_case in [true, false] {
            let points: VecDeque<_> = (0..12)
                .map(|bin| {
                    let frame = (bin + 1) * 24_000;
                    let step = if drift_case { nominal / 0.99 } else { nominal };
                    let extra = if !drift_case && bin % 2 == 0 {
                        1_000_000
                    } else {
                        0
                    };
                    EnvelopePoint {
                        bin,
                        frame_end: frame,
                        received: EPOCH + (frame as f64 * step) as u64 + extra,
                    }
                })
                .collect();
            let rejection = fit_envelope(&points, 0, EPOCH, nominal).unwrap_err();
            let estimate = rejection.estimate(remote(700, 0.0), false);
            assert!(!estimate.valid);
            assert_eq!(estimate.mode, ClockMode::Unstable);
            if drift_case {
                assert!(estimate.reason.contains("drift"));
                assert!(estimate.drift_ppm.abs() > MAX_DRIFT_PPM);
                assert!((estimate.drift_ppm + 10_000.0).abs() < 0.1);
            } else {
                assert!(estimate.reason.contains("spread"));
                assert!(estimate.uncertainty_ms > MAX_ENVELOPE_SPREAD_MS);
                assert!((estimate.uncertainty_ms - 100.0).abs() < 0.1);
            }
        }
    }

    #[test]
    fn old_bad_learning_bins_cannot_veto_new_stable_audio_indefinitely() {
        let mut clock = ClockRecovery::new();
        let mut first_recovered = None;
        let mut restarted = false;
        let mut drift_rejected = false;
        for packet in 0..3000 {
            let mut observation = remote(packet, 0.0);
            // A transient 3% receive-clock envelope ramp for eight seconds,
            // followed by stable reception with the same fixed delay. Arrival
            // times stay monotonic; device frames remain continuous throughout.
            observation.received_qpc_100ns += packet.min(800) * 3_000;
            let estimate = clock.observe(observation);
            if estimate.mode == ClockMode::Unstable {
                restarted |= estimate.reason.contains("restarted learning");
                drift_rejected |= estimate.drift_ppm.abs() > MAX_DRIFT_PPM;
            }
            if estimate.valid {
                first_recovered.get_or_insert(packet);
            }
            if packet >= 1700 {
                assert!(
                    estimate.valid,
                    "stable input did not recover: {packet}, {estimate:?}"
                );
            }
        }
        assert!(drift_rejected, "the bad window must not be accepted");
        assert!(restarted, "old rejected bins must get a fresh full warmup");
        assert!(
            first_recovered.is_some_and(|packet| packet <= 1600),
            "stable data remained blocked by old bins: {first_recovered:?}"
        );
    }

    #[test]
    fn rejected_packet_reports_its_actual_lateness() {
        let mut clock = ClockRecovery::new();
        for packet in 0..800 {
            clock.observe(remote(packet, 0.0));
        }
        let mut delayed = remote(800, 0.0);
        delayed.received_qpc_100ns += 5_000_000;
        let estimate = clock.observe(delayed);
        assert!(!estimate.valid);
        assert_eq!(estimate.mode, ClockMode::Unstable);
        assert!(estimate.reason.contains("too late"));
        assert!(estimate.uncertainty_ms > 400.0);
        let mut after = remote(801, 0.0);
        after.received_qpc_100ns += 5_000_000;
        assert!(
            !clock.observe(after).valid,
            "a fresh full warmup is required"
        );
    }
}
