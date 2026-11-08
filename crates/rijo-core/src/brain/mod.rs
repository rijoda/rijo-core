//! Brain v2 — SQLite-betriebene neurale DB
//!
//! Drei Submodule:
//! - `db` — SQLite-Schema, CRUD, Vektor-Suche (bruteforce cosine), Graph-Traversal
//! - `embed` — fastembed-Wrapper: Text → Vec<f32> (384-dim)
//! - `indexer` — Codebase-Scanner: chunked, embedded, stored

pub mod db;
pub mod embed;
pub mod indexer;
pub mod outline;
