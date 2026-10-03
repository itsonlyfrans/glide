//! Virtual desktop geometry and the pure cursor/forwarding state machine.

use glide_platform::{Button, Key, Monitor, Point};
use glide_proto::ipc::Layout;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use thiserror::Error;

const EPSILON: f64 = 1e-7;
const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(1_500);
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl Rect {
    fn contains(self, p: Point) -> bool {
        p.x >= self.left - EPSILON
            && p.x <= self.right + EPSILON
            && p.y >= self.top - EPSILON
            && p.y <= self.bottom + EPSILON
    }

    fn clamp(self, p: Point) -> Point {
        Point {
            x: p.x.clamp(self.left, self.right),
            y: p.y.clamp(self.top, self.bottom),
        }
    }
}

#[derive(Clone, Debug)]
struct Screen {
    device_id: String,
    rect: Rect,
}

#[derive(Debug, Error, PartialEq)]
pub enum LayoutError {
    #[error("layout contains no devices")]
    EmptyLayout,
    #[error("layout contains duplicate device id {0}")]
    DuplicateDevice(String),
    #[error("layout placement for {0} is not finite")]
    InvalidPlacement(String),
    #[error("monitor {monitor_id} on {device_id} has invalid geometry or scale")]
    InvalidMonitor {
        device_id: String,
        monitor_id: String,
    },
    #[error("monitors for {left_device} and {right_device} overlap")]
    OverlappingMonitors {
        left_device: String,
        right_device: String,
    },
    #[error("device {0} has no monitor snapshot")]
    UnknownDevice(String),
    #[error("initial cursor position is not finite")]
    InvalidCursor,
    #[error("desktop has no monitors")]
    EmptyDesktop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EdgeAxis {
    Horizontal,
    Vertical,
}

/// The first direct, shared-edge crossing made by a cursor move.
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeCrossing {
    pub source_device_id: String,
    pub target_device_id: String,
    pub position: Point,
    pub axis: EdgeAxis,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CursorMove {
    pub position: Point,
    pub crossing: Option<EdgeCrossing>,
}

/// Monitor rectangles in global logical pixels. Each layout origin anchors the normalized
/// top-left of that device's monitor bounding box, even when native monitor coordinates are negative.
#[derive(Clone, Debug)]
pub struct Desktop {
    screens: Vec<Screen>,
    origins: HashMap<String, Point>,
}

impl Desktop {
    /// Build a desktop from persisted placement and each device's current monitor list.
    /// Devices without a monitor snapshot remain in `origins` and can be added after discovery.
    pub fn from_layout(
        layout: &Layout,
        monitors: &HashMap<String, Vec<Monitor>>,
    ) -> Result<Self, LayoutError> {
        if layout.devices.is_empty() {
            return Err(LayoutError::EmptyLayout);
        }
        let mut origins = HashMap::with_capacity(layout.devices.len());
        let mut screens = Vec::new();
        for device in &layout.devices {
            if !device.x.is_finite() || !device.y.is_finite() {
                return Err(LayoutError::InvalidPlacement(device.device_id.clone()));
            }
            if origins
                .insert(
                    device.device_id.clone(),
                    Point {
                        x: device.x,
                        y: device.y,
                    },
                )
                .is_some()
            {
                return Err(LayoutError::DuplicateDevice(device.device_id.clone()));
            }
            let Some(device_monitors) = monitors.get(&device.device_id) else {
                continue;
            };
            if device_monitors.is_empty() {
                continue;
            }
            let mut min_x = f64::INFINITY;
            let mut min_y = f64::INFINITY;
            let mut monitor_ids = HashSet::with_capacity(device_monitors.len());
            for monitor in device_monitors {
                if monitor.id.is_empty()
                    || !monitor_ids.insert(monitor.id.as_str())
                    || !monitor.x.is_finite()
                    || !monitor.y.is_finite()
                    || !monitor.w.is_finite()
                    || !monitor.h.is_finite()
                    || !monitor.scale.is_finite()
                    || monitor.w <= 0.0
                    || monitor.h <= 0.0
                    || monitor.scale <= 0.0
                    || !(monitor.w * monitor.scale).is_finite()
                    || !(monitor.h * monitor.scale).is_finite()
                    || monitor.w * monitor.scale <= 0.0
                    || monitor.h * monitor.scale <= 0.0
                {
                    return Err(LayoutError::InvalidMonitor {
                        device_id: device.device_id.clone(),
                        monitor_id: monitor.id.clone(),
                    });
                }
                min_x = min_x.min(monitor.x);
                min_y = min_y.min(monitor.y);
            }
            for monitor in device_monitors {
                let left = device.x + monitor.x - min_x;
                let top = device.y + monitor.y - min_y;
                let right = left + monitor.w;
                let bottom = top + monitor.h;
                if !left.is_finite()
                    || !top.is_finite()
                    || !right.is_finite()
                    || !bottom.is_finite()
                    || right <= left
                    || bottom <= top
                {
                    return Err(LayoutError::InvalidMonitor {
                        device_id: device.device_id.clone(),
                        monitor_id: monitor.id.clone(),
                    });
                }
                screens.push(Screen {
                    device_id: device.device_id.clone(),
                    rect: Rect {
                        left,
                        top,
                        right,
                        bottom,
                    },
                });
            }
        }
        if screens.is_empty() {
            return Err(LayoutError::EmptyDesktop);
        }
        for (index, left) in screens.iter().enumerate() {
            for right in screens.iter().skip(index + 1) {
                if left.rect.left < right.rect.right - EPSILON
                    && right.rect.left < left.rect.right - EPSILON
                    && left.rect.top < right.rect.bottom - EPSILON
                    && right.rect.top < left.rect.bottom - EPSILON
                {
                    return Err(LayoutError::OverlappingMonitors {
                        left_device: left.device_id.clone(),
                        right_device: right.device_id.clone(),
                    });
                }
            }
        }
        Ok(Self { screens, origins })
    }

    /// Return a clamped, axis-slid position. Movement cannot cross an uncovered gap.
    pub fn move_cursor(&self, current: Point, delta: Point) -> CursorMove {
        let source = self.device_at(current, None);
        self.move_cursor_from(current, delta, source)
    }

    /// As `move_cursor`, preferring the caller's owner at a shared boundary.
    pub fn move_cursor_from(
        &self,
        current: Point,
        delta: Point,
        source_device: Option<&str>,
    ) -> CursorMove {
        if !current.x.is_finite()
            || !current.y.is_finite()
            || !delta.x.is_finite()
            || !delta.y.is_finite()
        {
            return CursorMove {
                position: self.clamp_to_union(current),
                crossing: None,
            };
        }
        let start = self.clamp_to_union(current);
        let source = source_device.or_else(|| self.device_at(start, None));
        let after_x = Point {
            x: self.slide_axis(start, start.x + delta.x, true),
            y: start.y,
        };
        let end = Point {
            x: after_x.x,
            y: self.slide_axis(after_x, after_x.y + delta.y, false),
        };
        let crossing = source.and_then(|source| self.first_crossing(start, after_x, end, source));
        CursorMove {
            position: end,
            crossing,
        }
    }

    /// Clamp movement to one device's monitor union when no peer edge was crossed.
    pub fn move_cursor_on_device(&self, current: Point, delta: Point, device_id: &str) -> Point {
        if !current.x.is_finite()
            || !current.y.is_finite()
            || !delta.x.is_finite()
            || !delta.y.is_finite()
        {
            return self.clamp_to_device(current, device_id);
        }
        let start = self.clamp_to_device(current, device_id);
        let after_x = Point {
            x: self.slide_axis_for(start, start.x + delta.x, true, Some(device_id)),
            y: start.y,
        };
        Point {
            x: after_x.x,
            y: self.slide_axis_for(after_x, after_x.y + delta.y, false, Some(device_id)),
        }
    }

    /// Device-local logical coordinates are relative to the device monitor bounding-box origin.
    pub fn global_to_device_logical(&self, device_id: &str, position: Point) -> Option<Point> {
        if !position.x.is_finite() || !position.y.is_finite() {
            return None;
        }
        let origin = self.origins.get(device_id)?;
        let local = Point {
            x: position.x - origin.x,
            y: position.y - origin.y,
        };
        (local.x.is_finite() && local.y.is_finite()).then_some(local)
    }

    pub fn device_to_global_logical(&self, device_id: &str, position: Point) -> Option<Point> {
        if !position.x.is_finite() || !position.y.is_finite() {
            return None;
        }
        let origin = self.origins.get(device_id)?;
        let global = Point {
            x: position.x + origin.x,
            y: position.y + origin.y,
        };
        (global.x.is_finite() && global.y.is_finite()).then_some(global)
    }

    fn device_at(&self, point: Point, preferred: Option<&str>) -> Option<&str> {
        self.screens
            .iter()
            .find(|screen| {
                preferred == Some(screen.device_id.as_str()) && screen.rect.contains(point)
            })
            .or_else(|| {
                self.screens
                    .iter()
                    .find(|screen| screen.rect.contains(point))
            })
            .map(|screen| screen.device_id.as_str())
    }

    fn clamp_to_union(&self, point: Point) -> Point {
        if !point.x.is_finite() || !point.y.is_finite() {
            return self.screens[0].rect.clamp(Point { x: 0.0, y: 0.0 });
        }
        if self
            .screens
            .iter()
            .any(|screen| screen.rect.contains(point))
        {
            return point;
        }
        self.screens
            .iter()
            .map(|screen| screen.rect.clamp(point))
            .min_by(|a, b| {
                let da = (a.x - point.x).hypot(a.y - point.y);
                let db = (b.x - point.x).hypot(b.y - point.y);
                da.total_cmp(&db)
            })
            .unwrap_or(point)
    }

    fn clamp_to_device(&self, point: Point, device_id: &str) -> Point {
        let point = if point.x.is_finite() && point.y.is_finite() {
            point
        } else {
            Point { x: 0.0, y: 0.0 }
        };
        if point.x.is_finite()
            && point.y.is_finite()
            && self
                .screens
                .iter()
                .any(|screen| screen.device_id == device_id && screen.rect.contains(point))
        {
            return point;
        }
        self.screens
            .iter()
            .filter(|screen| screen.device_id == device_id)
            .map(|screen| screen.rect.clamp(point))
            .min_by(|a, b| {
                let da = (a.x - point.x).hypot(a.y - point.y);
                let db = (b.x - point.x).hypot(b.y - point.y);
                da.total_cmp(&db)
            })
            .unwrap_or_else(|| self.screens[0].rect.clamp(Point { x: 0.0, y: 0.0 }))
    }

    fn slide_axis(&self, start: Point, destination: f64, horizontal: bool) -> f64 {
        self.slide_axis_for(start, destination, horizontal, None)
    }

    fn slide_axis_for(
        &self,
        start: Point,
        destination: f64,
        horizontal: bool,
        device_id: Option<&str>,
    ) -> f64 {
        let fixed = if horizontal { start.y } else { start.x };
        let initial = if horizontal { start.x } else { start.y };
        let mut low = f64::INFINITY;
        let mut high = f64::NEG_INFINITY;
        for screen in self
            .screens
            .iter()
            .filter(|screen| device_id.is_none_or(|device_id| screen.device_id == device_id))
        {
            let spans_fixed = if horizontal {
                fixed >= screen.rect.top - EPSILON && fixed <= screen.rect.bottom + EPSILON
            } else {
                fixed >= screen.rect.left - EPSILON && fixed <= screen.rect.right + EPSILON
            };
            let (start, end) = if horizontal {
                (screen.rect.left, screen.rect.right)
            } else {
                (screen.rect.top, screen.rect.bottom)
            };
            if spans_fixed && initial >= start - EPSILON && initial <= end + EPSILON {
                low = low.min(start);
                high = high.max(end);
            }
        }
        if !low.is_finite() {
            return initial;
        }
        if destination.is_nan() {
            return initial;
        }
        if destination == f64::INFINITY {
            return high;
        }
        if destination == f64::NEG_INFINITY {
            return low;
        }
        loop {
            let mut extended = false;
            for screen in self
                .screens
                .iter()
                .filter(|screen| device_id.is_none_or(|device_id| screen.device_id == device_id))
            {
                let spans_fixed = if horizontal {
                    fixed >= screen.rect.top - EPSILON && fixed <= screen.rect.bottom + EPSILON
                } else {
                    fixed >= screen.rect.left - EPSILON && fixed <= screen.rect.right + EPSILON
                };
                if !spans_fixed {
                    continue;
                }
                let (next_low, next_high) = if horizontal {
                    (screen.rect.left, screen.rect.right)
                } else {
                    (screen.rect.top, screen.rect.bottom)
                };
                if next_low <= high + EPSILON && next_high > high {
                    high = next_high;
                    extended = true;
                }
                if next_high >= low - EPSILON && next_low < low {
                    low = next_low;
                    extended = true;
                }
            }
            if !extended {
                break;
            }
        }
        destination.clamp(low, high)
    }

    fn first_crossing(
        &self,
        start: Point,
        after_x: Point,
        end: Point,
        source: &str,
    ) -> Option<EdgeCrossing> {
        let mut found: Option<(f64, EdgeCrossing)> = None;
        for (segment, (from, to)) in [(start, after_x), (after_x, end)].into_iter().enumerate() {
            for source_screen in self
                .screens
                .iter()
                .filter(|screen| screen.device_id == source)
            {
                for target_screen in self
                    .screens
                    .iter()
                    .filter(|screen| screen.device_id != source)
                {
                    if let Some((t, position, axis)) =
                        shared_edge_crossing(source_screen.rect, target_screen.rect, from, to)
                    {
                        let crossing = EdgeCrossing {
                            source_device_id: source.to_owned(),
                            target_device_id: target_screen.device_id.clone(),
                            position,
                            axis,
                        };
                        let global_t = if segment == 0 { t * 0.5 } else { 0.5 + t * 0.5 };
                        if found.as_ref().is_none_or(|(best, _)| global_t < *best) {
                            found = Some((global_t, crossing));
                        }
                    }
                }
            }
        }
        found.map(|(_, crossing)| crossing)
    }
}

fn shared_edge_crossing(
    source: Rect,
    target: Rect,
    from: Point,
    to: Point,
) -> Option<(f64, Point, EdgeAxis)> {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let candidates = [
        (
            source.right,
            target.left,
            1.0,
            from.x,
            dx,
            from.y,
            dy,
            source.top.max(target.top),
            source.bottom.min(target.bottom),
            EdgeAxis::Horizontal,
        ),
        (
            source.left,
            target.right,
            -1.0,
            from.x,
            dx,
            from.y,
            dy,
            source.top.max(target.top),
            source.bottom.min(target.bottom),
            EdgeAxis::Horizontal,
        ),
        (
            source.bottom,
            target.top,
            1.0,
            from.y,
            dy,
            from.x,
            dx,
            source.left.max(target.left),
            source.right.min(target.right),
            EdgeAxis::Vertical,
        ),
        (
            source.top,
            target.bottom,
            -1.0,
            from.y,
            dy,
            from.x,
            dx,
            source.left.max(target.left),
            source.right.min(target.right),
            EdgeAxis::Vertical,
        ),
    ];
    candidates
        .into_iter()
        .filter_map(
            |(
                source_edge,
                target_edge,
                direction,
                origin,
                movement,
                orth_origin,
                orth_delta,
                overlap_low,
                overlap_high,
                axis,
            )| {
                if (source_edge - target_edge).abs() > EPSILON
                    || overlap_high <= overlap_low + EPSILON
                    || direction * movement <= EPSILON
                {
                    return None;
                }
                let t = (source_edge - origin) / movement;
                if !(-EPSILON..=1.0 + EPSILON).contains(&t) {
                    return None;
                }
                let orth = orth_origin + orth_delta * t;
                if orth < overlap_low - EPSILON || orth > overlap_high + EPSILON {
                    return None;
                }
                let position = if axis == EdgeAxis::Horizontal {
                    Point {
                        x: source_edge,
                        y: orth,
                    }
                } else {
                    Point {
                        x: orth,
                        y: source_edge,
                    }
                };
                Some((t.clamp(0.0, 1.0), position, axis))
            },
        )
        .min_by(|a, b| a.0.total_cmp(&b.0))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeSettings {
    pub edge_delay: Duration,
    pub corner_dead_zone_px: f64,
    /// A second attempt at the same edge within 500 ms bypasses the dwell delay.
    pub double_tap: bool,
}

impl Default for EdgeSettings {
    fn default() -> Self {
        Self {
            edge_delay: Duration::from_millis(300),
            corner_dead_zone_px: 8.0,
            double_tap: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaveReason {
    ReturnedHome,
    SwitchedPeer,
    TakeOver,
    LinkLost,
    HeartbeatTimeout,
    PermissionRevoked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEvent {
    Key { key: Key, down: bool },
    Button { button: Button, down: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    Enter {
        device_id: String,
        position: Point,
        modifiers_down: Vec<Key>,
    },
    Leave {
        device_id: String,
        reason: LeaveReason,
        releases: Vec<InputEvent>,
    },
    Input {
        device_id: String,
        input: InputEvent,
    },
    TakeOver {
        device_id: String,
        position: Point,
    },
    BrainChanged {
        device_id: String,
        position: Point,
    },
}

#[derive(Clone, Debug)]
struct PendingEdge {
    crossing: EdgeCrossing,
    destination: Point,
    started: Instant,
}

/// Pure switching state: callers supply captured events and time; this type performs no I/O.
#[derive(Clone, Debug)]
pub struct EdgeEngine {
    desktop: Desktop,
    home_device: String,
    brain_device: String,
    cursor_device: String,
    cursor: Point,
    home_cursor: Point,
    forwarding_to: Option<String>,
    last_heartbeat: Option<Instant>,
    pending: Option<PendingEdge>,
    last_edge_attempt: Option<(String, Instant)>,
    source_keys: Vec<Key>,
    injected: Vec<InputEvent>,
    settings: EdgeSettings,
    forward_speed: f64,
    forward_acceleration: f64,
    last_forwarded_move: Option<Instant>,
    smoothed_speed: f64,
}

/// Speed (logical pixels per millisecond) at which acceleration has reached half of its full effect.
const ACCELERATION_KNEE: f64 = 2.0;

impl EdgeEngine {
    pub fn new(
        desktop: Desktop,
        home_device: impl Into<String>,
        cursor: Point,
        settings: EdgeSettings,
        now: Instant,
    ) -> Result<Self, LayoutError> {
        let home_device = home_device.into();
        if !cursor.x.is_finite() || !cursor.y.is_finite() {
            return Err(LayoutError::InvalidCursor);
        }
        if !desktop.origins.contains_key(&home_device)
            || !desktop
                .screens
                .iter()
                .any(|screen| screen.device_id == home_device)
        {
            return Err(LayoutError::UnknownDevice(home_device));
        }
        let cursor = desktop.clamp_to_device(cursor, &home_device);
        Ok(Self {
            desktop,
            home_device: home_device.clone(),
            brain_device: home_device.clone(),
            cursor_device: home_device.clone(),
            cursor,
            home_cursor: cursor,
            forwarding_to: None,
            last_heartbeat: Some(now),
            pending: None,
            last_edge_attempt: None,
            source_keys: Vec::new(),
            injected: Vec::new(),
            settings,
            forward_speed: 1.0,
            forward_acceleration: 0.0,
            last_forwarded_move: None,
            smoothed_speed: 0.0,
        })
    }

    /// Make fast mouse movement travel further than slow movement while controlling another computer (0.0 = off).
    pub fn set_forward_acceleration(&mut self, acceleration: f64) {
        if acceleration.is_finite() && acceleration >= 0.0 {
            self.forward_acceleration = acceleration;
        }
    }

    /// Total movement gain for this sample: the user's pointer speed times a gain that grows with how fast the mouse
    /// is moving. Speed is smoothed so one-pixel steps from a mouse do not make the gain flicker.
    fn forward_gain(&mut self, delta: Point, now: Instant) -> f64 {
        if self.forward_acceleration <= 0.0 {
            return self.forward_speed;
        }
        let elapsed_ms = self
            .last_forwarded_move
            .map_or(8.0, |last| {
                now.saturating_duration_since(last).as_secs_f64() * 1000.0
            })
            .clamp(1.0, 30.0);
        self.last_forwarded_move = Some(now);
        let speed = delta.x.hypot(delta.y) / elapsed_ms;
        self.smoothed_speed = 0.7 * self.smoothed_speed + 0.3 * speed;
        self.forward_speed
            * (1.0
                + self.forward_acceleration * self.smoothed_speed
                    / (self.smoothed_speed + ACCELERATION_KNEE))
    }

    /// Scale mouse movement while this computer is controlling another one; local movement is never scaled.
    pub fn set_forward_speed(&mut self, speed: f64) {
        if speed.is_finite() && speed > 0.0 {
            self.forward_speed = speed;
        }
    }

    pub fn cursor(&self) -> Point {
        self.cursor
    }
    pub fn brain_device(&self) -> &str {
        &self.brain_device
    }
    pub fn forwarding_to(&self) -> Option<&str> {
        self.forwarding_to.as_deref()
    }

    /// Read the current target cursor without allocating; send this as the latest-wins mouse datagram.
    pub fn forwarded_position(&self) -> Option<(&str, Point)> {
        let device_id = self.forwarding_to.as_deref()?;
        Some((
            device_id,
            self.desktop
                .global_to_device_logical(device_id, self.cursor)?,
        ))
    }

    /// While the cursor is on this computer, believe the operating system about where it really is. The engine adds up
    /// movement amounts, so any mismatch (acceleration rounding, a cursor jump, a missed event) would otherwise grow
    /// until the engine thinks the cursor is at an edge it has not reached. `after_move` is the cursor position the OS
    /// reported with this movement (device space) and `delta` the movement itself; call this just before `move_by`.
    /// A small mismatch is ignored so an OS that stops the cursor at a screen edge still lets the edge be crossed.
    pub fn sync_home_cursor(&mut self, after_move: Point, delta: Point) {
        const DRIFT_TOLERANCE_PX: f64 = 4.0;
        if self.forwarding_to.is_some()
            || self.cursor_device != self.home_device
            || self.pending.is_some()
        {
            return;
        }
        let Some(real) = self
            .desktop
            .device_to_global_logical(&self.home_device, after_move)
        else {
            return;
        };
        let expected = self
            .desktop
            .move_cursor_on_device(self.cursor, delta, &self.home_device);
        if (expected.x - real.x).hypot(expected.y - real.y) <= DRIFT_TOLERANCE_PX {
            return;
        }
        let before = Point {
            x: real.x - delta.x,
            y: real.y - delta.y,
        };
        self.cursor = self.desktop.clamp_to_device(before, &self.home_device);
        self.home_cursor = self.cursor;
    }

    /// Apply a captured logical-pixel delta. The destination is held at a candidate edge until its dwell expires.
    pub fn move_by(&mut self, delta: Point, now: Instant) -> Vec<EngineEvent> {
        let mut events = self.tick(now);
        let delta = if self.forwarding_to.is_some() {
            let gain = self.forward_gain(delta, now);
            Point {
                x: delta.x * gain,
                y: delta.y * gain,
            }
        } else {
            self.last_forwarded_move = None;
            self.smoothed_speed = 0.0;
            delta
        };
        let movement = self
            .desktop
            .move_cursor_from(self.cursor, delta, Some(&self.cursor_device));
        if let Some(crossing) = movement.crossing {
            if !self.edge_is_dead_zone(&crossing, self.settings.corner_dead_zone_px) {
                let pending_start = self.pending.as_ref().and_then(|pending| {
                    (pending.crossing.target_device_id == crossing.target_device_id)
                        .then_some(pending.started)
                });
                let repeated = pending_start.is_none()
                    && self.settings.double_tap
                    && self.last_edge_attempt.as_ref().is_some_and(|(device, at)| {
                        device == &crossing.target_device_id
                            && now.saturating_duration_since(*at) <= DOUBLE_TAP_WINDOW
                    });
                if pending_start.is_none() {
                    self.last_edge_attempt = Some((crossing.target_device_id.clone(), now));
                } else if let Some(started) = pending_start {
                    if now.saturating_duration_since(started) >= self.settings.edge_delay {
                        events.extend(self.transition(
                            crossing.target_device_id.clone(),
                            movement.position,
                            now,
                        ));
                        return events;
                    }
                }
                if repeated || self.settings.edge_delay.is_zero() {
                    events.extend(self.transition(
                        crossing.target_device_id.clone(),
                        movement.position,
                        now,
                    ));
                } else {
                    self.cursor = crossing.position;
                    if self.forwarding_to.is_none() && self.cursor_device == self.home_device {
                        self.home_cursor = self.cursor;
                    }
                    self.pending = Some(PendingEdge {
                        crossing,
                        destination: movement.position,
                        started: pending_start.unwrap_or(now),
                    });
                }
                return events;
            }
            self.pending = None;
            self.cursor = crossing.position;
            if self.forwarding_to.is_none() && self.cursor_device == self.home_device {
                self.home_cursor = self.cursor;
            }
            return events;
        }

        self.pending = None;
        self.cursor = self
            .desktop
            .move_cursor_on_device(self.cursor, delta, &self.cursor_device);
        if self.forwarding_to.is_none() && self.cursor_device == self.home_device {
            self.home_cursor = self.cursor;
        }
        events
    }

    /// Complete edge dwell and return home after three missed 500 ms heartbeats.
    pub fn tick(&mut self, now: Instant) -> Vec<EngineEvent> {
        if self.forwarding_to.is_some()
            && self
                .last_heartbeat
                .is_some_and(|last| now.saturating_duration_since(last) >= HEARTBEAT_TIMEOUT)
        {
            return self.return_home_with_reason(LeaveReason::HeartbeatTimeout);
        }
        if let Some((target, destination)) = self.pending.as_ref().and_then(|pending| {
            (now.saturating_duration_since(pending.started) >= self.settings.edge_delay).then(
                || {
                    (
                        pending.crossing.target_device_id.clone(),
                        pending.destination,
                    )
                },
            )
        }) {
            self.pending = None;
            return self.transition(target, destination, now);
        }
        Vec::new()
    }

    pub fn heartbeat_received(&mut self, device_id: &str, now: Instant) {
        if self.forwarding_to.as_deref() == Some(device_id) {
            self.last_heartbeat = Some(now);
        }
    }

    pub fn link_lost(&mut self) -> Vec<EngineEvent> {
        self.return_home_with_reason(LeaveReason::LinkLost)
    }
    pub fn permission_revoked(&mut self) -> Vec<EngineEvent> {
        self.return_home_with_reason(LeaveReason::PermissionRevoked)
    }
    pub fn return_home(&mut self) -> Vec<EngineEvent> {
        self.return_home_with_reason(LeaveReason::ReturnedHome)
    }

    /// Ignore injected hook events. Non-injected input on the receiving peer preempts forwarding.
    pub fn local_input(
        &mut self,
        device_id: &str,
        injected: bool,
        local_position: Point,
        _now: Instant,
    ) -> Vec<EngineEvent> {
        if injected || self.forwarding_to.as_deref() != Some(device_id) {
            return Vec::new();
        }
        let Some(position) = self
            .desktop
            .device_to_global_logical(device_id, local_position)
        else {
            return Vec::new();
        };
        let mut events = self.leave_forwarding(LeaveReason::TakeOver);
        self.home_device = device_id.to_owned();
        self.brain_device = device_id.to_owned();
        self.cursor_device = device_id.to_owned();
        self.cursor = self.desktop.clamp_to_device(position, device_id);
        self.home_cursor = self.cursor;
        self.source_keys.clear();
        self.pending = None;
        self.last_heartbeat = None;
        events.push(EngineEvent::TakeOver {
            device_id: device_id.to_owned(),
            position: local_position,
        });
        events.push(EngineEvent::BrainChanged {
            device_id: device_id.to_owned(),
            position: local_position,
        });
        events
    }

    /// Observe a physical source key so a later Enter event can synchronize its modifier state.
    pub fn observe_source_key(&mut self, key: Key, down: bool) {
        if !(0xE0..=0xE7).contains(&key.0) {
            return;
        }
        if down {
            if !self.source_keys.contains(&key) {
                self.source_keys.push(key);
            }
        } else {
            self.source_keys.retain(|held| *held != key);
        }
    }

    /// Convenience path for an untranslated event: observe it and send the same HID usage.
    pub fn key_event(&mut self, key: Key, down: bool) -> Option<EngineEvent> {
        self.observe_source_key(key, down);
        let target = self.forwarding_to.clone()?;
        let input = InputEvent::Key { key, down };
        self.track_injected(input);
        Some(EngineEvent::Input {
            device_id: target,
            input,
        })
    }

    pub fn button_event(&mut self, button: Button, down: bool) -> Option<EngineEvent> {
        let target = self.forwarding_to.clone()?;
        let input = InputEvent::Button { button, down };
        self.track_injected(input);
        Some(EngineEvent::Input {
            device_id: target,
            input,
        })
    }

    fn transition(&mut self, target: String, destination: Point, now: Instant) -> Vec<EngineEvent> {
        let from = self.forwarding_to.clone();
        let mut events = Vec::new();
        if let Some(old) = from {
            if old != target {
                let reason = if target == self.home_device {
                    LeaveReason::ReturnedHome
                } else {
                    LeaveReason::SwitchedPeer
                };
                events.extend(self.leave_forwarding(reason));
            }
        }
        self.cursor = self.desktop.clamp_to_device(destination, &target);
        self.cursor_device = target.clone();
        self.pending = None;
        if target == self.home_device {
            if self.forwarding_to.is_some() {
                events.extend(self.leave_forwarding(LeaveReason::ReturnedHome));
            }
            self.cursor = self.desktop.clamp_to_device(destination, &self.home_device);
            self.cursor_device = self.home_device.clone();
            self.last_heartbeat = Some(now);
            return events;
        }
        let Some(local) = self.desktop.global_to_device_logical(&target, self.cursor) else {
            return events;
        };
        if self.forwarding_to.as_deref() != Some(&target) {
            self.forwarding_to = Some(target.clone());
            self.last_heartbeat = Some(now);
            let mut modifiers_down = self.source_keys.clone();
            modifiers_down.sort_by_key(|key| key.0);
            events.push(EngineEvent::Enter {
                device_id: target,
                position: local,
                modifiers_down,
            });
        }
        events
    }

    fn return_home_with_reason(&mut self, reason: LeaveReason) -> Vec<EngineEvent> {
        self.pending = None;
        let mut events = self.leave_forwarding(reason);
        self.brain_device = self.home_device.clone();
        self.cursor_device = self.home_device.clone();
        self.cursor = self
            .desktop
            .clamp_to_device(self.home_cursor, &self.home_device);
        self.last_heartbeat = None;
        if !events.is_empty() {
            if let Some(position) = self
                .desktop
                .global_to_device_logical(&self.home_device, self.cursor)
            {
                events.push(EngineEvent::BrainChanged {
                    device_id: self.home_device.clone(),
                    position,
                });
            }
        }
        events
    }

    fn leave_forwarding(&mut self, reason: LeaveReason) -> Vec<EngineEvent> {
        let Some(device_id) = self.forwarding_to.take() else {
            return Vec::new();
        };
        let releases = self
            .injected
            .drain(..)
            .map(|input| match input {
                InputEvent::Key { key, .. } => InputEvent::Key { key, down: false },
                InputEvent::Button { button, .. } => InputEvent::Button {
                    button,
                    down: false,
                },
            })
            .collect();
        self.last_heartbeat = None;
        vec![EngineEvent::Leave {
            device_id,
            reason,
            releases,
        }]
    }

    fn track_injected(&mut self, input: InputEvent) {
        match input {
            InputEvent::Key { key, down } => {
                if down {
                    let held = InputEvent::Key { key, down: true };
                    if !self.injected.contains(&held) {
                        self.injected.push(held);
                    }
                } else {
                    self.injected
                        .retain(|held| *held != InputEvent::Key { key, down: true });
                }
            }
            InputEvent::Button { button, down } => {
                if down {
                    let held = InputEvent::Button { button, down: true };
                    if !self.injected.contains(&held) {
                        self.injected.push(held);
                    }
                } else {
                    self.injected
                        .retain(|held| *held != InputEvent::Button { button, down: true });
                }
            }
        }
    }

    fn edge_is_dead_zone(&self, crossing: &EdgeCrossing, dead_zone: f64) -> bool {
        if !dead_zone.is_finite() || dead_zone <= 0.0 {
            return false;
        }
        let Some(source) = self.desktop.screens.iter().find(|screen| {
            screen.device_id == crossing.source_device_id && screen.rect.contains(crossing.position)
        }) else {
            return true;
        };
        let Some(target) = self.desktop.screens.iter().find(|screen| {
            screen.device_id == crossing.target_device_id && screen.rect.contains(crossing.position)
        }) else {
            return true;
        };
        let (coordinate, low, high) = if crossing.axis == EdgeAxis::Horizontal {
            (
                crossing.position.y,
                source.rect.top.max(target.rect.top),
                source.rect.bottom.min(target.rect.bottom),
            )
        } else {
            (
                crossing.position.x,
                source.rect.left.max(target.rect.left),
                source.rect.right.min(target.rect.right),
            )
        };
        coordinate - low < dead_zone || high - coordinate < dead_zone
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glide_proto::ipc::LayoutDevice;

    mod allocation_counter {
        use std::{
            alloc::{GlobalAlloc, Layout, System},
            cell::Cell,
        };

        thread_local! {
            static ENABLED: Cell<bool> = const { Cell::new(false) };
            static COUNT: Cell<usize> = const { Cell::new(0) };
        }

        pub struct CountingAllocator;

        // SAFETY: Allocation and deallocation are delegated unchanged to the system allocator.
        unsafe impl GlobalAlloc for CountingAllocator {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                let _ = ENABLED
                    .try_with(|enabled| {
                        if enabled.get() {
                            COUNT.with(|count| count.set(count.get() + 1))
                        }
                    })
                    .ok();
                // SAFETY: `layout` is forwarded unchanged to `System`.
                unsafe { System.alloc(layout) }
            }

            unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
                // SAFETY: The pointer and layout came from `System` through this allocator.
                unsafe { System.dealloc(pointer, layout) }
            }

            unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
                let _ = ENABLED
                    .try_with(|enabled| {
                        if enabled.get() {
                            COUNT.with(|count| count.set(count.get() + 1))
                        }
                    })
                    .ok();
                // SAFETY: The pointer and layout came from `System` through this allocator.
                unsafe { System.realloc(pointer, layout, size) }
            }
        }

        #[global_allocator]
        static ALLOCATOR: CountingAllocator = CountingAllocator;

        pub fn count_allocations(run: impl FnOnce()) -> usize {
            COUNT.with(|count| count.set(0));
            ENABLED.with(|enabled| enabled.set(true));
            run();
            ENABLED.with(|enabled| enabled.set(false));
            COUNT.with(Cell::get)
        }
    }

    fn monitor(id: &str, x: f64, y: f64, w: f64, h: f64, scale: f64) -> Monitor {
        Monitor {
            id: id.to_owned(),
            x,
            y,
            w,
            h,
            scale,
            primary: false,
        }
    }

    fn desktop() -> Desktop {
        let layout = Layout {
            devices: vec![
                LayoutDevice {
                    device_id: "win".into(),
                    x: 0.0,
                    y: 0.0,
                },
                LayoutDevice {
                    device_id: "mac".into(),
                    x: 200.0,
                    y: 0.0,
                },
                LayoutDevice {
                    device_id: "low".into(),
                    x: 400.0,
                    y: 100.0,
                },
            ],
        };
        let monitors = HashMap::from([
            (
                "win".into(),
                vec![
                    monitor("left", -100.0, 0.0, 100.0, 100.0, 1.0),
                    monitor("main", 0.0, 0.0, 100.0, 100.0, 1.0),
                ],
            ),
            (
                "mac".into(),
                vec![
                    monitor("retina", 0.0, 0.0, 100.0, 100.0, 2.0),
                    monitor("external", 100.0, 0.0, 100.0, 100.0, 1.5),
                ],
            ),
            (
                "low".into(),
                vec![monitor("low", 0.0, 0.0, 100.0, 100.0, 1.0)],
            ),
        ]);
        Desktop::from_layout(&layout, &monitors).expect("valid desktop")
    }

    #[test]
    fn monitor_scale_metadata_does_not_rescale_device_cursor() {
        // Prevents mixed-DPI monitor metadata changing the shared logical cursor space.
        let desktop = desktop();
        for (device, global, local) in [
            (
                "win",
                Point { x: 25.0, y: 20.0 },
                Point { x: 25.0, y: 20.0 },
            ),
            (
                "mac",
                Point { x: 220.0, y: 20.0 },
                Point { x: 20.0, y: 20.0 },
            ),
            (
                "mac",
                Point { x: 320.0, y: 20.0 },
                Point { x: 120.0, y: 20.0 },
            ),
        ] {
            assert_eq!(
                desktop.global_to_device_logical(device, global),
                Some(local)
            );
            assert_eq!(
                desktop.device_to_global_logical(device, local),
                Some(global)
            );
        }
        assert_eq!(
            desktop.device_to_global_logical(
                "mac",
                Point {
                    x: f64::NAN,
                    y: 0.0
                }
            ),
            None
        );
    }

    #[test]
    fn cursor_clamps_and_slides_at_gaps_without_jumping() {
        let desktop = desktop();
        let moved = desktop.move_cursor(
            Point { x: 50.0, y: 50.0 },
            Point {
                x: f64::MAX,
                y: 0.0,
            },
        );
        assert_eq!(moved.position, Point { x: 400.0, y: 50.0 });
        assert_eq!(
            moved
                .crossing
                .as_ref()
                .map(|crossing| crossing.target_device_id.as_str()),
            Some("mac")
        );
        let moved = desktop.move_cursor(Point { x: 50.0, y: 50.0 }, Point { x: 50.0, y: 80.0 });
        assert_eq!(moved.position, Point { x: 100.0, y: 100.0 });
        let overflow = desktop.move_cursor(
            Point { x: 150.0, y: 50.0 },
            Point {
                x: f64::MAX,
                y: 0.0,
            },
        );
        assert_eq!(overflow.position, Point { x: 400.0, y: 50.0 });
        let rejected = desktop.move_cursor(
            Point { x: 50.0, y: 50.0 },
            Point {
                x: f64::INFINITY,
                y: 2.0,
            },
        );
        assert_eq!(rejected.position, Point { x: 50.0, y: 50.0 });
        let tiny = desktop.move_cursor(
            Point { x: 50.0, y: 50.0 },
            Point {
                x: f64::EPSILON,
                y: f64::EPSILON,
            },
        );
        assert!(tiny.position.x >= 50.0 && tiny.position.y >= 50.0);
        let disconnected = desktop.move_cursor(
            Point { x: 50.0, y: 50.0 },
            Point {
                x: 0.0,
                y: f64::MAX,
            },
        );
        assert_eq!(disconnected.position, Point { x: 50.0, y: 100.0 });
        let invalid = desktop.move_cursor(
            Point {
                x: f64::NAN,
                y: 5.0,
            },
            Point { x: 1.0, y: 1.0 },
        );
        assert_eq!(invalid.crossing, None);
    }

    #[test]
    fn adjacent_crossing_reports_target_and_edge() {
        let desktop = desktop();
        let moved = desktop.move_cursor(Point { x: 190.0, y: 50.0 }, Point { x: 20.0, y: 0.0 });
        let edge = moved.crossing.expect("edge crossing");
        assert_eq!(edge.source_device_id, "win");
        assert_eq!(edge.target_device_id, "mac");
        assert_eq!(edge.position, Point { x: 200.0, y: 50.0 });
    }

    // Bug: on the MacBook the cursor crossed to Windows after only a quarter of the screen, because the engine's own
    // running cursor drifted ahead of the real one and it believed an edge had been reached.
    #[test]
    fn a_drifted_engine_cursor_is_pulled_back_to_the_real_one_so_edges_are_not_reached_early() {
        let start = Instant::now();
        let settings = EdgeSettings {
            edge_delay: Duration::ZERO,
            corner_dead_zone_px: 0.0,
            double_tap: false,
        };
        // The engine wrongly believes the cursor is 2 px from the right edge while the OS says it is mid-screen.
        let mut engine = EdgeEngine::new(
            desktop(),
            "win",
            Point { x: 198.0, y: 50.0 },
            settings,
            start,
        )
        .expect("engine");
        engine.sync_home_cursor(Point { x: 45.0, y: 50.0 }, Point { x: 5.0, y: 0.0 });
        assert!(engine.move_by(Point { x: 5.0, y: 0.0 }, start).is_empty());
        assert_eq!(engine.forwarding_to(), None);
        assert_eq!(engine.cursor(), Point { x: 45.0, y: 50.0 });
        // A real approach still crosses: the OS stops the cursor at the edge while the movement keeps pushing.
        engine.sync_home_cursor(Point { x: 195.0, y: 50.0 }, Point { x: 150.0, y: 0.0 });
        assert!(engine.move_by(Point { x: 150.0, y: 0.0 }, start).is_empty());
        engine.sync_home_cursor(Point { x: 199.0, y: 50.0 }, Point { x: 20.0, y: 0.0 });
        let events = engine.move_by(Point { x: 20.0, y: 0.0 }, start);
        assert!(
            matches!(events.as_slice(), [EngineEvent::Enter { device_id, .. }] if device_id == "mac"),
            "{events:?}"
        );
    }

    // Bug: moving the mouse on the Windows PC felt slow on the Mac. Pointer speed scales movement only while the
    // cursor is on another computer, and never changes how the local cursor moves.
    #[test]
    fn pointer_speed_scales_movement_only_while_controlling_another_computer() {
        let start = Instant::now();
        let settings = EdgeSettings {
            edge_delay: Duration::ZERO,
            corner_dead_zone_px: 0.0,
            double_tap: false,
        };
        let build = |speed: f64| {
            let mut engine = EdgeEngine::new(
                desktop(),
                "win",
                Point { x: 100.0, y: 50.0 },
                settings,
                start,
            )
            .expect("engine");
            engine.set_forward_speed(speed);
            engine
        };
        let mut slow = build(1.0);
        let mut fast = build(2.5);
        // At home the same movement lands in the same place whatever the speed.
        slow.move_by(Point { x: 10.0, y: 0.0 }, start);
        fast.move_by(Point { x: 10.0, y: 0.0 }, start);
        assert_eq!(slow.cursor(), fast.cursor());
        for engine in [&mut slow, &mut fast] {
            let events = engine.move_by(Point { x: 200.0, y: 0.0 }, start);
            assert!(
                matches!(events.as_slice(), [EngineEvent::Enter { .. }]),
                "{events:?}"
            );
        }
        let before_slow = slow.forwarded_position().expect("forwarding").1;
        let before_fast = fast.forwarded_position().expect("forwarding").1;
        slow.move_by(Point { x: 4.0, y: 0.0 }, start);
        fast.move_by(Point { x: 4.0, y: 0.0 }, start);
        let moved_slow = slow.forwarded_position().expect("forwarding").1.x - before_slow.x;
        let moved_fast = fast.forwarded_position().expect("forwarding").1.x - before_fast.x;
        assert!((moved_slow - 4.0).abs() < 1e-9, "{moved_slow}");
        assert!((moved_fast - 10.0).abs() < 1e-9, "{moved_fast}");
    }

    // Bug: a Windows mouse felt sluggish on the Mac next to its trackpad, because every movement was applied in a
    // straight line. With acceleration, quick movement travels further per mouse step than slow movement, and with
    // acceleration off nothing changes.
    #[test]
    fn pointer_acceleration_makes_fast_movement_travel_further_than_slow_movement() {
        let start = Instant::now();
        let settings = EdgeSettings {
            edge_delay: Duration::ZERO,
            corner_dead_zone_px: 0.0,
            double_tap: false,
        };
        let forwarding = |acceleration: f64| {
            let mut engine = EdgeEngine::new(
                desktop(),
                "win",
                Point { x: 100.0, y: 50.0 },
                settings,
                start,
            )
            .expect("engine");
            engine.set_forward_acceleration(acceleration);
            let events = engine.move_by(Point { x: 200.0, y: 0.0 }, start);
            assert!(
                matches!(events.as_slice(), [EngineEvent::Enter { .. }]),
                "{events:?}"
            );
            engine
        };
        // Pixels the target cursor travels per pixel of mouse movement over 10 steady steps.
        let gain = |engine: &mut EdgeEngine, step: f64, every_ms: u64| {
            let before = engine.forwarded_position().expect("forwarding").1.x;
            for n in 1..=10 {
                engine.move_by(
                    Point { x: step, y: 0.0 },
                    start + Duration::from_millis(1 + n * every_ms),
                );
            }
            let after = engine.forwarded_position().expect("forwarding").1.x;
            (after - before) / (10.0 * step)
        };
        let mut off = forwarding(0.0);
        assert!((gain(&mut off, 1.0, 8) - 1.0).abs() < 1e-9);
        assert!((gain(&mut off, 5.0, 1) - 1.0).abs() < 1e-9);
        let mut on = forwarding(2.0);
        let slow = gain(&mut on, 1.0, 8);
        let mut on = forwarding(2.0);
        let fast = gain(&mut on, 5.0, 1);
        assert!(slow < 1.3, "slow movement stays precise, got {slow}");
        assert!(
            fast > 1.8,
            "quick movement travels clearly further, got {fast}"
        );
    }

    #[test]
    fn dwell_dead_zone_double_tap_and_forwarding_transitions() {
        let desktop = desktop();
        let start = Instant::now();
        let settings = EdgeSettings {
            edge_delay: Duration::from_millis(100),
            corner_dead_zone_px: 10.0,
            double_tap: false,
        };
        let mut corner = EdgeEngine::new(
            desktop.clone(),
            "win",
            Point { x: 190.0, y: 5.0 },
            settings,
            start,
        )
        .expect("engine");
        assert!(corner.move_by(Point { x: 20.0, y: 0.0 }, start).is_empty());
        assert_eq!(corner.forwarding_to(), None);
        let mut engine = EdgeEngine::new(
            desktop.clone(),
            "win",
            Point { x: 190.0, y: 50.0 },
            settings,
            start,
        )
        .expect("engine");
        assert!(engine.move_by(Point { x: 20.0, y: 0.0 }, start).is_empty());
        assert_eq!(engine.forwarding_to(), None);
        assert!(engine
            .move_by(
                Point { x: -10.0, y: 0.0 },
                start + Duration::from_millis(25)
            )
            .is_empty());
        assert!(engine.tick(start + Duration::from_millis(101)).is_empty());
        assert!(engine
            .move_by(
                Point { x: 20.0, y: 0.0 },
                start + Duration::from_millis(110)
            )
            .is_empty());
        let entered = engine.tick(start + Duration::from_millis(211));
        assert!(
            matches!(entered.as_slice(), [EngineEvent::Enter { device_id, .. }] if device_id == "mac")
        );
        assert_eq!(engine.forwarding_to(), Some("mac"));

        let settings = EdgeSettings {
            edge_delay: Duration::from_secs(1),
            corner_dead_zone_px: 0.0,
            double_tap: true,
        };
        let mut quick =
            EdgeEngine::new(desktop, "win", Point { x: 190.0, y: 50.0 }, settings, start)
                .expect("engine");
        assert!(quick.move_by(Point { x: 20.0, y: 0.0 }, start).is_empty());
        assert!(quick
            .move_by(
                Point { x: -10.0, y: 0.0 },
                start + Duration::from_millis(20)
            )
            .is_empty());
        let entered = quick.move_by(Point { x: 20.0, y: 0.0 }, start + Duration::from_millis(40));
        assert!(
            matches!(entered.as_slice(), [EngineEvent::Enter { device_id, .. }] if device_id == "mac")
        );
    }

    #[test]
    fn stuck_inputs_release_and_heartbeat_returns_home() {
        let start = Instant::now();
        let mut engine = EdgeEngine::new(
            desktop(),
            "win",
            Point { x: 190.0, y: 50.0 },
            EdgeSettings {
                corner_dead_zone_px: 0.0,
                edge_delay: Duration::ZERO,
                double_tap: false,
            },
            start,
        )
        .expect("engine");
        assert!(matches!(
            engine.move_by(Point { x: 20.0, y: 0.0 }, start).as_slice(),
            [EngineEvent::Enter { .. }]
        ));
        assert!(engine.key_event(Key(0x04), true).is_some());
        assert!(engine.button_event(Button::Left, true).is_some());
        let events = engine.tick(start + HEARTBEAT_TIMEOUT);
        assert!(
            matches!(events.as_slice(), [EngineEvent::Leave { reason: LeaveReason::HeartbeatTimeout, releases, .. }, EngineEvent::BrainChanged { device_id, .. }]
            if releases.len() == 2 && device_id == "win")
        );
        assert_eq!(engine.forwarding_to(), None);
        assert_eq!(engine.cursor(), Point { x: 190.0, y: 50.0 });
    }

    #[test]
    fn injected_input_does_not_take_over_but_physical_input_does() {
        let start = Instant::now();
        let mut engine = EdgeEngine::new(
            desktop(),
            "win",
            Point { x: 190.0, y: 50.0 },
            EdgeSettings {
                corner_dead_zone_px: 0.0,
                edge_delay: Duration::ZERO,
                double_tap: false,
            },
            start,
        )
        .expect("engine");
        engine.move_by(Point { x: 20.0, y: 0.0 }, start);
        assert!(engine
            .local_input("mac", true, Point { x: 20.0, y: 40.0 }, start)
            .is_empty());
        let events = engine.local_input("mac", false, Point { x: 20.0, y: 40.0 }, start);
        assert!(
            matches!(events.last(), Some(EngineEvent::BrainChanged { device_id, .. }) if device_id == "mac")
        );
        assert_eq!(engine.brain_device(), "mac");
        assert_eq!(engine.forwarding_to(), None);
    }

    #[test]
    fn heartbeat_refresh_and_return_across_the_edge_release_forwarding() {
        let start = Instant::now();
        let mut engine = EdgeEngine::new(
            desktop(),
            "win",
            Point { x: 190.0, y: 50.0 },
            EdgeSettings {
                corner_dead_zone_px: 0.0,
                edge_delay: Duration::ZERO,
                double_tap: false,
            },
            start,
        )
        .expect("engine");
        engine.move_by(Point { x: 20.0, y: 0.0 }, start);
        engine.heartbeat_received("mac", start + Duration::from_millis(1_000));
        assert!(engine.tick(start + HEARTBEAT_TIMEOUT).is_empty());
        let events = engine.move_by(
            Point { x: -20.0, y: 0.0 },
            start + Duration::from_millis(1_100),
        );
        assert!(
            matches!(events.as_slice(), [EngineEvent::Leave { device_id, reason: LeaveReason::ReturnedHome, .. }] if device_id == "mac")
        );
        assert_eq!(engine.forwarding_to(), None);
    }

    #[test]
    fn overlapping_devices_are_rejected() {
        let layout = Layout {
            devices: vec![
                LayoutDevice {
                    device_id: "a".into(),
                    x: 0.0,
                    y: 0.0,
                },
                LayoutDevice {
                    device_id: "b".into(),
                    x: 50.0,
                    y: 0.0,
                },
            ],
        };
        let monitors = HashMap::from([
            ("a".into(), vec![monitor("a", 0.0, 0.0, 100.0, 100.0, 1.0)]),
            ("b".into(), vec![monitor("b", 0.0, 0.0, 100.0, 100.0, 1.0)]),
        ]);
        assert!(matches!(
            Desktop::from_layout(&layout, &monitors),
            Err(LayoutError::OverlappingMonitors { .. })
        ));
    }

    #[test]
    fn invalid_or_ambiguous_monitor_geometry_is_rejected() {
        let layout = Layout {
            devices: vec![LayoutDevice {
                device_id: "win".into(),
                x: 0.0,
                y: 0.0,
            }],
        };
        let duplicate_ids = HashMap::from([(
            "win".into(),
            vec![
                monitor("same", 0.0, 0.0, 100.0, 100.0, 1.0),
                monitor("same", 100.0, 0.0, 100.0, 100.0, 1.0),
            ],
        )]);
        assert!(matches!(
            Desktop::from_layout(&layout, &duplicate_ids),
            Err(LayoutError::InvalidMonitor { .. })
        ));

        let collapsed = HashMap::from([(
            "win".into(),
            vec![monitor("large", 0.0, 0.0, 100.0, 100.0, f64::MAX)],
        )]);
        assert!(matches!(
            Desktop::from_layout(&layout, &collapsed),
            Err(LayoutError::InvalidMonitor { .. })
        ));
    }

    #[test]
    fn engine_starts_inside_its_home_device_union() {
        let start = Instant::now();
        let engine = EdgeEngine::new(
            desktop(),
            "win",
            Point { x: 450.0, y: 150.0 },
            EdgeSettings::default(),
            start,
        )
        .expect("engine");
        assert_eq!(engine.cursor(), Point { x: 200.0, y: 100.0 });
        assert_eq!(engine.brain_device(), "win");
    }

    #[test]
    fn leave_causes_return_home_and_clear_forwarding() {
        let start = Instant::now();
        for (reason, leave) in [
            (
                LeaveReason::LinkLost,
                EdgeEngine::link_lost as fn(&mut EdgeEngine) -> Vec<EngineEvent>,
            ),
            (
                LeaveReason::PermissionRevoked,
                EdgeEngine::permission_revoked as fn(&mut EdgeEngine) -> Vec<EngineEvent>,
            ),
        ] {
            let mut engine = EdgeEngine::new(
                desktop(),
                "win",
                Point { x: 190.0, y: 50.0 },
                EdgeSettings {
                    corner_dead_zone_px: 0.0,
                    edge_delay: Duration::ZERO,
                    double_tap: false,
                },
                start,
            )
            .expect("engine");
            engine.move_by(Point { x: 20.0, y: 0.0 }, start);
            assert!(matches!(
                leave(&mut engine).as_slice(),
                [EngineEvent::Leave { reason: actual, .. }, EngineEvent::BrainChanged { device_id, .. }]
                    if *actual == reason && device_id == "win"
            ));
            assert_eq!(engine.forwarding_to(), None);
            assert_eq!(engine.cursor(), Point { x: 190.0, y: 50.0 });
        }
    }

    #[test]
    fn corner_touching_peer_does_not_switch() {
        let start = Instant::now();
        let mut engine = EdgeEngine::new(
            desktop(),
            "mac",
            Point { x: 390.0, y: 90.0 },
            EdgeSettings {
                corner_dead_zone_px: 0.0,
                edge_delay: Duration::ZERO,
                double_tap: false,
            },
            start,
        )
        .expect("engine");
        assert!(engine.move_by(Point { x: 20.0, y: 20.0 }, start).is_empty());
        assert_eq!(engine.forwarding_to(), None);
        assert_eq!(engine.cursor(), Point { x: 400.0, y: 100.0 });
    }

    #[test]
    fn one_large_motion_crosses_only_the_first_peer_boundary() {
        let layout = Layout {
            devices: vec![
                LayoutDevice {
                    device_id: "a".into(),
                    x: 0.0,
                    y: 0.0,
                },
                LayoutDevice {
                    device_id: "b".into(),
                    x: 100.0,
                    y: 0.0,
                },
                LayoutDevice {
                    device_id: "c".into(),
                    x: 200.0,
                    y: 0.0,
                },
            ],
        };
        let monitors = HashMap::from([
            ("a".into(), vec![monitor("a", 0.0, 0.0, 100.0, 100.0, 1.0)]),
            ("b".into(), vec![monitor("b", 0.0, 0.0, 100.0, 100.0, 1.0)]),
            ("c".into(), vec![monitor("c", 0.0, 0.0, 100.0, 100.0, 1.0)]),
        ]);
        let start = Instant::now();
        let mut engine = EdgeEngine::new(
            Desktop::from_layout(&layout, &monitors).expect("desktop"),
            "a",
            Point { x: 90.0, y: 50.0 },
            EdgeSettings {
                corner_dead_zone_px: 0.0,
                edge_delay: Duration::ZERO,
                double_tap: false,
            },
            start,
        )
        .expect("engine");
        let events = engine.move_by(
            Point {
                x: f64::MAX,
                y: 0.0,
            },
            start,
        );
        assert!(matches!(
            events.as_slice(),
            [EngineEvent::Enter { device_id, position, .. }]
                if device_id == "b" && *position == (Point { x: 100.0, y: 50.0 })
        ));
        assert_eq!(engine.forwarding_to(), Some("b"));
        assert_eq!(engine.cursor(), Point { x: 200.0, y: 50.0 });
    }

    #[test]
    fn steady_forwarded_mouse_path_allocates_nothing() {
        let start = Instant::now();
        let mut engine = EdgeEngine::new(
            desktop(),
            "win",
            Point { x: 190.0, y: 50.0 },
            EdgeSettings {
                corner_dead_zone_px: 0.0,
                edge_delay: Duration::ZERO,
                double_tap: false,
            },
            start,
        )
        .expect("engine");
        assert!(matches!(
            engine.move_by(Point { x: 20.0, y: 0.0 }, start).as_slice(),
            [EngineEvent::Enter { .. }]
        ));
        let allocations = allocation_counter::count_allocations(|| {
            for index in 0..1_000 {
                let events = engine.move_by(
                    Point { x: 0.125, y: 0.25 },
                    start + Duration::from_millis(index),
                );
                assert!(events.is_empty());
                assert!(engine.forwarded_position().is_some());
            }
        });
        assert_eq!(allocations, 0);
    }
}
