//! Voice settings hierarchy with parent navigation; selections apply to subsequent conversations.

use super::*;
use crate::bottom_pane::MultiSelectItem;
use crate::bottom_pane::MultiSelectPicker;
use crate::realtime_frontend::FrontendSetting;
use codex_config::config_toml::MicrophoneChannels;
use codex_protocol::protocol::RealtimeBackend;
use codex_protocol::protocol::RealtimeVoice;
use codex_protocol::protocol::RealtimeVoicesList;
use codex_realtime_webrtc::AudioDevice;
use codex_realtime_webrtc::AudioDeviceKind;

fn back_item(parent: fn() -> AppEvent) -> SelectionItem {
    SelectionItem {
        name: "Back".to_string(),
        actions: vec![Box::new(move |tx| tx.send(parent()))],
        dismiss_on_select: true,
        ..Default::default()
    }
}

impl ChatWidget {
    pub(super) fn realtime_audio_settings(
        &mut self,
    ) -> Option<codex_config::config_toml::RealtimeAudioToml> {
        match self.local_settings.audio.clone() {
            Ok(audio) => Some(audio),
            Err(error) => {
                self.add_error_message(error);
                None
            }
        }
    }

    pub(crate) fn open_realtime_settings(&mut self) {
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Voice settings".to_string()),
            items: vec![
                SelectionItem {
                    name: "Set sound devices".to_string(),
                    actions: vec![Box::new(|tx| tx.send(AppEvent::OpenRealtimeSoundDevices))],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Choose a voice".to_string(),
                    disabled_reason: self.config.realtime.managed_voice.then(|| {
                        "Automatically assigned from this backend's managed voice pool.".into()
                    }),
                    actions: vec![Box::new(|tx| tx.send(AppEvent::OpenRealtimeVoices))],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Backend".into(),
                    description: Some(
                        if self.config.realtime.backend.is_none()
                            && self.config.realtime.local_adapter.is_some()
                        {
                            "Local adapter (legacy)"
                        } else {
                            match self.config.realtime.backend.unwrap_or_default() {
                                RealtimeBackend::Openai => "OpenAI",
                                RealtimeBackend::Gemini => "Gemini Live",
                            }
                        }
                        .into(),
                    ),
                    actions: vec![Box::new(|tx| tx.send(AppEvent::OpenRealtimeBackends))],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Model".into(),
                    description: Some(match self.config.realtime.backend.unwrap_or_default() {
                        RealtimeBackend::Openai => (if self.config.realtime.backend.is_none() {
                            self.config.experimental_realtime_ws_model.clone()
                        } else {
                            self.config.realtime.openai.model.clone()
                        })
                        .unwrap_or_else(|| "Server default".into()),
                        RealtimeBackend::Gemini => self
                            .config
                            .realtime
                            .gemini
                            .as_ref()
                            .map(|config| config.model.clone())
                            .unwrap_or_else(|| "Not configured".into()),
                    }),
                    actions: vec![Box::new(|tx| tx.send(AppEvent::OpenRealtimeModels))],
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            footer_hint: Some(standard_popup_hint_line()),
            ..SelectionViewParams::picker()
        });
    }

    pub(crate) fn set_unknown_realtime_voice(&mut self, unknown: bool) {
        self.realtime_openai_voice_unknown = unknown;
    }

    pub(crate) fn set_realtime_frontend_preferences(
        &mut self,
        settings: &codex_config::config_toml::RealtimeConfig,
    ) {
        self.config.realtime.backend = settings.backend;
        self.config.realtime.openai = settings.openai.clone();
        self.config.realtime.gemini = settings.gemini.clone();
        self.config.realtime.voice = settings.voice;
    }

    pub(crate) fn open_realtime_backends(&mut self) {
        let mut items: Vec<SelectionItem> = [
            (RealtimeBackend::Openai, "OpenAI"),
            (RealtimeBackend::Gemini, "Gemini Live"),
        ]
        .into_iter()
        .map(|(backend, name)| SelectionItem {
            name: name.into(),
            is_current: self.config.realtime.backend.unwrap_or_default() == backend
                && !(self.config.realtime.backend.is_none()
                    && self.config.realtime.local_adapter.is_some()),
            disabled_reason: (backend == RealtimeBackend::Gemini
                && self.config.realtime.gemini.is_none())
            .then(|| "Configure realtime.gemini.model and api_key_env first.".into()),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::PersistRealtimeFrontend {
                    setting: FrontendSetting::Backend(backend),
                })
            })],
            dismiss_on_select: true,
            ..Default::default()
        })
        .collect();
        items.push(back_item(|| AppEvent::OpenRealtimeSettings));
        self.show_frontend_picker(
            "Voice backend",
            "Applies to the next voice conversation. Coding model is unchanged.",
            items,
        );
    }

    fn show_frontend_picker(&mut self, title: &str, subtitle: &str, items: Vec<SelectionItem>) {
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some(title.into()),
            subtitle: Some(subtitle.into()),
            items,
            on_cancel: Some(Box::new(|tx| tx.send(AppEvent::OpenRealtimeSettings))),
            footer_hint: Some(standard_popup_hint_line()),
            ..SelectionViewParams::picker()
        });
    }

    pub(crate) fn open_realtime_models(&mut self) {
        let backend = self.config.realtime.backend.unwrap_or_default();
        let (selected, mut models) = match backend {
            RealtimeBackend::Openai => (
                if self.config.realtime.backend.is_none() {
                    self.config.experimental_realtime_ws_model.clone()
                } else {
                    self.config.realtime.openai.model.clone()
                },
                self.config.realtime.openai.models.clone(),
            ),
            RealtimeBackend::Gemini => match &self.config.realtime.gemini {
                Some(config) => (Some(config.model.clone()), config.models.clone()),
                None => {
                    self.add_error_message(
                        "Configure realtime.gemini.model before selecting a Gemini model.".into(),
                    );
                    return;
                }
            },
        };
        if models.len() > 32
            || models.iter().chain(selected.iter()).any(|model| {
                model.is_empty() || model.len() > 128 || model.chars().any(char::is_control)
            })
        {
            self.add_error_message(
                "Configure at most 32 voice model identifiers, each at most 128 characters.".into(),
            );
            return;
        }
        if let Some(model) = &selected {
            models.push(model.clone());
        }
        models.sort();
        models.dedup();
        let choices = (backend == RealtimeBackend::Openai)
            .then_some(None)
            .into_iter()
            .chain(models.into_iter().map(Some));
        let mut items: Vec<SelectionItem> = choices
            .map(|model| SelectionItem {
                name: model.clone().unwrap_or_else(|| "Server default".into()),
                is_current: model == selected,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::PersistRealtimeFrontend {
                        setting: FrontendSetting::Model {
                            backend,
                            model: model.clone(),
                        },
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            })
            .collect();
        items.push(back_item(|| AppEvent::OpenRealtimeSettings));
        self.show_frontend_picker(
            "Voice model",
            "Configured model choices; provider availability is checked when starting voice.",
            items,
        );
    }

    pub(crate) fn open_gemini_voices(&mut self) {
        if self.config.realtime.managed_voice {
            self.add_info_message("Voice is assigned automatically from the Gemini pool. Configure it with voice-control pool --provider gemini.".into(), None);
            return;
        }
        let Some(config) = &self.config.realtime.gemini else {
            self.add_error_message("Gemini Live is not configured.".into());
            return;
        };
        let current = config.voice.clone();
        let mut voices: Vec<String> = crate::realtime_frontend::GEMINI_VOICES
            .iter()
            .map(|voice| (*voice).into())
            .collect();
        voices.push(current.clone());
        voices.sort();
        voices.dedup();
        let mut items: Vec<SelectionItem> = voices
            .into_iter()
            .map(|voice| SelectionItem {
                name: voice.clone(),
                is_current: voice == current,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::PersistRealtimeFrontend {
                        setting: FrontendSetting::GeminiVoice(voice.clone()),
                    })
                })],
                dismiss_on_select: true,
                ..Default::default()
            })
            .collect();
        items.push(back_item(|| AppEvent::OpenRealtimeSettings));
        self.show_frontend_picker(
            "Gemini Live voice",
            "Applies to your next voice conversation.",
            items,
        );
    }

    pub(crate) fn open_realtime_voices(
        &mut self,
        current: Option<RealtimeVoice>,
        voices: RealtimeVoicesList,
    ) {
        if self.config.realtime.managed_voice {
            self.add_info_message("Voice is assigned automatically from the OpenAI pool. Configure it with voice-control pool --provider openai.".into(), None);
            return;
        }
        // The TUI uses V3, which shares the V1 voice catalog.
        let mut items: Vec<SelectionItem> = voices
            .v1
            .into_iter()
            .map(|voice| SelectionItem {
                name: voice.wire_name().to_string(),
                is_current: Some(voice) == current,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::PersistRealtimeVoiceSelection { voice });
                })],
                dismiss_on_select: true,
                ..Default::default()
            })
            .collect();
        items.push(back_item(|| AppEvent::OpenRealtimeSettings));
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Select voice".to_string()),
            subtitle: Some("Applies to your next voice conversation.".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            on_cancel: Some(Box::new(|tx| tx.send(AppEvent::OpenRealtimeSettings))),
            ..SelectionViewParams::picker()
        });
    }

    pub(crate) fn open_realtime_sound_devices(&mut self) {
        let Some(audio) = self.realtime_audio_settings() else {
            return;
        };
        let mut items = Vec::new();
        for (kind, label, selected) in [
            (AudioDeviceKind::Input, "Input device", audio.microphone),
            (AudioDeviceKind::Output, "Output device", audio.speaker),
        ] {
            items.push(SelectionItem {
                name: label.into(),
                description: Some(selected.unwrap_or_else(|| "System default".into())),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::OpenRealtimeDevicePicker { kind })
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }
        items.push(back_item(|| AppEvent::OpenRealtimeSettings));
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Sound devices".to_string()),
            subtitle: Some("Applies to your next voice conversation.".into()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            on_cancel: Some(Box::new(|tx| tx.send(AppEvent::OpenRealtimeSettings))),
            ..SelectionViewParams::picker()
        });
    }

    pub(crate) fn open_realtime_device_picker(
        &mut self,
        kind: AudioDeviceKind,
        devices: Vec<AudioDevice>,
    ) {
        let Some(audio) = self.realtime_audio_settings() else {
            return;
        };
        let (title, current) = match kind {
            AudioDeviceKind::Input => ("Input device", audio.microphone),
            AudioDeviceKind::Output => ("Output device", audio.speaker),
        };
        let selected = devices
            .iter()
            .find(|device| {
                current
                    .as_ref()
                    .map_or(device.is_default, |name| *name == device.name)
            })
            .cloned();
        let ambiguous: std::collections::HashSet<_> = devices
            .iter()
            .filter(|device| {
                devices
                    .iter()
                    .filter(|other| other.name == device.name)
                    .count()
                    > 1
            })
            .map(|device| device.name.clone())
            .collect();
        let mut items: Vec<SelectionItem> = std::iter::once(/*value*/ None)
            .chain(devices.into_iter().map(Some))
            .map(|device| {
                let name = device.map(|device| device.name);
                let is_current = name == current;
                let disabled_reason = name
                    .as_ref()
                    .filter(|name| ambiguous.contains(*name))
                    .map(|_| "Identical device names; use System default.".into());
                SelectionItem {
                    name: name.clone().unwrap_or_else(|| "System default".into()),
                    is_current,
                    disabled_reason,
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::PersistRealtimeDevice {
                            kind,
                            name: name.clone(),
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                }
            })
            .collect();
        if kind == AudioDeviceKind::Input
            && let Some(device) = selected.filter(|device| device.channels >= 3)
        {
            let parent = items.iter().position(|item| item.is_current).unwrap_or(0);
            items.insert(
                parent + 1,
                SelectionItem {
                    name: "Input channels".into(),
                    child_label: Some('a'),
                    description: Some(audio.microphone_channel.as_ref().map_or_else(
                        || "All channels (mixed)".into(),
                        |channels| {
                            format!(
                                "Inputs {}",
                                channels
                                    .as_slice()
                                    .iter()
                                    .map(ToString::to_string)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        },
                    )),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::OpenRealtimeInputChannels {
                            device: device.clone(),
                        })
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
            );
        }
        items.push(back_item(|| AppEvent::OpenRealtimeSoundDevices));
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some(title.into()),
            subtitle: Some("Applies to your next voice conversation.".into()),
            items,
            footer_hint: Some(standard_popup_hint_line()),
            on_cancel: Some(Box::new(|tx| tx.send(AppEvent::OpenRealtimeSoundDevices))),
            ..SelectionViewParams::picker()
        });
    }

    pub(crate) fn open_realtime_input_channels(&mut self, microphone: AudioDevice) {
        let Some(audio) = self.realtime_audio_settings() else {
            return;
        };
        let current = audio.microphone_channel;
        let count = microphone.channels;
        let picker = MultiSelectPicker::builder(
            format!("Microphone: {}", microphone.name),
            Some("Select the input channels to mix. Select at least one.".into()),
            self.app_event_tx.clone(),
        )
        .list_keymap(self.bottom_pane.list_keymap())
        .items(
            (1..=count)
                .map(|number| MultiSelectItem {
                    id: number.to_string(),
                    name: format!("Input {number}"),
                    enabled: current.as_ref().is_none_or(|channels| {
                        channels
                            .as_slice()
                            .iter()
                            .any(|channel| channel.get() == number)
                    }),
                    ..Default::default()
                })
                .collect(),
        )
        .require_selection()
        .on_preview(|items| {
            items
                .iter()
                .all(|item| !item.enabled)
                .then(|| "Select at least one input channel.".red().into())
        })
        .on_confirm(move |ids, tx| {
            let selected: Vec<_> = (1..=count)
                .filter(|number| ids.contains(&number.to_string()))
                .filter_map(std::num::NonZeroU16::new)
                .collect();
            let channel = if selected.len() == usize::from(count) {
                None
            } else if selected.len() == 1 {
                selected.first().copied().map(MicrophoneChannels::Single)
            } else {
                Some(MicrophoneChannels::Multiple(selected))
            };
            tx.send(AppEvent::PersistRealtimeInputChannel { channel });
        })
        .on_cancel(|tx| {
            tx.send(AppEvent::OpenRealtimeDevicePicker {
                kind: AudioDeviceKind::Input,
            })
        })
        .build();
        self.bottom_pane.show_view(Box::new(picker));
    }

    pub(crate) fn set_realtime_voice(&mut self, voice: Option<RealtimeVoice>) {
        self.config.realtime.voice = voice;
    }

    pub(crate) fn on_realtime_voice_saved(&mut self, voice: RealtimeVoice) {
        self.realtime_openai_voice_unknown = false;
        self.set_realtime_voice(Some(voice));
        self.add_info_message(
            format!(
                "Voice set to {}. Applies to your next voice conversation.",
                voice.wire_name()
            ),
            /*hint*/ None,
        );
    }
}

#[cfg(test)]
#[path = "realtime_settings_tests.rs"]
mod tests;
