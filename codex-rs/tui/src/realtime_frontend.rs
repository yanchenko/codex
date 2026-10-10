//! Per-attempt voice frontend choices; coding-model preferences are independent.
use codex_config::config_toml::RealtimeConfig;
use codex_protocol::protocol::RealtimeBackend;
use codex_protocol::protocol::RealtimeFrontend;
use codex_protocol::protocol::RealtimeVoice;

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct RealtimeCallSelection {
    pub frontend: RealtimeFrontend,
    pub model: Option<String>,
    pub voice: Option<RealtimeVoice>,
    pub omit_openai_voice: bool,
}
impl RealtimeCallSelection {
    pub fn apply_managed_voice(&mut self, provider: &str, voice: &str) -> Result<(), String> {
        if self.frontend.backend.pool_key() != provider {
            return Err(
                "Allocated voice belongs to a different backend; start voice again.".into(),
            );
        }
        crate::managed_voice::validate_managed_voice(self.frontend.backend, voice)
            .map_err(|error| error.to_string())?;
        match self.frontend.backend {
            RealtimeBackend::Openai => {
                self.voice = Some(
                    serde_json::from_value(serde_json::Value::String(voice.to_owned()))
                        .map_err(|error| error.to_string())?,
                );
                self.frontend.voice = None;
                self.omit_openai_voice = false;
            }
            RealtimeBackend::Gemini => {
                self.frontend.voice = Some(voice.to_owned());
                self.voice = None;
            }
        }
        Ok(())
    }

    pub fn capture(
        config: &RealtimeConfig,
        unknown_openai_voice: bool,
    ) -> Result<Option<Self>, String> {
        let Some(backend) = config.backend else {
            return Ok(None);
        };
        match backend {
            RealtimeBackend::Openai => Ok(Some(Self {
                frontend: RealtimeFrontend {
                    backend,
                    voice: None,
                },
                model: config.openai.model.clone(),
                voice: config.voice,
                omit_openai_voice: unknown_openai_voice,
            })),
            RealtimeBackend::Gemini => {
                let config = config.gemini.as_ref().ok_or(
                    "Configure realtime.gemini.model and api_key_env before starting Gemini Live.",
                )?;
                Ok(Some(Self {
                    frontend: RealtimeFrontend {
                        backend,
                        voice: Some(config.voice.clone()),
                    },
                    model: Some(config.model.clone()),
                    voice: None,
                    omit_openai_voice: false,
                }))
            }
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) enum FrontendSetting {
    Backend(RealtimeBackend),
    Model {
        backend: RealtimeBackend,
        model: Option<String>,
    },
    GeminiVoice(String),
}
impl FrontendSetting {
    pub fn edit(&self) -> (&'static str, serde_json::Value) {
        match self {
            Self::Backend(backend) => ("realtime.backend", serde_json::json!(backend)),
            Self::Model {
                backend: RealtimeBackend::Openai,
                model,
            } => ("realtime.openai.model", serde_json::json!(model)),
            Self::Model {
                backend: RealtimeBackend::Gemini,
                model,
            } => ("realtime.gemini.model", serde_json::json!(model)),
            Self::GeminiVoice(voice) => ("realtime.gemini.voice", serde_json::json!(voice)),
        }
    }
}

pub(crate) use codex_protocol::protocol::GEMINI_VOICES;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_voice_overrides_only_its_frozen_provider_and_never_the_model() {
        let mut selection = RealtimeCallSelection {
            frontend: RealtimeFrontend {
                backend: RealtimeBackend::Gemini,
                voice: Some("Kore".into()),
            },
            model: Some("configured-live-model".into()),
            voice: None,
            omit_openai_voice: false,
        };
        selection.apply_managed_voice("gemini", "Aoede").unwrap();
        assert_eq!(selection.frontend.voice.as_deref(), Some("Aoede"));
        assert_eq!(selection.model.as_deref(), Some("configured-live-model"));
        assert_eq!(selection.voice, None);
        let before = selection.clone();
        assert!(selection.apply_managed_voice("openai", "cove").is_err());
        assert!(selection.apply_managed_voice("gemini", "cove").is_err());
        assert_eq!(selection, before);
        selection.frontend.backend = RealtimeBackend::Openai;
        selection.apply_managed_voice("openai", "maple").unwrap();
        assert_eq!(selection.voice, Some(RealtimeVoice::Maple));
        assert_eq!(selection.frontend.voice, None);
        assert_eq!(selection.model.as_deref(), Some("configured-live-model"));
    }
    #[test]
    fn selected_frontend_is_frozen_and_does_not_use_openai_voice_for_gemini() {
        let mut config = RealtimeConfig {
            backend: Some(RealtimeBackend::Gemini),
            gemini: Some(codex_config::config_toml::RealtimeGeminiConfig {
                model: "gemini-test-live".into(),
                models: vec![],
                voice: "Kore".into(),
                api_key_env: "GEMINI_API_KEY".into(),
            }),
            ..Default::default()
        };
        let selection = RealtimeCallSelection::capture(&config, false)
            .unwrap()
            .unwrap();
        config.backend = Some(RealtimeBackend::Openai);
        config.voice = Some(RealtimeVoice::Maple);
        config.gemini.as_mut().unwrap().voice = "Aoede".into();
        assert_eq!(selection.model.as_deref(), Some("gemini-test-live"));
        assert_eq!(selection.frontend.voice.as_deref(), Some("Kore"));
        assert_eq!(selection.voice, None);
        let next = RealtimeCallSelection::capture(&config, false)
            .unwrap()
            .unwrap();
        assert_eq!(next.voice, Some(RealtimeVoice::Maple));
        assert_eq!(next.frontend.voice, None);
        assert_eq!(next.frontend.backend, RealtimeBackend::Openai);
    }
    #[test]
    fn unconfigured_legacy_path_remains_unmodified_and_missing_gemini_is_actionable() {
        assert!(
            RealtimeCallSelection::capture(&RealtimeConfig::default(), false)
                .unwrap()
                .is_none()
        );
        let config = RealtimeConfig {
            backend: Some(RealtimeBackend::Gemini),
            ..Default::default()
        };
        assert!(
            RealtimeCallSelection::capture(&config, false)
                .unwrap_err()
                .contains("Configure realtime.gemini")
        );
    }
}
