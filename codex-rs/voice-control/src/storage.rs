use crate::Registry;
use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

const MAX_JOURNAL: u64 = 1024 * 1024;

pub(crate) fn private_open(path: &Path, create: bool) -> Result<File> {
    if path.exists() || path.symlink_metadata().is_ok() {
        let metadata = path.symlink_metadata()?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "control file is not a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // SAFETY: geteuid has no preconditions.
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
                "control file is not private to current user"
            );
        }
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    Ok(options.open(path)?)
}

pub(crate) struct Store {
    pub(crate) directory: PathBuf,
    pub token: String,
    _lock: File,
}

impl Store {
    pub async fn open(directory: &Path) -> Result<Self> {
        if directory.symlink_metadata().is_ok() {
            let metadata = directory.symlink_metadata()?;
            ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "runtime directory must not be a link"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                // SAFETY: geteuid has no preconditions.
                ensure!(
                    metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
                    "runtime directory must already be private to current user"
                );
            }
        }
        codex_uds::prepare_private_socket_directory(directory).await?;
        let lock = private_open(&directory.join("lock"), true)?;
        lock.try_lock()
            .context("voice coordinator is already running")?;
        let token_path = directory.join("token");
        let mut token_file = private_open(&token_path, true)?;
        let mut token = String::new();
        (&mut token_file).take(129).read_to_string(&mut token)?;
        if token.is_empty() {
            token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            token_file.write_all(token.as_bytes())?;
            token_file.sync_all()?;
        }
        ensure!(
            token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid coordinator token file"
        );
        Ok(Self {
            directory: directory.to_owned(),
            token,
            _lock: lock,
        })
    }

    pub fn load(&self) -> Result<Registry> {
        let path = self.directory.join("state.json");
        if !path.try_exists()? {
            return Ok(Registry::default());
        }
        let file = private_open(&path, false)?;
        ensure!(
            file.metadata()?.len() <= MAX_JOURNAL,
            "coordinator journal exceeds size limit"
        );
        let mut state: Registry = serde_json::from_reader(file)?;
        state.recover()?;
        Ok(state)
    }

    pub fn persist(&self, state: &Registry) -> Result<()> {
        state.validate()?;
        let bytes = serde_json::to_vec(state)?;
        ensure!(
            bytes.len() <= crate::wire::MAX_FRAME - 1024,
            "coordinator registry capacity reached"
        );
        let temporary = self.directory.join(format!("state-{}.tmp", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = private_open(&temporary, true)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            replace_durably(&temporary, &self.directory.join("state.json"))?;
            #[cfg(unix)]
            File::open(&self.directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }
}

#[cfg(not(windows))]
fn replace_durably(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination)?;
    Ok(())
}

#[cfg(windows)]
fn replace_durably(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both UTF-16 buffers are NUL terminated and remain alive throughout
    // the synchronous call. The temporary file was flushed before replacement.
    // Microsoft documents WRITE_THROUGH as waiting for the on-disk move.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    ensure!(
        result != 0,
        "journal replacement failed: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Call;
    use crate::Operation;
    use crate::Phase;

    #[tokio::test]
    async fn restart_retains_reservations_and_quarantines_exact_generations() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("control");
        let store = Store::open(&path).await.unwrap();
        let call = Call {
            instance: Uuid::new_v4(),
            owner: "thread".into(),
            generation: 7,
        };
        let mut state = Registry::default()
            .transition(Operation::Configure {
                provider: "openai".into(),
                voices: vec!["ash".into()],
            })
            .unwrap();
        state = state
            .transition(Operation::Register {
                instance: call.instance,
                owner: call.owner.clone(),
                provider: "openai".into(),
            })
            .unwrap();
        state = state
            .transition(Operation::Reserve {
                call: call.clone(),
                epoch: state.epoch,
            })
            .unwrap();
        store.persist(&state).unwrap();
        drop(store);
        let store = Store::open(&path).await.unwrap();
        let recovered = store.load().unwrap();
        assert_eq!(recovered.leases[&call.instance].call, call);
        assert_eq!(recovered.leases[&call.instance].phase, Phase::Unknown);
        assert!(recovered.stopping);
        assert!(!recovered.instances[&call.instance].connected);
        assert!(recovered.epoch > state.epoch);
    }

    #[tokio::test]
    async fn corrupt_journal_is_an_error_not_an_empty_registry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("control");
        let store = Store::open(&path).await.unwrap();
        private_open(&path.join("state.json"), true)
            .unwrap()
            .write_all(b"{broken")
            .unwrap();
        assert!(store.load().is_err());
    }
}
