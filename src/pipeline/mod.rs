//! Import pipeline: the structured-concurrency traversal that turns URLs into a
//! populated music DB, plus its dedup and progress helpers. Shared by the
//! `import` and `dedup` binaries.

pub mod dedup;
pub mod embedding;
#[cfg(feature = "ffi")]
pub(crate) mod ffi;
pub mod flush;
pub mod importer;
pub mod ingest;
pub mod pair_facts;
pub mod progress;
pub(crate) mod script_file;
pub mod softmatch;
pub mod state;
