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
/// Single-line ellipsizing labels report their true text width instead,
/// matching `AdwWindowTitle` behaviour.
pub(crate) fn enforce_single_line_labels(widget: &gtk4::Widget) {
    if let Some(label) = widget.downcast_ref::<gtk4::Label>() {
        label.set_wrap(false);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        return;
    }
    let mut child = widget.first_child();
    while let Some(current) = child {
        enforce_single_line_labels(&current);
        child = current.next_sibling();
    }
}
