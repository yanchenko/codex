//! Explicit, headless voice coordination. Hardware ownership is a separate opt-in.
use clap::Parser;
use clap::Subcommand;
use codex_protocol::protocol::RealtimeVoice;
use codex_protocol::protocol::RealtimeVoicesList;
use codex_voice_control::Operation;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Debug, Parser)]
pub(crate) struct VoiceControlCommand {
    /// Private per-user runtime directory (normally discovered automatically).
    #[arg(long, global = true)]
    directory: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Explicit Caps permission, ownership and recovery controls.
    Caps {
        #[command(subcommand)]
        command: Caps,
    },
    /// Run the coordinator without claiming Caps or opening audio devices.
    Serve,
    /// Inspect confirmed calls, reservations, and unknown owners.
    Status,
    /// Configure an ordered voice pool while that provider has no occupied leases.
    Pool {
        #[arg(long, default_value = "openai")]
        provider: String,
        #[arg(required = true, num_args = 1..)]
        voices: Vec<String>,
    },
    /// Select the enrolled instance that a subsequent start targets.
    Bind { instance: Uuid },
    /// Request voice startup in the explicitly bound instance.
    Start,
    /// Stop all enrolled voice calls, without canceling coding tasks.
    Off,
    /// Stop one enrolled voice call.
    Stop { instance: Uuid },
    /// Remove an idle registration. Occupied or unknown leases cannot be forgotten.
    Forget { instance: Uuid },
}

#[derive(Debug, Subcommand)]
enum Caps {
    Status,
    Setup,
    Run,
    Disable,
}

pub(crate) async fn run(args: VoiceControlCommand) -> anyhow::Result<()> {
    let directory = args
        .directory
        .unwrap_or_else(codex_voice_control::runtime_directory);
    if let Command::Caps { command } = args.command {
        let command = match command {
            Caps::Status => codex_voice_control::CapsCommand::Status,
            Caps::Setup => codex_voice_control::CapsCommand::Setup,
            Caps::Run => codex_voice_control::CapsCommand::Run,
            Caps::Disable => codex_voice_control::CapsCommand::Disable,
        };
        return codex_voice_control::caps_command(&directory, command).await;
    }
    if matches!(args.command, Command::Serve) {
        return codex_voice_control::serve(&directory).await;
    }
    let mut client =
        codex_voice_control::connect_or_start(&directory, &std::env::current_exe()?).await?;
    let operation = match args.command {
        Command::Serve | Command::Caps { .. } => unreachable!(),
        Command::Status => None,
        Command::Pool { provider, voices } => {
            match provider.as_str() {
                "openai" => {
                    for voice in &voices {
                        let parsed = serde_json::from_value::<RealtimeVoice>(
                            serde_json::Value::String(voice.clone()),
                        )
                        .map_err(|_| anyhow::anyhow!("unsupported OpenAI voice: {voice}"))?;
                        anyhow::ensure!(
                            RealtimeVoicesList::builtin().v3.contains(&parsed),
                            "OpenAI voice {voice} is unavailable for the managed WebRTC V3 backend"
                        );
                    }
                }
                "gemini" => {
                    for voice in &voices {
                        anyhow::ensure!(
                            codex_protocol::protocol::GEMINI_VOICES.contains(&voice.as_str()),
                            "unsupported managed Gemini voice: {voice}"
                        );
                    }
                }
                _ => anyhow::bail!("provider voice routing is not integrated: {provider}"),
            };
            Some(Operation::Configure { provider, voices })
        }
        Command::Bind { instance } => Some(Operation::Bind { instance }),
        Command::Start => Some(Operation::Start {
            epoch: client.snapshot().await?.epoch,
        }),
        Command::Off => Some(Operation::GlobalOff),
        Command::Stop { instance } => Some(Operation::Stop { instance }),
        Command::Forget { instance } => Some(Operation::Forget { instance }),
    };
    let state = match operation {
        Some(operation) => client.request(operation).await?,
        None => client.snapshot().await?,
    };
    println!("{}", serde_json::to_string_pretty(&state)?);
    if state.stopping {
        eprintln!(
            "Voice stop is pending; native retirement has not been confirmed for every call."
        );
    }
    Ok(())
}
