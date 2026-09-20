//! Layout integration with `waterui-layout`.
//!
//! This module provides the bridge between `WaterUI`'s layout system and GTK widgets:
//! - [`GtkSubView`] implements [`SubView`](waterui_core::layout::SubView) using GTK's measurement API
//! - [`apply_placements`] applies layout results to GTK widgets
//! - [`proposal`] carries each child's selected proposal on the widget itself

pub mod placer;
pub(crate) mod proposal;
pub mod subview;

pub use placer::apply_placements;
pub(crate) use subview::layout_measure_key;
pub use subview::{FixedSizeSubView, GtkSubView, LayoutMeasureMemo};
