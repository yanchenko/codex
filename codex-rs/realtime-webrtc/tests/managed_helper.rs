//! Opt-in packaged helper proof. No runtime, transport or audio devices are opened.
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Duration;

use codex_realtime_webrtc::Message;
use codex_realtime_webrtc::NativeLeaseProof;
use codex_realtime_webrtc::encode_frame;
use codex_realtime_webrtc::read_message;
use codex_voice_control::Call;
use codex_voice_control::LeaseGuard;

struct Helper {
    child: Child,
    replies: mpsc::Receiver<std::io::Result<Option<Message>>>,
}
impl Helper {
    fn start() -> Self {
        let path = PathBuf::from(
            std::env::var_os("CODEX_VOICE_HOST")
                .expect("set CODEX_VOICE_HOST to the matching packaged helper"),
        );
        let version = Command::new(&path).arg("--build-commit").output().unwrap();
        assert!(version.status.success());
        let commit = String::from_utf8(version.stdout).unwrap().trim().to_owned();
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut output = child.stdout.take().unwrap();
        let (send, replies) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let message = read_message(&mut output);
                let done = !matches!(&message, Ok(Some(_)));
                if send.send(message).is_err() || done {
                    break;
                }
            }
        });
        let mut helper = Self { child, replies };
        helper.send(Message::Hello {
            protocol: 1,
            build_commit: commit,
        });
        assert_eq!(helper.reply(), Some(Message::Ready {}));
        helper
    }
    fn send(&mut self, message: Message) {
        self.child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&encode_frame(&message).unwrap())
            .unwrap();
    }
    fn reply(&self) -> Option<Message> {
        self.replies
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
    }
    fn wait(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "helper failed to retire"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn guard() -> (tempfile::TempDir, LeaseGuard, NativeLeaseProof) {
    let temp = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let guard = LeaseGuard::create(
        temp.path(),
        &Call {
            instance: uuid::Uuid::new_v4(),
            owner: "test".into(),
            generation: 1,
        },
    )
    .unwrap();
    let proof = NativeLeaseProof {
        path: guard.path().canonicalize().unwrap(),
        nonce: guard.nonce().to_owned(),
    };
    (temp, guard, proof)
}

#[test]
#[ignore = "requires CODEX_VOICE_HOST pointing to a built helper; opens no devices"]
fn actual_helper_holds_guard_until_process_exit_and_rejects_tombstone() {
    let (_temp, owner, proof) = guard();
    let mut helper = Helper::start();
    helper.send(Message::AcquireLeaseGuard {
        proof: proof.clone(),
    });
    assert_eq!(helper.reply(), Some(Message::LeaseGuardAcquired {}));
    drop(owner);
    let mut inspector = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&proof.path)
        .unwrap();
    assert!(matches!(
        inspector.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    helper.send(Message::Close {});
    assert_eq!(helper.reply(), Some(Message::Closed {}));
    helper.wait();
    inspector.try_lock().unwrap();
    let bytes =
        serde_json::to_vec(&serde_json::json!({"version":1,"nonce":proof.nonce,"retired":true}))
            .unwrap();
    inspector.write_all(&bytes).unwrap();
    inspector.set_len(bytes.len() as u64).unwrap();
    inspector.sync_all().unwrap();
    drop(inspector);
    let mut late = Helper::start();
    late.send(Message::AcquireLeaseGuard { proof });
    assert_eq!(late.reply(), None);
    late.wait();
    assert!(!late.child.try_wait().unwrap().unwrap().success());
}

#[test]
#[ignore = "requires CODEX_VOICE_HOST pointing to a built helper; opens no devices"]
fn duplicate_helper_guard_control_exits_without_releasing_until_death() {
    let (_temp, owner, proof) = guard();
    let mut helper = Helper::start();
    helper.send(Message::AcquireLeaseGuard {
        proof: proof.clone(),
    });
    assert_eq!(helper.reply(), Some(Message::LeaseGuardAcquired {}));
    drop(owner);
    helper.send(Message::AcquireLeaseGuard {
        proof: proof.clone(),
    });
    assert_eq!(helper.reply(), None);
    helper.wait();
    assert!(!helper.child.try_wait().unwrap().unwrap().success());
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&proof.path)
        .unwrap()
        .try_lock()
        .unwrap();
}
