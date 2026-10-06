//! Structural observations and CSS-like selectors over owned terminal grids.
//!
//! No VT parsing, daemon state, application roles, or frame identity lives here.
//! Coordinates are zero-based half-open cell rectangles; text preserves spaces
//! and soft-wrapped physical rows. See the crate README for selector semantics.

mod grid;
pub mod segment;
mod selector;
mod tree;

pub use grid::*;
pub use selector::{Selector, SelectorError, SelectorMatch};
pub use tree::{analyze, Node, NodeAnnotations, NodeId, ScreenTree, TreeError};
