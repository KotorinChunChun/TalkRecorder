//! KotorinToys の管理契約2。パイプの実装は ScriptSync と同じ利用者・セッションの制限を使う。
mod pipe;

use crate::{APP_VERSION, engine::RecorderPhase};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::json;
use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TOOL_ID: &str = "talk-recorder";
const CAPABILITIES: [&str; 4] = ["status", "show", "stop", "silent-start"];
type PhaseReader = Box<dyn Fn() -> RecorderPhase + Send + Sync>;

#[derive(Default)]
pub(crate) struct RecordingStatus(RwLock<Option<PhaseReader>>);

impl RecordingStatus {
    pub fn watch(&self, read: impl Fn() -> RecorderPhase + Send + Sync + 'static) {
        *self.0.write().unwrap_or_else(|err| err.into_inner()) = Some(Box::new(read));
    }

    fn detail(&self) -> &'static str {
        match self
            .0
            .read()
            .ok()
            .and_then(|reader| reader.as_ref().map(|read| read()))
        {
            Some(RecorderPhase::Recording) => "録音中",
            Some(RecorderPhase::Preparing) => "録音の準備中",
            Some(RecorderPhase::Preview) => "入力確認中",
            Some(RecorderPhase::Finalizing) => "録音を保存中",
            Some(RecorderPhase::Failed) => {
                "録音を開始または継続できません。設定画面で確認してください。"
            }
            _ => "待機中",
        }
    }
}

pub(crate) struct Management {
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Management {
    pub fn start(
        origin: &'static str,
        status: Arc<RecordingStatus>,
        show: impl Fn() + Send + 'static,
        stop: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        Self::start_on(TOOL_ID.to_owned(), origin, status, show, stop)
    }

    fn start_on(
        id: String,
        origin: &'static str,
        status: Arc<RecordingStatus>,
        show: impl Fn() + Send + 'static,
        stop: impl Fn() + Send + 'static,
    ) -> Result<Self> {
        let shutdown = Arc::new(AtomicBool::new(false));
        let cancel = shutdown.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("talkrecorder-management".into())
            .spawn(move || {
                let server = match pipe::ManagementPipe::create(&id) {
                    Ok(server) => {
                        let _ = sender.send(Ok(()));
                        server
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        return;
                    }
                };
                let mut stopping = false;
                while !cancel.load(Ordering::Acquire) {
                    match server.accept(&cancel) {
                        Ok(true) => {}
                        Ok(false) => continue,
                        Err(error) => {
                            eprintln!("管理窓口の待機を終了: {error:#}");
                            break;
                        }
                    }
                    let deadline = Instant::now() + Duration::from_millis(1500);
                    let parsed = server
                        .read_line(deadline)
                        .and_then(|line| parse_request(&line));
                    let (request_id, result, detail, action) =
                        response(&server, &parsed, &status, stopping);
                    let reply = json!({
                        "protocolVersion": 2, "requestId": request_id, "pid": std::process::id(),
                        "ready": !stopping, "version": APP_VERSION, "launchOrigin": origin,
                        "capabilities": CAPABILITIES, "result": result, "detail": detail,
                    });
                    if server.write_line(&reply.to_string(), deadline).is_ok() {
                        match action {
                            Action::None => {}
                            Action::Show => show(),
                            Action::Stop => {
                                stopping = true;
                                stop();
                            }
                        }
                        server.wait_for_client_close(deadline);
                    }
                    server.disconnect();
                }
            })?;
        let result = Self {
            shutdown,
            worker: Some(worker),
        };
        receiver
            .recv_timeout(Duration::from_secs(3))
            .context("管理窓口の起動に応答がありません")??;
        Ok(result)
    }
}

impl Drop for Management {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    command: String,
    #[serde(default)]
    request_id: String,
    expected_pid: Option<u32>,
}

fn parse_request(line: &str) -> Result<Request> {
    if line == "status" {
        return Ok(Request {
            command: "status".into(),
            request_id: String::new(),
            expected_pid: None,
        });
    }
    let request: Request = serde_json::from_str(line)?;
    ensure!(request.request_id.len() <= 128, "要求IDが長すぎます");
    Ok(request)
}

enum Action {
    None,
    Show,
    Stop,
}

fn response<'a>(
    server: &pipe::ManagementPipe,
    parsed: &'a Result<Request>,
    status: &RecordingStatus,
    stopping: bool,
) -> (&'a str, &'static str, &'static str, Action) {
    if !server.same_session().unwrap_or(false) {
        return ("", "refused", "別の対話セッションです。", Action::None);
    }
    let Ok(request) = parsed else {
        return ("", "failed", "管理要求が不正です。", Action::None);
    };
    let id = request.request_id.as_str();
    match request.command.as_str() {
        "status" => (
            id,
            "completed",
            if stopping {
                "終了処理中"
            } else {
                status.detail()
            },
            Action::None,
        ),
        "show" | "stop" if request.expected_pid != Some(std::process::id()) => {
            (id, "refused", "対象PIDが一致しません。", Action::None)
        }
        "show" | "stop" if stopping => (id, "refused", "終了処理中です。", Action::None),
        "show" => (id, "completed", "", Action::Show),
        "stop" => (
            id,
            "accepted",
            "録音を終了し、保存の完了を待ちます。",
            Action::Stop,
        ),
        _ => (id, "unsupported", "未対応の操作です。", Action::None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    fn check_status(id: &str, detail: &str) -> Result<Duration> {
        let timeout = Duration::from_secs(2);
        let started = Instant::now();
        let mut client = pipe::test_client(id, timeout)?;
        client.write_all(b"status\n")?;
        let mut line = String::new();
        BufReader::new(client).read_line(&mut line)?;
        let elapsed = started.elapsed();
        ensure!(elapsed < timeout, "本体の照会期限を超えました: {elapsed:?}");
        let reply: serde_json::Value = serde_json::from_str(&line)?;
        assert_eq!(reply["pid"], std::process::id());
        assert_eq!(reply["version"], APP_VERSION);
        assert_eq!(reply["protocolVersion"], 2);
        assert_eq!(reply["ready"], true);
        assert_eq!(reply["launchOrigin"], "via_kotorintoys");
        assert_eq!(reply["result"], "completed");
        assert_eq!(reply["detail"], detail);
        Ok(elapsed)
    }

    #[test]
    fn status_survives_idle_wait_and_new_clients_during_recording_and_save() -> Result<()> {
        let id = format!("talk-recorder-status-{}", std::process::id());
        let state = Arc::new(RecordingStatus::default());
        let management =
            Management::start_on(id.clone(), "via_kotorintoys", state.clone(), || {}, || {})?;
        let mut abandoned = pipe::test_client(&id, Duration::from_secs(2))?;
        abandoned.write_all(b"stat")?;
        drop(abandoned);
        // 旧版が接続I/Oを再発行した100msの周期を複数回跨ぐ。
        thread::sleep(Duration::from_millis(350));
        for (phase, detail) in [
            (RecorderPhase::Finished, "待機中"),
            (RecorderPhase::Recording, "録音中"),
            (RecorderPhase::Finalizing, "録音を保存中"),
        ] {
            state.watch(move || phase);
            for _ in 0..4 {
                check_status(&id, detail)?;
            }
        }
        drop(management);
        Ok(())
    }

    #[test]
    fn concurrent_status_clients_reconnect_after_an_interrupted_request() -> Result<()> {
        const CLIENTS: usize = 8;
        let id = format!("talk-recorder-overlapping-{}", std::process::id());
        let status = Arc::new(RecordingStatus::default());
        status.watch(|| RecorderPhase::Recording);
        let _management =
            Management::start_on(id.clone(), "via_kotorintoys", status, || {}, || {})?;
        let timeout = Duration::from_secs(2);
        let mut longest = Duration::ZERO;
        for _ in 0..6 {
            let mut interrupted = pipe::test_client(&id, timeout)?;
            interrupted.write_all(b"stat")?;
            let ready = std::sync::Barrier::new(CLIENTS + 1);
            thread::scope(|scope| -> Result<()> {
                let callers = (0..CLIENTS)
                    .map(|_| {
                        scope.spawn(|| -> Result<Duration> {
                            ready.wait();
                            check_status(&id, "録音中")
                        })
                    })
                    .collect::<Vec<_>>();
                ready.wait();
                // 他の照会が待機してから、改行を送らずに前のクライアントを閉じる。
                thread::sleep(Duration::from_millis(100));
                drop(interrupted);
                for caller in callers {
                    longest = longest.max(caller.join().expect("照会試験スレッド")?);
                }
                Ok(())
            })?;
        }
        eprintln!("{} 件の同時・連続照会。最長 {longest:?}", 6 * CLIENTS);
        Ok(())
    }
}
