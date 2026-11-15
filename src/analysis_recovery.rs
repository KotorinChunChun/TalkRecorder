//! Audio-model failure recovery. None means the current audio was NOT analyzed:
//! callers must preserve its original samples and mark it Unknown, never Silence.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::speech::{Analysis, DenoiseMode, SpeechClient, SpeechSettings};
use anyhow::{Result, anyhow};

type Loader<C> = Arc<dyn Fn(Arc<AtomicBool>) -> Result<C> + Send + Sync>;

struct Pending<C> {
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<Result<C>>,
}

/// At most one loader is in flight. Model initialization occurs on that thread,
/// while ordinary calls return None without waiting for it.
struct Recovery<C> {
    active: Option<C>,
    pending: Option<Pending<C>>,
    loader: Loader<C>,
    retry: Duration,
    next_attempt: Instant,
}

impl<C: Send + 'static> Recovery<C> {
    fn new(client: C, loader: Loader<C>, retry: Duration) -> Self {
        Self {
            active: Some(client),
            pending: None,
            loader,
            retry: retry.max(Duration::from_millis(1)),
            next_attempt: Instant::now(),
        }
    }

    fn call<T>(&mut self, operation: impl FnOnce(&mut C) -> Result<T>) -> Result<Option<T>> {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.handle.is_finished())
        {
            let pending = self.pending.take().expect("completed loader exists");
            match pending
                .handle
                .join()
                .map_err(|_| anyhow!("音声モデルの再準備スレッドが異常終了しました"))
                .and_then(|value| value)
            {
                Ok(client) => self.active = Some(client),
                Err(error) => {
                    self.next_attempt = Instant::now() + self.retry;
                    return Err(error.context(
                        "音声モデルの再準備に失敗しました。次回の再試行まで原音を保持します",
                    ));
                }
            }
        }
        if let Some(client) = self.active.as_mut() {
            return match operation(client) {
                Ok(value) => Ok(Some(value)),
                Err(error) => {
                    // Reusing a failed worker could consume a stale reply for
                    // another track. Destroy it and its context immediately.
                    self.active.take();
                    self.next_attempt = Instant::now() + self.retry;
                    Err(error)
                }
            };
        }
        if self.pending.is_none() && Instant::now() >= self.next_attempt {
            let cancel = Arc::new(AtomicBool::new(false));
            let thread_cancel = cancel.clone();
            let loader = self.loader.clone();
            let handle = match thread::Builder::new()
                .name("speech-reload".into())
                .spawn(move || loader(thread_cancel))
            {
                Ok(handle) => handle,
                Err(error) => {
                    self.next_attempt = Instant::now() + self.retry;
                    return Err(error.into());
                }
            };
            self.pending = Some(Pending { cancel, handle });
        }
        Ok(None)
    }
}

impl<C> Drop for Recovery<C> {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancel.store(true, Ordering::Release);
            // The production loader polls cancellation every 50 ms during its
            // handshake and preload. SpeechClient Drop reaps the child and I/O.
            let _ = pending.handle.join();
        }
    }
}

pub struct ResilientSpeech {
    state: Recovery<SpeechClient>,
    timeout: Duration,
}

impl ResilientSpeech {
    pub fn new(
        client: SpeechClient,
        settings: SpeechSettings,
        modes: Vec<DenoiseMode>,
        retry: Duration,
    ) -> Self {
        let loader: Loader<SpeechClient> = Arc::new(move |cancel| {
            let mut client = SpeechClient::spawn_cancellable(&settings, &cancel)?;
            client.prepare_denoise_cancellable(&modes, &cancel)?;
            Ok(client)
        });
        Self {
            state: Recovery::new(client, loader, retry),
            timeout: Duration::from_secs(10),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_millis(1));
        self
    }

    pub fn process(
        &mut self,
        track: usize,
        rate: u32,
        channels: u16,
        samples: &[f32],
        mode: DenoiseMode,
    ) -> Result<Option<Analysis>> {
        let timeout = self.timeout;
        self.state.call(|client| {
            client.process_with_timeout(track, rate, channels, samples, mode, timeout)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, atomic::AtomicUsize, mpsc};

    fn expire_retry<C>(state: &mut Recovery<C>) {
        state.next_attempt = Instant::now();
    }

    #[test]
    fn failure_becomes_nonblocking_warmup_with_one_loader_then_uses_new_context() {
        let loads = Arc::new(AtomicUsize::new(0));
        let started = loads.clone();
        let release = Arc::new(AtomicBool::new(false));
        let thread_release = release.clone();
        let fallback_release = release.clone();
        let (watchdog_done, watchdog_signal) = mpsc::channel();
        let watchdog = thread::spawn(move || {
            if watchdog_signal
                .recv_timeout(Duration::from_secs(2))
                .is_err()
            {
                // Even a regression which waits synchronously for the loader
                // must fail this test in finite time rather than hang forever.
                fallback_release.store(true, Ordering::Release);
            }
        });
        let loader: Loader<u32> = Arc::new(move |cancel| {
            started.fetch_add(1, Ordering::SeqCst);
            while !thread_release.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(1));
            }
            Ok(42)
        });
        let mut state = Recovery::new(1, loader, Duration::from_secs(60));
        assert!(state.call::<()>(|_| Err(anyhow!("worker failed"))).is_err());
        assert!(state.active.is_none());
        assert!(state.call(|_| Ok(0)).unwrap().is_none());
        assert_eq!(
            loads.load(Ordering::SeqCst),
            0,
            "retry interval must be respected"
        );
        expire_retry(&mut state);
        let start = Instant::now();
        assert!(state.call(|_| Ok(0)).unwrap().is_none());
        for _track in 0..100 {
            assert!(state.call(|_| Ok(0)).unwrap().is_none());
        }
        assert!(start.elapsed() < Duration::from_millis(250));
        let deadline = Instant::now() + Duration::from_secs(1);
        while loads.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        let _ = watchdog_done.send(());
        watchdog.join().unwrap();
        release.store(true, Ordering::Release);
        let mut value = None;
        while value.is_none() && Instant::now() < deadline {
            value = state.call(|client| Ok(*client)).unwrap();
            thread::yield_now();
        }
        assert_eq!(value, Some(42));
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn failed_loader_reports_once_and_waits_before_another_attempt() {
        let loads = Arc::new(AtomicUsize::new(0));
        let called = loads.clone();
        let loader: Loader<u8> = Arc::new(move |_| {
            called.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("load failure"))
        });
        let mut state = Recovery::new(0, loader, Duration::from_secs(60));
        assert!(state.call::<()>(|_| Err(anyhow!("first failure"))).is_err());
        expire_retry(&mut state);
        assert!(state.call(|_| Ok(())).unwrap().is_none());
        let deadline = Instant::now() + Duration::from_secs(1);
        while state
            .pending
            .as_ref()
            .is_some_and(|pending| !pending.handle.is_finished())
            && Instant::now() < deadline
        {
            thread::yield_now();
        }
        assert!(state.call(|_| Ok(())).is_err());
        for _ in 0..20 {
            assert!(state.call(|_| Ok(())).unwrap().is_none());
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn dropping_warmup_cancels_and_joins_the_only_loader() {
        let (started, seen) = mpsc::channel();
        let started = Mutex::new(started);
        let stopped = Arc::new(AtomicBool::new(false));
        let thread_stopped = stopped.clone();
        let loader: Loader<u8> = Arc::new(move |cancel| {
            started.lock().unwrap().send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !cancel.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            thread_stopped.store(cancel.load(Ordering::Acquire), Ordering::Release);
            Err(anyhow!("cancelled"))
        });
        let mut state = Recovery::new(0, loader, Duration::from_secs(1));
        assert!(state.call::<()>(|_| Err(anyhow!("failed"))).is_err());
        expire_retry(&mut state);
        assert!(state.call(|_| Ok(())).unwrap().is_none());
        seen.recv_timeout(Duration::from_secs(1)).unwrap();
        let started = Instant::now();
        drop(state);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(stopped.load(Ordering::Acquire));
    }
}
