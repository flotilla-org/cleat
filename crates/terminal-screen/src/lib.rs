//! Structural observations and CSS-like selectors over owned terminal grids.
//!
//! No VT parsing, daemon state, application roles, or frame identity lives here.
//! Coordinates are zero-based half-open cell rectangles; text preserves spaces
//! and soft-wrapped physical rows. See the crate README for selector semantics.

mod detect;
mod grid;
mod selector;
mod tree;

pub use detect::{detect_bands, detect_boxes, Band, BandKind, BorderStyle, DetectedBox};
pub use grid::*;
pub use selector::{Selector, SelectorError};
pub use tree::{analyze, Node, NodeAnnotations, NodeId, ScreenTree, TreeError};
