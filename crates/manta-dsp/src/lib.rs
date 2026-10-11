//! Channel extraction and frequency estimation for manta.
//!
//! At M0 this crate holds the Kaiser prototype designer (SPEC §1.2 — NEW code,
//! coppa-dsp has no FIR designer), a single-channel extractor shim, and an
//! FFT-peak frequency estimator. `channelizer` is the M2 full N-channel WOLA
//! polyphase filterbank (SPEC §1.1-1.3) that supersedes `single`/`freqest`.
//! `refine` is decode-core-v2 §3's optional per-track narrowband refiner.
//! `carrier` is the narrowband carrier-frequency estimator behind frequency calibration.

pub mod carrier;
pub mod channelizer;
pub mod decimate;
pub mod floor;
pub mod freqest;
pub mod hilbert;
pub mod proto;
pub mod refine;
pub mod single;
