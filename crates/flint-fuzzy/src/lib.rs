//! Flint search engine core: folding, matching kernels, the in-memory name
//! table, query parsing and the staged progressive search engine.

pub mod engine;
pub mod fold;
pub mod kernels;
pub mod query;
pub mod session;
pub mod table;
