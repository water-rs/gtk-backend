//! Applies `waterui-layout` results to GTK widgets.
//!
//! A layout [`Rect`] becomes a [`Widget::allocate`] call carrying a translate
//! transform: the widget receives the rect's border box and sits at
//! `rect.origin + (margin_start, margin_top)` — [`GtkSubView`](super::GtkSubView)
//! reports sizes in margin-box units, so the margin stays inside the rect.
//!
//! Placement deliberately never touches [`Widget::set_size_request`]. A size
//! request is the widget's *intrinsic* size: `gtk_widget_measure` returns it
//! verbatim, so writing a placement rect into the request would feed one
//! transient allocation back into every later measurement and corrupt the
//! layout engine's view of the tree.

use gtk4::prelude::*;
use gtk4::{Widget, graphene, gsk};
use waterui_core::layout::{Rect, StretchAxis};

/// Allocates each child at its layout-computed [`Rect`].
///
/// `children` carries the same `(widget, axis)` tuples the container measured
/// with; the axis is measurement metadata and plays no role in allocation —
/// the rect already encodes everything the engine decided.
///
/// # Panics
///
/// Panics if `rects` and `children` differ in length — the layout engine must
/// return one rect per measured child.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "WaterUI layout rects are f32 points while GTK allocation is integer pixels"
)]
pub fn apply_rects(rects: &[Rect], children: &[(Widget, StretchAxis)]) {
    assert_eq!(
        rects.len(),
        children.len(),
        "layout must return one rect per child"
    );
    for ((child, _axis), rect) in children.iter().zip(rects.iter()) {
        let margin_start = child.margin_start() as f32;
        let margin_top = child.margin_top() as f32;
        let width = (rect.width() - margin_start - child.margin_end() as f32).max(0.0);
        let height = (rect.height() - margin_top - child.margin_bottom() as f32).max(0.0);
        child.allocate(
            width.round() as i32,
            height.round() as i32,
            -1,
            Some(gsk::Transform::default().translate(&graphene::Point::new(
                rect.x() + margin_start,
                rect.y() + margin_top,
            ))),
        );
    }
}
