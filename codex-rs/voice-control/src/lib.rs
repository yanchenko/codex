//! Headless voice allocation and lifecycle coordination; no audio or provider credentials.
mod state;
pub use state::*;

mod service;
mod storage;
mod wire;
pub use service::Client;
pub use service::connect_or_start;
pub use service::runtime_directory;
pub use service::serve;

mod guard;
pub use guard::GuardIdentity;
pub use guard::LeaseGuard;

mod caps;
pub use caps::CapsCommand;
pub use caps::caps_command;
