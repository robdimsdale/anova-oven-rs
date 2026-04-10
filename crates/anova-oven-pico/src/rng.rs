//! Simple seeded PRNG for the two `rand_core` versions in use.
//!
//! The RP2040 has no hardware TRNG (only a Ring Oscillator, which is not
//! cryptographically secure). For this dev scaffold we use a fixed seed.
//! A production build would combine the ROSC with additional entropy
//! sources (chip unique ID, ADC noise, timing jitter).

use rand_core_06;

/// SplitMix64-based PRNG implementing `rand_core` 0.6 traits.
///
/// Used by `embedded-tls` for the TLS handshake (via the WebSocket path).
pub struct Rng06 {
    state: u64,
}

impl Rng06 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn splitmix64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
}

impl rand_core_06::RngCore for Rng06 {
    fn next_u32(&mut self) -> u32 {
        self.splitmix64() as u32
    }

    fn next_u64(&mut self) -> u64 {
        self.splitmix64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        let mut i = 0;
        while i < dest.len() {
            let val = self.splitmix64();
            let bytes = val.to_le_bytes();
            let remaining = dest.len() - i;
            let to_copy = if remaining < 8 { remaining } else { 8 };
            dest[i..i + to_copy].copy_from_slice(&bytes[..to_copy]);
            i += to_copy;
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core_06::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl rand_core_06::CryptoRng for Rng06 {}
