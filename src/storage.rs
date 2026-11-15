use crate::config::OutputFormat;
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub struct Spool {
    pub channels: Vec<u16>,
    pub frames: u64,
    pub dir: PathBuf,
}

impl Spool {
    pub fn create(dir: &Path, channels: Vec<u16>) -> Result<Self> {
        fs::create_dir_all(dir)?;
        ensure!(
            !channels.is_empty() && channels.iter().all(|&c| c > 0),
            "チャンネル数が不正です"
        );
        Ok(Self {
            channels,
            frames: 0,
            dir: dir.into(),
        })
    }
    pub fn open(dir: &Path, channels: Vec<u16>) -> Result<Self> {
        ensure!(dir.is_dir(), "一時音声フォルダーが見つかりません");
        Ok(Self {
            channels,
            frames: 0,
            dir: dir.into(),
        })
    }
    pub fn append(&mut self, tracks: &[Vec<f32>]) -> Result<u64> {
        ensure!(
            tracks.len() == self.channels.len(),
            "トラック数が一致しません"
        );
        let frames = tracks
            .first()
            .map_or(0, |v| v.len() / usize::from(self.channels[0]));
        ensure!(
            self.frames.is_multiple_of(48000) && frames > 0 && frames <= 48000,
            "一時保存の区切りが不正です"
        );
        for (i, (samples, channels)) in tracks.iter().zip(&self.channels).enumerate() {
            ensure!(
                samples.len() == frames * usize::from(*channels),
                "トラックの尺が一致しません"
            );
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(self.dir.join(format!("{:020}_{i}.pcm", self.frames)))?;
            for chunk in samples.chunks(8192) {
                let bytes: Vec<u8> = chunk.iter().flat_map(|s| s.to_le_bytes()).collect();
                file.write_all(&bytes)?;
            }
        }
        self.frames += frames as u64;
        Ok(self.frames)
    }
    pub fn read(&mut self, start: u64, end: u64) -> Result<Vec<Vec<f32>>> {
        ensure!(
            end >= start && end - start <= 48000 * 10,
            "一時音声の読み取り範囲が不正です"
        );
        self.channels
            .iter()
            .enumerate()
            .map(|(i, &channels)| {
                let mut out = Vec::with_capacity((end - start) as usize * usize::from(channels));
                let mut at = start;
                while at < end {
                    let base = at / 48000 * 48000;
                    let n = (end.min(base + 48000) - at) as usize * usize::from(channels);
                    let mut file = File::open(self.dir.join(format!("{base:020}_{i}.pcm")))?;
                    file.seek(SeekFrom::Start((at - base) * u64::from(channels) * 4))?;
                    let mut bytes = vec![0; n * 4];
                    file.read_exact(&mut bytes)?;
                    out.extend(
                        bytes
                            .chunks_exact(4)
                            .map(|v| f32::from_le_bytes(v.try_into().unwrap())),
                    );
                    at = end.min(base + 48000);
                }
                Ok(out)
            })
            .collect()
    }
    pub fn discard_before(&self, frame: u64) -> Result<()> {
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let base = name.split('_').next().and_then(|v| v.parse::<u64>().ok());
            if base.is_some_and(|base| base + 48000 <= frame)
                && (name.ends_with(".pcm") || name.ends_with(".json"))
            {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }
}

enum Sink {
    Wav {
        file: File,
        channels: u16,
        frames: u64,
    },
    Encoder {
        input: EncoderInput,
    },
}

/// stdin の同期書込みが詰まっても、制御側から子プロセスを終了できる。
/// 転送要求と完了通知はそれぞれ容量1、常に最大1件だけを処理する。
struct EncoderInput {
    child: Child,
    requests: Option<SyncSender<Option<Vec<u8>>>>,
    replies: Receiver<std::io::Result<()>>,
    writer: Option<JoinHandle<()>>,
    timeout: Duration,
}

impl EncoderInput {
    fn new(mut child: Child, timeout: Duration) -> Result<Self> {
        let Some(mut stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            bail!("エンコーダーの入力パイプがありません");
        };
        let (requests, incoming) = mpsc::sync_channel::<Option<Vec<u8>>>(1);
        let (completed, replies) = mpsc::sync_channel(1);
        let writer = match thread::Builder::new()
            .name("encoder-input".into())
            .spawn(move || {
                while let Ok(request) = incoming.recv() {
                    match request {
                        Some(bytes) => {
                            let result = stdin.write_all(&bytes);
                            let failed = result.is_err();
                            // 制御側がタイムアウトしても返信送信で停止しない。
                            if completed.try_send(result).is_err() || failed {
                                return;
                            }
                        }
                        None => {
                            let result = stdin.flush();
                            drop(stdin);
                            let _ = completed.try_send(result);
                            return;
                        }
                    }
                }
            }) {
            Ok(writer) => writer,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("圧縮入力スレッドを開始できません");
            }
        };
        Ok(Self {
            child,
            requests: Some(requests),
            replies,
            writer: Some(writer),
            timeout,
        })
    }

    fn exchange(&mut self, mut request: Option<Vec<u8>>) -> Result<()> {
        // キューへの投入と実際の write_all の両方を同じ期限に含める。
        let deadline = Instant::now() + self.timeout;
        let result = (|| -> Result<()> {
            let sender = self.requests.as_ref().context("圧縮入力は終了しています")?;
            loop {
                match sender.try_send(request) {
                    Ok(()) => break,
                    Err(TrySendError::Disconnected(_)) => bail!("圧縮入力スレッドが終了しました"),
                    Err(TrySendError::Full(returned)) => {
                        ensure!(
                            Instant::now() < deadline,
                            "圧縮入力の転送が制限時間を超えました"
                        );
                        request = returned;
                        thread::sleep(Duration::from_millis(10));
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.replies
                .recv_timeout(remaining)
                .context("圧縮入力の書込みが制限時間内に完了しませんでした")??;
            Ok(())
        })();
        if result.is_err() {
            self.abort();
        }
        result
    }

    fn close_input(&mut self) -> Result<()> {
        self.exchange(None)?;
        self.requests.take();
        self.join_writer()
    }

    fn wait(&mut self) -> Result<()> {
        ensure!(
            wait_for_encoder(&mut self.child, self.timeout)?.success(),
            "圧縮処理が失敗しました。encoder.log を確認してください"
        );
        Ok(())
    }

    fn join_writer(&mut self) -> Result<()> {
        if let Some(writer) = self.writer.take() {
            writer
                .join()
                .map_err(|_| anyhow::anyhow!("圧縮入力スレッドが異常終了しました"))?;
        }
        Ok(())
    }

    fn abort(&mut self) {
        self.requests.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        // 子プロセスの入力ハンドルが閉じれば、書込みは BrokenPipe で戻る。
        let _ = self.join_writer();
    }
}

impl Drop for EncoderInput {
    fn drop(&mut self) {
        self.abort();
    }
}

pub struct SegmentWriter {
    sinks: Vec<Sink>,
    paths: Vec<(PathBuf, PathBuf)>,
    frames: u64,
    channels: Vec<u16>,
    format: OutputFormat,
    pub number: u64,
    dir: PathBuf,
}

fn rf64_header(channels: u16, frames: u64) -> [u8; 80] {
    let mut h = [0u8; 80];
    h[0..4].copy_from_slice(b"RF64");
    h[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"ds64");
    h[16..20].copy_from_slice(&28u32.to_le_bytes());
    let size = frames * u64::from(channels) * 4;
    h[20..28].copy_from_slice(&(size + 72).to_le_bytes());
    h[28..36].copy_from_slice(&size.to_le_bytes());
    h[36..44].copy_from_slice(&frames.to_le_bytes());
    h[48..52].copy_from_slice(b"fmt ");
    h[52..56].copy_from_slice(&16u32.to_le_bytes());
    h[56..58].copy_from_slice(&3u16.to_le_bytes());
    h[58..60].copy_from_slice(&channels.to_le_bytes());
    h[60..64].copy_from_slice(&48000u32.to_le_bytes());
    h[64..68].copy_from_slice(&(48000 * u32::from(channels) * 4).to_le_bytes());
    h[68..70].copy_from_slice(&(channels * 4).to_le_bytes());
    h[70..72].copy_from_slice(&32u16.to_le_bytes());
    h[72..76].copy_from_slice(b"data");
    h[76..80].copy_from_slice(&u32::MAX.to_le_bytes());
    h
}

impl SegmentWriter {
    pub fn new(
        dir: &Path,
        number: u64,
        channels: Vec<u16>,
        format: OutputFormat,
        ffmpeg: &Path,
    ) -> Result<Self> {
        ensure!(
            !channels.is_empty() && channels.iter().all(|&ch| (1..=8).contains(&ch)),
            "保存チャンネル数が不正です"
        );
        ensure!(dir.is_dir(), "保存先フォルダーが見つかりません");
        ensure!(
            !dir.join(format!("{number:06}.complete.json")).exists(),
            "同じ番号の確定済みセグメントがあります"
        );
        let mut s = Self {
            sinks: vec![],
            paths: vec![],
            frames: 0,
            channels: channels.clone(),
            format,
            number,
            dir: dir.into(),
        };
        for (i, &ch) in channels.iter().enumerate() {
            let ext = format.extension();
            let final_path = dir.join(format!("{number:06}_track{}.{}", i + 1, ext));
            let partial = final_path.with_extension(format!("{ext}.partial"));
            ensure!(
                !final_path.exists() && !partial.exists(),
                "保存先ファイルが既にあります"
            );
            let sink = if matches!(format, OutputFormat::Wav) {
                let mut file = File::create(&partial)?;
                file.write_all(&rf64_header(ch, 0))?;
                Sink::Wav {
                    file,
                    channels: ch,
                    frames: 0,
                }
            } else {
                let err =
                    File::create(dir.join(format!("{number:06}_track{}.encoder.log", i + 1)))?;
                let mut cmd = Command::new(ffmpeg);
                cmd.args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-nostdin",
                    "-f",
                    "f32le",
                    "-ar",
                    "48000",
                    "-ac",
                    &ch.to_string(),
                    "-i",
                    "pipe:0",
                ]);
                match format {
                    OutputFormat::Flac => {
                        cmd.args(["-c:a", "flac", "-sample_fmt", "s32", "-f", "flac"]);
                    }
                    OutputFormat::Opus => {
                        cmd.args([
                            "-c:a",
                            "libopus",
                            "-b:a",
                            if ch == 1 { "64000" } else { "128000" },
                            "-f",
                            "opus",
                        ]);
                    }
                    OutputFormat::M4a => {
                        // The MP4 movie clock must use the audio sample clock:
                        // a 1 kHz clock (older FFmpeg default) rounds edit-list
                        // endpoints. Keep this explicit across encoder versions.
                        cmd.args([
                            "-c:a",
                            "aac",
                            "-profile:a",
                            "aac_low",
                            "-b:a",
                            if ch == 1 { "64000" } else { "128000" },
                            "-f",
                            "ipod",
                            "-movie_timescale",
                            "48000",
                            "-use_editlist",
                            "1",
                            "-movflags",
                            "+faststart",
                        ]);
                    }
                    OutputFormat::Wav => unreachable!(),
                }
                hide_console(&mut cmd);
                let child = cmd
                    .arg(&partial)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::from(err))
                    .spawn()
                    .context(
                        "FFmpeg を起動できません。WAV を選ぶか FFmpeg の場所を設定してください",
                    )?;
                Sink::Encoder {
                    input: EncoderInput::new(child, Duration::from_secs(30))?,
                }
            };
            s.sinks.push(sink);
            s.paths.push((partial, final_path));
        }
        Ok(s)
    }
    pub fn write(&mut self, tracks: &[Vec<f32>]) -> Result<()> {
        ensure!(
            tracks.len() == self.channels.len(),
            "保存トラック数が不正です"
        );
        let count = tracks[0].len() / usize::from(self.channels[0]);
        for (data, &ch) in tracks.iter().zip(&self.channels) {
            ensure!(
                data.len() == count * usize::from(ch),
                "保存音声の尺が一致しません"
            );
            ensure!(
                data.iter().all(|sample| sample.is_finite()),
                "保存音声に不正な値があります"
            );
        }
        for ((sink, data), &ch) in self.sinks.iter_mut().zip(tracks).zip(&self.channels) {
            ensure!(
                data.len() == count * usize::from(ch),
                "保存音声の尺が一致しません"
            );
            let bytes: Vec<u8> = data.iter().flat_map(|s| s.to_le_bytes()).collect();
            match sink {
                Sink::Wav { file, frames, .. } => {
                    file.write_all(&bytes)?;
                    *frames += count as u64;
                }
                Sink::Encoder { input } => input.exchange(Some(bytes))?,
            }
        }
        self.frames += count as u64;
        Ok(())
    }
    pub fn finish(mut self, discard: bool) -> Result<()> {
        // 全エンコーダーに EOF を通知してから待つ。後続トラックのパイプを
        // 開いたまま先頭トラックの終了だけを待たない。
        for sink in &mut self.sinks {
            if let Sink::Encoder { input } = sink {
                input.close_input()?;
            }
        }
        for sink in &mut self.sinks {
            match sink {
                Sink::Wav {
                    file,
                    channels,
                    frames,
                } => {
                    ensure!(*frames == self.frames, "保存音声の尺が一致しません");
                    file.seek(SeekFrom::Start(0))?;
                    file.write_all(&rf64_header(*channels, *frames))?;
                    file.sync_all()?;
                }
                Sink::Encoder { input } => input.wait()?,
            }
        }
        self.sinks.clear();
        for (partial, final_path) in &self.paths {
            if discard || self.frames == 0 {
                fs::remove_file(partial)?;
            } else {
                fs::rename(partial, final_path)?;
            }
        }
        if !discard && self.frames > 0 {
            #[derive(Serialize)]
            struct Commit<'a> {
                number: u64,
                frames: u64,
                sample_rate: u32,
                channels: &'a [u16],
                format: OutputFormat,
            }
            let value = Commit {
                number: self.number,
                frames: self.frames,
                sample_rate: 48000,
                channels: &self.channels,
                format: self.format,
            };
            let path = self.dir.join(format!("{:06}.complete.json", self.number));
            let temporary = self
                .dir
                .join(format!("{:06}.complete.json.partial", self.number));
            let mut manifest = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            serde_json::to_writer_pretty(&mut manifest, &value)?;
            manifest.write_all(b"\n")?;
            manifest.sync_all()?;
            drop(manifest);
            fs::rename(&temporary, path)?;
        }
        Ok(())
    }
}
fn wait_for_encoder(
    child: &mut Child,
    timeout: std::time::Duration,
) -> Result<std::process::ExitStatus> {
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "圧縮処理の終了が{}秒以内に完了しませんでした",
                    timeout.as_secs()
                );
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("圧縮プロセスの状態を取得できません");
            }
        }
    }
}
pub fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    #[cfg(not(windows))]
    let _ = cmd;
}
pub fn check_encoder(format: OutputFormat, ffmpeg: &Path) -> Result<()> {
    if matches!(format, OutputFormat::Wav) {
        return Ok(());
    }
    let mut cmd = Command::new(ffmpeg);
    hide_console(&mut cmd);
    let output = cmd
        .args(["-hide_banner", "-encoders"])
        .output()
        .context("FFmpeg が見つかりません。WAV を選ぶか実行ファイルを設定してください")?;
    if !output.status.success() {
        bail!("FFmpeg の動作確認に失敗しました");
    }
    let codec = match format {
        OutputFormat::Flac => "flac",
        OutputFormat::Opus => "libopus",
        OutputFormat::M4a => "aac",
        OutputFormat::Wav => unreachable!(),
    };
    ensure!(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some(codec)),
        "FFmpeg に {codec} エンコーダーがありません"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoder_pipe_test_process() {
        // 親テストは必ず環境値付きの別プロセスでこの1件だけを実行する。
        match std::env::var("TALK_RECORDER_ENCODER_PIPE_TEST").as_deref() {
            Ok("block") => thread::sleep(Duration::from_secs(60)),
            Ok("read") => {
                let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
            }
            _ => {}
        }
    }

    fn pipe_test_process(mode: &str) -> Result<Child> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([
                "--exact",
                "storage::tests::encoder_pipe_test_process",
                "--nocapture",
            ])
            .env("TALK_RECORDER_ENCODER_PIPE_TEST", mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hide_console(&mut command);
        command
            .spawn()
            .context("エンコーダー試験用プロセスを開始できません")
    }

    #[test]
    fn blocked_encoder_stdin_times_out_and_reaps_process_and_thread() -> Result<()> {
        let child = pipe_test_process("block")?;
        let mut input = EncoderInput::new(child, Duration::from_millis(250))?;
        let started = Instant::now();
        // OS パイプ容量を十分に上回る音声を送り、実際の write_all を詰まらせる。
        let error = input
            .exchange(Some(vec![0; 8 * 1_024 * 1_024]))
            .unwrap_err();
        assert!(format!("{error:#}").contains("制限時間"));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(input.child.try_wait()?.is_some());
        assert!(input.writer.is_none());
        assert!(input.requests.is_none());
        Ok(())
    }

    #[test]
    fn encoder_input_transfers_multiple_blocks_and_closes_cleanly() -> Result<()> {
        let child = pipe_test_process("read")?;
        let mut input = EncoderInput::new(child, Duration::from_secs(5))?;
        for _ in 0..3 {
            input.exchange(Some(vec![42; 128 * 1_024]))?;
        }
        input.close_input()?;
        input.wait()?;
        assert!(input.writer.is_none());
        assert!(input.child.try_wait()?.is_some());
        Ok(())
    }

    #[test]
    fn spool_and_rf64_preserve_frame_count_for_different_channels() -> Result<()> {
        let d = tempfile::tempdir()?;
        let mut spool = Spool::create(&d.path().join("spool"), vec![1, 2])?;
        let mono: Vec<f32> = (0..480).map(|i| i as f32 / 480.).collect();
        let stereo: Vec<f32> = mono.iter().flat_map(|&s| [s, -s]).collect();
        spool.append(&[mono.clone(), stereo.clone()])?;
        let out = spool.read(100, 300)?;
        assert_eq!(out[0], mono[100..300]);
        assert_eq!(out[1], stereo[200..600]);
        let mut writer = SegmentWriter::new(
            d.path(),
            1,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        writer.write(&out)?;
        writer.finish(false)?;
        for (i, ch) in [1u64, 2].into_iter().enumerate() {
            let b = fs::read(d.path().join(format!("000001_track{}.wav", i + 1)))?;
            assert_eq!(&b[0..4], b"RF64");
            assert_eq!(u64::from_le_bytes(b[36..44].try_into()?), 200);
            assert_eq!(b.len() as u64, 80 + 200 * ch * 4);
        }
        assert!(d.path().join("000001.complete.json").exists());
        Ok(())
    }
    #[test]
    fn invalid_writer_input_is_rejected_before_any_track_is_written() -> Result<()> {
        let d = tempfile::tempdir()?;
        for channels in [vec![], vec![0], vec![9]] {
            assert!(
                SegmentWriter::new(
                    d.path(),
                    1,
                    channels,
                    OutputFormat::Wav,
                    Path::new("missing")
                )
                .is_err()
            );
        }
        let mut writer = SegmentWriter::new(
            d.path(),
            1,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        assert!(writer.write(&[vec![0.1; 20], vec![0.1; 39]]).is_err());
        assert_eq!(writer.frames, 0);
        for sink in &writer.sinks {
            if let Sink::Wav { frames, .. } = sink {
                assert_eq!(*frames, 0);
            }
        }
        writer.finish(true)?;
        assert!(!d.path().join("000001.complete.json").exists());
        Ok(())
    }
    #[test]
    fn encoder_timeout_reaps_child_and_never_waits_indefinitely() -> Result<()> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--list")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        hide_console(&mut command);
        let mut child = command.spawn()?;
        let started = std::time::Instant::now();
        let _ = wait_for_encoder(&mut child, std::time::Duration::ZERO);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(child.try_wait()?.is_some());
        Ok(())
    }
}
