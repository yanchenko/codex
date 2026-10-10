use super::*;
use crate::managed_voice::Event;
use crate::managed_voice::ManagedVoice;

impl ChatWidget {
    pub(crate) fn accepts_managed_instance(&self, instance: Option<uuid::Uuid>) -> bool {
        instance
            == self
                .realtime_conversation
                .managed
                .as_ref()
                .map(|managed| managed.instance)
    }
    pub(crate) fn managed_start_is_current(
        &self,
        identity: &crate::managed_voice::StartIdentity,
    ) -> bool {
        self.realtime_conversation.phase == RealtimeConversationPhase::Starting
            && self.realtime_conversation.attempt_id == identity.generation
            && self.realtime_conversation.managed_epoch == Some(identity.epoch)
            && self
                .realtime_conversation
                .startup_abort
                .as_ref()
                .is_some_and(|abort| !abort.is_aborted())
            && self
                .realtime_conversation
                .managed
                .as_ref()
                .is_some_and(|managed| {
                    managed.instance == identity.instance && managed.epoch() == Some(identity.epoch)
                })
    }

    pub(crate) fn enroll_managed_voice(&mut self) {
        if !self.config.realtime.managed_voice {
            return;
        }
        let Some(owner) = self.thread_id() else {
            return;
        };
        if self
            .realtime_conversation
            .managed
            .as_ref()
            .is_some_and(|managed| managed.owner == owner)
        {
            return;
        }
        // Never enroll an already running unmanaged call or relabel its owner.
        if self.realtime_conversation_is_running() {
            return;
        }
        if self.config.realtime.local_adapter.is_some()
            || self.config.experimental_realtime_ws_base_url.is_some()
            || self
                .config
                .experimental_realtime_webrtc_call_base_url
                .is_some()
        {
            if !self.realtime_conversation.enrollment_error_reported {
                self.add_realtime_error("Managed voice allocation requires a built-in OpenAI or Gemini backend; custom realtime endpoints are not supported.".into());
                self.realtime_conversation.enrollment_error_reported = true;
            }
            return;
        }
        self.realtime_conversation.managed = Some(ManagedVoice::new(
            owner,
            self.app_event_tx.clone(),
            self.config.realtime.backend.unwrap_or_default(),
        ));
    }

    pub(crate) fn on_managed_voice_event(&mut self, instance: uuid::Uuid, event: Event) {
        let Some(managed) = self
            .realtime_conversation
            .managed
            .as_ref()
            .filter(|managed| {
                managed.instance == instance && Some(managed.owner) == self.thread_id()
            })
        else {
            return;
        };
        match event {
            Event::Admission { generation, result } => {
                if self.realtime_conversation.phase != RealtimeConversationPhase::Starting
                    || self.realtime_conversation.attempt_id != generation
                {
                    return;
                }
                let admission = match result {
                    Ok(admission) if managed.epoch() == Some(admission.lease.epoch) => admission,
                    Ok(_) => {
                        self.on_realtime_error(
                            "Voice start was invalidated by global stop.".into(),
                        );
                        return;
                    }
                    Err(error) => {
                        self.on_realtime_error(error);
                        return;
                    }
                };
                let Some((abort, selection)) = self.realtime_conversation.pending_native.take()
                else {
                    return;
                };
                let applied = self
                    .realtime_conversation
                    .frontend_selection
                    .as_mut()
                    .ok_or_else(|| "Managed voice requires a frozen frontend selection.".to_owned())
                    .and_then(|selection| {
                        selection
                            .apply_managed_voice(&admission.lease.provider, &admission.lease.voice)
                    });
                if let Err(error) = applied {
                    self.on_realtime_error(error);
                    return;
                }
                self.realtime_conversation.managed_epoch = Some(admission.lease.epoch);
                self.spawn_realtime_native(
                    managed.owner,
                    generation,
                    abort,
                    selection,
                    Some(admission),
                );
            }
            Event::Start { epoch } => {
                if managed.epoch() != Some(epoch) || self.realtime_conversation_is_running() {
                    return;
                }
                self.realtime_conversation.external_start_epoch = Some(epoch);
                self.toggle_realtime_conversation();
                self.realtime_conversation.external_start_epoch = None;
                if !self.realtime_conversation_is_running()
                    && let Some(managed) = &self.realtime_conversation.managed
                {
                    managed.stop(self.realtime_conversation.attempt_id);
                }
            }
            Event::Stop { generation, error } => {
                if self.realtime_conversation.attempt_id != generation
                    || !self.realtime_conversation_is_running()
                {
                    return;
                }
                self.stop_realtime_conversation();
                if let Some(error) = error {
                    self.add_realtime_error(error);
                }
            }
            Event::Unavailable(error) => self.add_realtime_error(error),
        }
    }
}
