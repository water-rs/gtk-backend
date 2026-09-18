//! GTK widget implementations for `WaterUI` navigation components.

pub mod menu;
pub mod navigation;
pub mod tabs;

use gtk4::prelude::*;

/// Chrome labels — bar titles, tab labels — must stay on one line. GTK
/// negotiates chrome space through minimum widths and width-for-height
/// probes, both of which a wrapping `GtkLabel` answers with a sliver:
/// `GtkNotebook` sizes each tab to its *minimum* requisition, and
/// `GtkCenterLayout` measures the bar title under the bar's height. A
/// wrapping label then collapses into wrapped, hyphenated fragments.
///
/// The `ellipsize` mode decides what the single line's minimum reports:
/// `End` keeps a centered bar title shrinkable under a narrow bar —
/// `AdwWindowTitle` behaviour — but it also collapses the minimum to a
/// sliver, so a `GtkNotebook` tab would allot the label a single
/// character. `None` pins the minimum to the full text, which is what a
/// tab needs to take its natural width.
pub(crate) fn enforce_single_line_labels(
    widget: &gtk4::Widget,
    ellipsize: gtk4::pango::EllipsizeMode,
) {
    if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
        label.set_wrap(false);
        label.set_ellipsize(ellipsize);
        return;
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        enforce_single_line_labels(&current, ellipsize);
        child = current.next_sibling();
    }
}
