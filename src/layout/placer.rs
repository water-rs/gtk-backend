//! Applies `waterui-layout` results to GTK widgets.
//!
//! A [`SubviewPlacement`]'s `frame` becomes a [`Widget::allocate`] call
//! carrying the margin-box frame and its origin. GTK removes the child's
//! margins and applies their offset inside `Widget::allocate`; doing that
//! here too would count each margin twice.
//!
//! Placement deliberately never touches [`Widget::set_size_request`]. A size
//! request is the widget's *intrinsic* size: `gtk_widget_measure` returns it
//! verbatim, so writing a placement rect into the request would feed one
//! transient allocation back into every later measurement and corrupt the
//! layout engine's view of the tree.

use gtk4::prelude::*;
use gtk4::{Widget, graphene, gsk};
use waterui_core::layout::{StretchAxis, SubviewPlacement};

use super::proposal::deliver_proposal;

/// Allocates each child at its [`SubviewPlacement`].
///
/// The placement's selected `proposal` is delivered to the child *before*
/// its frame is allocated: a container child that lays its own children out
/// on the allocation then holds the same packet the engine negotiated, and a
/// proposal that changed while the frame did not still queues the child's
/// relayout (see [`deliver_proposal`]).
///
/// `children` carries the same `(widget, axis)` tuples the container measured
/// with; the axis is measurement metadata and plays no role in allocation —
/// the placement already encodes everything the engine decided.
///
/// # Panics
///
/// Panics if `placements` and `children` differ in length — the layout
/// engine must return one placement per measured child.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "WaterUI layout rects are f32 points while GTK allocation is integer pixels"
)]
pub fn apply_placements(placements: &[SubviewPlacement], children: &[(Widget, StretchAxis)]) {
    assert_eq!(
        placements.len(),
        children.len(),
        "layout must return one placement per child"
    );
    for ((child, _axis), placement) in children.iter().zip(placements.iter()) {
        deliver_proposal(child, placement.proposal);
        let rect = &placement.frame;
        let width = rect.width().max(0.0);
        let height = rect.height().max(0.0);
        child.allocate(
            width.round() as i32,
            height.round() as i32,
            -1,
            Some(gsk::Transform::default().translate(&graphene::Point::new(rect.x(), rect.y()))),
        );
    }
}
