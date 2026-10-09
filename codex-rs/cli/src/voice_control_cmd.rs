//! Explicit, headless voice coordination. Hardware ownership is a separate opt-in.
use clap::Parser;
use clap::Subcommand;
use codex_protocol::protocol::RealtimeVoice;
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

pub(crate) async fn run(args: VoiceControlCommand) -> anyhow::Result<()> {
    let directory = args
        .directory
        .unwrap_or_else(codex_voice_control::runtime_directory);
    if matches!(args.command, Command::Serve) {
        return codex_voice_control::serve(&directory).await;
    }
    let mut client =
        codex_voice_control::connect_or_start(&directory, &std::env::current_exe()?).await?;
    let operation = match args.command {
        Command::Serve => unreachable!(),
        Command::Status => None,
        Command::Pool { provider, voices } => {
            anyhow::ensure!(
                provider == "openai",
                "provider voice routing is not integrated: {provider}"
            );
            for voice in &voices {
                serde_json::from_value::<RealtimeVoice>(serde_json::Value::String(voice.clone()))
                    .map_err(|_| anyhow::anyhow!("unsupported OpenAI voice: {voice}"))?;
            }
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
