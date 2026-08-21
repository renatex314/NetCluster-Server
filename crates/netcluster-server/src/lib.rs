//! A clustering server: ingest moving points, serve clustered vector tiles.
//!
//! The binary is a thin shell around this. Everything worth testing lives here.

pub mod collection;
pub mod mvt;
pub mod routes;
pub mod snapshot;
