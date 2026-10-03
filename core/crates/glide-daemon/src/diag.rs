//! The cursor report: timings of every step a shared cursor move takes, so a slow or uneven cursor can be traced to
//! its cause (the mouse, the network, or the computer that moves the cursor) instead of guessed at.
//! Only counts and timings are kept: never positions, keys or anything typed.

use glide_platform::MoveTimings;
use glide_proto::ipc::State;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// Samples kept per measurement (several seconds of fast mouse movement).
const KEEP: usize = 6000;
/// A pause longer than this is the mouse resting, not a delay.
const IDLE: Duration = Duration::from_millis(250);
/// One frame of a 120 Hz display.
const FRAME_120HZ_MS: f64 = 1000.0 / 120.0;

#[derive(Default)]
pub struct CursorDiag {
    /// This computer's mouse moved while it controls another computer.
    captured: VecDeque<Instant>,
    /// Microseconds this computer took to handle and send each of those moves.
    handle_us: VecDeque<u32>,
    sent: u64,
    held_back: u64,
    /// Moves from another computer's mouse that reached this computer's engine.
    received: VecDeque<Instant>,
    /// How quickly this computer's input system applied them (macOS).
    platform: MoveTimings,
}

fn push<T>(queue: &mut VecDeque<T>, value: T) {
    if queue.len() == KEEP {
        queue.pop_front();
    }
    queue.push_back(value);
}

fn keep_last(values: &mut Vec<u32>) {
    if values.len() > KEEP {
        values.drain(..values.len() - KEEP);
    }
}

/// Gaps between consecutive moves (milliseconds), leaving out pauses where the mouse was resting.
fn gaps_ms(times: &VecDeque<Instant>) -> Vec<f64> {
    times
        .iter()
        .zip(times.iter().skip(1))
        .map(|(a, b)| b.saturating_duration_since(*a))
        .filter(|gap| *gap <= IDLE)
        .map(|gap| gap.as_secs_f64() * 1000.0)
        .collect()
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn sorted(mut values: Vec<f64>) -> Vec<f64> {
    values.sort_by(f64::total_cmp);
    values
}

fn us_to_ms(values: &[u32]) -> Vec<f64> {
    sorted(values.iter().map(|v| f64::from(*v) / 1000.0).collect())
}

impl CursorDiag {
    pub fn captured(&mut self, started: Instant) {
        push(&mut self.captured, started);
        push(
            &mut self.handle_us,
            u32::try_from(started.elapsed().as_micros()).unwrap_or(u32::MAX),
        );
    }

    pub fn sent(&mut self) {
        self.sent += 1;
    }

    pub fn held_back(&mut self) {
        self.held_back += 1;
    }

    pub fn received(&mut self, at: Instant) {
        push(&mut self.received, at);
    }

    pub fn add_platform(&mut self, timings: MoveTimings) {
        self.platform.posted += timings.posted;
        self.platform.replaced += timings.replaced;
        self.platform.wait_us.extend(timings.wait_us);
        self.platform.post_us.extend(timings.post_us);
        keep_last(&mut self.platform.wait_us);
        keep_last(&mut self.platform.post_us);
    }

    /// A plain-text report to copy and share. Numbers first, then what they suggest.
    pub fn report(&self, state: &State) -> String {
        let me = &state.self_info;
        let os = match me.os {
            glide_platform::Os::Macos => "Mac",
            glide_platform::Os::Windows => "Windows",
        };
        let s = &state.settings.switching;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Glide cursor report: {} ({os}), Glide {}",
            me.name, me.version
        );
        let _ = writeln!(
            out,
            "Settings: pointer speed {:.1}x, acceleration {:.1}, smoothing {}",
            s.pointer_speed,
            s.pointer_acceleration,
            if s.smooth_moves { "on" } else { "off" }
        );
        for peer in state.peers.iter().filter(|p| p.online) {
            match peer.latency_ms {
                Some(ms) => {
                    let _ = writeln!(out, "Connection to {}: {ms:.1} ms round trip", peer.name);
                }
                None => {
                    let _ = writeln!(out, "Connection to {}: connected", peer.name);
                }
            }
        }
        let mut hints = Vec::new();

        let gaps = sorted(gaps_ms(&self.captured));
        let _ = writeln!(
            out,
            "\nSending (this computer's mouse moving another computer's cursor):"
        );
        if gaps.len() < 50 {
            let _ = writeln!(out, "  not enough movement recorded yet");
        } else {
            let seconds: f64 = gaps.iter().sum::<f64>() / 1000.0;
            let rate = gaps.len() as f64 / seconds.max(0.001);
            let handle = us_to_ms(&self.handle_us.iter().copied().collect::<Vec<_>>());
            let total = (self.sent + self.held_back).max(1);
            let _ = writeln!(
                out,
                "  {seconds:.1} s of movement, {rate:.0} mouse updates per second"
            );
            let _ = writeln!(
                out,
                "  gaps between updates: median {:.1} ms, 99% under {:.1} ms, longest {:.1} ms",
                percentile(&gaps, 0.5),
                percentile(&gaps, 0.99),
                percentile(&gaps, 1.0)
            );
            let _ = writeln!(
                out,
                "  handling each update: median {:.2} ms, 99% under {:.2} ms",
                percentile(&handle, 0.5),
                percentile(&handle, 0.99)
            );
            let _ = writeln!(
                out,
                "  held back because the connection was busy: {:.1}%",
                self.held_back as f64 * 100.0 / total as f64
            );
            if rate < 120.0 {
                hints.push(format!(
                    "This computer's mouse only reports about {rate:.0} updates per second while sharing; a 120 Hz screen needs at least 120."
                ));
            }
            if percentile(&handle, 0.99) > 4.0 {
                hints
                    .push("This computer is slow to handle mouse updates (it may be busy).".into());
            }
        }

        let gaps = sorted(gaps_ms(&self.received));
        let _ = writeln!(
            out,
            "\nReceiving (another computer's mouse moving this computer's cursor):"
        );
        if gaps.len() < 50 {
            let _ = writeln!(out, "  not enough movement recorded yet");
        } else {
            let seconds: f64 = gaps.iter().sum::<f64>() / 1000.0;
            let rate = gaps.len() as f64 / seconds.max(0.001);
            let late = gaps.iter().filter(|g| **g > FRAME_120HZ_MS).count() as f64 * 100.0
                / gaps.len() as f64;
            let _ = writeln!(
                out,
                "  {seconds:.1} s of movement, {rate:.0} moves arriving per second"
            );
            let _ = writeln!(
                out,
                "  gaps between moves: median {:.1} ms, 90% under {:.1} ms, 99% under {:.1} ms, longest {:.1} ms",
                percentile(&gaps, 0.5),
                percentile(&gaps, 0.9),
                percentile(&gaps, 0.99),
                percentile(&gaps, 1.0)
            );
            let _ = writeln!(
                out,
                "  gaps longer than one 120 Hz frame (8.3 ms): {late:.1}%"
            );
            if late > 5.0 {
                hints.push(format!(
                    "{late:.0}% of moves arrive more than a frame apart: they come in bursts. That is usually Wi-Fi; a cable, or 5/6 GHz Wi-Fi near the router, makes it even."
                ));
            }
            if rate < 100.0 {
                hints.push(format!(
                    "Only about {rate:.0} moves per second arrive here; the sending computer or the network delivers too few for a smooth 120 Hz cursor."
                ));
            }
        }
        if self.platform.posted > 0 {
            let wait = us_to_ms(&self.platform.wait_us);
            let post = us_to_ms(&self.platform.post_us);
            let replaced = self.platform.replaced as f64 * 100.0
                / (self.platform.posted + self.platform.replaced).max(1) as f64;
            let _ = writeln!(
                out,
                "  waiting for this computer to apply a move: median {:.2} ms, 99% under {:.2} ms, longest {:.2} ms",
                percentile(&wait, 0.5),
                percentile(&wait, 0.99),
                percentile(&wait, 1.0)
            );
            let _ = writeln!(
                out,
                "  applying each move: median {:.2} ms, 99% under {:.2} ms",
                percentile(&post, 0.5),
                percentile(&post, 0.99)
            );
            let _ = writeln!(
                out,
                "  moves skipped because a newer one was already waiting: {replaced:.1}%"
            );
            if percentile(&wait, 0.99) > 4.0 || percentile(&post, 0.99) > 4.0 {
                hints.push(
                    "This computer is slow to apply cursor moves (it is busy), which adds delay after they arrive."
                        .into(),
                );
            }
        }

        let _ = writeln!(out, "\nWhat this suggests:");
        if hints.is_empty() {
            let _ = writeln!(
                out,
                "  Moves arrive often and evenly and are applied quickly. If the cursor still feels heavy, it is the feel of the movement: try Pointer speed and Pointer acceleration on the computer whose mouse you use."
            );
        }
        for hint in hints {
            let _ = writeln!(out, "  - {hint}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Feature: the cursor report tells bursts apart from steady delivery.
    #[tokio::test]
    async fn bursty_arrivals_are_called_out_and_steady_ones_are_not() {
        let start = Instant::now();
        let mut steady = CursorDiag::default();
        for i in 0..500 {
            steady.received(start + Duration::from_millis(i * 2));
        }
        let mut bursty = CursorDiag::default();
        for burst in 0..60u64 {
            for i in 0..8u64 {
                bursty.received(
                    start + Duration::from_millis(burst * 24) + Duration::from_micros(i * 200),
                );
            }
        }
        let dir = tempfile::tempdir().expect("dir");
        let state = crate::Core::mock(dir.path(), None)
            .await
            .expect("core")
            .snapshot();
        let steady = steady.report(&state);
        let bursty = bursty.report(&state);
        assert!(steady.contains("500 moves arriving per second"), "{steady}");
        assert!(!steady.contains("bursts"), "{steady}");
        assert!(bursty.contains("bursts"), "{bursty}");
        // Long pauses are the mouse resting, not delays.
        let mut paused = CursorDiag::default();
        for i in 0..100 {
            paused.received(start + Duration::from_millis(i * 2 + if i >= 50 { 5000 } else { 0 }));
        }
        assert!(paused.report(&state).contains("longest 2.0 ms"));
    }
}
