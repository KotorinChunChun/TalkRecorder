//! GUI-only instance ownership and activation for one Windows logon session.
//!
//! Call after handling worker and explicit CLI modes. The names intentionally do
//! not contain an EXE path, configuration path, or application version.

use anyhow::Result;

/// Keep this guard alive until the GUI, recording and maintenance have stopped.
pub struct SingleInstance {
    #[cfg(windows)]
    inner: platform::Instance,
}

/// Become the primary GUI, or notify the existing primary and return `None`.
/// No lock file survives normal or abnormal process termination.
pub fn acquire_or_notify(notify: bool) -> Result<Option<SingleInstance>> {
    #[cfg(windows)]
    {
        platform::acquire("Local\\TalkRecorder.Gui.v1", notify)
            .map(|instance| instance.map(|inner| SingleInstance { inner }))
    }
    #[cfg(not(windows))]
    {
        let _ = notify;
        anyhow::bail!("GUIの多重起動制御はWindowsのみ対応しています")
    }
}

impl SingleInstance {
    /// Consume any activation request; multiple requests may be coalesced.
    pub fn take_activation_request(&self) -> bool {
        #[cfg(windows)]
        {
            self.inner.take_activation_request()
        }
        #[cfg(not(windows))]
        false
    }

    /// Install a nonblocking repaint callback. Requests received before this
    /// call remain pending and also cause this callback to run.
    pub fn set_waker(&mut self, callback: impl Fn() + Send + Sync + 'static) -> Result<()> {
        #[cfg(windows)]
        {
            self.inner.set_waker(callback);
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = callback;
            anyhow::bail!("GUIの多重起動制御はWindowsのみ対応しています")
        }
    }
}

#[cfg(windows)]
mod platform {
    use anyhow::{Context, Result};
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
    };
    use windows::{
        Win32::{
            Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0},
            System::Threading::{
                CreateEventW, CreateMutexW, INFINITE, SetEvent, WaitForMultipleObjects,
            },
        },
        core::PCWSTR,
    };

    struct Handle(HANDLE);

    // Kernel mutex/event handles may be waited on and signaled from any thread.
    // Arc keeps the handle alive until all waits finish; there is no mutex owner
    // thread because CreateMutexW is called with bInitialOwner=false.
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    type Waker = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

    pub(super) struct Instance {
        // Retain ownership until after the watcher and activation handles close.
        // Declared last so automatic field destruction releases it last.
        pending: Arc<AtomicBool>,
        waker: Waker,
        stop: Arc<Handle>,
        watcher: Option<JoinHandle<()>>,
        _activation: Arc<Handle>,
        _mutex: Handle,
    }

    fn name(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    pub(super) fn acquire(prefix: &str, notify: bool) -> Result<Option<Instance>> {
        let activation_name = name(&format!("{prefix}.Activate"));
        // Open/create the auto-reset event before the mutex. A second launch
        // can now signal immediately, even while the primary is initializing.
        // Creating an existing event does not reset a previously posted signal.
        let activation = Arc::new(Handle(
            unsafe { CreateEventW(None, false, false, PCWSTR(activation_name.as_ptr())) }
                .context("既存画面への表示通知を準備できません")?,
        ));
        let mutex_name = name(&format!("{prefix}.Instance"));
        let mutex = unsafe { CreateMutexW(None, false, PCWSTR(mutex_name.as_ptr())) };
        // Capture LastError before any other OS operation or destructor.
        let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        let mutex = Handle(mutex.context("アプリの起動制限を準備できません")?);
        if already_exists {
            if notify {
                unsafe { SetEvent(activation.0) }
                    .context("既存の設定画面へ表示を要求できません")?;
            }
            return Ok(None);
        }
        let stop = Arc::new(Handle(
            unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
                .context("画面表示通知の停止処理を準備できません")?,
        ));
        let pending = Arc::new(AtomicBool::new(false));
        let waker: Waker = Arc::new(Mutex::new(None));
        let thread_stop = stop.clone();
        let thread_activation = activation.clone();
        let thread_pending = pending.clone();
        let thread_waker = waker.clone();
        let watcher = thread::Builder::new()
            .name("gui-activation".into())
            .spawn(move || {
                loop {
                    let result = unsafe {
                        WaitForMultipleObjects(
                            &[thread_stop.0, thread_activation.0],
                            false,
                            INFINITE,
                        )
                    };
                    if result.0 != WAIT_OBJECT_0.0 + 1 {
                        break;
                    }
                    thread_pending.store(true, Ordering::Release);
                    wake(&thread_waker);
                }
            })
            .context("画面表示通知の待受を開始できません")?;
        Ok(Some(Instance {
            pending,
            waker,
            stop,
            watcher: Some(watcher),
            _activation: activation,
            _mutex: mutex,
        }))
    }

    fn wake(waker: &Waker) {
        // Do not hold the lock while calling UI code.
        let callback = waker.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(callback) = callback {
            // A faulty repaint callback must not leave activation unattended.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback()));
        }
    }

    impl Instance {
        #[cfg(test)]
        pub(super) fn activation_is_pending(&self) -> bool {
            self.pending.load(Ordering::Acquire)
        }

        pub(super) fn take_activation_request(&self) -> bool {
            self.pending.swap(false, Ordering::AcqRel)
        }

        pub(super) fn set_waker(&mut self, callback: impl Fn() + Send + Sync + 'static) {
            *self.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(callback));
            if self.pending.load(Ordering::Acquire) {
                wake(&self.waker);
            }
        }
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            let _ = unsafe { SetEvent(self.stop.0) };
            if let Some(watcher) = self.watcher.take() {
                let _ = watcher.join();
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::platform;
    use std::{
        fs,
        io::Write,
        process::{Child, Command, Stdio},
        sync::mpsc,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    const PREFIX_ENV: &str = "TALKRECORDER_INSTANCE_TEST_PREFIX";
    const MODE_ENV: &str = "TALKRECORDER_INSTANCE_TEST_MODE";
    const DIR_ENV: &str = "TALKRECORDER_INSTANCE_TEST_DIR";

    fn unique_prefix() -> String {
        format!(
            "Local\\TalkRecorder.Test.{}.{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "子プロセスの応答期限超過");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    struct TestChild(Child);

    impl Drop for TestChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn child(prefix: &str, mode: &str, dir: &std::path::Path) -> TestChild {
        TestChild(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "single_instance::tests::subprocess_helper",
                    "--nocapture",
                ])
                .env(PREFIX_ENV, prefix)
                .env(MODE_ENV, mode)
                .env(DIR_ENV, dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }

    #[test]
    fn early_activation_is_kept_until_waker_is_installed() {
        let prefix = unique_prefix();
        let mut primary = platform::acquire(&prefix, true).unwrap().unwrap();
        assert!(platform::acquire(&prefix, true).unwrap().is_none());
        // Wait until the watcher has consumed the OS signal before attaching
        // the UI. set_waker must still repaint for this already-pending request.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !primary.activation_is_pending() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let (sender, receiver) = mpsc::channel();
        primary.set_waker(move || {
            let _ = sender.send(());
        });
        receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(primary.take_activation_request());
        assert!(!primary.take_activation_request());
        drop(primary);
        assert!(platform::acquire(&prefix, true).unwrap().is_some());
    }

    #[test]
    fn separate_process_is_forwarded_and_wakes_primary() {
        let prefix = unique_prefix();
        let dir = tempfile::tempdir().unwrap();
        let mut primary = platform::acquire(&prefix, true).unwrap().unwrap();
        let (sender, receiver) = mpsc::channel();
        primary.set_waker(move || {
            let _ = sender.send(());
        });
        let mut secondary = child(&prefix, "secondary", dir.path());
        wait_until(|| secondary.0.try_wait().unwrap().is_some());
        assert!(secondary.0.wait().unwrap().success());
        assert_eq!(
            fs::read_to_string(dir.path().join("result")).unwrap(),
            "forwarded"
        );
        receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(primary.take_activation_request());
        drop(primary);
        assert!(platform::acquire(&prefix, true).unwrap().is_some());
    }

    #[test]
    fn simultaneous_processes_select_exactly_one_primary() {
        let prefix = unique_prefix();
        let dir = tempfile::tempdir().unwrap();
        let mut first = child(&prefix, "compete-1", dir.path());
        let mut second = child(&prefix, "compete-2", dir.path());
        wait_until(|| {
            dir.path().join("ready-compete-1").is_file()
                && dir.path().join("ready-compete-2").is_file()
        });
        fs::write(dir.path().join("go"), "go").unwrap();
        wait_until(|| {
            dir.path().join("result-compete-1").is_file()
                && dir.path().join("result-compete-2").is_file()
        });
        let results = [
            fs::read_to_string(dir.path().join("result-compete-1")).unwrap(),
            fs::read_to_string(dir.path().join("result-compete-2")).unwrap(),
        ];
        assert_eq!(
            results.iter().filter(|value| *value == "primary").count(),
            1
        );
        assert_eq!(
            results.iter().filter(|value| *value == "forwarded").count(),
            1
        );
        for child in [&mut first, &mut second] {
            child.0.stdin.take().unwrap().write_all(b"quit\n").unwrap();
            wait_until(|| child.0.try_wait().unwrap().is_some());
            assert!(child.0.wait().unwrap().success());
        }
        assert!(platform::acquire(&prefix, true).unwrap().is_some());
    }

    #[test]
    fn abnormal_primary_exit_releases_os_ownership() {
        let prefix = unique_prefix();
        let dir = tempfile::tempdir().unwrap();
        let mut primary = child(&prefix, "primary", dir.path());
        wait_until(|| dir.path().join("ready").is_file());
        assert!(platform::acquire(&prefix, true).unwrap().is_none());
        primary.0.kill().unwrap();
        primary.0.wait().unwrap();
        assert!(platform::acquire(&prefix, true).unwrap().is_some());
    }

    #[test]
    fn normal_primary_exit_releases_os_ownership() {
        let prefix = unique_prefix();
        let dir = tempfile::tempdir().unwrap();
        let mut primary = child(&prefix, "primary", dir.path());
        wait_until(|| dir.path().join("ready").is_file());
        assert!(platform::acquire(&prefix, true).unwrap().is_none());
        primary
            .0
            .stdin
            .take()
            .unwrap()
            .write_all(b"quit\n")
            .unwrap();
        wait_until(|| primary.0.try_wait().unwrap().is_some());
        assert!(primary.0.wait().unwrap().success());
        assert!(platform::acquire(&prefix, true).unwrap().is_some());
    }

    #[test]
    fn subprocess_helper() {
        let Ok(prefix) = std::env::var(PREFIX_ENV) else {
            return;
        };
        assert!(prefix.starts_with("Local\\TalkRecorder.Test."));
        let mode = std::env::var(MODE_ENV).unwrap();
        let dir = std::path::PathBuf::from(std::env::var_os(DIR_ENV).unwrap());
        if mode.starts_with("compete-") {
            fs::write(dir.join(format!("ready-{mode}")), "ready").unwrap();
            wait_until(|| dir.join("go").is_file());
            let instance = platform::acquire(&prefix, true).unwrap();
            fs::write(
                dir.join(format!("result-{mode}")),
                if instance.is_some() {
                    "primary"
                } else {
                    "forwarded"
                },
            )
            .unwrap();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap();
            assert_eq!(input.trim(), "quit");
            drop(instance);
            return;
        }
        let instance = platform::acquire(&prefix, true).unwrap();
        if mode == "secondary" {
            assert!(instance.is_none());
            fs::write(dir.join("result"), "forwarded").unwrap();
        } else {
            let _instance = instance.expect("primary test process must own the instance");
            fs::write(dir.join("ready"), "ready").unwrap();
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap();
            assert_eq!(input.trim(), "quit");
        }
    }
}
