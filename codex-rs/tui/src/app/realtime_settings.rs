//! Persist server voice preferences and machine-local audio selections before updating the UI.

use super::*;
use crate::legacy_core::config::edit::ConfigEdit;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadRealtimeListVoicesParams;
use codex_app_server_protocol::ThreadRealtimeListVoicesResponse;
use codex_config::config_toml::MicrophoneChannels;
use codex_protocol::protocol::RealtimeVoice;
use codex_protocol::protocol::RealtimeVoicesList;
use codex_realtime_webrtc::AudioDeviceKind;
use uuid::Uuid;

/// Unknown voices belong to newer servers; omit the override instead of blocking audio.
/// An unset preference explicitly selects the V1/V3 server default, not a stale thread preference.
fn configured_voice(
    config: &codex_app_server_protocol::Config,
    default_voice: RealtimeVoice,
) -> Result<Option<RealtimeVoice>> {
    let value = config
        .additional
        .get("realtime")
        .and_then(|realtime| realtime.get("voice"));
    match value {
        None | Some(serde_json::Value::Null) => Ok(Some(default_voice)),
        Some(value @ serde_json::Value::String(_)) => {
            Ok(serde_json::from_value(value.clone()).ok())
        }
        Some(value) => Ok(Some(serde_json::from_value(value.clone())?)),
    }
}

impl App {
    pub(super) async fn refresh_realtime_frontend(
        &mut self,
        app_server: &AppServerSession,
    ) -> Result<bool> {
        if let Some(response) = crate::config_update::read_effective_config_if_supported(
            app_server.request_handle(),
            &self.chat_widget.config_ref().cwd,
        )
        .await?
        {
            let mut value = response
                .config
                .additional
                .get("realtime")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            let unknown_voice = value.get("voice").is_some_and(|voice| {
                voice.is_string() && serde_json::from_value::<RealtimeVoice>(voice.clone()).is_err()
            });
            if unknown_voice {
                value["voice"] = serde_json::Value::Null;
            }
            let settings: codex_config::config_toml::RealtimeToml = serde_json::from_value(value)?;
            self.chat_widget.set_unknown_realtime_voice(unknown_voice);
            self.config.realtime.backend = settings.backend;
            self.config.realtime.openai = settings.openai;
            self.config.realtime.gemini = settings.gemini;
            self.config.realtime.voice = settings.voice;
            self.chat_widget
                .set_realtime_frontend_preferences(&self.config.realtime);
            return Ok(true);
        }
        Ok(false)
    }

    pub(super) async fn persist_realtime_frontend(
        &mut self,
        app_server: &AppServerSession,
        setting: crate::realtime_frontend::FrontendSetting,
    ) {
        let (key, value) = setting.edit();
        let previous = self.config.realtime.clone();
        let mut edits = vec![crate::config_update::replace_config_value(
            key,
            value.clone(),
        )];
        if let crate::realtime_frontend::FrontendSetting::Model { backend, .. } = &setting {
            edits.push(crate::config_update::replace_config_value(
                "realtime.backend",
                serde_json::json!(backend),
            ));
        }
        let result =
            crate::config_update::write_config_batch(app_server.request_handle(), edits).await;
        match result {
            Ok(response) => {
                // A successful write does not mutate the already frozen active/startup attempt.
                if let crate::realtime_frontend::FrontendSetting::Model { backend, .. } = &setting {
                    self.config.realtime.backend = Some(*backend);
                }
                match &setting {
                    crate::realtime_frontend::FrontendSetting::Backend(backend) => {
                        self.config.realtime.backend = Some(*backend)
                    }
                    crate::realtime_frontend::FrontendSetting::Model {
                        backend: codex_protocol::protocol::RealtimeBackend::Openai,
                        model,
                    } => self.config.realtime.openai.model = model.clone(),
                    crate::realtime_frontend::FrontendSetting::Model {
                        backend: codex_protocol::protocol::RealtimeBackend::Gemini,
                        model: Some(model),
                    } => {
                        if let Some(config) = &mut self.config.realtime.gemini {
                            config.model = model.clone();
                        }
                    }
                    crate::realtime_frontend::FrontendSetting::GeminiVoice(voice) => {
                        if let Some(config) = &mut self.config.realtime.gemini {
                            config.voice = voice.clone();
                        }
                    }
                    _ => {}
                }
                self.chat_widget
                    .set_realtime_frontend_preferences(&self.config.realtime);
                match self.refresh_realtime_frontend(app_server).await {
                    Ok(false)
                        if response.status
                            == codex_app_server_protocol::WriteStatus::OkOverridden =>
                    {
                        self.config.realtime = previous;
                        self.chat_widget
                            .set_realtime_frontend_preferences(&self.config.realtime);
                        self.chat_widget.add_error_message("Voice preference was saved but is overridden; this server cannot read effective settings.".into());
                        return;
                    }
                    Err(error) => {
                        self.config.realtime = previous;
                        self.chat_widget
                            .set_realtime_frontend_preferences(&self.config.realtime);
                        self.chat_widget.add_error_message(format!("Voice preference was saved, but effective settings could not be read: {error}"));
                        return;
                    }
                    _ => {}
                }
                let settings = serde_json::to_value(&self.config.realtime).unwrap_or_default();
                let actual = key
                    .strip_prefix("realtime.")
                    .unwrap_or(key)
                    .split('.')
                    .try_fold(&settings, |value, part| value.get(part));
                if actual == Some(&value)
                    && response.status != codex_app_server_protocol::WriteStatus::OkOverridden
                {
                    self.chat_widget.add_info_message(
                        "Voice preference saved. Applies to your next voice conversation.".into(),
                        None,
                    );
                } else {
                    self.chat_widget.add_error_message("Voice preference was saved but is overridden by another configuration layer.".into());
                }
            }
            Err(error) => self
                .chat_widget
                .add_error_message(format!("Failed to save voice preference: {error}")),
        }
    }

    pub(super) async fn persist_realtime_input_channel(
        &mut self,
        channel: Option<MicrophoneChannels>,
    ) {
        self.persist_realtime_audio(
            "microphone_channel",
            channel.map(|channel| match channel {
                MicrophoneChannels::Single(channel) => i64::from(channel.get()).into(),
                MicrophoneChannels::Multiple(channels) => toml::Value::Array(
                    channels
                        .into_iter()
                        .map(|channel| i64::from(channel.get()).into())
                        .collect(),
                ),
            }),
        )
        .await;
    }

    pub(super) async fn persist_realtime_device(
        &mut self,
        kind: AudioDeviceKind,
        name: Option<String>,
    ) {
        let key = match kind {
            AudioDeviceKind::Input => "microphone",
            AudioDeviceKind::Output => "speaker",
        };
        self.persist_realtime_audio(key, name.map(Into::into)).await;
        self.list_realtime_devices(kind);
    }

    async fn persist_realtime_audio(&mut self, key: &str, value: Option<toml::Value>) {
        // Audio runs on the TUI's machine, even when the app server is remote.
        let segments = vec!["audio".to_string(), key.to_string()];
        let mut edits = vec![match &value {
            Some(toml::Value::String(value)) => ConfigEdit::SetPath {
                segments,
                value: value.clone().into(),
            },
            Some(toml::Value::Integer(value)) => ConfigEdit::SetPath {
                segments,
                value: (*value).into(),
            },
            Some(toml::Value::Array(channels)) => ConfigEdit::SetPath {
                segments,
                value: channels
                    .iter()
                    .map(|channel| match channel {
                        toml::Value::Integer(channel) => *channel,
                        _ => unreachable!("audio channel arrays contain only integers"),
                    })
                    .collect::<toml_edit::Array>()
                    .into(),
            },
            Some(_) => unreachable!("audio settings are device names or channel numbers"),
            None => ConfigEdit::ClearPath { segments },
        }];
        if key == "microphone" {
            edits.push(ConfigEdit::ClearPath {
                segments: vec!["audio".into(), "microphone_channel".into()],
            });
        }
        if let Err(error) =
            ConfigEditsBuilder::for_config_path(self.local_settings.user_config_path.as_path())
                .with_edits(edits)
                .apply()
                .await
        {
            self.chat_widget
                .add_error_message(format!("Failed to save audio setting: {error}"));
            return;
        }
        match crate::legacy_core::config::load_config_toml_with_layer_stack(
            self.local_settings.codex_home.as_path(),
            /*cwd*/ None,
            self.cli_kv_overrides.clone(),
            codex_config::ConfigLoadOptions {
                loader_overrides: self.loader_overrides.clone(),
                cloud_config_bundle: self.cloud_config_bundle.clone(),
                ..Default::default()
            },
        )
        .await
        {
            Ok(config) => {
                self.config.config_layer_stack = self
                    .config
                    .config_layer_stack
                    .with_user_layer_from(&config.config_layer_stack);
                let audio = config.config_toml.audio.unwrap_or_default();
                let effective = toml::Value::try_from(&audio)
                    .ok()
                    .and_then(|audio| audio.get(key).cloned());
                let channel_overridden = key == "microphone" && audio.microphone_channel.is_some();
                self.local_settings.audio = Ok(audio);
                self.chat_widget.local_settings.audio = self.local_settings.audio.clone();
                if channel_overridden {
                    self.chat_widget.add_error_message("Input device saved, but the input channel is overridden by another configuration layer. Update that override before starting voice.".into());
                } else if effective == value {
                    self.chat_widget.add_info_message(
                        "Audio setting saved. Applies to your next voice conversation.".to_string(),
                        /*hint*/ None,
                    );
                } else {
                    self.chat_widget.add_error_message(
                        "Audio setting was saved but is overridden by another configuration layer."
                            .to_string(),
                    );
                }
            }
            Err(error) => self.chat_widget.add_error_message(format!(
                "Audio setting was saved, but effective settings could not be read: {error}"
            )),
        }
    }

    pub(super) fn list_realtime_devices(&self, kind: AudioDeviceKind) {
        let tx = self.app_event_tx.clone();
        let origin = self.active_thread_id;
        tokio::spawn(async move {
            let result = async {
                let package = codex_install_context::InstallContext::current()
                    .package_layout
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("voice package unavailable"))?;
                let mut host = codex_realtime_webrtc::VoiceHost::connect(
                    package,
                    codex_build_info::BuildInfo::get().build_commit(),
                )
                .await?;
                let result = host.list_devices(kind).await;
                host.close().await?;
                result
            }
            .await;
            tx.send(AppEvent::RealtimeDevicesListed {
                origin,
                kind,
                result: result.map_err(|_| {
                    "Could not list audio devices. Check your audio device and voice package."
                        .to_owned()
                }),
            });
        });
    }

    pub(super) async fn realtime_voices(
        &self,
        app_server: &AppServerSession,
    ) -> Result<RealtimeVoicesList> {
        let response = app_server
            .request_handle()
            .request_typed::<ThreadRealtimeListVoicesResponse>(
                ClientRequest::ThreadRealtimeListVoices {
                    request_id: RequestId::String(format!("tui-voice-list-{}", Uuid::new_v4())),
                    params: ThreadRealtimeListVoicesParams {},
                },
            )
            .await?;
        Ok(response.voices)
    }

    pub(super) async fn effective_realtime_voice(
        &self,
        app_server: &AppServerSession,
        voices: &RealtimeVoicesList,
    ) -> Result<Option<RealtimeVoice>> {
        // Session setup installs the active conversation cwd for both server modes.
        let config = crate::config_update::read_effective_config_if_supported(
            app_server.request_handle(),
            &self.chat_widget.config_ref().cwd,
        )
        .await?;
        config
            .as_ref()
            .map(|config| configured_voice(&config.config, voices.default_v1))
            .transpose()
            .map(Option::flatten)
    }

    pub(super) async fn open_realtime_voices(&mut self, app_server: &AppServerSession) {
        if self.chat_widget.config_ref().realtime.backend
            == Some(codex_protocol::protocol::RealtimeBackend::Gemini)
        {
            self.chat_widget.open_gemini_voices();
            return;
        }
        let voices = match self.realtime_voices(app_server).await {
            Ok(voices) => voices,
            Err(error) => {
                self.chat_widget
                    .add_error_message(format!("Failed to list voices: {error}"));
                return;
            }
        };
        match self.effective_realtime_voice(app_server, &voices).await {
            Ok(voice) => self.chat_widget.open_realtime_voices(voice, voices),
            Err(error) => self
                .chat_widget
                .add_error_message(format!("Failed to read voice settings: {error}")),
        }
    }

    pub(super) async fn persist_realtime_voice(
        &mut self,
        app_server: &AppServerSession,
        voice: RealtimeVoice,
    ) {
        match crate::config_update::write_config_batch(
            app_server.request_handle(),
            vec![crate::config_update::replace_config_value(
                "realtime.voice",
                serde_json::json!(voice),
            )],
        )
        .await
        {
            Ok(response) => {
                let voices = match self.realtime_voices(app_server).await {
                    Ok(voices) => voices,
                    Err(error) => {
                        self.chat_widget.add_error_message(format!(
                            "Voice preference was saved, but the effective voice could not be confirmed: {error}"
                        ));
                        return;
                    }
                };
                let effective = crate::config_update::read_effective_config_if_supported(
                    app_server.request_handle(), &self.chat_widget.config_ref().cwd,
                ).await.and_then(|config| match config.as_ref() {
                    Some(config) => configured_voice(&config.config, voices.default_v1),
                    None if response.status == codex_app_server_protocol::WriteStatus::OkOverridden => Err(color_eyre::eyre::eyre!("the saved voice is overridden, but this server cannot read effective settings")),
                    None => Ok(Some(voice)),
                });
                match effective {
                    Ok(effective_voice) => {
                        self.config.realtime.voice = effective_voice;
                        self.chat_widget.set_realtime_voice(effective_voice);
                        if effective_voice == Some(voice) {
                            self.chat_widget.on_realtime_voice_saved(voice);
                        } else {
                            self.chat_widget.add_error_message(format!(
                                "Voice preference was saved but not applied: {}",
                                super::config_persistence::overridden_write_message(&response),
                            ));
                        }
                    }
                    Err(error) => self.chat_widget.add_error_message(format!("Voice preference was saved, but effective settings could not be read: {error}")),
                }
            }
            Err(error) => self
                .chat_widget
                .add_error_message(format!("Failed to save voice: {error}")),
        }
    }
}
