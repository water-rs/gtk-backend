//! GTK `Gradient` component implementation.

use gtk4::Widget;
use gtk4::prelude::*;
use waterui_core::{Environment, Native};
use waterui_graphics::Gradient;
use waterui_graphics::cherenkov::{ColorStop, Paint, SweepGradient};
use waterui_graphics::color::{WorkingColor, working};

use crate::component::GtkComponent;
use crate::renderer::GtkRenderer;
use crate::util::resolved_color_to_srgba_f64;

const ANGULAR_SEGMENTS: usize = 720;

impl GtkComponent for Native<Gradient> {
    fn render(self, _env: &Environment, _renderer: &mut GtkRenderer) -> Widget {
        let gradient = self.into_inner();
        let mut stops = match gradient.paint() {
            Paint::Linear(paint) => paint.stops.clone(),
            Paint::Radial(paint) => paint.stops.clone(),
            Paint::Sweep(paint) => paint.stops.clone(),
            Paint::Mesh(_) => {
                unreachable!("Native<Gradient> never carries a mesh paint")
            }
            Paint::Solid(_) | Paint::Image(_) | Paint::Shader(_) | Paint::Transformed(_) => {
                unreachable!("a Gradient only carries linear, radial, or sweep paints")
            }
        };
        stops.sort_by(|a, b| a.offset.total_cmp(&b.offset));
        assert!(!stops.is_empty(), "gradient must contain at least one stop");

        let area = gtk4::DrawingArea::new();
        area.set_hexpand(true);
        area.set_vexpand(true);

        area.set_draw_func(move |_area, cr, width, height| {
            let width = f64::from(width);
            let height = f64::from(height);
            // GTK invokes the draw func before allocation (and while hidden)
            // with zero geometry; a zero-area surface has nothing to paint.
            if width <= 0.0 || height <= 0.0 {
                return;
            }

            match gradient.paint() {
                Paint::Linear(paint) => {
                    let cairo = gtk4::cairo::LinearGradient::new(
                        paint.start.x * width,
                        paint.start.y * height,
                        paint.end.x * width,
                        paint.end.y * height,
                    );
                    for stop in &stops {
                        let (red, green, blue, alpha) = to_rgba(stop.color);
                        cairo.add_color_stop_rgba(f64::from(stop.offset), red, green, blue, alpha);
                    }
                    cr.rectangle(0.0, 0.0, width, height);
                    cr.set_source(&cairo)
                        .expect("failed to bind linear gradient source");
                    cr.fill().expect("failed to draw linear gradient");
                }
                Paint::Radial(paint) => {
                    let scale = width.min(height);
                    let start_radius = paint.start_radius * scale;
                    let end_radius = paint.end_radius * scale;
                    assert!(end_radius > 0.0, "radial gradient end radius must be > 0");

                    let cairo = gtk4::cairo::RadialGradient::new(
                        paint.start_center.x * width,
                        paint.start_center.y * height,
                        start_radius,
                        paint.end_center.x * width,
                        paint.end_center.y * height,
                        end_radius,
                    );
                    for stop in &stops {
                        let normalized = (end_radius - start_radius)
                            .mul_add(f64::from(stop.offset), start_radius)
                            / end_radius;
                        let (red, green, blue, alpha) = to_rgba(stop.color);
                        cairo.add_color_stop_rgba(normalized, red, green, blue, alpha);
                    }
                    cr.rectangle(0.0, 0.0, width, height);
                    cr.set_source(&cairo)
                        .expect("failed to bind radial gradient source");
                    cr.fill().expect("failed to draw radial gradient");
                }
                Paint::Sweep(paint) => {
                    draw_angular_gradient(cr, paint, stops.as_slice(), width, height);
                }
                Paint::Mesh(_) => {
                    unreachable!("Native<Gradient> never carries a mesh paint")
                }
                Paint::Solid(_) | Paint::Image(_) | Paint::Shader(_) | Paint::Transformed(_) => {
                    unreachable!("a Gradient only carries linear, radial, or sweep paints")
                }
            }
        });

        area.upcast()
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "GTK widget geometry is integer pixels while WaterUI layout is f32"
)]
fn draw_angular_gradient(
    cr: &gtk4::cairo::Context,
    gradient: &SweepGradient,
    stops: &[ColorStop],
    width: f64,
    height: f64,
) {
    let sweep = gradient.end_angle - gradient.start_angle;
    assert!(sweep > 0.0, "angular gradient sweep must be positive");
    assert!(
        sweep <= core::f64::consts::TAU,
        "angular gradient sweep must be <= TAU"
    );

    let cx = gradient.center.x * width;
    let cy = gradient.center.y * height;
    let radius = width.hypot(height);
    let start = gradient.start_angle;
    let sweep_fraction = sweep / core::f64::consts::TAU;

    for segment in 0..ANGULAR_SEGMENTS {
        let t0 = segment as f64 / ANGULAR_SEGMENTS as f64;
        let t1 = (segment + 1) as f64 / ANGULAR_SEGMENTS as f64;
        let tm = f64::midpoint(t0, t1);
        let mapped = if tm <= sweep_fraction {
            (tm / sweep_fraction) as f32
        } else {
            1.0
        };

        let color = sample_stop_color(stops, mapped);
        let (red, green, blue, alpha) = to_rgba(color);
        let angle0 = t0.mul_add(core::f64::consts::TAU, start);
        let angle1 = t1.mul_add(core::f64::consts::TAU, start);

        cr.new_sub_path();
        cr.move_to(cx, cy);
        cr.line_to(
            radius.mul_add(angle0.cos(), cx),
            radius.mul_add(angle0.sin(), cy),
        );
        cr.arc(cx, cy, radius, angle0, angle1);
        cr.close_path();
        cr.set_source_rgba(red, green, blue, alpha);
        cr.fill().expect("failed to draw angular gradient segment");
    }
}

fn sample_stop_color(stops: &[ColorStop], t: f32) -> WorkingColor {
    assert!(
        (0.0..=1.0).contains(&t),
        "gradient sampling position must be within [0, 1]"
    );
    if t <= stops[0].offset {
        return stops[0].color;
    }

    for window in stops.windows(2) {
        let left = window[0];
        let right = window[1];
        if t <= right.offset {
            let span = right.offset - left.offset;
            assert!(
                span > 0.0,
                "gradient stop positions must be strictly increasing"
            );
            let local_t = (t - left.offset) / span;
            return working::lerp(left.color, right.color, local_t);
        }
    }

    stops
        .last()
        .expect("gradient must contain at least one stop")
        .color
}

fn to_rgba(color: WorkingColor) -> (f64, f64, f64, f64) {
    resolved_color_to_srgba_f64(color)
}
