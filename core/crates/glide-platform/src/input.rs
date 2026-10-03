use crate::{BackendError, Permissions};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// The operating system represented by a platform backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    /// Microsoft Windows.
    Windows,
    /// macOS.
    Macos,
}

#[cfg(test)]
mod status_tests {
    use super::*;
    use crate::MockInput;
    #[test]
    fn outside_cursor_clamps_to_a_real_monitor_and_metadata_does_not_move_it() {
        // Prevents coordinates in a monitor gap or outside the desktop ending input sharing.
        let monitors = [
            Monitor {
                id: "left".into(),
                x: 0.0,
                y: 10.0,
                w: 100.0,
                h: 100.0,
                scale: 2.0,
                primary: true,
            },
            Monitor {
                id: "right".into(),
                x: 200.0,
                y: 0.0,
                w: 100.0,
                h: 100.0,
                scale: 1.0,
                primary: false,
            },
        ];
        assert_eq!(
            clamp_cursor_to_monitors(&monitors, Point { x: 190.0, y: 50.0 }),
            Some(Point { x: 200.0, y: 50.0 })
        );
        assert_eq!(
            clamp_cursor_to_monitors(&monitors, Point { x: -10.0, y: -10.0 }),
            Some(Point { x: 0.0, y: 10.0 })
        );
        assert_eq!(
            clamp_cursor_to_monitors(&monitors, Point { x: 50.0, y: 60.0 }),
            Some(Point { x: 50.0, y: 60.0 })
        );
        let p = clamp_cursor_to_monitors(&monitors, Point { x: 300.0, y: 100.0 })
            .expect("last edge clamps");
        assert!(p.x < 300.0 && p.y < 100.0 && p.x >= 200.0);
        assert_eq!(clamp_cursor_to_monitors(&[], p), None);
        assert_eq!(
            clamp_cursor_to_monitors(
                &monitors,
                Point {
                    x: f64::NAN,
                    y: 0.0
                }
            ),
            None
        );
    }
    #[test]
    fn native_loss_latches_even_when_status_queue_is_full() {
        let (sink, events) = InputSink::bounded(1).expect("sink");
        let backend = MockInput::default();
        backend.start_capture(sink.clone()).expect("capture");
        let statuses = backend.capture_status_changes();
        for _ in 0..32 {
            backend
                .script_capture_status(CaptureStatus::SecureInput(false))
                .expect("status");
        }
        backend
            .script_capture_status(CaptureStatus::QueueOverflow)
            .expect("loss");
        assert!(sink.take_overflow());
        assert!(!sink.take_overflow());
        assert!(events.is_empty());
        assert!(statuses.len() <= 16);
        assert_eq!(backend.mode().expect("mode"), CaptureMode::Local);
        assert_eq!(backend.secure_input_enabled(), Some(false));
        for status in [
            CaptureStatus::SecureInput(true),
            CaptureStatus::PermissionLost,
            CaptureStatus::TapDisabled,
            CaptureStatus::UnsupportedInput,
        ] {
            while statuses.try_recv().is_ok() {}
            backend.script_capture_status(status).expect("status");
            assert_eq!(statuses.try_recv().expect("notification"), status);
            assert!(sink.take_overflow());
        }
    }
}

/// A logical-pixel coordinate in a device's local monitor space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Horizontal logical-pixel coordinate.
    pub x: f64,
    /// Vertical logical-pixel coordinate.
    pub y: f64,
}

/// One display in a uniform device-wide logical space, origin at the monitor bounding-box
/// top-left. Windows divides ALL physical coordinates/extents by the primary DPI scale;
/// macOS uses global CG display points without scaling. Gaps and shared edges are preserved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Monitor {
    /// Stable native display identifier for this running system.
    pub id: String,
    /// Left edge relative to the device's local top-left origin.
    pub x: f64,
    /// Top edge relative to the device's local top-left origin.
    pub y: f64,
    /// Display width in logical pixels.
    pub w: f64,
    /// Display height in logical pixels.
    pub h: f64,
    /// Informational native DPI/backing scale. Never rescale device coordinates with this
    /// value: on Windows only the primary scale defines the device-wide logical pixel.
    pub scale: f64,
    /// Whether the OS marks this as the primary display.
    pub primary: bool,
}

/// Clamp a finite cursor to the nearest point of the nearest monitor, preserving gaps.
/// Right/bottom edges are exclusive, as in native display rectangles. No allocation.
pub fn clamp_cursor_to_monitors(monitors: &[Monitor], point: Point) -> Option<Point> {
    if !point.x.is_finite() || !point.y.is_finite() {
        return None;
    }
    monitors
        .iter()
        .filter_map(|m| {
            let right = m.x + m.w;
            let bottom = m.y + m.h;
            if !m.x.is_finite()
                || !m.y.is_finite()
                || !right.is_finite()
                || !bottom.is_finite()
                || m.w <= 0.0
                || m.h <= 0.0
            {
                return None;
            }
            let candidate = Point {
                x: point.x.clamp(m.x, right.next_down().max(m.x)),
                y: point.y.clamp(m.y, bottom.next_down().max(m.y)),
            };
            Some(candidate)
        })
        .min_by(|a, b| {
            (a.x - point.x)
                .hypot(a.y - point.y)
                .total_cmp(&(b.x - point.x).hypot(b.y - point.y))
        })
}

/// A USB HID keyboard usage from usage page 0x07.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Key(pub u16);

/// A mouse button reported by the host or injected on a target.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    /// Primary (usually left) button.
    Left,
    /// Secondary (usually right) button.
    Right,
    /// Middle button.
    Middle,
    /// Browser back button.
    Back,
    /// Browser forward button.
    Forward,
    /// A platform button not covered by the common names above.
    Other(u16),
}

/// One input sample crossing the platform boundary.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputEvent {
    /// The key, button, pointer, or wheel change.
    pub kind: InputEventKind,
    /// True when the OS identified the event as synthetic/injected.
    pub injected: bool,
}

/// The allocation-free payload captured or injected by a backend.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum InputEventKind {
    /// An absolute local cursor position and high-resolution movement delta, both in logical pixels.
    PointerMoved {
        /// Current cursor position in logical pixels when the OS provides one.
        position: Point,
        /// Horizontal movement since the preceding sample, converted to logical pixels.
        delta_x: f64,
        /// Vertical movement since the preceding sample, converted to logical pixels.
        delta_y: f64,
    },
    /// A USB HID keyboard usage changed state.
    Key {
        /// Physical HID usage on page 0x07.
        key: Key,
        /// True on key-down and false on key-up.
        down: bool,
    },
    /// A mouse button changed state.
    Button {
        /// The button whose state changed.
        button: Button,
        /// True on button-down and false on button-up.
        down: bool,
    },
    /// A high-resolution wheel sample.
    Wheel {
        /// Horizontal wheel delta in native high-resolution units.
        dx: f64,
        /// Vertical wheel delta in native high-resolution units.
        dy: f64,
        /// True for precise trackpad scrolling rather than detented wheel motion.
        precise: bool,
    },
}

/// Bounded, non-blocking path from native capture callbacks to daemon logic.
#[derive(Clone)]
pub struct InputSink {
    sender: Sender<InputEvent>,
    overflowed: Arc<AtomicBool>,
}

/// Failure to enqueue a captured input event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputSinkError {
    /// The queue is full; the caller must activate the local-input escape path.
    Full,
    /// The daemon stopped receiving captured input.
    Disconnected,
    /// A sink cannot be created with a zero-capacity channel.
    InvalidCapacity,
}

impl InputSink {
    /// Creates a bounded input queue and its consumer. Capacity must be positive.
    pub fn bounded(capacity: usize) -> Result<(Self, Receiver<InputEvent>), InputSinkError> {
        if capacity == 0 {
            return Err(InputSinkError::InvalidCapacity);
        }
        let (sender, receiver) = bounded(capacity);
        Ok((
            Self {
                sender,
                overflowed: Arc::new(AtomicBool::new(false)),
            },
            receiver,
        ))
    }

    /// Attempts to enqueue one copyable event without waiting or allocating.
    ///
    /// A full or disconnected queue latches overflow. The native hook must switch its atomic
    /// capture mode to local immediately; the daemon must check `take_overflow` and release any
    /// forwarded keys/buttons on its next control turn.
    pub fn try_push(&self, event: InputEvent) -> Result<(), InputSinkError> {
        match self.sender.try_send(event) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.overflowed.store(true, Ordering::Release);
                Err(InputSinkError::Full)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.overflowed.store(true, Ordering::Release);
                Err(InputSinkError::Disconnected)
            }
        }
    }

    /// Clears and returns the overflow latch so the daemon can perform its escape action once.
    pub fn take_overflow(&self) -> bool {
        self.overflowed.swap(false, Ordering::AcqRel)
    }

    /// Latches loss in a backend's native queue, independent of this queue's capacity.
    pub fn mark_overflow(&self) {
        self.overflowed.store(true, Ordering::Release);
    }
}

/// Native capture transitions. Receivers must return home and release forwarded
/// input on every loss (SecureInput(false) is a recovery notification).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureStatus {
    /// Secure Input is separate from the user's permission grants.
    SecureInput(bool),
    /// An OS permission was revoked.
    PermissionLost,
    /// The native tap stopped capturing.
    TapDisabled,
    /// A physical sample cannot be represented safely.
    UnsupportedInput,
    /// A native or sink queue lost input.
    QueueOverflow,
}

/// Capture policy while the daemon owns or forwards input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureMode {
    /// Observe physical input and leave it available to local applications.
    Local,
    /// Consume physical input; optionally hold the local cursor in place while doing so.
    Swallow {
        /// If true, keep the local cursor fixed or recentered while reporting logical-pixel deltas.
        lock_pos: bool,
    },
}

/// Native input capture and event injection.
///
/// Methods are called from daemon worker threads, never from native callbacks. Backends own
/// their hooks and marshal thread-affine OS operations internally. A successful synchronous
/// call means the requested operation is applied; an error must not hide a pending operation.
/// Dropping the backend removes its hooks and restores local capture.
pub trait InputBackend: Send + Sync {
    /// Smooth remote cursor moves that arrive in bursts (macOS only; elsewhere moves are always posted at once).
    fn set_move_smoothing(&self, _on: bool) {}

    /// Authoritative Secure Input state where supported; independent of permissions.
    fn secure_input_enabled(&self) -> Option<bool> {
        None
    }

    /// Bounded, nonblocking capture-status stream. On notification pressure a
    /// backend must also latch capture loss through InputSink::mark_overflow so
    /// orchestration cannot miss an escape. Default backends emit nothing.
    fn capture_status_changes(&self) -> Receiver<CaptureStatus> {
        bounded(1).1
    }

    /// Starts capture and sends samples to `sink` using only non-blocking bounded operations.
    ///
    /// Native hook/tap callbacks must do O(1) work, must not call daemon logic, and must not
    /// block. Physical samples have `injected == false`; synthetic events recognized by the OS
    /// retain `injected == true`. If enqueue fails because the queue is full or disconnected,
    /// the callback drops the sample and atomically switches capture to `Local` before returning;
    /// the daemon observes the sink overflow latch and releases forwarded input. Calling this
    /// again replaces the current sink without creating another capture hook.
    fn start_capture(&self, sink: InputSink) -> Result<(), BackendError>;

    /// Changes whether physical input remains local or is swallowed by the daemon.
    ///
    /// Implementations apply the new mode promptly on the capture thread. In `Swallow` mode,
    /// movement must still report high-resolution deltas in logical pixels even if `lock_pos`
    /// pins or recenters the visible local cursor. Failure leaves the previous mode active.
    fn set_mode(&self, mode: CaptureMode) -> Result<(), BackendError>;

    /// Hides the inactive computer's OS cursor or restores it. Default is a no-op for
    /// backends without visibility support. Independent of capture mode; native backends
    /// must guard repeated hides and restore on Drop/panic. Visibility failure is cosmetic
    /// and must never revoke injection permission or end a link.
    fn set_cursor_visible(&self, _visible: bool) -> Result<(), BackendError> {
        Ok(())
    }

    /// Turns this computer's screen back on when the cursor arrives from another computer, as if the person had
    /// touched its own mouse. Best effort and cosmetic; it cannot unlock a locked screen. Default is a no-op.
    fn wake_display(&self) -> Result<(), BackendError> {
        Ok(())
    }

    /// Injects one event into this machine and marks it with the backend's native magic value.
    ///
    /// The event must not be re-captured as physical input or trigger local-input takeover.
    /// Key/button state is tracked by the implementation so `release_all` can clear it after
    /// every leave, disconnect, or daemon shutdown. Finite positions outside monitors are
    /// clamped to the nearest monitor. Errors are classified by BackendError::injection_failure;
    /// only PermissionDenied represents lost OS permission.
    fn inject(&self, event: InputEvent) -> Result<(), BackendError>;

    /// Releases every key and mouse button this backend has injected, even after partial failure.
    ///
    /// This is the stuck-input safety guard and must be safe to call repeatedly. The backend
    /// should attempt all releases and return an error if any release could not be delivered.
    fn release_all(&self) -> Result<(), BackendError>;

    /// Returns a fresh monitor snapshot with positions relative to the device's local origin.
    fn monitors(&self) -> Result<Vec<Monitor>, BackendError>;

    /// Returns the bounded notification receiver for monitor topology changes.
    ///
    /// A notification may be coalesced or dropped under pressure; on every notification the
    /// daemon must call `monitors` for an authoritative snapshot. Repeated calls may clone a
    /// receiver that shares the same stream.
    fn monitor_changes(&self) -> Receiver<Vec<Monitor>>;

    /// Returns the local cursor position in logical pixels.
    fn local_cursor_pos(&self) -> Result<Point, BackendError>;

    /// Returns current input-capture and injection permission states.
    fn permissions(&self) -> Permissions;

    /// Requests OS permissions and returns their status immediately after the call.
    ///
    /// Must be safe and cheap to call repeatedly and never wait for user interaction.
    /// Native prompts (including macOS TCC) are asynchronous. Backends without
    /// permission prompts simply return the current status.
    fn request_permissions(&self) -> Permissions {
        self.permissions()
    }
}
