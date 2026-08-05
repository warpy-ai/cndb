//! Document encoding.
//!
//! M2 adds the graph model — typed nodes and edges — on top of this.

pub mod codec;

pub use codec::{from_bson_bytes, to_bson_bytes};
