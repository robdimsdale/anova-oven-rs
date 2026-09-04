//! Integrating debouncer for the rotary encoder's push button.
//!
//! The button shares a connector (and, on a breadboard, a wire loom) with the
//! encoder's A/B lines, and the RP2040's internal pull-up is a weak ~50-80 kΩ.
//! Turning the shaft therefore couples short negative glitches onto the switch
//! line — some sub-millisecond, some tens of milliseconds — which an
//! edge-triggered reader happily reports as button presses. Sampling the line
//! and integrating those samples rejects both: a press only counts once the
//! line has been *predominantly* low for [`Debouncer::PRESS_MS`].
//!
//! This is Kenneth Kuhn's integrator: low samples charge a counter, high
//! samples discharge it, and only the saturated ends of the range change the
//! reported state. Unlike a "N consecutive identical samples" filter it
//! survives contact chatter (a stray high mid-press costs a little credit
//! instead of restarting the count), and unlike an edge reader it cannot miss
//! a transition it never sampled.
//!
//! The integrator is charged in **milliseconds of elapsed time, not in sample
//! counts**. The firmware runs a single-threaded executor whose display task
//! flushes SPI *blocking*, so the sampling loop's timer can in principle be
//! starved for several periods at a stretch; counting samples would then
//! under-count a real press by exactly the stalled time and reject it. Field
//! measurement says the loop is in fact sampling on time (~5.3-5.9 ms against
//! a 5 ms nominal, back when the nominal was 5 ms), so this is insurance
//! rather than a fix for an observed bug — but it costs nothing and it removes
//! a whole class of timing dependency from the filter.
//!
//! The bin drives this from `input.rs`: it blocks on a level-triggered
//! `wait_for_low()` while the line is idle, then samples every
//! [`Debouncer::SAMPLE_INTERVAL_MS`] until the verdict says to stop.

/// What the caller should do after feeding one sample.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ButtonVerdict {
    /// Nothing decided yet — keep sampling.
    Idle,
    /// A press is now confirmed. Emit the input event exactly once; keep
    /// sampling to find the release.
    Pressed,
    /// The confirmed press has ended. Stop sampling and re-arm.
    Released,
    /// The line went low but never stayed low long enough to be a press —
    /// electrical noise. Stop sampling; emit nothing.
    Glitch,
}

/// Integrating debouncer. One instance per low-going excursion: construct it
/// when the line first reads low, feed it samples, drop it on `Released` or
/// `Glitch`.
pub struct Debouncer {
    /// Accumulated net-low evidence in milliseconds, clamped to `0..=PRESS_MS`.
    charge_ms: u32,
    /// Whether a press has been reported for this excursion.
    pressed: bool,
    /// Diagnostics for the bin's logs — see [`Debouncer::samples`],
    /// [`Debouncer::peak_charge_ms`] and [`Debouncer::contact_ms`].
    samples: u32,
    peak_charge_ms: u32,
    elapsed_ms: u32,
    last_low_ms: u32,
}

impl Default for Debouncer {
    fn default() -> Self {
        Self::new()
    }
}

impl Debouncer {
    /// Nominal sampling period. The loop only runs while the line is low, so
    /// this costs nothing at idle; 2 ms buys six samples of evidence inside
    /// [`PRESS_MS`], which is what lets the threshold sit as low as it does.
    /// The debouncer does not assume samples actually arrive this often.
    ///
    /// [`PRESS_MS`]: Debouncer::PRESS_MS
    pub const SAMPLE_INTERVAL_MS: u64 = 2;

    /// Net-low evidence required to call it a press.
    ///
    /// Calibrated against measured contact times, not guessed: deliberate
    /// presses ran 100-190 ms, but quick taps came in at 20-35 ms, and an
    /// earlier 50 ms threshold rejected every one of those as noise. 12 ms
    /// catches a 20 ms tap with 8 ms of margin while still needing six
    /// consecutive low samples.
    ///
    /// This is only safe because the switch line has an RC filter on it (100 nF
    /// to ground at the pin). Without it, coupled noise from the encoder's A/B
    /// lines reached ~35 ms and the threshold had to clear that instead — see
    /// §1.6 of `docs/pico-review.md`. Do not lower the threshold on a board
    /// with an unfiltered switch line.
    pub const PRESS_MS: u32 = 12;

    /// Ceiling on what one sample may contribute, however late it arrives.
    /// Half the threshold, so **no single sample can confirm a press** — a
    /// stall still contributes its full weight, but it takes two low samples
    /// to cross the line, which keeps one badly-timed sample landing inside a
    /// noise dip from being enough on its own.
    const MAX_STEP_MS: u32 = Self::PRESS_MS / 2;

    pub fn new() -> Self {
        Self {
            charge_ms: 0,
            pressed: false,
            samples: 0,
            peak_charge_ms: 0,
            elapsed_ms: 0,
            last_low_ms: 0,
        }
    }

    /// Feed one sample. `is_low` is the raw pin read (the switch pulls to
    /// ground, so low = contact closed); `elapsed_ms` is the time since the
    /// previous sample, which the caller measures rather than assumes.
    ///
    /// A sample stands for the whole interval preceding it, so a late sample
    /// carries proportionally more weight — that is what makes a press survive
    /// an executor stall. Its contribution is capped at [`MAX_STEP_MS`].
    ///
    /// [`MAX_STEP_MS`]: Debouncer::MAX_STEP_MS
    pub fn sample(&mut self, is_low: bool, elapsed_ms: u32) -> ButtonVerdict {
        // The integrator sees the capped step; the wall-clock diagnostics see
        // the real interval, so a stall is visible in the log rather than
        // hidden by the cap.
        let step = elapsed_ms.min(Self::MAX_STEP_MS);
        self.samples += 1;
        self.elapsed_ms += elapsed_ms;
        if is_low {
            self.last_low_ms = self.elapsed_ms;
        }

        if is_low {
            self.charge_ms = (self.charge_ms + step).min(Self::PRESS_MS);
            self.peak_charge_ms = self.peak_charge_ms.max(self.charge_ms);
        } else {
            self.charge_ms = self.charge_ms.saturating_sub(step);
        }

        if !self.pressed && self.charge_ms >= Self::PRESS_MS {
            self.pressed = true;
            return ButtonVerdict::Pressed;
        }

        if self.charge_ms == 0 {
            // Fully discharged: either a clean release, or a low excursion
            // that never earned a press.
            return if self.pressed {
                ButtonVerdict::Released
            } else {
                ButtonVerdict::Glitch
            };
        }

        ButtonVerdict::Idle
    }

    /// Whether a press has been reported for this excursion. Exposed so the
    /// caller can log press duration without tracking the flag itself.
    pub fn is_pressed(&self) -> bool {
        self.pressed
    }

    /// How long the contact was actually closed, in ms: the elapsed time at
    /// the last sample that read low. This is the number to log, *not* the
    /// wall-clock excursion — the integrator has to discharge fully before it
    /// reports `Released` or `Glitch`, so the excursion runs to roughly twice
    /// the contact time and reading it as a press duration is misleading.
    pub fn contact_ms(&self) -> u32 {
        self.last_low_ms
    }

    /// Samples fed so far. Logged on rejection: comparing this against
    /// `excursion_ms / SAMPLE_INTERVAL_MS` shows whether the sampling loop was
    /// starved (far fewer samples than the elapsed time allows) or whether the
    /// line was genuinely chattering.
    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// High-water mark of the integrator, in ms. Logged on rejection: a peak
    /// that stalled just short of [`PRESS_MS`] means a marginal press, one
    /// that never got past a few ms means true noise.
    ///
    /// [`PRESS_MS`]: Debouncer::PRESS_MS
    pub fn peak_charge_ms(&self) -> u32 {
        self.peak_charge_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: u32 = Debouncer::SAMPLE_INTERVAL_MS as u32;

    /// Feed `n` samples of the same level at the nominal cadence, returning
    /// every non-idle verdict.
    fn feed(d: &mut Debouncer, is_low: bool, n: usize) -> alloc::vec::Vec<ButtonVerdict> {
        let mut out = alloc::vec::Vec::new();
        for _ in 0..n {
            match d.sample(is_low, TICK) {
                ButtonVerdict::Idle => {}
                v => out.push(v),
            }
        }
        out
    }

    /// Samples needed to cross the threshold at the nominal cadence.
    const TICKS_TO_PRESS: usize = (Debouncer::PRESS_MS / TICK) as usize;

    #[test]
    fn sustained_low_confirms_a_press_exactly_once() {
        let mut d = Debouncer::new();
        // Well past the threshold: still one Pressed, no repeats.
        let verdicts = feed(&mut d, true, TICKS_TO_PRESS * 3);
        assert_eq!(verdicts, alloc::vec![ButtonVerdict::Pressed]);
        assert!(d.is_pressed());
    }

    #[test]
    fn press_is_confirmed_on_the_threshold_sample_not_before() {
        let mut d = Debouncer::new();
        for _ in 0..TICKS_TO_PRESS - 1 {
            assert_eq!(d.sample(true, TICK), ButtonVerdict::Idle);
        }
        assert_eq!(d.sample(true, TICK), ButtonVerdict::Pressed);
    }

    #[test]
    fn single_sample_glitch_is_rejected() {
        // The 106 µs excursion from the field logs: one low sample at most,
        // then the line is back high.
        let mut d = Debouncer::new();
        assert_eq!(d.sample(true, TICK), ButtonVerdict::Idle);
        assert_eq!(d.sample(false, TICK), ButtonVerdict::Glitch);
        assert!(!d.is_pressed());
    }

    #[test]
    fn sub_threshold_excursion_is_rejected() {
        // Low for less than PRESS_MS, then back high: never confirmed. (The
        // caller stops at the first non-idle verdict, so only that one
        // matters.)
        let mut d = Debouncer::new();
        assert!(feed(&mut d, true, TICKS_TO_PRESS - 1).is_empty());
        let verdicts = feed(&mut d, false, TICKS_TO_PRESS);
        assert_eq!(verdicts[0], ButtonVerdict::Glitch);
        assert!(!d.is_pressed());
    }

    #[test]
    fn a_twenty_millisecond_tap_is_confirmed() {
        // Regression for the field bug: quick taps measured 20-35 ms of
        // contact and were all rejected by the earlier 50 ms threshold.
        let mut d = Debouncer::new();
        let mut verdicts = alloc::vec::Vec::new();
        for _ in 0..(20 / TICK) {
            verdicts.push(d.sample(true, TICK));
        }
        assert!(verdicts.contains(&ButtonVerdict::Pressed));
        assert_eq!(d.contact_ms(), 20);
    }

    #[test]
    fn contact_time_excludes_the_discharge_ramp() {
        // The excursion runs to roughly twice the contact time; `contact_ms`
        // must report the contact, which is what the logs need.
        let mut d = Debouncer::new();
        feed(&mut d, true, TICKS_TO_PRESS);
        let contact = d.contact_ms();
        feed(&mut d, false, TICKS_TO_PRESS);
        assert_eq!(d.contact_ms(), contact, "highs must not extend contact");
        assert_eq!(contact, Debouncer::PRESS_MS);
    }

    #[test]
    fn a_press_survives_a_blocking_display_flush() {
        // The display task flushes SPI blocking on the same executor, so a
        // sample could land many periods late. A sample-counting integrator
        // would score the stalled time as no evidence at all and reject a real
        // press; crediting the elapsed time confirms it.
        let mut d = Debouncer::new();
        // Three samples at the nominal cadence, then the executor stalls and
        // the next one lands 40 ms late with the button still down.
        assert!(feed(&mut d, true, 3).is_empty());
        assert_eq!(d.sample(true, 40), ButtonVerdict::Pressed);
        assert_eq!(d.samples(), 4, "four samples covered the whole press");
    }

    #[test]
    fn a_stall_does_not_manufacture_a_press_from_a_released_line() {
        // The other side of the same coin: if the line reads high after the
        // stall, the elapsed time discharges rather than charges.
        let mut d = Debouncer::new();
        assert!(feed(&mut d, true, 3).is_empty());
        assert_eq!(d.sample(false, 40), ButtonVerdict::Glitch);
        assert!(!d.is_pressed());
    }

    #[test]
    fn no_single_sample_can_confirm_a_press() {
        // However long the gap, one low sample is never enough on its own —
        // MAX_STEP_MS caps it at half the threshold.
        let mut d = Debouncer::new();
        assert_eq!(d.sample(true, 5_000), ButtonVerdict::Idle);
        assert_eq!(d.sample(true, 5_000), ButtonVerdict::Pressed);
    }

    #[test]
    fn contact_chatter_during_a_press_still_confirms() {
        // Alternating samples never reach the threshold on their own, but a
        // press that chatters once early still lands: each high costs a tick
        // of credit rather than resetting the count.
        let mut d = Debouncer::new();
        assert!(feed(&mut d, true, 3).is_empty());
        assert_eq!(d.sample(false, TICK), ButtonVerdict::Idle); // chatter, not a glitch
        let verdicts = feed(&mut d, true, TICKS_TO_PRESS);
        assert_eq!(verdicts, alloc::vec![ButtonVerdict::Pressed]);
    }

    #[test]
    fn release_is_reported_after_the_integrator_discharges() {
        let mut d = Debouncer::new();
        assert_eq!(
            feed(&mut d, true, TICKS_TO_PRESS),
            alloc::vec![ButtonVerdict::Pressed]
        );
        // Discharging takes as much high time as the press took low time.
        for _ in 0..TICKS_TO_PRESS - 1 {
            assert_eq!(d.sample(false, TICK), ButtonVerdict::Idle);
        }
        assert_eq!(d.sample(false, TICK), ButtonVerdict::Released);
    }

    #[test]
    fn a_held_button_never_reports_a_second_press() {
        let mut d = Debouncer::new();
        feed(&mut d, true, TICKS_TO_PRESS);
        // Hold for ~5 s at the nominal tick.
        assert!(feed(&mut d, true, 1000).is_empty());
    }

    #[test]
    fn brief_release_chatter_does_not_end_a_press_early() {
        let mut d = Debouncer::new();
        feed(&mut d, true, TICKS_TO_PRESS);
        // A few high samples mid-press discharge partially, then recharge.
        assert!(feed(&mut d, false, 3).is_empty());
        assert!(feed(&mut d, true, 3).is_empty());
        assert!(d.is_pressed());
    }

    #[test]
    fn diagnostics_distinguish_a_short_press_from_chatter() {
        // A clean tap just under the threshold: charge climbs monotonically,
        // so the peak lands at the contact time and the excursion runs to
        // about twice it. This is the signature that identified the field
        // bug — every rejection had peak ~= elapsed / 2.
        let mut tap = Debouncer::new();
        feed(&mut tap, true, TICKS_TO_PRESS - 1);
        let contact = tap.contact_ms();
        feed(&mut tap, false, TICKS_TO_PRESS);
        assert_eq!(tap.peak_charge_ms(), contact);
        assert_eq!(tap.peak_charge_ms(), Debouncer::PRESS_MS - TICK);

        // Chattering: the peak never gets near the contact time, because
        // every high gives back what the preceding low earned.
        let mut chatter = Debouncer::new();
        for _ in 0..8 {
            chatter.sample(true, TICK);
            chatter.sample(false, TICK);
        }
        assert_eq!(chatter.samples(), 16);
        assert_eq!(chatter.peak_charge_ms(), TICK);
    }
}
