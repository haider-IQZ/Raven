//! Render helper utilities for flicker-free rendering.
//!
//! This module provides rendering utilities that prevent element-specific
//! black flickers.

pub mod solid_color;

pub use solid_color::{SolidColorBuffer, SolidColorRenderElement};
