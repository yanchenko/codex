//! An OS lock shared by the starting TUI worker and the native audio helper.
//! Reconciliation tombstones the original file while exclusively locked, so a
//! delayed helper cannot acquire a newly freed voice after its parent crashed.
use crate::Call;
use crate::storage::private_open;
use anyhow::Result;
use anyhow::ensure;
use serde::Deserialize;
use serde::Serialize;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GuardIdentity {
    pub device: u64,
    pub file: u64,
    pub nonce: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    nonce: String,
    retired: bool,
}

/// Drop-only lock lifetime. Never call File::unlock: another holder may share
/// this open-file description. Clones keep canceled startup workers guarded.
#[derive(Clone)]
pub struct LeaseGuard {
    file: Arc<File>,
    path: PathBuf,
    nonce: String,
}

impl std::fmt::Debug for LeaseGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LeaseGuard([private])")
    }
}

impl LeaseGuard {
    pub fn create(directory: &Path, call: &Call) -> Result<Self> {
        let path = guard_path(directory, call);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options.open(&path)?;
        file.try_lock_shared()?;
        let nonce = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        write_record(
            &mut file,
            &Record {
                version: 1,
                nonce: nonce.clone(),
                retired: false,
            },
        )?;
        #[cfg(unix)]
        File::open(directory)?.sync_all()?;
        Ok(Self {
            file: Arc::new(file),
            path,
            nonce,
        })
    }

    /// The helper must call this before initializing/opening any audio, then
    /// retain the returned guard until actual process teardown. Read after lock.
    pub fn attach(path: &Path, expected_nonce: &str) -> Result<Self> {
        let file = private_open(path, false)?;
        file.try_lock_shared()?;
        let record = read_record(&file)?;
        ensure!(
            !record.retired && record.nonce == expected_nonce,
            "voice reservation retired before helper startup"
        );
        Ok(Self {
            file: Arc::new(file),
            path: path.to_owned(),
            nonce: record.nonce,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    pub fn identity(&self) -> Result<GuardIdentity> {
        let (device, file) = file_identity(&self.file)?;
        Ok(GuardIdentity {
            device,
            file,
            nonce: self.nonce.clone(),
        })
    }
}

pub(crate) fn guard_path(directory: &Path, call: &Call) -> PathBuf {
    directory.join(format!("lease-{}-{}.lock", call.instance, call.generation))
}

pub(crate) fn inspect_held(directory: &Path, call: &Call) -> Result<GuardIdentity> {
    let file = private_open(&guard_path(directory, call), false)?;
    // A reservation cannot be granted before the TUI's startup lifetime guard.
    match file.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => {}
        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        Ok(()) => anyhow::bail!("voice startup guard is not held"),
    }
    let record = read_record(&file)?;
    ensure!(!record.retired, "voice startup guard already retired");
    let (device, file) = file_identity(&file)?;
    Ok(GuardIdentity {
        device,
        file,
        nonce: record.nonce,
    })
}

/// Returns true only while an exclusive lock proves all TUI/helper holders gone
/// and a durable tombstone prevents queued late helpers from starting afterward.
pub(crate) fn retire_if_unheld(
    directory: &Path,
    call: &Call,
    identity: &GuardIdentity,
) -> Result<bool> {
    let mut file = private_open(&guard_path(directory, call), false)?;
    let (device, inode) = file_identity(&file)?;
    ensure!(
        device == identity.device && inode == identity.file,
        "voice guard file was replaced; lease remains quarantined"
    );
    match file.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => return Ok(false),
        Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
        Ok(()) => {}
    }
    let record = Record {
        version: 1,
        nonce: identity.nonce.clone(),
        retired: true,
    };
    write_record(&mut file, &record)?;
    Ok(true)
}

fn read_record(mut file: &File) -> Result<Record> {
    ensure!(file.metadata()?.len() <= 512, "invalid voice guard length");
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(513).read_to_end(&mut bytes)?;
    let record: Record = serde_json::from_slice(&bytes)?;
    ensure!(
        record.version == 1
            && record.nonce.len() == 64
            && record.nonce.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid voice guard record"
    );
    Ok(record)
}

fn write_record(file: &mut File, record: &Record) -> Result<()> {
    let bytes = serde_json::to_vec(record)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&bytes)?;
    file.set_len(bytes.len() as u64)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn file_identity(file: &File) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn file_identity(file: &File) -> Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION;
    use windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle;
    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the live File owns this handle, and the output pointer is valid.
    ensure!(
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) } != 0,
        "cannot identify voice guard"
    );
    // SAFETY: successful GetFileInformationByHandle initialized the structure.
    let information = unsafe { information.assume_init() };
    Ok((
        information.dwVolumeSerialNumber as u64,
        ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_eventually_retired(directory: &Path, call: &Call, identity: &GuardIdentity) {
        // Parallel process-spawn tests can briefly inherit CLOEXEC descriptors
        // between fork and exec. Reclamation correctly waits for every holder.
        for _ in 0..100 {
            if retire_if_unheld(directory, call, identity).unwrap() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("voice guard remained held after all expected owners exited");
    }

    #[test]
    fn no_reclaim_until_tui_and_helper_drop_then_late_helper_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let call = Call {
            instance: Uuid::new_v4(),
            owner: "thread".into(),
            generation: 1,
        };
        let tui = LeaseGuard::create(temp.path(), &call).unwrap();
        let identity = inspect_held(temp.path(), &call).unwrap();
        let helper = LeaseGuard::attach(tui.path(), tui.nonce()).unwrap();
        let path = tui.path().to_owned();
        let nonce = tui.nonce().to_owned();
        drop(tui);
        assert!(!retire_if_unheld(temp.path(), &call, &identity).unwrap());
        drop(helper);
        assert_eventually_retired(temp.path(), &call, &identity);
        assert!(LeaseGuard::attach(&path, &nonce).is_err());
    }
    #[test]
    fn missing_or_replaced_guard_is_not_evidence_of_retirement() {
        let temp = tempfile::tempdir().unwrap();
        let call = Call {
            instance: Uuid::new_v4(),
            owner: "thread".into(),
            generation: 1,
        };
        let guard = LeaseGuard::create(temp.path(), &call).unwrap();
        let identity = guard.identity().unwrap();
        let path = guard.path().to_owned();
        drop(guard);
        std::fs::rename(&path, temp.path().join("old")).unwrap();
        assert!(retire_if_unheld(temp.path(), &call, &identity).is_err());
        let replacement = LeaseGuard::create(temp.path(), &call).unwrap();
        drop(replacement);
        assert!(retire_if_unheld(temp.path(), &call, &identity).is_err());
    }
    #[test]
    fn child_holder() {
        let Some(path) = std::env::var_os("CODEX_TEST_VOICE_GUARD_PATH") else {
            return;
        };
        let nonce = std::env::var("CODEX_TEST_VOICE_GUARD_NONCE").unwrap();
        let _guard = LeaseGuard::attach(Path::new(&path), &nonce).unwrap();
        println!("GUARD_READY");
        std::io::stdout().flush().unwrap();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).unwrap();
    }

    #[test]
    fn actual_child_death_releases_lock_after_parent_guard_is_gone() {
        use std::io::BufRead;
        let temp = tempfile::tempdir().unwrap();
        let call = Call {
            instance: Uuid::new_v4(),
            owner: "thread".into(),
            generation: 1,
        };
        let guard = LeaseGuard::create(temp.path(), &call).unwrap();
        let identity = guard.identity().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "guard::tests::child_holder", "--nocapture"])
            .env("CODEX_TEST_VOICE_GUARD_PATH", guard.path())
            .env("CODEX_TEST_VOICE_GUARD_NONCE", guard.nonce())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert!(output.read_line(&mut line).unwrap() > 0);
            if line.contains("GUARD_READY") {
                break;
            }
        }
        drop(guard);
        assert!(!retire_if_unheld(temp.path(), &call, &identity).unwrap());
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eventually_retired(temp.path(), &call, &identity);
    }
    #[test]
    fn partial_tombstone_is_repaired_only_after_original_holders_exit() {
        let temp = tempfile::tempdir().unwrap();
        let call = Call {
            instance: Uuid::new_v4(),
            owner: "thread".into(),
            generation: 1,
        };
        let guard = LeaseGuard::create(temp.path(), &call).unwrap();
        let identity = guard.identity().unwrap();
        let path = guard.path().to_owned();
        let mut file = private_open(&path, false).unwrap();
        file.set_len(0).unwrap();
        file.write_all(b"{partial").unwrap();
        file.sync_all().unwrap();
        assert!(!retire_if_unheld(temp.path(), &call, &identity).unwrap());
        drop(guard);
        assert_eventually_retired(temp.path(), &call, &identity);
        assert!(LeaseGuard::attach(&path, &identity.nonce).is_err());
    }
}
