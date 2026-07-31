//! Development-mode world generation diagnostics.
//!
//! World generation runs on the rayon pool for every chunk, so these helpers are
//! built to cost nothing when `logging.development = false`: every entry point
//! takes plain `Copy` scalars, checks [`pumpkin_config::development_mode`] before
//! anything else and returns before any string formatting happens. Call sites
//! inside loops hoist [`enabled`] into a local instead of re-reading the flag on
//! every iteration.
//!
//! The high-volume sites (chunk-edge biome fallback, and every reference-sweep
//! site: [`structure_lazy_accepted`], [`structure_lazy_declined`],
//! [`structure_reference_missing_start`]) are additionally rate limited via
//! [`sampled`]: only the first hit and every Nth hit after it reach the log, so a
//! busy generation pool cannot flood the console. Each of them carries its own
//! running counter in the message, so the real volume stays readable.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use pumpkin_data::structures::{StructureKeys, WeightedEntry};
use pumpkin_util::math::position::BlockPos;
use tracing::{debug, info};

use crate::generation::structure::placement::PlacementVerdict;

/// A generation stage slower than this is reported with its chunk position.
pub const SLOW_STAGE_MS: u128 = 80;
/// A single feature/structure step (0..11) slower than this is reported.
pub const SLOW_FEATURE_STEP_MS: u128 = 40;

/// Rate limit for the reference sweep: a structure start is recomputed for every
/// chunk within 8 chunks of it, so an unfiltered line per candidate would mean
/// ~289 identical lines per structure.
const REFERENCE_SAMPLE_STRIDE: u64 = 32;
/// Rate limit for the chunk-edge biome fallback, which can fire per block column.
const QUART_SAMPLE_STRIDE: u64 = 512;

static REFERENCE_MISSES: AtomicU64 = AtomicU64::new(0);
static QUART_CLAMPS: AtomicU64 = AtomicU64::new(0);
static LAZY_ACCEPTS: AtomicU64 = AtomicU64::new(0);
static LAZY_DECLINES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Mineshaft piece-tree truncations, per generating thread. Mineshaft
    /// expansion is a single synchronous call, so a thread-local counter avoids
    /// threading a diagnostics argument through the whole recursive port.
    static MINESHAFT_DEPTH_STOPS: Cell<u32> = const { Cell::new(0) };
    static MINESHAFT_RANGE_STOPS: Cell<u32> = const { Cell::new(0) };
    static MINESHAFT_COLLISION_STOPS: Cell<u32> = const { Cell::new(0) };
}

/// Whether development diagnostics should be emitted.
///
/// Hoist this into a local before entering a loop; the individual report
/// functions check it again so single call sites can just call them directly.
#[inline]
#[must_use]
pub fn enabled() -> bool {
    pumpkin_config::development_mode()
}

/// Returns the running hit count when this hit should be reported.
fn sampled(counter: &AtomicU64, stride: u64) -> Option<u64> {
    let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
    (n == 1 || n.is_multiple_of(stride)).then_some(n)
}

/// Why a structure that reached its generator produced no start.
#[derive(Clone, Copy, Debug)]
pub enum StructureReject {
    /// The generator itself found no valid position (height limits, jigsaw
    /// expansion failure, piece collisions, ...).
    NoPosition,
    /// A position was found, but the biome there is not in the structure's
    /// biome tag.
    Biome {
        /// Biome id actually present at the start position.
        biome_id: u16,
    },
    /// The structure's biome tag could not be resolved at all — the structure
    /// can never generate. Only reachable on the reference path; the start path
    /// panics instead.
    MissingBiomeTag,
}

/// Names every candidate of a structure set, so a set with several entries is
/// not reported under whichever one happens to be first.
fn describe_set(entries: &[WeightedEntry]) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        // Writing into a `String` cannot fail.
        let _ = write!(out, "{:?}", entry.structure);
    }
    out
}

/// A structure set was skipped by the placement gate.
///
/// Only frequency-reduction rejections are logged: `NotStartChunk` is the
/// expected outcome for all but one chunk per `spacing²` region, so reporting it
/// would drown out everything else.
///
/// The gate applies to the whole set, so all candidates are named rather than
/// just the first entry.
pub fn structure_placement_rejected(
    entries: &[WeightedEntry],
    chunk_x: i32,
    chunk_z: i32,
    verdict: PlacementVerdict,
) {
    if !enabled() {
        return;
    }
    if matches!(verdict, PlacementVerdict::FrequencyReduced) {
        debug!(
            "worldgen/structure: set [{}] is the placement chunk for ({chunk_x}, {chunk_z}) but lost the frequency roll",
            describe_set(entries)
        );
    }
}

/// Every weighted candidate of a structure set failed in this chunk.
pub fn structure_set_exhausted(entries: &[WeightedEntry], chunk_x: i32, chunk_z: i32) {
    if !enabled() {
        return;
    }
    info!(
        "worldgen/structure: set [{}] placed nothing in chunk ({chunk_x}, {chunk_z}): all {} candidates rejected",
        describe_set(entries),
        entries.len()
    );
}

/// A structure start was accepted and stored in this chunk.
pub fn structure_start_accepted(key: StructureKeys, chunk_x: i32, chunk_z: i32, start: BlockPos) {
    if !enabled() {
        return;
    }
    let pos = start.0;
    info!(
        "worldgen/structure: {key:?} start placed in chunk ({chunk_x}, {chunk_z}) at ({}, {}, {})",
        pos.x, pos.y, pos.z
    );
}

/// A structure passed the placement gate but produced no start.
pub fn structure_start_declined(
    key: StructureKeys,
    chunk_x: i32,
    chunk_z: i32,
    reject: StructureReject,
) {
    if !enabled() {
        return;
    }
    info!(
        "worldgen/structure: {key:?} rejected in chunk ({chunk_x}, {chunk_z}): {reject:?} (start pass)"
    );
}

/// A neighbour's start was recomputed for the reference pass and accepted.
///
/// Rate limited like [`structure_reference_missing_start`]: the reference sweep
/// re-runs the same start for every chunk within 8 chunks of it, so the raw hit
/// count is ~289x the number of distinct structures. The `#{n}` counter is what
/// makes the real volume readable from a sampled log.
pub fn structure_lazy_accepted(key: StructureKeys, chunk_x: i32, chunk_z: i32, start: BlockPos) {
    if !enabled() {
        return;
    }
    if let Some(n) = sampled(&LAZY_ACCEPTS, REFERENCE_SAMPLE_STRIDE) {
        let pos = start.0;
        info!(
            "worldgen/structure: {key:?} start computed for chunk ({chunk_x}, {chunk_z}) at ({}, {}, {}) (reference pass, accept #{n})",
            pos.x, pos.y, pos.z
        );
    }
}

/// A neighbour's start was recomputed for the reference pass and rejected.
///
/// Rate limited for the same reason as [`structure_lazy_accepted`]; this is the
/// higher-volume of the two, since a rejected candidate is retried from every
/// surrounding chunk without ever being cached as a start.
pub fn structure_lazy_declined(
    key: StructureKeys,
    chunk_x: i32,
    chunk_z: i32,
    reject: StructureReject,
) {
    if !enabled() {
        return;
    }
    if let Some(n) = sampled(&LAZY_DECLINES, REFERENCE_SAMPLE_STRIDE) {
        info!(
            "worldgen/structure: {key:?} rejected for chunk ({chunk_x}, {chunk_z}): {reject:?} (reference pass, reject #{n})"
        );
    }
}

/// This chunk picked up pieces of a structure started elsewhere.
pub fn structure_reference_attached(
    key: StructureKeys,
    start_chunk_x: i32,
    start_chunk_z: i32,
    chunk_x: i32,
    chunk_z: i32,
) {
    if !enabled() {
        return;
    }
    debug!(
        "worldgen/structure: chunk ({chunk_x}, {chunk_z}) references {key:?} started at ({start_chunk_x}, {start_chunk_z})"
    );
}

/// A candidate chunk passed the placement gate, yet recomputing its start
/// yielded nothing.
///
/// Pieces that vanilla would have placed here are missing — the signature of a
/// truncated structure (broken mineshaft, half a village).
pub fn structure_reference_missing_start(
    key: StructureKeys,
    candidate_chunk_x: i32,
    candidate_chunk_z: i32,
    chunk_x: i32,
    chunk_z: i32,
) {
    if !enabled() {
        return;
    }
    if let Some(n) = sampled(&REFERENCE_MISSES, REFERENCE_SAMPLE_STRIDE) {
        info!(
            "worldgen/structure: {key:?} placement passed at ({candidate_chunk_x}, {candidate_chunk_z}) but no start was produced, so chunk ({chunk_x}, {chunk_z}) gets no pieces (miss #{n})"
        );
    }
}

/// Resets the mineshaft truncation counters for the current thread.
pub fn mineshaft_begin() {
    if !enabled() {
        return;
    }
    MINESHAFT_DEPTH_STOPS.set(0);
    MINESHAFT_RANGE_STOPS.set(0);
    MINESHAFT_COLLISION_STOPS.set(0);
}

/// A branch stopped because it hit the vanilla recursion depth limit.
pub fn mineshaft_depth_truncated() {
    if !enabled() {
        return;
    }
    MINESHAFT_DEPTH_STOPS.set(MINESHAFT_DEPTH_STOPS.get() + 1);
}

/// A branch stopped because it left the 80-block box around the start room.
pub fn mineshaft_range_truncated() {
    if !enabled() {
        return;
    }
    MINESHAFT_RANGE_STOPS.set(MINESHAFT_RANGE_STOPS.get() + 1);
}

/// A branch stopped because the candidate piece collided with an existing one.
pub fn mineshaft_collision_truncated() {
    if !enabled() {
        return;
    }
    MINESHAFT_COLLISION_STOPS.set(MINESHAFT_COLLISION_STOPS.get() + 1);
}

/// Reports the finished piece tree: how big the mineshaft got and why it
/// stopped growing.
pub fn mineshaft_finished(chunk_x: i32, chunk_z: i32, pieces: usize, y_offset: i32) {
    if !enabled() {
        return;
    }
    info!(
        "worldgen/mineshaft: chunk ({chunk_x}, {chunk_z}) built {pieces} pieces, y offset {y_offset}, stopped at: depth {}, 80-block range {}, collision {}",
        MINESHAFT_DEPTH_STOPS.get(),
        MINESHAFT_RANGE_STOPS.get(),
        MINESHAFT_COLLISION_STOPS.get()
    );
}

/// Reports a feature/structure step of a chunk when it took longer than
/// [`SLOW_FEATURE_STEP_MS`]; cheaper steps are dropped without formatting.
pub fn feature_step_slow(
    chunk_x: i32,
    chunk_z: i32,
    step: usize,
    structures: usize,
    elapsed_ms: u128,
) {
    if !enabled() || elapsed_ms < SLOW_FEATURE_STEP_MS {
        return;
    }
    info!(
        "worldgen/slow: chunk ({chunk_x}, {chunk_z}) feature step {step} took {elapsed_ms}ms ({structures} structure piece collectors)"
    );
}

/// The biome-zoom fuzz picked a quart outside this chunk and was clamped to the
/// chunk edge.
///
/// The surface rule therefore saw a different biome than vanilla would. This is
/// a known source of chunk-border surface seams.
#[inline]
pub fn biome_quart_clamped(
    chunk_x: i32,
    chunk_z: i32,
    quart_x: i32,
    quart_z: i32,
    clamped_x: i32,
    clamped_z: i32,
) {
    if !enabled() {
        return;
    }
    if let Some(n) = sampled(&QUART_CLAMPS, QUART_SAMPLE_STRIDE) {
        info!(
            "worldgen/seam: chunk ({chunk_x}, {chunk_z}) biome quart ({quart_x}, {quart_z}) is outside the chunk, clamped to ({clamped_x}, {clamped_z}) (fallback #{n})"
        );
    }
}
