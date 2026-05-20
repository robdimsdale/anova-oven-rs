//! Pure quadrature decoder for the rotary encoder. The embassy GPIO wait
//! + debounce + channel send stays in the bin; this module owns the
//! state machine (QEM lookup + accumulator + direction-reversal reset).

/// One direction-tick at a detent boundary. Maps to `InputEvent::EncoderCW`
/// / `EncoderCCW` at the bin's call site.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EncoderTick {
    Cw,
    Ccw,
}

/// Standard quadrature decoder table indexed by `(prev_ab << 2) | curr_ab`,
/// where `ab` is `(a_low << 1) | b_low`. Values are +1 (CW), -1 (CCW), or
/// 0 for "no change" / "illegal transition" (both bits flipped at once —
/// indicates a missed sample, conservatively dropped).
const QEM: [i8; 16] = [0, -1, 1, 0, 1, 0, 0, -1, -1, 0, 0, 1, 0, 1, -1, 0];

/// Each detent of the physical encoder corresponds to 4 valid AB
/// transitions in the same direction. Emit a tick only after a full
/// detent's worth has accumulated, so half-clicks don't fire.
const TRANSITIONS_PER_DETENT: i8 = 4;

/// State for the quadrature decoder. Fed sampled (a_low, b_low) pin values
/// via [`update`], emits at most one [`EncoderTick`] per call (once per
/// detent).
///
/// [`update`]: QuadratureDecoder::update
pub struct QuadratureDecoder {
    prev: u8,
    accum: i8,
}

impl QuadratureDecoder {
    /// Construct from the initial sampled state of the A/B pins. Both
    /// arguments are "is_low" — i.e. `true` when the pin reads low.
    pub fn new(a_low: bool, b_low: bool) -> Self {
        Self {
            prev: encode(a_low, b_low),
            accum: 0,
        }
    }

    /// Feed a freshly-sampled (a_low, b_low) state. Returns:
    /// - `Some(Cw)` / `Some(Ccw)` when a full detent has accumulated in
    ///   that direction;
    /// - `None` mid-detent, for an illegal transition, or for "no change".
    ///
    /// A direction reversal mid-detent resets the accumulator so partial
    /// rotations don't carry credit across reversals (the physical click
    /// is the user's intent boundary).
    pub fn update(&mut self, a_low: bool, b_low: bool) -> Option<EncoderTick> {
        let curr = encode(a_low, b_low);
        let dir = QEM[((self.prev << 2) | curr) as usize];
        self.prev = curr;

        if dir == 0 {
            return None;
        }

        // Direction reversal: drop the partial accumulation rather than
        // letting bounce noise emit a tick of the wrong sign.
        if (self.accum > 0 && dir < 0) || (self.accum < 0 && dir > 0) {
            self.accum = 0;
        }

        self.accum += dir;

        if self.accum >= TRANSITIONS_PER_DETENT {
            self.accum = 0;
            Some(EncoderTick::Cw)
        } else if self.accum <= -TRANSITIONS_PER_DETENT {
            self.accum = 0;
            Some(EncoderTick::Ccw)
        } else {
            None
        }
    }
}

/// Pack the two pin samples into a 2-bit code matching QEM's index encoding.
/// Both args are "is_low" — the bin already maps GPIO reads through
/// `pin.is_low()` before calling `update`.
fn encode(a_low: bool, b_low: bool) -> u8 {
    ((a_low as u8) << 1) | (b_low as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CW Gray-code sequence starting from (a=high, b=high) = 0b00:
    /// 00 → 10 → 11 → 01 → 00. Each step is +1 in the QEM table.
    const CW_SEQUENCE: [(bool, bool); 5] = [
        (false, false), // 00
        (true, false),  // 10
        (true, true),   // 11
        (false, true),  // 01
        (false, false), // 00
    ];

    /// CCW: 00 → 01 → 11 → 10 → 00. Each step is -1.
    const CCW_SEQUENCE: [(bool, bool); 5] = [
        (false, false),
        (false, true),
        (true, true),
        (true, false),
        (false, false),
    ];

    fn feed(d: &mut QuadratureDecoder, seq: &[(bool, bool)]) -> alloc::vec::Vec<EncoderTick> {
        let mut ticks = alloc::vec::Vec::new();
        for &(a, b) in seq {
            if let Some(t) = d.update(a, b) {
                ticks.push(t);
            }
        }
        ticks
    }

    #[test]
    fn one_full_cw_detent_emits_one_cw_tick() {
        let mut d = QuadratureDecoder::new(false, false);
        // Skip the initial (0,0) since it matches `prev` already (no-op).
        let ticks = feed(&mut d, &CW_SEQUENCE[1..]);
        assert_eq!(ticks, alloc::vec![EncoderTick::Cw]);
    }

    #[test]
    fn one_full_ccw_detent_emits_one_ccw_tick() {
        let mut d = QuadratureDecoder::new(false, false);
        let ticks = feed(&mut d, &CCW_SEQUENCE[1..]);
        assert_eq!(ticks, alloc::vec![EncoderTick::Ccw]);
    }

    #[test]
    fn three_consecutive_cw_detents_emit_three_ticks() {
        let mut d = QuadratureDecoder::new(false, false);
        let mut all = alloc::vec::Vec::new();
        for _ in 0..3 {
            all.extend(feed(&mut d, &CW_SEQUENCE[1..]));
        }
        assert_eq!(
            all,
            alloc::vec![EncoderTick::Cw, EncoderTick::Cw, EncoderTick::Cw]
        );
    }

    #[test]
    fn partial_cw_then_reversal_resets_and_does_not_carry_credit() {
        // 2 CW transitions (accum=+2), then 2 CCW: reversal resets accum
        // to 0 on the first CCW step, then accum becomes -2. Net: no tick.
        let mut d = QuadratureDecoder::new(false, false);
        let ticks_partial = feed(&mut d, &CW_SEQUENCE[1..3]); // 00→10, 10→11
        assert!(ticks_partial.is_empty());
        // Now reverse: 11→10, 10→00 (CCW direction in QEM)
        let ticks_reverse = feed(&mut d, &[(true, false), (false, false)]);
        assert!(ticks_reverse.is_empty(), "reversal mid-detent must not emit");
    }

    #[test]
    fn partial_cw_then_full_ccw_emits_one_ccw() {
        // 2 CW steps land us at state 11 with accum=+2. The CCW detent
        // *from state 11* is 11→10→00→01→11, not the canonical 00-starting
        // CCW_SEQUENCE. The first CCW step triggers the reversal reset
        // (accum goes +2 → 0 → -1), then three more accumulate to -4 and
        // emit one Ccw tick at the detent boundary.
        let mut d = QuadratureDecoder::new(false, false);
        feed(&mut d, &CW_SEQUENCE[1..3]); // 00→10→11, accum = +2 (no tick)
        let ticks = feed(
            &mut d,
            &[
                (true, false),  // 11 → 10 (reset, accum = -1)
                (false, false), // 10 → 00 (accum = -2)
                (false, true),  // 00 → 01 (accum = -3)
                (true, true),   // 01 → 11 (accum = -4 → emit Ccw)
            ],
        );
        assert_eq!(
            ticks,
            alloc::vec![EncoderTick::Ccw],
            "post-reversal direction should emit one tick at the next detent boundary"
        );
    }

    #[test]
    fn same_state_twice_is_no_op() {
        let mut d = QuadratureDecoder::new(false, false);
        for _ in 0..10 {
            assert_eq!(d.update(false, false), None);
        }
    }

    #[test]
    fn illegal_transition_returns_none_and_does_not_advance_accum() {
        // 00 → 11 is a two-bit change (both A and B flipped between
        // samples) — QEM[3] = 0. Treated as "missed sample", dropped.
        let mut d = QuadratureDecoder::new(false, false);
        assert_eq!(d.update(true, true), None);
        // Same for the other diagonal: 01 → 10 (QEM[6] = 0).
        let mut d2 = QuadratureDecoder::new(false, true);
        assert_eq!(d2.update(true, false), None);
    }

    #[test]
    fn illegal_transition_does_not_emit_tick_even_at_threshold() {
        // Build accum to +3 with CW steps, then feed an illegal transition.
        // The illegal step should not change accum or emit a tick.
        let mut d = QuadratureDecoder::new(false, false);
        feed(&mut d, &CW_SEQUENCE[1..4]); // accum = +3
        // From state 01 (last CW state before final 00), 01 → 10 is illegal.
        assert_eq!(d.update(true, false), None);
    }

    #[test]
    fn detents_emit_independently_after_partial_and_reset() {
        // Drive a full CW detent, then a full CCW detent (no reversal
        // mid-detent — accum reset to 0 by the tick itself).
        let mut d = QuadratureDecoder::new(false, false);
        let cw_ticks = feed(&mut d, &CW_SEQUENCE[1..]);
        let ccw_ticks = feed(&mut d, &CCW_SEQUENCE[1..]);
        assert_eq!(cw_ticks, alloc::vec![EncoderTick::Cw]);
        assert_eq!(ccw_ticks, alloc::vec![EncoderTick::Ccw]);
    }

    #[test]
    fn ctor_seeds_prev_from_initial_state() {
        // Start from (a=high, b=low) = 0b10. The first valid CW transition
        // from there is 10 → 11. Without proper ctor seeding, the decoder
        // would interpret it as starting from prev=00 → 11, an illegal
        // transition (QEM[3]=0) and never advance.
        let mut d = QuadratureDecoder::new(true, false);
        // Feed 4 CW transitions starting from 10: 10→11→01→00→10.
        let ticks = feed(
            &mut d,
            &[(true, true), (false, true), (false, false), (true, false)],
        );
        assert_eq!(ticks, alloc::vec![EncoderTick::Cw]);
    }
}
