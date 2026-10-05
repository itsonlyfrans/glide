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

/// Which modifier keys (HID 0xE0..=0xE7) could be down under these system flags; a key whose group flag is clear is up.
pub(crate) fn modifiers_maybe_down(flags: u64) -> [bool; 8] {
    std::array::from_fn(|index| flags & MODIFIERS[index].0 != 0)
}

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

/// Replays remote cursor moves at the pace they were made. Wi-Fi (and a busy network) delivers mouse packets in clumps
/// with gaps of 10-250 ms; posting each clump as one jump makes the cursor stall and then leap. Every move carries the
/// time it was made on the other computer, so the moves are played back on that timeline, a little behind: only as
/// far behind as recent clumps require (about nothing on a cable, more on Wi-Fi), never more than `max_delay`.
pub(crate) struct MovePacer {
    /// Local clock origin for the arithmetic below.
    epoch: std::time::Instant,
    /// Moves not yet fully played: (time made on the other computer, position), oldest first. The first one is the
    /// point the cursor is travelling from.
    samples: std::collections::VecDeque<(f64, Point)>,
    shown: Option<Point>,
    /// Smallest recent (arrival - made) difference: the clock offset plus the fastest delivery.
    base: Option<f64>,
    base_at: f64,
    /// How far behind playback runs, and when it was last updated.
    delay: f64,
    delay_at: f64,
    /// Playback position on the other computer's timeline; it never runs backwards.
    render: Option<f64>,
    /// The timeline moment the cursor is showing, and when it was last updated (local).
    shown_t: f64,
    last_tick: f64,
    last_arrival: Option<f64>,
    max_delay: f64,
}

/// After a pause this long, the next move is a fresh start and is posted at once (seconds).
const IDLE_GAP: f64 = 0.12;
/// Headroom on top of the latest delivery delay (seconds).
const DELAY_MARGIN: f64 = 0.002;
/// How quickly a clump is forgotten once delivery is steady again (seconds).
const DELAY_FORGET: f64 = 2.0;
/// How fast the fastest-delivery estimate may rise, so clock drift and route changes are followed (seconds per second).
const BASE_DRIFT: f64 = 0.002;
/// A larger jump means the other computer's clock restarted: start the estimate over (seconds).
const RESYNC: f64 = 1.0;
/// After a stall, play back at most this many times faster than real time to catch up, instead of leaping.
const CATCHUP: f64 = 4.0;
/// One 240 Hz display frame (seconds).
const FRAME: f64 = 1.0 / 240.0;
/// Playback limit with smoothing on (Wi-Fi) and off (seconds).
pub(crate) const SMOOTH_DELAY: f64 = 0.05;
pub(crate) const TIGHT_DELAY: f64 = 0.012;

impl MovePacer {
    pub(crate) fn new() -> Self {
        Self::with_max_delay(SMOOTH_DELAY)
    }

    pub(crate) fn with_max_delay(max_delay: f64) -> Self {
        Self {
            epoch: std::time::Instant::now(),
            samples: std::collections::VecDeque::new(),
            shown: None,
            base: None,
            base_at: 0.0,
            delay: 0.0,
            delay_at: 0.0,
            render: None,
            shown_t: 0.0,
            last_tick: 0.0,
            last_arrival: None,
            max_delay,
        }
    }

    /// Limit how far behind playback may run (smoothing on or off).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn set_max_delay(&mut self, max_delay: f64) {
        self.max_delay = max_delay;
        self.delay = self.delay.min(max_delay);
    }

    fn local(&self, now: std::time::Instant) -> f64 {
        now.saturating_duration_since(self.epoch).as_secs_f64()
    }

    /// A new position arrived, made at `made` seconds on the other computer's clock (when known). Returns the point to
    /// post right away, if any; display-frame ticks play the rest while `pending()`.
    pub(crate) fn arrive(
        &mut self,
        p: Point,
        made: Option<f64>,
        now: std::time::Instant,
    ) -> Option<Point> {
        let local = self.local(now);
        let Some(made) = made.filter(|t| t.is_finite()) else {
            // No timing (an older Glide on the other side): post it as it comes.
            self.samples.clear();
            self.render = None;
            self.last_arrival = Some(local);
            return self.jump(p);
        };
        let offset = local - made;
        let base = match self.base {
            Some(base) if offset - base < RESYNC && base - offset < RESYNC => {
                (base + BASE_DRIFT * (local - self.base_at).max(0.0)).min(offset)
            }
            _ => {
                self.delay = 0.0;
                self.render = None;
                self.samples.clear();
                offset
            }
        };
        self.base = Some(base);
        self.base_at = local;
        let late = offset - base;
        let kept = self.delay * (-(local - self.delay_at).max(0.0) / DELAY_FORGET).exp();
        self.delay = (late + DELAY_MARGIN).max(kept).min(self.max_delay);
        self.delay_at = local;

        let fresh =
            self.last_arrival.is_none_or(|last| local - last > IDLE_GAP) || self.samples.is_empty();
        self.last_arrival = Some(local);
        if fresh {
            // The cursor was resting: start right away from here.
            self.samples.clear();
            self.samples.push_back((made, p));
            self.render = Some(made);
            self.shown_t = made;
            self.last_tick = local;
            return self.jump(p);
        }
        if self.samples.back().is_some_and(|(t, _)| made <= *t) {
            // Out of order or a duplicate: the newest position wins, at the newest time.
            if let Some(last) = self.samples.back_mut() {
                last.1 = p;
            }
        } else {
            self.samples.push_back((made, p));
        }
        // A clump of moves arrives at once: post at most once per display frame, the frame timer does the rest.
        if local - self.last_tick >= FRAME {
            self.tick(now)
        } else {
            None
        }
    }

    fn jump(&mut self, p: Point) -> Option<Point> {
        let changed = self.shown != Some(p);
        self.shown = Some(p);
        changed.then_some(p)
    }

    /// Whether display-frame ticks are still needed.
    pub(crate) fn pending(&self) -> bool {
        self.samples.len() > 1
            || self
                .samples
                .front()
                .is_some_and(|(_, p)| self.shown != Some(*p))
    }

    /// One display frame: the next point to post, if the cursor moved.
    pub(crate) fn tick(&mut self, now: std::time::Instant) -> Option<Point> {
        let base = self.base?;
        let local = self.local(now);
        let target = local - base - self.delay;
        let mut at = self.render.map_or(target, |render| render.max(target));
        // Behind after a stall: speed up for a moment rather than leap.
        let step = (local - self.last_tick).clamp(0.0, 2.0 * FRAME);
        at = at.min(self.shown_t + CATCHUP * step).max(self.shown_t);
        self.render = Some(at);
        self.last_tick = local;
        while self.samples.len() > 1 && self.samples[1].0 <= at {
            self.samples.pop_front();
        }
        let (from_t, from) = *self.samples.front()?;
        let next = match self.samples.get(1) {
            Some(&(to_t, to)) if at > from_t => {
                let k = ((at - from_t) / (to_t - from_t)).clamp(0.0, 1.0);
                Point {
                    x: from.x + (to.x - from.x) * k,
                    y: from.y + (to.y - from.y) * k,
                }
            }
            Some(_) => self.shown.unwrap_or(from),
            None => from,
        };
        self.shown_t = if self.samples.len() > 1 {
            at
        } else {
            at.min(from_t)
        };
        self.jump(next)
    }

    /// Jump to the newest position now (before a click, so it lands exactly where the other computer aimed).
    pub(crate) fn flush(&mut self) -> Option<Point> {
        let &(t, p) = self.samples.back()?;
        self.samples.clear();
        self.samples.push_back((t, p));
        self.render = Some(self.render.map_or(t, |render| render.max(t)));
        self.shown_t = t;
        self.jump(p)
    }

    /// Forget everything (control left this computer).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn reset(&mut self) {
        *self = Self::with_max_delay(self.max_delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clear_modifier_group_flag_means_both_sides_are_up() {
        assert_eq!(modifiers_maybe_down(0), [false; 8]);
        assert_eq!(
            modifiers_maybe_down(CTRL | 0x1),
            [true, false, false, false, true, false, false, false]
        );
        assert_eq!(
            modifiers_maybe_down(SHIFT | CMD | CAPS),
            [false, true, false, true, false, true, false, true]
        );
    }

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

    // Plays a stream of moves (arrived ms, made ms, x) through the pacer with 240 Hz display frames and returns the
    // posted x positions with their local times (ms).
    fn play(pacer: &mut MovePacer, arrivals: &[(f64, f64, f64)], until: f64) -> Vec<(f64, f64)> {
        use std::time::Duration;
        let start = pacer.epoch;
        let at = |ms: f64| start + Duration::from_secs_f64(ms / 1000.0);
        let mut posted = Vec::new();
        let mut next = 0;
        let mut frame = 0.0;
        while frame <= until {
            while next < arrivals.len() && arrivals[next].0 <= frame {
                let (arrived, made, x) = arrivals[next];
                if let Some(p) = pacer.arrive(Point { x, y: 0.0 }, Some(made / 1000.0), at(arrived))
                {
                    posted.push((arrived, p.x));
                }
                next += 1;
            }
            if pacer.pending() {
                if let Some(p) = pacer.tick(at(frame)) {
                    posted.push((frame, p.x));
                }
            }
            frame += 1000.0 / 240.0;
        }
        posted
    }

    // Bug: on a MacBook over Wi-Fi the remote cursor stalled and leapt, because moves that arrived in clumps were
    // posted as single jumps. A steady 1 px/ms drag delivered in 30 ms clumps must come out as steady motion.
    #[test]
    fn moves_delivered_in_clumps_play_back_at_the_pace_they_were_made() {
        let mut pacer = MovePacer::new();
        // Made every 1 ms (x = t); the network holds them and hands over each 30 ms batch at once.
        let arrivals: Vec<_> = (0..300)
            .map(|t| {
                let t = f64::from(t);
                let arrived = (t / 30.0).ceil() * 30.0 + 0.5;
                (arrived, t, t)
            })
            .collect();
        let posted = play(&mut pacer, &arrivals, 400.0);
        // Once the pacer has seen a clump, the cursor moves on every frame, never backwards, and never by much more
        // than one frame's worth of motion.
        let settled: Vec<_> = posted
            .iter()
            .copied()
            .filter(|(t, _)| *t > 90.0 && *t < 290.0)
            .collect();
        assert!(
            settled.len() > 40,
            "moved on most frames: {}",
            settled.len()
        );
        for pair in settled.windows(2) {
            let step = pair[1].1 - pair[0].1;
            assert!(step >= 0.0, "never backwards: {pair:?}");
            assert!(step <= 9.0, "no leaps: {pair:?}");
        }
        // And it does not trail far behind the hand.
        let (t, x) = *settled.last().expect("moves");
        assert!(t - x <= SMOOTH_DELAY * 1000.0 + 31.0, "lag {} ms", t - x);
        assert_eq!(
            posted.last().map(|p| p.1),
            Some(299.0),
            "ends exactly where aimed"
        );
    }

    // On a cable moves arrive as they are made; they must be posted at once, with no added delay.
    #[test]
    fn steady_moves_post_at_once() {
        let mut pacer = MovePacer::new();
        let arrivals: Vec<_> = (0..100)
            .map(|t| (f64::from(t) + 0.3, f64::from(t), f64::from(t)))
            .collect();
        let posted = play(&mut pacer, &arrivals, 120.0);
        assert!(!posted.is_empty());
        for (t, x) in posted {
            assert!(t - x <= 4.5 + DELAY_MARGIN * 1000.0, "posted {x} at {t} ms");
        }
    }

    // A click snaps to the exact newest position first, and after a pause the next move is posted at once.
    #[test]
    fn clicks_flush_and_a_fresh_start_is_immediate() {
        use std::time::Duration;
        let mut pacer = MovePacer::new();
        let start = pacer.epoch;
        let at = |ms: f64| start + Duration::from_secs_f64(ms / 1000.0);
        let p = |x: f64| Point { x, y: 0.0 };
        assert_eq!(pacer.arrive(p(0.0), Some(0.0), at(0.0)), Some(p(0.0)));
        pacer.arrive(p(10.0), Some(0.010), at(40.0));
        pacer.arrive(p(20.0), Some(0.020), at(40.0));
        assert_eq!(pacer.flush(), Some(p(20.0)));
        assert!(!pacer.pending());
        assert_eq!(pacer.arrive(p(25.0), Some(0.5), at(540.0)), Some(p(25.0)));
        // Without timing (an older Glide sends the moves) every move is posted as it comes.
        assert_eq!(pacer.arrive(p(30.0), None, at(541.0)), Some(p(30.0)));
        assert!(!pacer.pending());
    }

    // A long Wi-Fi stall is not hidden by lagging further: playback never runs more than the limit behind.
    #[test]
    fn playback_never_lags_more_than_the_limit() {
        let mut pacer = MovePacer::with_max_delay(TIGHT_DELAY);
        // Delivered on time, then one 200 ms stall, then on time again. Playback catches up (faster than real time
        // for a moment, not in one leap) and is soon back within the limit.
        let arrivals: Vec<_> = (0..500)
            .map(|t| {
                let t = f64::from(t);
                let arrived = if (100.0..300.0).contains(&t) {
                    300.0
                } else {
                    t + 0.2
                };
                (arrived, t, t)
            })
            .collect();
        let posted = play(&mut pacer, &arrivals, 520.0);
        let after: Vec<_> = posted
            .iter()
            .copied()
            .filter(|(t, _)| *t > 380.0 && *t < 495.0)
            .collect();
        assert!(!after.is_empty());
        for (t, x) in after {
            assert!(
                t - x <= TIGHT_DELAY * 1000.0 + 5.0,
                "lag {} ms at {t}",
                t - x
            );
        }
    }
}
