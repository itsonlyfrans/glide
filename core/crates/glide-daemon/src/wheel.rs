//! Scroll amounts differ between systems: a Windows wheel notch is 120 units, a Mac trackpad reports small point
//! amounts, and the horizontal axis points the opposite way. On the wire scrolling is therefore carried in one
//! unit ("scroll points", Mac convention: positive dy = content moves down, positive dx = content moves right) and
//! each side converts to and from what its own system expects.

use glide_platform::Os;

/// One Windows wheel notch (120 units) is this many scroll points, about what a browser scrolls per notch.
const POINTS_PER_NOTCH: f64 = 100.0;
const WINDOWS_UNITS_PER_NOTCH: f64 = 120.0;
/// A Mac mouse wheel reports whole lines; a notch is about three lines.
const POINTS_PER_MAC_LINE: f64 = POINTS_PER_NOTCH / 3.0;

/// Convert a scroll sample captured on `os` into wire scroll points.
pub fn to_wire(os: Os, dx: f64, dy: f64, precise: bool) -> (f64, f64) {
    match os {
        Os::Windows => {
            let k = POINTS_PER_NOTCH / WINDOWS_UNITS_PER_NOTCH;
            (-dx * k, dy * k)
        }
        Os::Macos if precise => (dx, dy),
        Os::Macos => (dx * POINTS_PER_MAC_LINE, dy * POINTS_PER_MAC_LINE),
    }
}

/// Convert wire scroll points into what `os` injects, as `(dx, dy, precise)`.
pub fn from_wire(os: Os, dx: f64, dy: f64) -> (f64, f64, bool) {
    match os {
        Os::Windows => {
            let k = WINDOWS_UNITS_PER_NOTCH / POINTS_PER_NOTCH;
            (-dx * k, dy * k, true)
        }
        Os::Macos => (dx, dy, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Bug: a normal two-finger swipe on the MacBook arrived on Windows as about 1/20 of a wheel notch, so scrolling
    // and moving left/right barely worked.
    #[test]
    fn a_mac_trackpad_swipe_becomes_a_usable_amount_of_windows_wheel() {
        let (dx, dy) = to_wire(Os::Macos, 0.0, -30.0, true);
        let (wx, wy, _) = from_wire(Os::Windows, dx, dy);
        assert_eq!(wx, 0.0);
        assert!(
            (wy - -36.0).abs() < 1e-9,
            "30 points is 36 Windows units, got {wy}"
        );
        assert!(wy < 0.0, "swiping content up scrolls down on Windows");
    }

    // Bug: horizontal scrolling pointed the wrong way between a Mac and a Windows PC.
    #[test]
    fn horizontal_scroll_direction_is_flipped_between_mac_and_windows() {
        // Mac: positive dx moves the content right (view goes left). Windows: positive dx scrolls the view right.
        let (dx, _) = to_wire(Os::Macos, 20.0, 0.0, true);
        let (wx, _, _) = from_wire(Os::Windows, dx, 0.0);
        assert!(wx < 0.0);
        let (dx, _) = to_wire(Os::Windows, 120.0, 0.0, false);
        let (mx, _, _) = from_wire(Os::Macos, dx, 0.0);
        assert!(
            mx < 0.0,
            "a Windows tilt right shows content further right: Mac content moves left"
        );
    }

    // Bug: a Windows wheel notch was injected on the Mac as 120 lines.
    #[test]
    fn one_windows_notch_is_a_modest_mac_scroll_and_vertical_direction_is_kept() {
        let (dx, dy) = to_wire(Os::Windows, 0.0, 120.0, false);
        assert_eq!(dx, 0.0);
        assert!((dy - 100.0).abs() < 1e-9);
        let (mx, my, precise) = from_wire(Os::Macos, dx, dy);
        assert_eq!((mx, my, precise), (0.0, 100.0, true));
    }

    #[test]
    fn mac_mouse_wheel_lines_scale_to_about_one_notch_per_three_lines() {
        let (_, dy) = to_wire(Os::Macos, 0.0, 3.0, false);
        assert!((dy - 100.0).abs() < 1e-9);
    }

    #[test]
    fn windows_to_windows_round_trip_keeps_the_amount() {
        let (dx, dy) = to_wire(Os::Windows, 60.0, -240.0, false);
        let (wx, wy, _) = from_wire(Os::Windows, dx, dy);
        assert!((wx - 60.0).abs() < 1e-9 && (wy - -240.0).abs() < 1e-9);
    }
}
