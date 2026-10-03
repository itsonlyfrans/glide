use super::*;

pub(super) struct PermissionUpdate {
    grants: Permissions,
    capture: Option<Result<(), glide_platform::BackendError>>,
}

pub(super) fn missing(state: PermissionState) -> bool {
    [state.accessibility, state.input_monitoring, state.injection]
        .iter()
        .any(|status| matches!(status, PermissionStatus::Denied | PermissionStatus::Unknown))
}

fn interval(state: PermissionState) -> Option<Duration> {
    if [state.accessibility, state.input_monitoring, state.injection]
        .iter()
        .all(|status| *status == PermissionStatus::NotApplicable)
    {
        None
    } else {
        Some(Duration::from_secs(if missing(state) { 2 } else { 10 }))
    }
}

impl Core {
    pub(super) fn cancel_permission_requests(&mut self) {
        if let Some((id, job)) = self.permission_job.take() {
            job.abort();
            if let Some(id) = id {
                self.permission_requests.push_front(id);
            }
        }
        for id in self.permission_requests.drain(..) {
            self.deferred_responses.push(Response::failure(
                id,
                error(
                    ErrorCode::Internal,
                    "Glide stopped before the permission request completed.",
                ),
            ));
        }
    }

    /// Start/collect one OS worker without waiting on it in the input/escape loop.
    pub(super) async fn poll_permissions(&mut self, now: Instant) {
        if self
            .permission_job
            .as_ref()
            .is_some_and(|(_, job)| job.is_finished())
        {
            let (request_id, job) = self.permission_job.take().expect("finished job");
            let update = job.await;
            match update {
                Ok(update) => {
                    let mut next = PermissionState::from(update.grants);
                    next.restart_required =
                        self.state.permissions.restart_required && !missing(next);
                    if missing(next) {
                        // Unknown during an OS desktop switch is not explicit revocation.
                        let lost = [
                            update.grants.accessibility,
                            update.grants.input_monitoring,
                            update.grants.injection,
                        ]
                        .contains(&PermissionStatus::Denied)
                            && !missing(self.state.permissions);
                        self.capture_pending = true;
                        // Restore local input and release remote holds before publishing loss.
                        if lost {
                            if let Err(failure) = self.end_forwarding("permission_denied").await {
                                self.notify_failure(&failure);
                            }
                        }
                    }
                    if let Some(capture) = update.capture {
                        next.restart_required = capture.is_err() && !missing(next);
                        if capture.is_ok() {
                            self.capture_pending = false;
                            if !self.permission_recovery_notified {
                                self.permission_recovery_notified = true;
                                self.events.push(Event::Notification(Notification {
                                    level: "info".into(),
                                    title: "Glide can now share your mouse and keyboard".into(),
                                    body: "Input permissions are ready.".into(),
                                    action: None,
                                }));
                            }
                        }
                    }
                    if next != self.state.permissions {
                        self.state.permissions = next;
                        self.dirty = true;
                    }
                    self.next_permission_check =
                        now + interval(next).unwrap_or(Duration::from_secs(10));
                    if let Some(id) = request_id {
                        self.deferred_responses
                            .push(Response::success(id, json!({})));
                        // A request always publishes the refreshed snapshot.
                        self.dirty = true;
                    }
                }
                Err(_) => {
                    let failure = error(
                        ErrorCode::Internal,
                        "The input permission worker failed. Restart Glide to try again.",
                    );
                    if let Some(id) = request_id {
                        self.deferred_responses
                            .push(Response::failure(id, failure.clone()));
                    }
                    self.events.push(Event::Notification(Notification {
                        level: "error".into(),
                        title: "Glide could not check input permissions".into(),
                        body: failure.message,
                        action: None,
                    }));
                    // A panicked worker cannot safely retain capture ownership.
                    self.shutdown = true;
                    self.cancel_permission_requests();
                }
            }
        }
        if self.permission_job.is_some() || self.shutdown {
            return;
        }
        let request_id = self.permission_requests.pop_front();
        if request_id.is_none()
            && (now < self.next_permission_check || interval(self.state.permissions).is_none())
        {
            return;
        }
        let platform = self.platform.clone();
        let sink = self.capture_sink.clone();
        let retry = self.capture_pending
            && (!self.state.permissions.restart_required || request_id.is_some());
        self.permission_job = Some((
            request_id,
            tokio::task::spawn_blocking(move || {
                let input = platform.input_backend();
                let mut grants = if request_id.is_some() {
                    input.request_permissions()
                } else {
                    input.permissions()
                };
                let mut capture = (retry
                    && !missing(grants.into())
                    && input.secure_input_enabled() != Some(true))
                .then(|| input.start_capture(sink));
                if capture.as_ref().is_some_and(Result::is_err) {
                    // A grant may have been revoked between preflight and tap creation.
                    grants = input.permissions();
                    if input.secure_input_enabled() == Some(true) {
                        capture = None;
                    }
                }
                PermissionUpdate { grants, capture }
            }),
        ));
    }
}
