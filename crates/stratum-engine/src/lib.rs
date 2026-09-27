//! The offline/maintenance side of the Stratum engine, ported from the
//! research repo's Python harness (`bench/ingest_segments.py`,
//! `bench/build_locator.py`, `bench/compact.py`, `bench/gc_epochs.py`).
//!
//! The serving path (stratum-proto) never does store work through this
//! crate at request time — writes happen here, at ingest/sync/compaction
//! time, keeping the read path's zero-request-time-delta-work property
//! (invariant I1). The one thing it does borrow is [`objwrite`]: pure
//! in-memory object hashing and pack sealing, which the changeset
//! workspace uses to build its three-object synthetic repository rather
//! than carry a second encoder that could disagree with this one.
//!
//! Faithfulness note: the segment recipes here must match the research
//! findings exactly — path-major cold ordering, `--no-reuse-delta
//! --no-sparse --no-use-bitmap-index` on every walk-driven pack, global
//! seen-set + `^oid` exclusions, and entry-count asserts (invariants I2–I6).
//! The dead-ends ledger in the research HANDOFF explains why each flag
//! exists; do not "optimize" them away.

pub mod ancestry;
pub mod compact;
pub mod errclass;
pub mod fork;
pub mod gc;
pub mod gitcmd;
pub mod ingest;
pub mod locator;
pub mod materialize;
pub mod objwrite;
pub mod read;
pub mod refops;
pub mod repoinit;
pub mod treediff;

pub use ingest::{ingest, IngestConfig, IngestOutput};
pub use locator::build_locator;
