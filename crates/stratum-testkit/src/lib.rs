//! Hermetic test infrastructure for the Stratum platform.
//!
//! Everything here runs in-process or as local subprocesses with no external
//! credentials: MinIO stands in for S3 (SigV4-signed, anonymous access never
//! relied on), fake origins stand in for GitHub, and the real `git` CLI is
//! the protocol oracle.

pub mod adversarial;
pub mod browser;
pub mod closure;
pub mod contract;
pub mod detach;
pub mod fake_github;
pub mod faultproxy;
pub mod fleet;
pub mod gateproxy;
pub mod gitcli;
pub mod httpfake;
pub mod mailbox;
pub mod minio;
pub mod oidc;
pub mod pg;
pub mod proxy;
pub mod runner_bin;
pub mod server;
pub mod smtpfake;
pub(crate) mod tempdir;
pub mod wait;

pub use faultproxy::{FaultHandle, FaultProxy};
pub use fleet::Fleet;
pub use gateproxy::{GateHandle, GateProxy};
pub use minio::{Bucket, Minio};
pub use proxy::CountingProxy;
pub use server::{Server, ServerBuilder};
pub use wait::{wait_for, wait_until};
