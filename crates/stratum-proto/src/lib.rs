//! Stratum's git wire-protocol serving and receive paths, extracted from the
//! research repo's `stratum-cgi` binary as a transport-agnostic library.
//!
//! STRATUM-CORE DIVERGENCE from the research CGI program: these functions
//! write protocol *bodies* only — HTTP status lines and Content-Type headers
//! are the embedding server's job. Everything protocol-level (pkt-line
//! framing, v2 ls-refs/fetch planning, sideband streaming, the receive-pack
//! quarantine + WAL/CAS write path) is byte-identical to the research
//! implementation; see `reference/invariants.md` for the contract it keeps.

pub mod pktline;
pub mod receive;
pub mod serve;
pub mod workspace;

/// Content types the smart-HTTP transport must set on responses.
pub const UPLOAD_PACK_ADVERT_TYPE: &str = "application/x-git-upload-pack-advertisement";
pub const UPLOAD_PACK_RESULT_TYPE: &str = "application/x-git-upload-pack-result";
pub const RECEIVE_PACK_ADVERT_TYPE: &str = "application/x-git-receive-pack-advertisement";
pub const RECEIVE_PACK_RESULT_TYPE: &str = "application/x-git-receive-pack-result";
