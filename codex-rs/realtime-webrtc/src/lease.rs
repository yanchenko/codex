//! Optional managed-call guard held by the helper until process exit.
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeLeaseProof {
    pub path: PathBuf,
    pub nonce: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_voice_control::Call;
    use codex_voice_control::LeaseGuard;

    #[test]
    fn coordinator_guard_and_native_guard_share_lifetime_and_reject_late_start() {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let call = Call {
            instance: uuid::Uuid::new_v4(),
            owner: "thread".into(),
            generation: 1,
        };
        let owner = LeaseGuard::create(temp.path(), &call).unwrap();
        let proof = NativeLeaseProof {
            path: owner.path().canonicalize().unwrap(),
            nonce: owner.nonce().to_owned(),
        };
        let native = proof.acquire().unwrap();
        let mut inspector = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&proof.path)
            .unwrap();
        drop(owner);
        assert!(matches!(
            inspector.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(native);
        inspector.try_lock().unwrap();
        use std::io::Write;
        let tombstone = serde_json::to_vec(
            &serde_json::json!({"version":1,"nonce":proof.nonce,"retired":true}),
        )
        .unwrap();
        inspector.write_all(&tombstone).unwrap();
        inspector.set_len(tombstone.len() as u64).unwrap();
        inspector.sync_all().unwrap();
        drop(inspector);
        assert!(proof.acquire().is_err());
    }

    #[test]
    fn wrong_nonce_and_replaced_guard_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let call = Call {
            instance: uuid::Uuid::new_v4(),
            owner: "thread".into(),
            generation: 1,
        };
        let owner = LeaseGuard::create(temp.path(), &call).unwrap();
        let mut proof = NativeLeaseProof {
            path: owner.path().canonicalize().unwrap(),
            nonce: "0".repeat(64),
        };
        assert!(proof.acquire().is_err());
        proof.nonce = owner.nonce().to_owned();
        #[cfg(unix)]
        {
            let link = temp.path().join("link.lock");
            std::os::unix::fs::symlink(&proof.path, &link).unwrap();
            proof.path = link;
            assert!(proof.acquire().is_err());
        }
    }
}

impl std::fmt::Debug for NativeLeaseProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeLeaseProof([private])")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    nonce: String,
    retired: bool,
}

impl NativeLeaseProof {
    /// Lock BEFORE reading the tombstone. A parent may die after queuing this
    /// message; coordinator retirement then prevents this late helper starting.
    pub fn acquire(&self) -> io::Result<File> {
        if !self.path.is_absolute()
            || self.path.as_os_str().len() > 4096
            || self.nonce.len() != 64
            || !self.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(io::Error::other("invalid voice guard descriptor"));
        }
        let parent = self
            .path
            .parent()
            .ok_or_else(|| io::Error::other("missing voice guard directory"))?;
        if parent.canonicalize()? != parent {
            return Err(io::Error::other("voice guard directory must be physical"));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_OPEN_REPARSE_POINT: inspect, never follow a replacement link.
            options.custom_flags(0x00200000);
        }
        let mut file = options.open(&self.path)?;
        file.try_lock_shared().map_err(io::Error::other)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > 512 {
            return Err(io::Error::other("invalid voice guard file"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let directory = parent.metadata()?;
            // SAFETY: geteuid has no preconditions and does not mutate process state.
            let uid = unsafe { libc::geteuid() };
            if metadata.uid() != uid
                || metadata.mode() & 0o077 != 0
                || directory.uid() != uid
                || directory.mode() & 0o077 != 0
            {
                return Err(io::Error::other("voice guard must be private to this user"));
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(io::Error::other("voice guard cannot be a reparse point"));
            }
        }
        let mut bytes = Vec::new();
        (&mut file).take(513).read_to_end(&mut bytes)?;
        let record: Record = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        if record.version != 1 || record.retired || record.nonce != self.nonce {
            return Err(io::Error::other("voice reservation is no longer admitted"));
        }
        Ok(file)
    }
}
