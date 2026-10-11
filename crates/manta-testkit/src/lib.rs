//! Synthetic CW generator and golden-vector harness (SPEC-decode-core §7).
//!
//! Determinism: all randomness is ChaCha8 seeded per fixture, consumed only
//! via `next_u64()` with hand-rolled conversions (pinned decision 2), so
//! fixtures are bit-stable across dependency upgrades and platforms.

mod callsigns;
pub mod cer;
pub mod keyer;
pub mod noise;
pub mod oracle;
pub mod scene;
pub mod sensitivity;
pub mod vectors;
pub mod wav;

/// Short V1 variant for fast integration/determinism tests. Same code path
/// as the full 120 s V1 gate. SPEC §7.
pub fn vectorspec_short() -> vectors::VectorSpec {
    vectors::VectorSpec {
        duration_s: 20.0,
        ..vectors::v1()
    }
}

pub(crate) fn u01(rng: &mut rand_chacha::ChaCha8Rng) -> f64 {
    use rand_core::Rng;
    // 53-bit mantissa, strictly in (0, 1): never 0 (ln-safe), never 1.
    ((rng.next_u64() >> 11) as f64 + 0.5) * (1.0 / 9007199254740992.0)
}

pub(crate) fn gaussian_pair(rng: &mut rand_chacha::ChaCha8Rng) -> (f64, f64) {
    let u1 = u01(rng);
    let u2 = u01(rng);
    let r = (-2.0 * u1.ln()).sqrt();
    let th = std::f64::consts::TAU * u2;
    (r * th.cos(), r * th.sin())
}
