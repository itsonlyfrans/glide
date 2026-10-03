use glide_platform::{Button, InputEventKind, Point};

pub(crate) fn permission_statuses(
    accessibility: bool,
    monitoring: bool,
    posting: bool,
) -> glide_platform::Permissions {
    use glide_platform::PermissionStatus::{Denied, Granted};
    glide_platform::Permissions {
        accessibility: if accessibility { Granted } else { Denied },
        input_monitoring: if monitoring { Granted } else { Denied },
        injection: if posting { Granted } else { Denied },
    }
}

#[test]
fn permission_grants_are_independent() {
    use glide_platform::PermissionStatus::{Denied, Granted};
    for bits in 0..8 {
        let statuses = permission_statuses(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
        assert_eq!(
            statuses.accessibility,
            if bits & 1 != 0 { Granted } else { Denied }
        );
        assert_eq!(
            statuses.input_monitoring,
            if bits & 2 != 0 { Granted } else { Denied }
        );
        assert_eq!(
            statuses.injection,
            if bits & 4 != 0 { Granted } else { Denied }
        );
    }
}

// Physical positions from Apple's Events.h and USB HID Usage Tables, keyboard page 0x07.
// Unsupported usages stay unsupported: never reinterpret PrintScreen/Pause as a function key.
const KEYS: &[(u16, u16)] = &[
    (0x04, 0x00),
    (0x05, 0x0b),
    (0x06, 0x08),
    (0x07, 0x02),
    (0x08, 0x0e),
    (0x09, 0x03),
    (0x0a, 0x05),
    (0x0b, 0x04),
    (0x0c, 0x22),
    (0x0d, 0x26),
    (0x0e, 0x28),
    (0x0f, 0x25),
    (0x10, 0x2e),
    (0x11, 0x2d),
    (0x12, 0x1f),
    (0x13, 0x23),
    (0x14, 0x0c),
    (0x15, 0x0f),
    (0x16, 0x01),
    (0x17, 0x11),
    (0x18, 0x20),
    (0x19, 0x09),
    (0x1a, 0x0d),
    (0x1b, 0x07),
    (0x1c, 0x10),
    (0x1d, 0x06),
    (0x1e, 0x12),
    (0x1f, 0x13),
    (0x20, 0x14),
    (0x21, 0x15),
    (0x22, 0x17),
    (0x23, 0x16),
    (0x24, 0x1a),
    (0x25, 0x1c),
    (0x26, 0x19),
    (0x27, 0x1d),
    (0x28, 0x24),
    (0x29, 0x35),
    (0x2a, 0x33),
    (0x2b, 0x30),
    (0x2c, 0x31),
    (0x2d, 0x1b),
    (0x2e, 0x18),
    (0x2f, 0x21),
    (0x30, 0x1e),
    (0x31, 0x2a),
    (0x33, 0x29),
    (0x34, 0x27),
    (0x35, 0x32),
    (0x36, 0x2b),
    (0x37, 0x2f),
    (0x38, 0x2c),
    (0x39, 0x39),
    (0x3a, 0x7a),
    (0x3b, 0x78),
    (0x3c, 0x63),
    (0x3d, 0x76),
    (0x3e, 0x60),
    (0x3f, 0x61),
    (0x40, 0x62),
    (0x41, 0x64),
    (0x42, 0x65),
    (0x43, 0x6d),
    (0x44, 0x67),
    (0x45, 0x6f),
    (0x49, 0x72),
    (0x4a, 0x73),
    (0x4b, 0x74),
    (0x4c, 0x75),
    (0x4d, 0x77),
    (0x4e, 0x79),
    (0x4f, 0x7c),
    (0x50, 0x7b),
    (0x51, 0x7d),
    (0x52, 0x7e),
    (0x53, 0x47),
    (0x54, 0x4b),
    (0x55, 0x43),
    (0x56, 0x4e),
    (0x57, 0x45),
    (0x58, 0x4c),
    (0x59, 0x53),
    (0x5a, 0x54),
    (0x5b, 0x55),
    (0x5c, 0x56),
    (0x5d, 0x57),
    (0x5e, 0x58),
    (0x5f, 0x59),
    (0x60, 0x5b),
    (0x61, 0x5c),
    (0x62, 0x52),
    (0x63, 0x41),
    (0x64, 0x0a),
    (0x67, 0x51),
    (0x68, 0x69),
    (0x69, 0x6b),
    (0x6a, 0x71),
    (0x6b, 0x6a),
    (0x6c, 0x40),
    (0x6d, 0x4f),
    (0x6e, 0x50),
    (0x6f, 0x5a),
    (0x7f, 0x4a),
    (0x80, 0x48),
    (0x81, 0x49),
    (0x85, 0x5f),
    (0x87, 0x5e),
    (0x89, 0x5d),
    (0x90, 0x68),
    (0x91, 0x66),
    (0xe0, 0x3b),
    (0xe1, 0x38),
    (0xe2, 0x3a),
    (0xe3, 0x37),
    (0xe4, 0x3e),
    (0xe5, 0x3c),
    (0xe6, 0x3d),
    (0xe7, 0x36),
];

const fn key_table(reverse: bool) -> [Option<u16>; 256] {
    let mut table = [None; 256];
    let mut i = 0;
    while i < KEYS.len() {
        let (hid, code) = KEYS[i];
        if reverse {
            table[code as usize] = Some(hid);
        } else {
            table[hid as usize] = Some(code);
        }
        i += 1;
    }
    if !reverse {
        table[0x32] = Some(0x2a);
    } // ISO key beside Return.
    table
}

pub(crate) fn keycode(hid: u16) -> Option<u16> {
    const TABLE: [Option<u16>; 256] = key_table(false);
    TABLE.get(usize::from(hid)).copied().flatten()
}

pub(crate) fn hid_usage(code: u16, keyboard_type: i64) -> Option<u16> {
    const TABLE: [Option<u16>; 256] = key_table(true);
    if code == 0x2a && keyboard_type == 41 {
        return Some(0x32);
    }
    TABLE.get(usize::from(code)).copied().flatten()
}

pub(crate) const CAPS: u64 = 1 << 16;
pub(crate) const SHIFT: u64 = 1 << 17;
pub(crate) const CTRL: u64 = 1 << 18;
pub(crate) const ALT: u64 = 1 << 19;
pub(crate) const CMD: u64 = 1 << 20;
pub(crate) const NUMPAD: u64 = 1 << 21;
const MODIFIERS: [(u64, u64); 8] = [
    (CTRL, 0x1),
    (SHIFT, 0x2),
    (ALT, 0x20),
    (CMD, 0x8),
    (CTRL, 0x2000),
    (SHIFT, 0x4),
    (ALT, 0x40),
    (CMD, 0x10),
];

pub(crate) fn modifier_down(hid: u16, flags: u64, previous: bool) -> Option<bool> {
    if hid == 0x39 {
        return Some(flags & CAPS != 0);
    }
    let index = usize::from(hid.checked_sub(0xe0)?);
    let &(group, side) = MODIFIERS.get(index)?;
    let pair = MODIFIERS[index ^ 4].1;
    Some(
        flags & group != 0
            && (if flags & (side | pair) != 0 {
                flags & side != 0
            } else {
                !previous
            }),
    )
}

pub(crate) fn key_flags(held: &[bool; 256], caps: bool, hid: Option<u16>) -> u64 {
    let mut flags = if caps { CAPS } else { 0 };
    for (index, (group, side)) in MODIFIERS.iter().enumerate() {
        if held[0xe0 + index] {
            flags |= group | side;
        }
    }
    if hid.is_some_and(|key| (0x53..=0x63).contains(&key) || key == 0x67 || key == 0x85) {
        flags |= NUMPAD;
    }
    flags
}

pub(crate) fn shared_key_held(held: &[bool; 256], hid: u16) -> bool {
    match hid {
        0x31 => held[0x32],
        0x32 => held[0x31],
        _ => false,
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct Click {
    time_ms: u64,
    position: Option<Point>,
    pub count: i64,
}
impl Click {
    pub fn press(&mut self, time_ms: u64, position: Point) -> i64 {
        // ponytail: fixed 500 ms/4-point grouping; use native double-click preferences if required.
        self.count = if time_ms.saturating_sub(self.time_ms) <= 500
            && self
                .position
                .is_some_and(|p| (p.x - position.x).abs() <= 4.0 && (p.y - position.y).abs() <= 4.0)
        {
            (self.count + 1).min(3)
        } else {
            1
        };
        self.time_ms = time_ms;
        self.position = Some(position);
        self.count
    }
}

pub(crate) fn button_number(button: Button) -> Option<usize> {
    let number = match button {
        Button::Left => 0,
        Button::Right => 1,
        Button::Middle => 2,
        Button::Back => 3,
        Button::Forward => 4,
        Button::Other(n) => usize::from(n),
    };
    (number < 32).then_some(number)
}

pub(crate) fn button_from_number(n: usize) -> Option<Button> {
    Some(match n {
        0 => Button::Left,
        1 => Button::Right,
        2 => Button::Middle,
        3 => Button::Back,
        4 => Button::Forward,
        5..=31 => Button::Other(n as u16),
        _ => return None,
    })
}

pub(crate) fn movement_type(buttons: &[bool; 32]) -> (u32, usize) {
    if buttons[0] {
        (6, 0)
    } else if buttons[1] {
        (7, 1)
    } else if let Some(n) = buttons.iter().position(|held| *held) {
        (27, n)
    } else {
        (5, 0)
    }
}

#[derive(Clone, Copy, Default)]
pub(crate) struct DisplayRect {
    pub id: u32,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub scale: f64,
}

impl DisplayRect {
    pub fn contains(self, p: Point) -> bool {
        p.x >= self.x && p.y >= self.y && p.x < self.x + self.w && p.y < self.y + self.h
    }
}

pub(crate) fn origin(displays: &[DisplayRect]) -> Option<Point> {
    if displays.is_empty()
        || displays.iter().any(|d| {
            ![d.x, d.y, d.w, d.h, d.scale].iter().all(|v| v.is_finite())
                || d.w <= 0.0
                || d.h <= 0.0
                || d.scale <= 0.0
                || !(d.x + d.w).is_finite()
                || !(d.y + d.h).is_finite()
        })
    {
        return None;
    }
    Some(Point {
        x: displays.iter().map(|d| d.x).fold(f64::INFINITY, f64::min),
        y: displays.iter().map(|d| d.y).fold(f64::INFINITY, f64::min),
    })
}

pub(crate) fn to_local(p: Point, origin: Point) -> Point {
    Point {
        x: p.x - origin.x,
        y: p.y - origin.y,
    }
}
pub(crate) fn to_global(p: Point, origin: Point) -> Option<Point> {
    let p = Point {
        x: p.x + origin.x,
        y: p.y + origin.y,
    };
    (p.x.is_finite() && p.y.is_finite()).then_some(p)
}

pub(crate) fn finite_deltas(dx: f64, dy: f64) -> bool {
    dx.is_finite() && dy.is_finite()
}

pub(crate) fn is_release(kind: InputEventKind) -> bool {
    matches!(
        kind,
        InputEventKind::Key { down: false, .. } | InputEventKind::Button { down: false, .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_tables_cover_ansi_iso_jis_without_aliasing_special_keys() {
        for &(hid, code) in KEYS {
            assert_eq!(keycode(hid), Some(code));
            assert_eq!(hid_usage(code, 40), Some(hid));
        }
        assert_eq!(keycode(0x04), Some(0));
        assert_eq!(keycode(0x28), Some(36));
        assert_eq!(keycode(0x64), Some(10));
        assert_eq!(hid_usage(42, 41), Some(0x32));
        assert_eq!(hid_usage(42, 40), Some(0x31));
        assert_eq!(keycode(0x89), Some(93));
        assert_eq!(keycode(0x87), Some(94));
        assert_eq!(keycode(0x90), Some(104));
        assert_eq!(keycode(0x91), Some(102));
        assert_eq!(keycode(0x46), None); // PrintScreen has no physical Mac counterpart.
        assert_eq!(keycode(0xffff), None);
        assert_eq!(hid_usage(0xffff, 40), None);
        let mut codes = [false; 256];
        for &(_, code) in KEYS {
            assert!(!codes[code as usize]);
            codes[code as usize] = true;
        }
    }

    #[test]
    fn modifiers_preserve_other_side_and_caps_lock() {
        let mut held = [false; 256];
        held[0xe1] = true;
        held[0xe5] = true;
        assert_eq!(key_flags(&held, false, None), SHIFT | 2 | 4);
        held[0xe1] = false;
        assert_eq!(
            key_flags(&held, true, Some(0x59)),
            SHIFT | 4 | CAPS | NUMPAD
        );
        assert_eq!(modifier_down(0xe1, SHIFT | 4, true), Some(false));
        assert_eq!(modifier_down(0xe5, SHIFT | 4, false), Some(true));
        assert_eq!(modifier_down(0xe5, 0, true), Some(false));
        assert_eq!(modifier_down(0x39, CAPS, false), Some(true));
        assert_eq!(modifier_down(0x04, 0, false), None);
    }

    #[test]
    fn buttons_drag_and_extra_button_bounds() {
        let mut held = [false; 32];
        assert_eq!(movement_type(&held), (5, 0));
        held[4] = true;
        assert_eq!(movement_type(&held), (27, 4));
        held[1] = true;
        assert_eq!(movement_type(&held), (7, 1));
        held[0] = true;
        assert_eq!(movement_type(&held), (6, 0));
        assert_eq!(button_number(Button::Other(32)), None);
        for n in 0..32 {
            assert_eq!(button_from_number(n).and_then(button_number), Some(n));
        }
    }

    #[test]
    fn shared_iso_key_stays_down_until_both_usages_release() {
        let mut held = [false; 256];
        held[0x31] = true;
        held[0x32] = true;
        assert!(shared_key_held(&held, 0x31));
        held[0x31] = false;
        assert!(!shared_key_held(&held, 0x32));
        assert!(!shared_key_held(&held, 0xffff));
    }

    #[test]
    fn repeated_clicks_expire_or_reset_after_moving() {
        let mut click = Click::default();
        let p = Point { x: 100.0, y: 200.0 };
        assert_eq!(click.press(0, p), 1);
        assert_eq!(click.press(100, p), 2);
        assert_eq!(click.press(200, p), 3);
        assert_eq!(click.press(1000, p), 1);
        assert_eq!(click.press(1100, Point { x: 105.0, y: 200.0 }), 1);
    }

    #[test]
    fn negative_mixed_retina_coordinates_are_points_not_pixels() {
        // Prevents backing scale changing captured/injected points on a mixed-Retina desk.
        let displays = [
            DisplayRect {
                id: 1,
                x: -1440.0,
                y: -200.0,
                w: 1440.0,
                h: 900.0,
                scale: 2.0,
            },
            DisplayRect {
                id: 2,
                x: 0.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0,
                scale: 1.0,
            },
        ];
        assert_eq!(displays[0].id, 1);
        let base = origin(&displays).expect("valid monitor geometry");
        assert_eq!(
            base,
            Point {
                x: -1440.0,
                y: -200.0
            }
        );
        let local = to_local(Point { x: -100.0, y: 30.0 }, base);
        assert_eq!(
            local,
            Point {
                x: 1340.0,
                y: 230.0
            }
        );
        assert_eq!(to_global(local, base), Some(Point { x: -100.0, y: 30.0 }));
        for display in displays {
            for x in [display.x, display.x + display.w - 1.0] {
                for y in [display.y, display.y + display.h - 1.0] {
                    let global = Point { x, y };
                    let local = to_local(global, base);
                    assert_eq!(to_global(local, base), Some(global));
                    assert_eq!(
                        to_local(to_global(local, base).expect("unmaps"), base),
                        local
                    );
                }
            }
        }
        assert!(displays[0].contains(Point { x: -100.0, y: 30.0 }));
        assert!(!displays[0].contains(Point { x: 0.0, y: 30.0 }));
        assert_eq!(origin(&[]), None);
        let mut invalid = displays;
        invalid[0].scale = f64::NAN;
        assert_eq!(origin(&invalid), None);
        invalid[0] = DisplayRect {
            id: 1,
            x: f64::MAX,
            y: 0.0,
            w: f64::MAX,
            h: 1.0,
            scale: 1.0,
        };
        assert_eq!(origin(&invalid), None);
        assert_eq!(
            to_global(
                Point {
                    x: f64::NAN,
                    y: 0.0
                },
                base
            ),
            None
        );
    }

    #[test]
    fn malformed_native_deltas_fail_closed() {
        assert!(finite_deltas(-0.5, 3.0));
        assert!(!finite_deltas(f64::NAN, 0.0));
        assert!(!finite_deltas(0.0, f64::INFINITY));
    }

    #[test]
    fn secure_input_escape_allows_releases_only() {
        use glide_platform::Key;
        assert!(is_release(InputEventKind::Key {
            key: Key(0x04),
            down: false
        }));
        assert!(is_release(InputEventKind::Button {
            button: Button::Left,
            down: false
        }));
        assert!(!is_release(InputEventKind::Key {
            key: Key(0x04),
            down: true
        }));
        assert!(!is_release(InputEventKind::Button {
            button: Button::Left,
            down: true
        }));
        assert!(!is_release(InputEventKind::Wheel {
            dx: 0.0,
            dy: 1.0,
            precise: false
        }));
    }
}
