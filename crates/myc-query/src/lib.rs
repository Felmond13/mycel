//! Queries that only a content-addressed runtime can answer.
//!
//! Because every environment is an explicit file graph where each file is
//! identified by its BLAKE3 hash, the whole store is a database you can
//! query: exact diffs between environments, reverse lookup ("which
//! environments contain this blob?"), SBOMs derived from real file content,
//! and precise deduplication accounting.

pub mod dedup;
pub mod diff;
pub mod doctor;
pub mod sbom;
pub mod which;

pub use dedup::{dedup_report, ManifestDedup};
pub use diff::{diff, Change, ChangeKind, DiffReport};
pub use doctor::{run_checks, Check};
pub use sbom::{sbom, Package, Sbom, SbomFile};
pub use which::{which, MatchBy, WhichHit};
