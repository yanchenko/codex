use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use uuid::Uuid;

pub const MAX_OWNERS: usize = 32;
pub const MAX_VOICES: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Pool {
    pub voices: Vec<String>,
    pub cursor: usize,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Reserved,
    Active,
    Stopping,
    Unknown,
}

/// Stable identity of one native call. Thread navigation cannot change its owner.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub instance: Uuid,
    pub owner: String,
    pub generation: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub guard: Option<crate::GuardIdentity>,
    pub call: Call,
    pub provider: String,
    pub voice: String,
    pub epoch: u64,
    pub phase: Phase,
    pub stop_requested: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    /// A UUID generated at TUI process start, not a recyclable PID.
    pub id: Uuid,
    pub owner: String,
    pub provider: String,
    pub connected: bool,
    pub start_requested: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub revision: u64,
    pub epoch: u64,
    pub stopping: bool,
    pub pools: BTreeMap<String, Pool>,
    pub instances: BTreeMap<Uuid, Instance>,
    pub leases: BTreeMap<Uuid, Lease>,
    pub retired: BTreeMap<Uuid, u64>,
    pub bound: Option<Uuid>,
    /// Last confirmed aggregate indicator. Unknown never invents an all-off result.
    pub confirmed_active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Configure {
        provider: String,
        voices: Vec<String>,
    },
    Register {
        instance: Uuid,
        owner: String,
        provider: String,
    },
    Bind {
        instance: Uuid,
    },
    Start {
        epoch: u64,
    },
    GlobalOff,
    Reserve {
        call: Call,
        epoch: u64,
    },
    Active {
        call: Call,
        epoch: u64,
    },
    /// Sent only after native child-exit acknowledgment, including canceled startup.
    Retired {
        call: Call,
    },
    Stop {
        instance: Uuid,
    },
    Disconnect {
        instance: Uuid,
    },
    Forget {
        instance: Uuid,
    },
}

impl Registry {
    /// Restart never treats an occupied reservation as available. An existing TUI
    /// must reconnect and reconcile its generation before the voice can be reused.
    pub fn recover(&mut self) -> Result<()> {
        self.validate()?;
        self.epoch = self
            .epoch
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("epoch exhausted"))?;
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("revision exhausted"))?;
        for instance in self.instances.values_mut() {
            instance.connected = false;
            instance.start_requested = false;
        }
        for lease in self.leases.values_mut() {
            lease.phase = Phase::Unknown;
            lease.stop_requested = true;
        }
        self.stopping = !self.leases.is_empty();
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.instances.len() <= MAX_OWNERS
                && self.leases.len() <= MAX_OWNERS
                && self.retired.len() <= MAX_OWNERS
                && self.pools.len() <= 16,
            "invalid registry bounds"
        );
        for (provider, pool) in &self.pools {
            identifier(provider)?;
            ensure!(
                !pool.voices.is_empty()
                    && pool.voices.len() <= MAX_VOICES
                    && pool.cursor < pool.voices.len(),
                "invalid journal voice pool"
            );
            let mut unique = BTreeSet::new();
            for voice in &pool.voices {
                identifier(voice)?;
                ensure!(unique.insert(voice), "duplicate journal voice");
            }
        }
        for (id, instance) in &self.instances {
            identifier(&instance.owner)?;
            ensure!(
                *id == instance.id && self.pools.contains_key(&instance.provider),
                "invalid journal instance"
            );
        }
        let mut occupied = BTreeSet::new();
        for (id, lease) in &self.leases {
            let instance = self.instance(*id)?;
            ensure!(
                lease.call.instance == *id
                    && lease.call.owner == instance.owner
                    && lease.provider == instance.provider
                    && lease.epoch <= self.epoch,
                "invalid journal lease identity"
            );
            ensure!(
                lease.call.generation > self.retired.get(id).copied().unwrap_or(0),
                "retired journal generation"
            );
            let pool = self
                .pools
                .get(&lease.provider)
                .ok_or_else(|| anyhow::anyhow!("journal pool missing"))?;
            ensure!(
                pool.voices.contains(&lease.voice)
                    && occupied.insert((&lease.provider, &lease.voice)),
                "invalid or duplicate journal voice"
            );
        }
        ensure!(
            self.retired
                .keys()
                .all(|id| self.instances.contains_key(id)),
            "orphaned retirement watermark"
        );
        ensure!(
            self.bound.is_none_or(|id| self.instances.contains_key(&id)),
            "invalid bound instance"
        );
        Ok(())
    }

    /// Apply to a copy; the service must durably persist it before publishing a
    /// reply or dispatching native work. Errors leave the caller's registry intact.
    pub fn transition(&self, operation: Operation) -> Result<Self> {
        let mut next = self.clone();
        next.apply(operation)?;
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("revision exhausted"))?;
        Ok(next)
    }

    fn apply(&mut self, operation: Operation) -> Result<()> {
        match operation {
            Operation::Configure { provider, voices } => {
                identifier(&provider)?;
                ensure!(
                    !voices.is_empty() && voices.len() <= MAX_VOICES,
                    "voice pool must contain 1..64 voices"
                );
                let mut unique = BTreeSet::new();
                for voice in &voices {
                    identifier(voice)?;
                    ensure!(unique.insert(voice), "duplicate voice in pool");
                }
                ensure!(
                    !self.leases.values().any(|lease| lease.provider == provider),
                    "cannot change an occupied provider pool"
                );
                ensure!(
                    self.pools.contains_key(&provider) || self.pools.len() < 16,
                    "provider limit reached"
                );
                self.pools.insert(provider, Pool { voices, cursor: 0 });
            }
            Operation::Register {
                instance,
                owner,
                provider,
            } => {
                identifier(&owner)?;
                identifier(&provider)?;
                ensure!(
                    self.pools.contains_key(&provider),
                    "provider pool is not configured"
                );
                if let Some(previous) = self.instances.get_mut(&instance) {
                    if previous.owner != owner || previous.provider != provider {
                        ensure!(
                            !self.leases.contains_key(&instance) && !previous.start_requested,
                            "cannot rebind an instance with a pending or active call"
                        );
                        previous.owner = owner;
                        previous.provider = provider;
                        if self.bound == Some(instance) {
                            self.bound = None;
                        }
                    }
                    previous.connected = true;
                } else {
                    ensure!(self.instances.len() < MAX_OWNERS, "instance limit reached");
                    self.instances.insert(
                        instance,
                        Instance {
                            id: instance,
                            owner,
                            provider,
                            connected: true,
                            start_requested: false,
                        },
                    );
                }
            }
            Operation::Bind { instance } => {
                ensure!(self.instance(instance)?.connected, "target is disconnected");
                self.bound = Some(instance);
            }
            Operation::Start { epoch } => {
                ensure!(epoch == self.epoch, "stale global-start epoch");
                ensure!(!self.stopping, "global stop is awaiting native retirement");
                let id = self
                    .bound
                    .ok_or_else(|| anyhow::anyhow!("bind a live instance first"))?;
                ensure!(
                    !self.leases.values().any(|lease| lease.call.instance == id),
                    "target already has a voice call"
                );
                let instance = self
                    .instances
                    .get_mut(&id)
                    .ok_or_else(|| anyhow::anyhow!("unknown target"))?;
                ensure!(instance.connected, "target is disconnected");
                instance.start_requested = true;
            }
            Operation::GlobalOff => {
                self.epoch = self
                    .epoch
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("epoch exhausted"))?;
                self.stopping = !self.leases.is_empty();
                for instance in self.instances.values_mut() {
                    instance.start_requested = false;
                }
                for lease in self.leases.values_mut() {
                    lease.stop_requested = true;
                    if lease.phase != Phase::Unknown {
                        lease.phase = Phase::Stopping;
                    }
                }
            }
            Operation::Reserve { call, epoch } => {
                ensure!(epoch == self.epoch, "stale admission epoch");
                ensure!(
                    call.generation > self.retired.get(&call.instance).copied().unwrap_or(0),
                    "generation has already retired"
                );
                if let Some(previous) = self.leases.get(&call.instance) {
                    ensure!(previous.call == call, "generation identity mismatch");
                    ensure!(
                        !previous.stop_requested && previous.phase != Phase::Unknown,
                        "reservation is stopping or unknown"
                    );
                    return Ok(());
                }
                ensure!(!self.stopping, "global stop is awaiting native retirement");
                let instance = self.instance(call.instance)?;
                ensure!(
                    instance.connected && instance.owner == call.owner,
                    "owner is stale or disconnected"
                );
                ensure!(
                    !self
                        .leases
                        .values()
                        .any(|lease| lease.call.instance == call.instance),
                    "instance already owns a reservation"
                );
                let provider = instance.provider.clone();
                let pool = self
                    .pools
                    .get_mut(&provider)
                    .ok_or_else(|| anyhow::anyhow!("provider pool is not configured"))?;
                let free = (0..pool.voices.len())
                    .map(|offset| (pool.cursor + offset) % pool.voices.len())
                    .find(|index| {
                        !self.leases.values().any(|lease| {
                            lease.provider == provider && lease.voice == pool.voices[*index]
                        })
                    });
                let Some(index) = free else {
                    bail!("no free voice in provider pool");
                };
                let voice = pool.voices[index].clone();
                pool.cursor = (index + 1) % pool.voices.len();
                self.leases.insert(
                    call.instance,
                    Lease {
                        guard: None,
                        call: call.clone(),
                        provider,
                        voice,
                        epoch: self.epoch,
                        phase: Phase::Reserved,
                        stop_requested: false,
                    },
                );
                self.instances
                    .get_mut(&call.instance)
                    .ok_or_else(|| anyhow::anyhow!("unknown instance"))?
                    .start_requested = false;
            }
            Operation::Active { call, epoch } => {
                let lease = self.lease_mut(&call)?;
                ensure!(
                    lease.epoch == epoch && !lease.stop_requested && lease.phase != Phase::Unknown,
                    "stale start acknowledgment; retire native call"
                );
                lease.phase = Phase::Active;
                self.confirmed_active = true;
            }
            Operation::Retired { call } => {
                if self.leases.contains_key(&call.instance) {
                    self.lease_mut(&call)?;
                    self.leases.remove(&call.instance);
                    self.retired.insert(call.instance, call.generation);
                }
                self.finish_retirement();
            }
            Operation::Stop { instance } => {
                self.instance(instance)?;
                self.instances
                    .get_mut(&instance)
                    .ok_or_else(|| anyhow::anyhow!("unknown instance"))?
                    .start_requested = false;
                for lease in self
                    .leases
                    .values_mut()
                    .filter(|lease| lease.call.instance == instance)
                {
                    lease.stop_requested = true;
                    if lease.phase != Phase::Unknown {
                        lease.phase = Phase::Stopping;
                    }
                }
            }
            Operation::Disconnect { instance } => {
                self.instance(instance)?;
                let owner = self
                    .instances
                    .get_mut(&instance)
                    .ok_or_else(|| anyhow::anyhow!("unknown instance"))?;
                owner.connected = false;
                owner.start_requested = false;
                for lease in self
                    .leases
                    .values_mut()
                    .filter(|lease| lease.call.instance == instance)
                {
                    lease.phase = Phase::Unknown;
                    lease.stop_requested = true;
                }
                if self.bound == Some(instance) {
                    self.bound = None;
                }
            }
            Operation::Forget { instance } => {
                ensure!(
                    !self
                        .leases
                        .values()
                        .any(|lease| lease.call.instance == instance),
                    "cannot forget an owner with unconfirmed native retirement"
                );
                self.instances.remove(&instance);
                self.retired.remove(&instance);
                if self.bound == Some(instance) {
                    self.bound = None;
                }
            }
        }
        Ok(())
    }

    fn finish_retirement(&mut self) {
        if self.leases.is_empty() {
            self.confirmed_active = false;
            self.stopping = false;
        } else if self
            .leases
            .values()
            .all(|lease| lease.phase == Phase::Reserved)
        {
            self.confirmed_active = false;
        }
    }

    fn instance(&self, id: Uuid) -> Result<&Instance> {
        self.instances
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("unknown instance"))
    }

    fn lease_mut(&mut self, call: &Call) -> Result<&mut Lease> {
        let lease = self
            .leases
            .get_mut(&call.instance)
            .ok_or_else(|| anyhow::anyhow!("unknown reservation"))?;
        ensure!(lease.call == *call, "reservation owner mismatch");
        Ok(lease)
    }
}

fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b)),
        "invalid bounded identifier"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
