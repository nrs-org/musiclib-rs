//! Import pipeline: the structured-concurrency traversal that turns URLs into a
//! populated music DB, plus its dedup and progress helpers. Shared by the
//! `import` and `dedup` binaries.

pub mod dedup;
pub mod flush;
pub mod importer;
pub mod progress;
pub mod state;
