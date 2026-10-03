//! A custom arrangement of this computer's own screens, independent of how the operating system arranges them.
//!
//! The person can place, say, the monitor that physically sits above the main one to its right instead. The engine
//! then reports the ARRANGED screens to the Glide window and to paired computers, so cursor crossings follow that
//! arrangement, and converts positions back to the operating system's real ("native") screen positions whenever it
//! moves this computer's cursor. Positions are device-local logical pixels, origin at the top-left of all screens.

use glide_platform::{Monitor, Point};
use glide_proto::ipc::MonitorPlacement;

const LIMIT: f64 = 1_000_000.0;

fn overlap(a: &Monitor, b: &Monitor) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// The screens as Glide should treat them. Falls back to the operating system's own arrangement whenever the custom
/// one does not fit the screens that exist right now (a screen was added or removed, or placements overlap).
pub fn arrange(native: &[Monitor], placements: &[MonitorPlacement]) -> Vec<Monitor> {
    if placements.is_empty() || native.is_empty() {
        return native.to_vec();
    }
    let mut out = Vec::with_capacity(native.len());
    for monitor in native {
        let Some(place) = placements.iter().find(|p| p.monitor_id == monitor.id) else {
            return native.to_vec();
        };
        if !place.x.is_finite()
            || !place.y.is_finite()
            || place.x.abs() > LIMIT
            || place.y.abs() > LIMIT
        {
            return native.to_vec();
        }
        out.push(Monitor {
            x: place.x,
            y: place.y,
            ..monitor.clone()
        });
    }
    for (index, a) in out.iter().enumerate() {
        if out[index + 1..].iter().any(|b| overlap(a, b)) {
            return native.to_vec();
        }
    }
    let min_x = out.iter().map(|m| m.x).fold(f64::INFINITY, f64::min);
    let min_y = out.iter().map(|m| m.y).fold(f64::INFINITY, f64::min);
    for monitor in &mut out {
        monitor.x -= min_x;
        monitor.y -= min_y;
    }
    out
}

/// Moves placements so the top-left of all screens is at (0, 0). Returns them with the offset that was removed, which
/// the caller adds to this computer's position on the desk so nothing visibly jumps.
pub fn normalize(placements: &[MonitorPlacement]) -> (Vec<MonitorPlacement>, Point) {
    if placements.is_empty() {
        return (Vec::new(), Point { x: 0.0, y: 0.0 });
    }
    let min_x = placements.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
    let min_y = placements.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
    let shifted = placements
        .iter()
        .map(|p| MonitorPlacement {
            monitor_id: p.monitor_id.clone(),
            x: p.x - min_x,
            y: p.y - min_y,
        })
        .collect();
    (shifted, Point { x: min_x, y: min_y })
}

/// The screen containing `p`, or the nearest one.
fn screen_for(screens: &[Monitor], p: Point) -> Option<&Monitor> {
    screens
        .iter()
        .find(|m| p.x >= m.x && p.x < m.x + m.w && p.y >= m.y && p.y < m.y + m.h)
        .or_else(|| {
            screens.iter().min_by(|a, b| {
                let da = (p.x.clamp(a.x, a.x + a.w) - p.x).hypot(p.y.clamp(a.y, a.y + a.h) - p.y);
                let db = (p.x.clamp(b.x, b.x + b.w) - p.x).hypot(p.y.clamp(b.y, b.y + b.h) - p.y);
                da.total_cmp(&db)
            })
        })
}

fn convert(from: &[Monitor], to: &[Monitor], p: Point) -> Point {
    let Some(source) = screen_for(from, p) else {
        return p;
    };
    let Some(target) = to.iter().find(|m| m.id == source.id) else {
        return p;
    };
    Point {
        x: target.x + (p.x - source.x),
        y: target.y + (p.y - source.y),
    }
}

/// A real cursor position (as the operating system reports it) in the arranged screens.
pub fn to_arranged(native: &[Monitor], arranged: &[Monitor], p: Point) -> Point {
    convert(native, arranged, p)
}

/// An arranged position as the operating system's real cursor position.
pub fn to_native(native: &[Monitor], arranged: &[Monitor], p: Point) -> Point {
    convert(arranged, native, p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Desktop;
    use glide_proto::ipc::{Layout, LayoutDevice};
    use std::collections::HashMap;

    fn monitor(id: &str, x: f64, y: f64, w: f64, h: f64) -> Monitor {
        Monitor {
            id: id.into(),
            x,
            y,
            w,
            h,
            scale: 1.0,
            primary: id == "bottom",
        }
    }
    fn place(id: &str, x: f64, y: f64) -> MonitorPlacement {
        MonitorPlacement {
            monitor_id: id.into(),
            x,
            y,
        }
    }
    // A real two-monitor desk as Windows arranges it: a 3413x960 monitor centred above a 5120x1440 one.
    fn native() -> Vec<Monitor> {
        vec![
            monitor("top", 870.0, 0.0, 5120.0 / 1.5, 960.0),
            monitor("bottom", 0.0, 960.0, 5120.0, 1440.0),
        ]
    }

    // Feature request: "put the top monitor to the right of my bottom monitor, so moving left from the MacBook goes to
    // the top monitor first, and further left to the bottom one". Proves the arrangement, the position conversion
    // both ways, and the crossings on the shared desk.
    #[test]
    fn top_monitor_moved_to_the_right_of_the_bottom_one_routes_the_mac_through_it() {
        let arranged = arrange(
            &native(),
            &[place("bottom", 0.0, 0.0), place("top", 5120.0, 0.0)],
        );
        assert_eq!(
            (arranged[0].x, arranged[0].y),
            (5120.0, 0.0),
            "top now sits right of bottom"
        );
        assert_eq!((arranged[1].x, arranged[1].y), (0.0, 0.0));
        // A point on the arranged top monitor is the same pixel of the real top monitor, and back.
        let p = Point {
            x: 5200.0,
            y: 100.0,
        };
        let real = to_native(&native(), &arranged, p);
        assert_eq!(real, Point { x: 950.0, y: 100.0 });
        assert_eq!(to_arranged(&native(), &arranged, real), p);

        // On the desk: the MacBook touches the right edge of the arranged top monitor.
        let pc = "pc".to_string();
        let mac = "mac".to_string();
        let layout = Layout {
            devices: vec![
                LayoutDevice {
                    device_id: pc.clone(),
                    x: 0.0,
                    y: 0.0,
                },
                LayoutDevice {
                    device_id: mac.clone(),
                    x: 5120.0 + 5120.0 / 1.5,
                    y: 0.0,
                },
            ],
        };
        let screens = HashMap::from([
            (pc.clone(), arranged.clone()),
            (
                mac.clone(),
                vec![monitor("retina", 0.0, 0.0, 1728.0, 1117.0)],
            ),
        ]);
        let desktop = Desktop::from_layout(&layout, &screens).expect("desk");
        // Leaving the MacBook to the left lands on the top monitor...
        let from_mac = desktop.move_cursor_from(
            Point {
                x: 5120.0 + 5120.0 / 1.5 + 5.0,
                y: 300.0,
            },
            Point { x: -20.0, y: 0.0 },
            Some(&mac),
        );
        let crossing = from_mac.crossing.expect("crosses to the PC");
        assert_eq!(crossing.target_device_id, pc);
        let landed = desktop
            .global_to_device_logical(&pc, from_mac.position)
            .expect("on the PC");
        assert!(
            landed.x > 5120.0,
            "lands on the top monitor, got {landed:?}"
        );
        // ...and keeps going left onto the bottom monitor, within the same computer.
        let further =
            desktop.move_cursor_on_device(from_mac.position, Point { x: -4000.0, y: 0.0 }, &pc);
        let further = desktop
            .global_to_device_logical(&pc, further)
            .expect("on the PC");
        assert!(
            further.x < 5120.0,
            "continues onto the bottom monitor, got {further:?}"
        );
    }

    // Prevents a half-applied arrangement after a screen is unplugged or placements overlap: Glide falls back to the
    // operating system's own arrangement instead of reporting impossible screens.
    #[test]
    fn missing_or_overlapping_placements_fall_back_to_the_system_arrangement() {
        assert_eq!(arrange(&native(), &[place("bottom", 0.0, 0.0)]), native());
        assert_eq!(
            arrange(
                &native(),
                &[place("bottom", 0.0, 0.0), place("top", 100.0, 100.0)]
            ),
            native()
        );
        assert_eq!(
            arrange(
                &native(),
                &[place("bottom", 0.0, 0.0), place("top", f64::NAN, 0.0)]
            ),
            native()
        );
        assert_eq!(arrange(&native(), &[]), native());
    }

    #[test]
    fn placements_are_normalized_and_report_the_offset_to_keep_screens_in_place() {
        let (placements, offset) =
            normalize(&[place("bottom", 300.0, -200.0), place("top", 5420.0, -200.0)]);
        assert_eq!(
            offset,
            Point {
                x: 300.0,
                y: -200.0
            }
        );
        assert_eq!((placements[0].x, placements[0].y), (0.0, 0.0));
        assert_eq!((placements[1].x, placements[1].y), (5120.0, 0.0));
    }

    // With no custom arrangement nothing is converted, so ordinary setups behave exactly as before.
    #[test]
    fn without_an_arrangement_positions_are_unchanged() {
        let same = arrange(&native(), &[]);
        for p in [
            Point { x: 900.0, y: 10.0 },
            Point {
                x: 4000.0,
                y: 2000.0,
            },
            Point { x: -5.0, y: 1000.0 },
        ] {
            assert_eq!(to_native(&native(), &same, p), p);
            assert_eq!(to_arranged(&native(), &same, p), p);
        }
    }
}
