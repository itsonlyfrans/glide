use super::*;

const MAX_DEVICES: usize = 32;
const MAX_POSITION: f64 = 1_000_000.0;

/// Remote positions are proposals; membership and monitor geometry are local facts.
pub(super) fn reconcile_layout(
    devices: &[LayoutDevice],
    state: &State,
) -> Result<Layout, IpcError> {
    let mut ids = HashSet::new();
    if devices.len() > MAX_DEVICES
        || devices.iter().any(|device| {
            !ids.insert(&device.device_id)
                || device.device_id.len() != 64
                || !device
                    .device_id
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                || !valid_position(device.x, device.y)
        })
    {
        return Err(error(
            ErrorCode::InvalidParams,
            "malformed replicated layout",
        ));
    }
    let known = |id: &str| {
        id == state.self_info.device_id || state.peers.iter().any(|peer| peer.device_id == id)
    };
    let mut proposed: Vec<_> = devices
        .iter()
        .filter(|device| known(&device.device_id))
        .cloned()
        .collect();
    let mut missing = vec![state.self_info.device_id.clone()];
    let mut peers: Vec<_> = state
        .peers
        .iter()
        .map(|peer| peer.device_id.clone())
        .collect();
    peers.sort();
    missing.extend(peers);
    missing.retain(|id| !proposed.iter().any(|device| &device.device_id == id));
    if proposed.len() + missing.len() > MAX_DEVICES {
        return Err(error(
            ErrorCode::InvalidParams,
            "layout device limit reached",
        ));
    }
    // Normalize individual screens, reserving one provisional screen until first Hello.
    let screens: HashMap<_, Vec<_>> =
        std::iter::once((&state.self_info.device_id, &state.self_info.monitors))
            .chain(state.peers.iter().map(|p| (&p.device_id, &p.monitors)))
            .map(|(id, ms)| {
                let min_x = ms.iter().map(|m| m.x).fold(f64::INFINITY, f64::min);
                let min_y = ms.iter().map(|m| m.y).fold(f64::INFINITY, f64::min);
                let rects = if ms.is_empty() {
                    vec![(0.0, 0.0, 1920.0, 1080.0, true)]
                } else {
                    ms.iter()
                        .map(|m| (m.x - min_x, m.y - min_y, m.w, m.h, m.primary))
                        .collect()
                };
                (id.as_str(), rects)
            })
            .collect();
    if screens.values().flatten().any(|m| {
        ![m.0, m.1, m.2, m.3, m.0 + m.2, m.1 + m.3]
            .iter()
            .all(|n| n.is_finite())
            || m.2 <= 0.0
            || m.3 <= 0.0
    }) {
        return Err(error(ErrorCode::InvalidParams, "invalid monitor geometry"));
    }
    let mut placed: Vec<LayoutDevice> = Vec::with_capacity(MAX_DEVICES);
    let mut occupied = Vec::new();
    // ponytail: cap rectangle comparisons on untrusted complex topologies; use a spatial index
    // if 32 devices with 64 screens each must all be auto-reconciled in a single turn.
    let mut comparisons = 0usize;
    for (id, position) in proposed
        .drain(..)
        .map(|d| (d.device_id, Some((d.x, d.y))))
        .chain(missing.into_iter().map(|id| (id, None)))
    {
        let local = &screens[id.as_str()];
        let preferred = occupied
            .iter()
            .max_by(|a: &&(f64, f64, f64, f64, bool), b| {
                (a.0 + a.2)
                    .total_cmp(&(b.0 + b.2))
                    .then_with(|| a.4.cmp(&b.4))
            });
        let incoming = local.iter().find(|m| m.4).unwrap_or(&local[0]);
        let (desired_x, desired_y) = position.unwrap_or_else(|| {
            preferred.map_or((0.0, 0.0), |m| (m.0 + m.2 - incoming.0, m.1 - incoming.1))
        });
        let mut free = |x: f64, y: f64| -> bool {
            if !valid_position(x, y) {
                return false;
            }
            local.iter().all(|m| {
                occupied.iter().all(|o| {
                    comparisons += 1;
                    comparisons <= 1_000_000
                        && (x + m.0 >= o.0 + o.2 - 1e-7
                            || x + m.0 + m.2 <= o.0 + 1e-7
                            || y + m.1 >= o.1 + o.3 - 1e-7
                            || y + m.1 + m.3 <= o.1 + 1e-7)
                })
            })
        };
        let (x, y) = if free(desired_x, desired_y) {
            (desired_x, desired_y)
        } else {
            // A slot that touches a neighbour along only a sliver (a corner) is a poor place for the cursor to cross, so
            // slots sharing at least a quarter of this device's shortest screen side win over any sliver slot.
            let shortest = local
                .iter()
                .map(|m| m.2.min(m.3))
                .fold(f64::INFINITY, f64::min);
            let shared = |x: f64, y: f64| -> f64 {
                let mut total = 0.0;
                for m in local {
                    for o in &occupied {
                        let (l, t, r, b) = (x + m.0, y + m.1, x + m.0 + m.2, y + m.1 + m.3);
                        let (ol, ot, or, ob) = (o.0, o.1, o.0 + o.2, o.1 + o.3);
                        if (r - ol).abs() < 1e-7 || (l - or).abs() < 1e-7 {
                            total += (b.min(ob) - t.max(ot)).max(0.0);
                        }
                        if (b - ot).abs() < 1e-7 || (t - ob).abs() < 1e-7 {
                            total += (r.min(or) - l.max(ol)).max(0.0);
                        }
                    }
                }
                total
            };
            let mut best: Option<(f64, f64, bool)> = None;
            for o in &occupied {
                for m in local {
                    // Project onto a real shared edge; a corner alone isn't adjacency.
                    let cy = if desired_y + m.1 < o.1 + o.3 && desired_y + m.1 + m.3 > o.1 {
                        desired_y
                    } else {
                        desired_y.clamp(
                            o.1 + (o.3 - m.3).min(0.0) - m.1,
                            o.1 + (o.3 - m.3).max(0.0) - m.1,
                        )
                    };
                    let cx = if desired_x + m.0 < o.0 + o.2 && desired_x + m.0 + m.2 > o.0 {
                        desired_x
                    } else {
                        desired_x.clamp(
                            o.0 + (o.2 - m.2).min(0.0) - m.0,
                            o.0 + (o.2 - m.2).max(0.0) - m.0,
                        )
                    };
                    for (cx, cy) in [
                        (o.0 + o.2 - m.0, cy),
                        (o.0 - m.0 - m.2, cy),
                        (cx, o.1 + o.3 - m.1),
                        (cx, o.1 - m.1 - m.3),
                    ] {
                        if !free(cx, cy) {
                            continue;
                        }
                        let strong = shared(cx, cy) >= 0.25 * shortest;
                        if best.is_none_or(|(bx, by, best_strong)| {
                            best_strong
                                .cmp(&strong)
                                .then_with(|| {
                                    (cx - desired_x)
                                        .hypot(cy - desired_y)
                                        .total_cmp(&(bx - desired_x).hypot(by - desired_y))
                                })
                                .then_with(|| bx.total_cmp(&cx))
                                .then_with(|| cy.total_cmp(&by))
                                .is_lt()
                        }) {
                            best = Some((cx, cy, strong));
                        }
                    }
                }
            }
            best.map(|(x, y, _)| (x, y))
                .ok_or_else(|| error(ErrorCode::InvalidParams, "no free adjacent layout slot"))?
        };
        occupied.extend(local.iter().map(|m| (x + m.0, y + m.1, m.2, m.3, m.4)));
        placed.push(LayoutDevice {
            device_id: id,
            x,
            y,
        });
    }
    validate_layout(&placed, state)?;
    Ok(Layout { devices: placed })
}

fn valid_position(x: f64, y: f64) -> bool {
    x.is_finite() && y.is_finite() && x.abs() <= MAX_POSITION && y.abs() <= MAX_POSITION
}

pub(super) fn same_placements(left: &Layout, right: &Layout) -> bool {
    left.devices.len() == right.devices.len()
        && left
            .devices
            .iter()
            .all(|device| right.devices.contains(device))
}

impl Core {
    /// Persist layout and its version together; only a reconciliation creates a new LWW write.
    pub(super) fn commit_layout(
        &mut self,
        next: State,
        remote: Option<((u64, String), bool)>,
    ) -> Result<(), IpcError> {
        let remote_update = remote.is_some();
        let (version, bump) = remote.unwrap_or_else(|| (self.layout_version.clone(), true));
        let clock = self.lamport.max(version.0);
        let clock = if bump {
            clock
                .checked_add(1)
                .ok_or_else(|| error(ErrorCode::Internal, "layout version exhausted"))?
        } else {
            clock
        };
        let version = if bump {
            (clock, self.state.self_info.device_id.clone())
        } else {
            version
        };
        let old_clock = std::mem::replace(&mut self.lamport, clock);
        let old_version = std::mem::replace(&mut self.layout_version, version);
        let saved = if remote_update && same_placements(&next.layout, &self.state.layout) {
            self.persist(&next).map(|()| {
                self.state.layout = next.layout;
                self.dirty = true;
            })
        } else {
            self.apply(next)
        };
        if let Err(failure) = saved {
            self.lamport = old_clock;
            self.layout_version = old_version;
            if remote_update && failure.code == ErrorCode::Internal {
                self.notify_failure(&failure);
            }
            return Err(failure);
        }
        Ok(())
    }
}
