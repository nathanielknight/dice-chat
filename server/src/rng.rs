//! Per-room persisted RNG (SPEC.md §7).
//!
//! Each room stores an opaque 32-byte state. A roll deserializes the state,
//! draws randomness, and the successor state is written back in the same
//! transaction as the message, serializing concurrent rolls.
//!
//! The generator is xoshiro256** — tiny, fast, and its state is exactly the
//! 32 bytes we persist. No audit/replay guarantees are made.

pub const STATE_LEN: usize = 32;

pub struct RoomRng {
    s: [u64; 4],
}

impl RoomRng {
    /// A fresh state from OS randomness, for room creation.
    pub fn fresh_state() -> [u8; STATE_LEN] {
        let mut state = [0u8; STATE_LEN];
        loop {
            getrandom::fill(&mut state).expect("OS randomness unavailable");
            // xoshiro's all-zero state is a fixed point; astronomically
            // unlikely, but cheap to rule out.
            if state.iter().any(|&b| b != 0) {
                return state;
            }
        }
    }

    /// Deserialize a persisted state. Returns `None` for corrupt state
    /// (wrong length or all-zero).
    pub fn from_state(state: &[u8]) -> Option<Self> {
        let bytes: &[u8; STATE_LEN] = state.try_into().ok()?;
        if bytes.iter().all(|&b| b == 0) {
            return None;
        }
        let mut s = [0u64; 4];
        for (i, chunk) in bytes.chunks_exact(8).enumerate() {
            s[i] = u64::from_le_bytes(chunk.try_into().unwrap());
        }
        Some(RoomRng { s })
    }

    pub fn state(&self) -> [u8; STATE_LEN] {
        let mut out = [0u8; STATE_LEN];
        for (i, word) in self.s.iter().enumerate() {
            out[i * 8..(i + 1) * 8].copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    fn next_u64(&mut self) -> u64 {
        // xoshiro256** by Blackman & Vigna (public domain).
        let result = self.s[1]
            .wrapping_mul(5)
            .rotate_left(7)
            .wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }
}

impl dice::Roller for RoomRng {
    fn roll(&mut self, sides: u32) -> u32 {
        // Unbiased via rejection sampling over the top of the u64 range.
        let sides = sides as u64;
        let zone = u64::MAX - (u64::MAX % sides);
        loop {
            let v = self.next_u64();
            if v < zone {
                return (v % sides) as u32 + 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dice::Roller;

    #[test]
    fn state_round_trips() {
        let state = RoomRng::fresh_state();
        let mut a = RoomRng::from_state(&state).unwrap();
        let mut b = RoomRng::from_state(&state).unwrap();
        let faces_a: Vec<u32> = (0..100).map(|_| a.roll(20)).collect();
        let faces_b: Vec<u32> = (0..100).map(|_| b.roll(20)).collect();
        assert_eq!(faces_a, faces_b);
        // Successor states also match and differ from the original.
        assert_eq!(a.state(), b.state());
        assert_ne!(a.state(), state);
    }

    #[test]
    fn faces_in_range_and_all_hit() {
        let mut rng = RoomRng::from_state(&RoomRng::fresh_state()).unwrap();
        let mut seen = [false; 6];
        for _ in 0..10_000 {
            let f = rng.roll(6);
            assert!((1..=6).contains(&f));
            seen[f as usize - 1] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn rejects_corrupt_state() {
        assert!(RoomRng::from_state(&[0u8; 32]).is_none());
        assert!(RoomRng::from_state(&[1u8; 31]).is_none());
        assert!(RoomRng::from_state(&[]).is_none());
    }
}
