//! Monorepo review: OWNERS-governed approval sufficiency.
//!
//! Layering, deliberately strict so the gate-heavy pieces stay pure:
//! - `owners` — the OWNERS grammar and directory-chain walk. No I/O.
//! - `sufficiency` — the verdict engine over resolved requirements. No I/O.
//! - `resolve` — OWNERS entries → user ids, against the control plane.
//! - `load` — OWNERS blobs out of a repo snapshot, via the layout reader.
//! - `changeset` — one verdict over the members of a changeset, from
//!   each member's own. No I/O.
//! - `suggestion` — the fenced *suggestion* block inside a review
//!   comment: parsing it, applying it to a file's lines, and building
//!   the commit that becomes the next patchset.
//!
//! The API handlers (`api::owners_api`) compose these four.

pub mod change_id;
pub mod changeset;
pub mod load;
pub mod owners;
pub mod resolve;
pub mod sufficiency;
pub mod suggestion;
pub mod transplant;
