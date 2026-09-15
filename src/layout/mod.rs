//! Layout integration with `waterui-layout`.
//!
//! This module provides the bridge between `WaterUI`'s layout system and GTK widgets:
//! - [`GtkSubView`] implements [`SubView`](waterui_core::layout::SubView) using GTK's measurement API
//! - [`apply_rects`] applies layout results to GTK widgets

pub mod placer;
pub mod subview;

pub use placer::apply_rects;
pub use subview::{FixedSizeSubView, GtkSubView};
