//! Import pipeline: the structured-concurrency traversal that turns URLs into a
//! populated music DB, plus its dedup and progress helpers. Shared by the
//! `import` and `dedup` binaries.

pub mod dedup;
pub mod dedup_model;
pub mod embedding;
#[cfg(feature = "ffi")]
pub(crate) mod ffi;
pub mod flush;
pub mod importer;
pub mod ingest;
pub mod jev;
pub mod progress;
pub mod softmatch;
pub mod state;
