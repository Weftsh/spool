//! The Stratum control plane: orgs, repos, tokens, audit — the relational
//! truth beside the object-store data plane.
//!
//! PostgreSQL behind one process-wide handle. Repo *content* lives in
//! object storage; this database holds only registry/auth/audit/metrics
//! state, which is small, hot, and relational. The `ControlDb` surface is
//! deliberately narrow: modules speak SQL through it, nothing else does.

pub mod association;
pub mod audit;
pub mod auth;
pub mod changes;
pub mod changeset_checks;
pub mod changesets;
pub mod checks;
pub mod commit_counts;
pub mod contribs;
pub mod db;
pub mod epoch_refs;
pub mod fileviews;
pub mod forks;
pub mod identities;
pub mod ids;
pub mod imports;
pub mod insights;
pub mod installations;
pub mod invites;
pub mod issues;
pub mod jobs;
pub mod members;
pub mod metrics;
pub mod profiles;
pub mod protections;
pub mod registry;
pub mod runners;
pub mod sessions;
pub mod signals;
pub mod sshkeys;
pub mod storage;
pub mod teams;
pub mod topics;
pub mod users;
pub mod usertokens;
pub mod watches;
pub mod webhooks;
pub mod workflows;

pub use db::ControlDb;
pub use registry::{Org, Repo, RepoKind, RepoPrefix};
