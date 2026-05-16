//! file: src/lib.rs
//! description: Library root re-exporting fire_ctrl modules for integration tests and benches.
//!
//! The CLI binary in `src/main.rs` re-uses these modules via `fire_ctrl::*` so
//! that integration tests under `tests/` and benches under `benches/` can
//! exercise the same internal types without a parallel module tree.
pub mod api;
pub mod config;
pub mod database;
pub mod llm;
pub mod models;
pub mod scraper;
pub mod worker;
