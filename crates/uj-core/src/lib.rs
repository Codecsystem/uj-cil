//! uj-core: Core data models, configurations, verdicts, and scoring logic.

pub mod config;
pub mod run;
pub mod tests;
pub mod verdict;

pub use run::{ArtifactReadResult, ReadArtifactError, read_artifact_bounded};
