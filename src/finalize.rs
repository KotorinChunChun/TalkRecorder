//! 内部確定済み音声の結合を、取得・解析から独立して実行する。
use crate::journal::JournalWriter;
use anyhow::{Result, anyhow, bail};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

struct Job {
    writer: JournalWriter,
    source_until: u64,
    discard: bool,
    _session_lock: Option<Arc<crate::recovery::SessionLock>>,
    #[cfg(test)]
    pause: Option<std::sync::mpsc::Receiver<()>>,
}

#[derive(Clone)]
pub struct FinalizerClient {
    sender: SyncSender<Option<Job>>,
    pending: Arc<AtomicUsize>,
    error: Arc<Mutex<Option<String>>>,
    session_lock: Option<Arc<crate::recovery::SessionLock>>,
    closed: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    lifecycle: Arc<Mutex<()>>,
}
impl FinalizerClient {
    pub fn with_session_lock(mut self, lock: Arc<crate::recovery::SessionLock>) -> Self {
        self.session_lock = Some(lock);
        self
    }
    pub fn submit(&self, writer: JournalWriter, source_until: u64, discard: bool) -> Result<()> {
        self.enqueue(Job {
            writer,
            source_until,
            discard,
            _session_lock: self.session_lock.clone(),
            #[cfg(test)]
            pause: None,
        })
    }
    fn enqueue(&self, job: Job) -> Result<()> {
        let _gate = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        anyhow::ensure!(
            !self.closed.load(Ordering::Acquire),
            "公開音声の結合処理は終了しています。内部確定音声は保持されています"
        );
        self.check()?;
        self.pending.fetch_add(1, Ordering::AcqRel);
        if let Err(error) = self.sender.try_send(Some(job)) {
            self.pending.fetch_sub(1, Ordering::AcqRel);
            bail!("公開音声の結合待ちが上限に達しました。内部確定音声は保持されています: {error}");
        }
        Ok(())
    }
    pub fn check(&self) -> Result<()> {
        anyhow::ensure!(
            self.alive.load(Ordering::Acquire) || self.pending.load(Ordering::Acquire) == 0,
            "公開音声の結合スレッドが未処理の音声を残して終了しました。内部音声は復旧可能です"
        );
        if let Some(error) = self
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            bail!("公開音声の確定に失敗しました。内部音声は復旧可能です: {error}");
        }
        Ok(())
    }
    pub fn wait(&self) -> Result<()> {
        while self.pending.load(Ordering::Acquire) > 0 {
            self.check()?;
            thread::sleep(Duration::from_millis(10));
        }
        self.check()
    }
    fn shutdown(&self) {
        let _gate = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        if !self.closed.swap(true, Ordering::AcqRel) {
            let _ = self.sender.send(None);
        }
    }
}

struct WorkerExit(Arc<AtomicBool>);
impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

pub struct Finalizer {
    client: Option<FinalizerClient>,
    handle: Option<JoinHandle<()>>,
}
impl Default for Finalizer {
    fn default() -> Self {
        Self::new()
    }
}
impl Finalizer {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Option<Job>>(2);
        let pending = Arc::new(AtomicUsize::new(0));
        let error = Arc::new(Mutex::new(None));
        let count = pending.clone();
        let fault = error.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let worker_alive = alive.clone();
        let handle = thread::spawn(move || {
            let _alive = WorkerExit(worker_alive);
            while let Ok(Some(job)) = receiver.recv() {
                #[cfg(test)]
                if let Some(gate) = &job.pause {
                    let _ = gate.recv_timeout(Duration::from_secs(5));
                }
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    job.writer.finish(job.source_until, job.discard)
                }));
                let failure = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(_) => Some("結合スレッドの処理が異常終了しました".into()),
                };
                if let Some(message) = failure {
                    let mut first = fault.lock().unwrap_or_else(|e| e.into_inner());
                    first.get_or_insert(message);
                }
                drop(job._session_lock);
                count.fetch_sub(1, Ordering::AcqRel);
            }
        });
        Self {
            client: Some(FinalizerClient {
                sender,
                pending,
                error,
                session_lock: None,
                closed,
                alive,
                lifecycle: Arc::new(Mutex::new(())),
            }),
            handle: Some(handle),
        }
    }
    pub fn client(&self) -> FinalizerClient {
        self.client.as_ref().expect("終了前の結合処理").clone()
    }
    pub fn finish(mut self) -> Result<()> {
        let client = self.client.take().expect("結合処理");
        client.shutdown();
        if let Some(handle) = self.handle.take() {
            handle
                .join()
                .map_err(|_| anyhow!("結合スレッドが異常終了しました"))?;
        }
        client.wait()
    }
}
impl Drop for Finalizer {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            client.shutdown();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::OutputFormat, journal, recovery::SessionLock};
    use std::{fs, path::Path};

    fn prepared(session: &Path, number: u64) -> Result<JournalWriter> {
        let mut writer = JournalWriter::new(
            session,
            number,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        writer.write(&[vec![0.25; 100], vec![0.25; 200]])?;
        writer.prepare_finish(100, false)?;
        Ok(writer)
    }
    fn bounded(action: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = sender.send(action());
        });
        receiver
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow!("結合処理の終了がcloneの破棄を待っています"))?
    }

    #[test]
    fn successful_wav_job_publishes_all_tracks_and_returns_with_an_external_clone() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let service = Finalizer::new();
        let external = service.client();
        external.submit(prepared(directory.path(), 1)?, 100, false)?;
        bounded(move || service.finish())?;
        assert!(directory.path().join("000001.complete.json").exists());
        for (track, channels) in [1, 2].into_iter().enumerate() {
            assert_eq!(
                fs::metadata(
                    directory
                        .path()
                        .join(format!("000001_track{}.wav", track + 1))
                )?
                .len(),
                80 + 100 * channels * 4
            );
        }
        external.wait()?;
        let second = prepared(directory.path(), 2)?;
        assert!(external.submit(second, 100, false).is_err());
        assert!(!journal::load_state(directory.path(), 2)?.unwrap().published);
        Ok(())
    }

    #[test]
    fn drop_finishes_without_waiting_for_an_external_client_clone() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let service = Finalizer::new();
        let external = service.client();
        external.submit(prepared(directory.path(), 1)?, 100, false)?;
        bounded(move || {
            drop(service);
            Ok(())
        })?;
        external.wait()?;
        assert!(directory.path().join("000001.complete.json").exists());
        Ok(())
    }

    #[test]
    fn path_conflict_is_reported_and_keeps_checkpoint_audio() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let service = Finalizer::new();
        let client = service.client();
        let writer = prepared(directory.path(), 1)?;
        fs::write(
            directory.path().join("000001_track1.wav"),
            b"existing user audio",
        )?;
        client.submit(writer, 100, false)?;
        assert!(service.finish().is_err());
        assert!(client.check().is_err());
        assert!(client.wait().is_err());
        assert_eq!(
            fs::read(directory.path().join("000001_track1.wav"))?,
            b"existing user audio"
        );
        assert!(!journal::load_state(directory.path(), 1)?.unwrap().published);
        assert!(
            directory
                .path()
                .join("journal/000001/000001_track1.wav")
                .exists()
        );
        Ok(())
    }

    fn disconnected_client() -> FinalizerClient {
        let (sender, receiver) = mpsc::sync_channel(2);
        drop(receiver);
        FinalizerClient {
            sender,
            pending: Arc::new(AtomicUsize::new(0)),
            error: Arc::new(Mutex::new(None)),
            session_lock: None,
            closed: Arc::new(AtomicBool::new(false)),
            alive: Arc::new(AtomicBool::new(true)),
            lifecycle: Arc::new(Mutex::new(())),
        }
    }

    #[test]
    fn closed_receiver_and_full_queue_preserve_checkpoint_parts() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let disconnected = disconnected_client();
        assert!(
            disconnected
                .submit(prepared(directory.path(), 1)?, 100, false)
                .is_err()
        );
        assert_eq!(disconnected.pending.load(Ordering::Acquire), 0);
        assert!(
            directory
                .path()
                .join("journal/000001/000001_track1.wav")
                .exists()
        );

        let (sender, receiver) = mpsc::sync_channel(2);
        let client = FinalizerClient {
            sender,
            ..disconnected_client()
        };
        client.submit(prepared(directory.path(), 2)?, 100, false)?;
        client.submit(prepared(directory.path(), 3)?, 100, false)?;
        assert!(
            client
                .submit(prepared(directory.path(), 4)?, 100, false)
                .is_err()
        );
        assert_eq!(client.pending.load(Ordering::Acquire), 2);
        assert!(
            directory
                .path()
                .join("journal/000004/000001_track1.wav")
                .exists()
        );
        drop(receiver);
        client.alive.store(false, Ordering::Release);
        assert!(client.wait().is_err());
        for number in 1..=4 {
            assert!(
                !journal::load_state(directory.path(), number)?
                    .unwrap()
                    .published
            );
        }
        Ok(())
    }

    #[test]
    fn session_lock_outlives_the_client_until_its_paused_job_finishes() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let service = Finalizer::new();
        let lock = Arc::new(SessionLock::acquire(directory.path())?);
        let client = service.client().with_session_lock(lock.clone());
        let (release, gate) = mpsc::sync_channel(1);
        client.enqueue(Job {
            writer: prepared(directory.path(), 1)?,
            source_until: 100,
            discard: false,
            _session_lock: client.session_lock.clone(),
            pause: Some(gate),
        })?;
        drop(lock);
        drop(client);
        assert!(SessionLock::acquire(directory.path()).is_err());
        release.send(())?;
        service.finish()?;
        assert!(SessionLock::acquire(directory.path()).is_ok());
        Ok(())
    }

    #[test]
    fn shutdown_rejects_new_jobs_before_the_worker_exits() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let service = Finalizer::new();
        let client = service.client();
        let (release, gate) = mpsc::sync_channel(1);
        client.enqueue(Job {
            writer: prepared(directory.path(), 1)?,
            source_until: 100,
            discard: false,
            _session_lock: None,
            pause: Some(gate),
        })?;
        client.shutdown();
        assert!(
            client
                .submit(prepared(directory.path(), 2)?, 100, false)
                .is_err()
        );
        assert_eq!(client.pending.load(Ordering::Acquire), 1);
        release.send(())?;
        service.finish()?;
        assert!(!journal::load_state(directory.path(), 2)?.unwrap().published);
        assert!(
            directory
                .path()
                .join("journal/000002/000001_track1.wav")
                .exists()
        );
        Ok(())
    }
}
