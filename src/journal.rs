//! 公開分割とは独立した可逆音声の確定。watermark は確定記録の後だけ進む。
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    config::OutputFormat,
    storage::{SegmentWriter, hide_console},
};

const RATE: u64 = 48_000;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalState {
    pub version: u32,
    pub number: u64,
    pub channels: Vec<u16>,
    pub format: OutputFormat,
    pub internal_format: OutputFormat,
    pub parts: u64,
    pub frames: u64,
    pub source_until: u64,
    pub published: bool,
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrackDigest {
    bytes: u64,
    fnv64: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Part {
    frames: u64,
    tracks: Vec<TrackDigest>,
}

#[derive(Serialize, Deserialize)]
struct FinalizationIntent {
    discard: bool,
}

pub struct JournalWriter {
    session: PathBuf,
    dir: PathBuf,
    ffmpeg: PathBuf,
    state: JournalState,
    writer: Option<SegmentWriter>,
    pending_frames: u64,
    failed: bool,
    prepared: Option<bool>,
}

impl JournalWriter {
    pub fn new(
        folder: &Path,
        number: u64,
        channels: Vec<u16>,
        format: OutputFormat,
        ffmpeg: &Path,
    ) -> Result<Self> {
        ensure!(
            number > 0 && !channels.is_empty() && channels.iter().all(|&c| (1..=8).contains(&c)),
            "内部保存の形式が不正です"
        );
        fs::create_dir_all(folder.join("journal"))?;
        let dir = journal_dir(folder, number);
        fs::create_dir(&dir).context("同じ番号の内部保存が既にあります")?;
        let internal_format = if format == OutputFormat::Wav {
            OutputFormat::Wav
        } else {
            OutputFormat::Flac
        };
        let state = JournalState {
            version: 1,
            number,
            channels,
            format,
            internal_format,
            parts: 0,
            frames: 0,
            source_until: 0,
            published: false,
            generation: 0,
        };
        Ok(Self {
            session: folder.into(),
            dir,
            ffmpeg: ffmpeg.into(),
            state,
            writer: None,
            pending_frames: 0,
            failed: false,
            prepared: None,
        })
    }
    pub fn write(&mut self, tracks: &[Vec<f32>]) -> Result<()> {
        ensure!(!self.failed, "内部保存は確定失敗後のため再利用できません");
        ensure!(
            self.prepared.is_none(),
            "公開準備済みの内部保存へ追記できません"
        );
        ensure!(
            tracks.len() == self.state.channels.len(),
            "内部保存のトラック数が不正です"
        );
        let frames = tracks
            .first()
            .context("内部保存する音声がありません")?
            .len()
            / usize::from(self.state.channels[0]);
        ensure!(
            frames > 0 && frames <= RATE as usize * 10,
            "内部保存への1回の入力は10秒以内にしてください"
        );
        for (samples, &channels) in tracks.iter().zip(&self.state.channels) {
            ensure!(
                samples.len() == frames * usize::from(channels)
                    && samples.iter().all(|v| v.is_finite()),
                "内部保存の尺または音声が不正です"
            );
        }
        if self.writer.is_none() {
            self.writer = Some(SegmentWriter::new(
                &self.dir,
                self.state.parts + 1,
                self.state.channels.clone(),
                self.state.internal_format,
                &self.ffmpeg,
            )?);
        }
        self.writer.as_mut().unwrap().write(tracks)?;
        self.pending_frames = self
            .pending_frames
            .checked_add(frames as u64)
            .context("内部保存の長さが上限を超えました")?;
        Ok(())
    }
    pub fn checkpoint(&mut self, source_until: u64) -> Result<()> {
        ensure!(!self.failed, "内部保存は確定失敗後のため再利用できません");
        ensure!(
            self.prepared.is_none(),
            "公開準備済みの内部保存は変更できません"
        );
        let result = self.checkpoint_inner(source_until);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn checkpoint_inner(&mut self, source_until: u64) -> Result<()> {
        ensure!(
            source_until >= self.state.source_until,
            "内部保存の元時刻が逆行しています"
        );
        let mut next = self.state.clone();
        if let Some(writer) = self.writer.take() {
            writer.finish(false)?;
            let number = next.parts + 1;
            let tracks = (0..next.channels.len())
                .map(|i| {
                    let path = part_path(&self.dir, number, i, next.internal_format);
                    OpenOptions::new().write(true).open(&path)?.sync_all()?;
                    digest(&path)
                })
                .collect::<Result<_>>()?;
            let part = Part {
                frames: self.pending_frames,
                tracks,
            };
            write_atomic_new(&self.dir.join(format!("{number:06}.part.json")), &part)?;
            next.parts = number;
            next.frames = next
                .frames
                .checked_add(self.pending_frames)
                .context("内部保存の長さが上限を超えました")?;
        }
        ensure!(
            next.frames <= source_until,
            "内部音声の尺が元の録音位置を超えています"
        );
        next.source_until = source_until;
        persist_state(&self.dir, &mut next)?;
        self.state = next;
        self.pending_frames = 0;
        Ok(())
    }
    pub fn durable_source(&self) -> u64 {
        self.state.source_until
    }
    pub fn prepare_finish(&mut self, source_until: u64, discard: bool) -> Result<()> {
        ensure!(!self.failed, "内部保存は確定失敗後のため再利用できません");
        if let Some(prepared) = self.prepared {
            ensure!(
                prepared == discard && source_until == self.state.source_until,
                "公開準備後の条件は変更できません"
            );
            return Ok(());
        }
        self.checkpoint(source_until)?;
        let intent = self.dir.join("intent.json");
        let result = if intent.exists() {
            (|| -> Result<()> {
                ensure!(
                    serde_json::from_slice::<FinalizationIntent>(&fs::read(&intent)?)?.discard
                        == discard,
                    "公開準備の条件が一致しません"
                );
                Ok(())
            })()
        } else {
            write_atomic_new(&intent, &FinalizationIntent { discard })
        };
        if result.is_err() {
            self.failed = true;
        }
        result?;
        self.prepared = Some(discard);
        Ok(())
    }
    pub(crate) fn number(&self) -> u64 {
        self.state.number
    }
    pub fn finish(self, source_until: u64, discard: bool) -> Result<()> {
        self.finish_with_progress(source_until, discard, |_, _| {})
            .map(|_| ())
    }

    pub(crate) fn finish_with_progress(
        mut self,
        source_until: u64,
        discard: bool,
        progress: impl FnMut(u64, u64),
    ) -> Result<bool> {
        self.prepare_finish(source_until, discard)?;
        finalize_journal_with_progress(
            &self.session,
            self.state.number,
            &self.ffmpeg,
            discard,
            progress,
        )?;
        Ok(!discard && self.state.frames > 0)
    }
}

fn journal_dir(session: &Path, number: u64) -> PathBuf {
    session.join("journal").join(format!("{number:06}"))
}
fn part_path(dir: &Path, number: u64, track: usize, format: OutputFormat) -> PathBuf {
    dir.join(format!(
        "{number:06}_track{}.{}",
        track + 1,
        format.extension()
    ))
}

fn digest(path: &Path) -> Result<TrackDigest> {
    let mut file = File::open(path)?;
    let bytes = file.metadata()?.len();
    let mut hash = 0xcbf29ce484222325u64;
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        for &b in &buffer[..n] {
            hash = (hash ^ u64::from(b)).wrapping_mul(0x100000001b3);
        }
    }
    Ok(TrackDigest { bytes, fnv64: hash })
}

fn write_atomic_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let temp = path.with_extension("json.partial");
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    ensure!(!path.exists(), "確定済みの内部記録を上書きできません");
    fs::rename(temp, path)?;
    Ok(())
}

fn persist_state(dir: &Path, state: &mut JournalState) -> Result<()> {
    state.generation = state
        .generation
        .checked_add(1)
        .context("内部記録の世代が上限を超えました")?;
    let target = dir.join(format!("state-{:020}.json", state.generation));
    write_atomic_new(&target, state)?;
    // 世代は最大2個。part数は音声の量に応じて増えるが、全partの一覧をRAMへ保持しない。
    if let Some(obsolete) = state
        .generation
        .checked_sub(2)
        .filter(|generation| *generation > 0)
    {
        let _ = fs::remove_file(dir.join(format!("state-{obsolete:020}.json")));
    }
    Ok(())
}

fn state_generation(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("state-")?.strip_suffix(".json")?;
    (digits.len() == 20 && digits.bytes().all(|c| c.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

pub fn load_state(session: &Path, number: u64) -> Result<Option<JournalState>> {
    let dir = journal_dir(session, number);
    if !dir.exists() {
        return Ok(None);
    }
    let latest = fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| state_generation(&e.file_name().to_string_lossy()).map(|g| (g, e.path())))
        .max_by_key(|(g, _)| *g);
    let Some((generation, path)) = latest else {
        return Ok(None);
    };
    let state: JournalState =
        serde_json::from_slice(&fs::read(&path)?).context("内部確定記録が破損しています")?;
    ensure!(
        state.version == 1 && state.number == number && state.generation == generation,
        "内部確定記録の識別情報が一致しません"
    );
    ensure!(
        !state.channels.is_empty() && state.channels.iter().all(|&c| (1..=8).contains(&c)),
        "内部確定記録のチャンネル数が不正です"
    );
    ensure!(
        matches!(
            state.internal_format,
            OutputFormat::Wav | OutputFormat::Flac
        ),
        "内部音声に非可逆形式は使用できません"
    );
    ensure!(
        state.frames <= state.source_until
            && state.parts <= state.frames
            && (state.parts == 0) == (state.frames == 0),
        "内部音声の区切り数または元位置が不正です"
    );
    Ok(Some(state))
}

fn part_metadata(dir: &Path, index: u64, state: &JournalState) -> Result<Part> {
    let part: Part = serde_json::from_slice(&fs::read(dir.join(format!("{index:06}.part.json")))?)?;
    ensure!(
        part.frames > 0 && part.tracks.len() == state.channels.len(),
        "内部音声の区切り情報が不正です"
    );
    for (i, expected) in part.tracks.iter().enumerate() {
        let actual = digest(&part_path(dir, index, i, state.internal_format))?;
        ensure!(
            actual.bytes == expected.bytes && actual.fnv64 == expected.fnv64,
            "内部音声が破損しています: 区切り{index} トラック{}",
            i + 1
        );
    }
    Ok(part)
}

pub fn finalize_journal(session: &Path, number: u64, ffmpeg: &Path, discard: bool) -> Result<()> {
    finalize_journal_with_progress(session, number, ffmpeg, discard, |_, _| {})
}

fn finalize_journal_with_progress(
    session: &Path,
    number: u64,
    ffmpeg: &Path,
    discard: bool,
    mut progress: impl FnMut(u64, u64),
) -> Result<()> {
    let mut state = load_state(session, number)?.context("内部確定記録がありません")?;
    let dir = journal_dir(session, number);
    if state.published {
        cleanup_parts(&dir, &state)?;
        return Ok(());
    }
    let _lock = exclusive_open(&dir.join("finalize.lock"))?;
    // ロック待ちの間に別処理が完了した可能性を再確認。
    state = load_state(session, number)?.context("内部確定記録がありません")?;
    if state.published {
        cleanup_parts(&dir, &state)?;
        return Ok(());
    }
    // 除外の意図も音声削除より先に確定する。再起動時に非発話を復活させない。
    let intent_path = dir.join("intent.json");
    let discard = if intent_path.exists() {
        serde_json::from_slice::<FinalizationIntent>(&fs::read(&intent_path)?)?.discard
    } else {
        let abandoned = intent_path.with_extension("json.partial");
        if abandoned.exists() {
            fs::remove_file(abandoned)?;
        }
        write_atomic_new(&intent_path, &FinalizationIntent { discard })?;
        discard
    };
    let manifest = session.join(format!("{number:06}.complete.json"));
    if manifest.exists() {
        ensure!(
            dir.join("finalizing.json").exists(),
            "別の確定済みファイルがあります"
        );
        validate_public(session, &state)?;
    } else if !discard && state.frames > 0 {
        prepare_public(session, &dir, &state)?;
        let mut writer = SegmentWriter::new(
            session,
            number,
            state.channels.clone(),
            state.format,
            ffmpeg,
        )?;
        let mut total = 0u64;
        progress(0, state.parts);
        for index in 1..=state.parts {
            let part = part_metadata(&dir, index, &state)?;
            let mut decoders = state
                .channels
                .iter()
                .enumerate()
                .map(|(i, &channels)| {
                    Decoder::open(
                        &part_path(&dir, index, i, state.internal_format),
                        state.internal_format,
                        channels,
                        part.frames,
                        ffmpeg,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            let mut at = 0;
            while at < part.frames {
                let count = (part.frames - at).min(RATE) as usize;
                let samples = decoders
                    .iter_mut()
                    .map(|d| d.read(count))
                    .collect::<Result<Vec<_>>>()?;
                writer.write(&samples)?;
                at += count as u64;
            }
            for decoder in &mut decoders {
                decoder.finish()?;
            }
            total = total
                .checked_add(part.frames)
                .context("内部音声の長さが上限を超えました")?;
            progress(index, state.parts);
        }
        ensure!(total == state.frames, "内部音声の合計尺が一致しません");
        writer.finish(false)?;
        validate_public(session, &state)?;
    }
    state.published = true;
    let next_generation = state
        .generation
        .checked_add(1)
        .context("内部記録の世代が上限を超えました")?;
    let abandoned = dir.join(format!("state-{next_generation:020}.json.partial"));
    if abandoned.exists() {
        fs::remove_file(abandoned)?;
    }
    persist_state(&dir, &mut state)?;
    cleanup_parts(&dir, &state)?;
    Ok(())
}

fn exclusive_open(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    options
        .open(path)
        .context("内部保存の確定は別の処理が使用中です")
}

fn validate_public(session: &Path, state: &JournalState) -> Result<()> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(
        session.join(format!("{:06}.complete.json", state.number)),
    )?)?;
    ensure!(
        value["number"] == state.number
            && value["frames"] == state.frames
            && value["sample_rate"] == RATE
            && value["channels"] == serde_json::to_value(&state.channels)?,
        "公開ファイルの確定記録が内部音声と一致しません"
    );
    let declared_format: Option<OutputFormat> = match value.get("format") {
        Some(value) => serde_json::from_value(value.clone())?,
        None => None,
    };
    let format = if state.published {
        // Published WAV files can later be compacted atomically to M4A. The
        // committed public manifest then owns their format, while the old
        // journal still identifies the original publication transaction.
        declared_format.unwrap_or(state.format)
    } else {
        ensure!(
            declared_format.is_none_or(|format| format == state.format),
            "公開ファイルの保存形式が内部音声と一致しません"
        );
        state.format
    };
    for i in 0..state.channels.len() {
        ensure!(
            part_path(session, state.number, i, format).is_file(),
            "公開音声ファイルが不足しています"
        );
    }
    Ok(())
}

fn prepare_public(session: &Path, dir: &Path, state: &JournalState) -> Result<()> {
    let marker = dir.join("finalizing.json");
    if !marker.exists() {
        for i in 0..state.channels.len() {
            let path = part_path(session, state.number, i, state.format);
            ensure!(
                !path.exists()
                    && !path
                        .with_extension(format!("{}.partial", state.format.extension()))
                        .exists(),
                "公開先に別のファイルがあります"
            );
        }
        let abandoned = marker.with_extension("json.partial");
        if abandoned.exists() {
            fs::remove_file(abandoned)?;
        }
        write_atomic_new(&marker, state)?;
    } else {
        // 自分の確定処理の途中だけを再実行する。公開済み記録がある場合はここへ来ない。
        for i in 0..state.channels.len() {
            let path = part_path(session, state.number, i, state.format);
            for owned in [
                path.clone(),
                path.with_extension(format!("{}.partial", state.format.extension())),
            ] {
                if owned.exists() {
                    fs::remove_file(owned)?;
                }
            }
        }
        let partial = session.join(format!("{:06}.complete.json.partial", state.number));
        if partial.exists() {
            fs::remove_file(partial)?;
        }
    }
    Ok(())
}

fn cleanup_parts(dir: &Path, state: &JournalState) -> Result<()> {
    for index in 1..=state.parts {
        for i in 0..state.channels.len() {
            let path = part_path(dir, index, i, state.internal_format);
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        for suffix in ["complete.json", "part.json"] {
            let path = dir.join(format!("{index:06}.{suffix}"));
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        for i in 0..state.channels.len() {
            let path = dir.join(format!("{index:06}_track{}.encoder.log", i + 1));
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct JournalRecovery {
    pub source_until: u64,
    pub recovered: usize,
}

pub fn recover_all(session: &Path, ffmpeg: &Path) -> Result<JournalRecovery> {
    let mut result = JournalRecovery::default();
    let root = session.join("journal");
    if !root.exists() {
        return Ok(result);
    }
    let mut numbers = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Ok(number) = entry.file_name().to_string_lossy().parse::<u64>()
        {
            numbers.push(number);
        }
    }
    numbers.sort_unstable();
    for number in numbers {
        if let Some(state) = load_state(session, number)? {
            result.source_until = result.source_until.max(state.source_until);
            finalize_journal(session, number, ffmpeg, false)?;
            if !state.published {
                result.recovered += 1;
            }
        }
    }
    Ok(result)
}

enum Decoder {
    Wav {
        file: File,
        channels: u16,
        remaining: u64,
    },
    Flac {
        child: Child,
        incoming: Option<Receiver<Result<Vec<f32>>>>,
        reader: Option<JoinHandle<()>>,
        channels: u16,
    },
}

impl Decoder {
    fn open(
        path: &Path,
        format: OutputFormat,
        channels: u16,
        frames: u64,
        ffmpeg: &Path,
    ) -> Result<Self> {
        if format == OutputFormat::Wav {
            let mut file = File::open(path)?;
            let mut header = [0u8; 80];
            file.read_exact(&mut header)?;
            ensure!(
                &header[..4] == b"RF64"
                    && &header[48..52] == b"fmt "
                    && u16::from_le_bytes(header[56..58].try_into()?) == 3
                    && u16::from_le_bytes(header[58..60].try_into()?) == channels
                    && u32::from_le_bytes(header[60..64].try_into()?) == RATE as u32
                    && u64::from_le_bytes(header[36..44].try_into()?) == frames,
                "内部WAVの形式が一致しません"
            );
            ensure!(
                Some(file.metadata()?.len())
                    == frames
                        .checked_mul(u64::from(channels))
                        .and_then(|n| n.checked_mul(4))
                        .and_then(|n| n.checked_add(80)),
                "内部WAVの長さが一致しません"
            );
            file.seek(SeekFrom::Start(80))?;
            return Ok(Self::Wav {
                file,
                channels,
                remaining: frames,
            });
        }
        ensure!(format == OutputFormat::Flac, "内部復号形式が不正です");
        let mut cmd = Command::new(ffmpeg);
        hide_console(&mut cmd);
        let mut child = cmd
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-xerror",
                "-i",
            ])
            .arg(path)
            .args([
                "-f",
                "f32le",
                "-ar",
                "48000",
                "-ac",
                &channels.to_string(),
                "pipe:1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdout = child.stdout.take().context("内部復号の出力がありません")?;
        let (sender, incoming) = mpsc::sync_channel(2);
        let reader = thread::spawn(move || {
            let result = (|| -> Result<()> {
                let mut remaining = frames;
                while remaining > 0 {
                    let count = remaining.min(RATE);
                    let mut bytes = vec![0; count as usize * usize::from(channels) * 4];
                    stdout.read_exact(&mut bytes)?;
                    let samples = bytes
                        .chunks_exact(4)
                        .map(|v| f32::from_le_bytes(v.try_into().unwrap()))
                        .collect();
                    if sender.send(Ok(samples)).is_err() {
                        return Ok(());
                    }
                    remaining -= count;
                }
                let mut extra = [0];
                ensure!(
                    stdout.read(&mut extra)? == 0,
                    "内部復号の出力が記録した尺を超えています"
                );
                Ok(())
            })();
            let _ = sender.send(result.map(|()| Vec::new()));
        });
        Ok(Self::Flac {
            child,
            incoming: Some(incoming),
            reader: Some(reader),
            channels,
        })
    }
    fn read(&mut self, frames: usize) -> Result<Vec<f32>> {
        let (samples, channels) = match self {
            Self::Wav {
                file,
                channels,
                remaining,
            } => {
                ensure!(*remaining >= frames as u64, "内部音声の末尾を超えています");
                let mut bytes = vec![0; frames * usize::from(*channels) * 4];
                file.read_exact(&mut bytes)?;
                *remaining -= frames as u64;
                (
                    bytes
                        .chunks_exact(4)
                        .map(|v| f32::from_le_bytes(v.try_into().unwrap()))
                        .collect::<Vec<_>>(),
                    *channels,
                )
            }
            Self::Flac {
                incoming, channels, ..
            } => (
                incoming
                    .as_ref()
                    .context("復号は終了しています")?
                    .recv_timeout(TIMEOUT)
                    .context("内部復号が時間切れになりました")??,
                *channels,
            ),
        };
        ensure!(
            samples.len() == frames * usize::from(channels)
                && samples.iter().all(|v| v.is_finite()),
            "内部復号の尺または値が不正です"
        );
        Ok(samples)
    }
    fn finish(&mut self) -> Result<()> {
        match self {
            Self::Wav { remaining, .. } => {
                ensure!(*remaining == 0, "内部WAVを最後まで読み取れていません")
            }
            Self::Flac {
                child,
                incoming,
                reader,
                ..
            } => {
                ensure!(
                    incoming
                        .as_ref()
                        .context("復号は終了しています")?
                        .recv_timeout(TIMEOUT)
                        .context("内部復号の終了が時間切れになりました")??
                        .is_empty(),
                    "内部復号の末尾が不正です"
                );
                let deadline = Instant::now() + TIMEOUT;
                loop {
                    if let Some(status) = child.try_wait()? {
                        ensure!(status.success(), "内部FLACの復号に失敗しました");
                        break;
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "内部復号の終了が時間切れになりました"
                    );
                    thread::sleep(Duration::from_millis(10));
                }
                incoming.take();
                if let Some(reader) = reader.take() {
                    reader
                        .join()
                        .map_err(|_| anyhow::anyhow!("内部復号スレッドが異常終了しました"))?;
                }
            }
        }
        Ok(())
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        if let Self::Flac {
            child,
            incoming,
            reader,
            ..
        } = self
        {
            incoming.take();
            let _ = child.kill();
            let _ = child.wait();
            if let Some(reader) = reader.take() {
                let _ = reader.join();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn audio(frames: usize) -> Vec<Vec<f32>> {
        vec![
            (0..frames).map(|i| i as f32 / frames as f32).collect(),
            (0..frames)
                .flat_map(|i| [i as f32 / frames as f32, -(i as f32) / frames as f32])
                .collect(),
        ]
    }
    #[test]
    fn checkpoints_advance_without_public_split_and_merge_exact_frames() -> Result<()> {
        let d = tempfile::tempdir()?;
        let mut journal = JournalWriter::new(
            d.path(),
            1,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        for second in 0..40 {
            journal.write(&audio(480))?;
            assert_eq!(journal.durable_source(), second * 480);
            journal.checkpoint((second + 1) * 480)?;
        }
        assert!(!d.path().join("000001.complete.json").exists());
        assert_eq!(
            fs::read_dir(journal_dir(d.path(), 1))?
                .filter_map(|e| e.ok())
                .filter(|e| state_generation(&e.file_name().to_string_lossy()).is_some())
                .count(),
            2
        );
        journal.finish(40 * 480, false)?;
        for (i, ch) in [1, 2].into_iter().enumerate() {
            let bytes = fs::read(part_path(d.path(), 1, i, OutputFormat::Wav))?;
            assert_eq!(bytes.len(), 80 + 40 * 480 * ch * 4);
            assert_eq!(u64::from_le_bytes(bytes[36..44].try_into()?), 40 * 480);
            let expected: Vec<u8> = (0..40)
                .flat_map(|_| audio(480)[i].clone())
                .flat_map(f32::to_le_bytes)
                .collect();
            assert_eq!(&bytes[80..], expected);
        }
        assert!(load_state(d.path(), 1)?.unwrap().published);
        assert!(recover_all(d.path(), Path::new("missing"))?.recovered == 0);
        Ok(())
    }
    #[test]
    fn failed_checkpoint_does_not_advance_watermark() -> Result<()> {
        let d = tempfile::tempdir()?;
        let mut journal = JournalWriter::new(
            d.path(),
            1,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        journal.write(&audio(10))?;
        journal.checkpoint(100)?;
        let collision = journal.dir.join(format!(
            "state-{:020}.json.partial",
            journal.state.generation + 1
        ));
        fs::write(collision, b"collision")?;
        assert!(journal.checkpoint(200).is_err());
        assert!(journal.checkpoint(200).is_err());
        assert!(journal.write(&audio(10)).is_err());
        assert_eq!(journal.durable_source(), 100);
        assert_eq!(load_state(d.path(), 1)?.unwrap().source_until, 100);
        Ok(())
    }
    #[test]
    fn corrupted_part_is_not_published_or_removed() -> Result<()> {
        let d = tempfile::tempdir()?;
        let mut journal = JournalWriter::new(
            d.path(),
            1,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        journal.write(&audio(10))?;
        journal.checkpoint(10)?;
        let path = part_path(&journal.dir, 1, 0, OutputFormat::Wav);
        let mut file = OpenOptions::new().write(true).open(&path)?;
        file.seek(SeekFrom::Start(82))?;
        file.write_all(&[255])?;
        drop(file);
        assert!(journal.finish(10, false).is_err());
        assert!(path.exists());
        assert!(!d.path().join("000001.complete.json").exists());
        Ok(())
    }
    #[test]
    fn source_watermark_supports_more_than_four_gigabytes_without_large_allocation() -> Result<()> {
        let d = tempfile::tempdir()?;
        let mut journal = JournalWriter::new(
            d.path(),
            1,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        let source = (4u64 * 1024 * 1024 * 1024) / (6 * 4) + RATE;
        journal.checkpoint(source)?;
        assert_eq!(journal.durable_source(), source);
        journal.finish(source, true)?;
        Ok(())
    }
    #[test]
    fn unfinished_journal_recovers_and_is_idempotent() -> Result<()> {
        let d = tempfile::tempdir()?;
        let mut journal = JournalWriter::new(
            d.path(),
            7,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        journal.write(&audio(23))?;
        journal.checkpoint(30)?;
        drop(journal);
        let recovered = recover_all(d.path(), Path::new("missing"))?;
        assert_eq!(recovered.source_until, 30);
        assert_eq!(recovered.recovered, 1);
        assert_eq!(recover_all(d.path(), Path::new("missing"))?.recovered, 0);
        Ok(())
    }

    #[test]
    fn damaged_newest_state_never_falls_back_to_released_source() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut journal = JournalWriter::new(
            directory.path(),
            1,
            vec![1],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        journal.checkpoint(100)?;
        fs::write(
            journal
                .dir
                .join(format!("state-{:020}.json", journal.state.generation + 1)),
            b"{broken",
        )?;
        assert!(load_state(directory.path(), 1).is_err());
        assert!(finalize_journal(directory.path(), 1, Path::new("missing"), false).is_err());
        Ok(())
    }

    #[test]
    fn retry_replaces_only_its_owned_incomplete_public_files() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut journal = JournalWriter::new(
            directory.path(),
            2,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        journal.write(&audio(100))?;
        journal.checkpoint(100)?;
        prepare_public(directory.path(), &journal.dir, &journal.state)?;
        fs::write(
            part_path(directory.path(), 2, 0, OutputFormat::Wav),
            b"unfinished",
        )?;
        drop(journal);
        finalize_journal(directory.path(), 2, Path::new("missing"), false)?;
        let bytes = fs::read(part_path(directory.path(), 2, 0, OutputFormat::Wav))?;
        assert_eq!(bytes.len(), 80 + 100 * 4);
        assert!(load_state(directory.path(), 2)?.unwrap().published);
        Ok(())
    }

    #[test]
    fn existing_unowned_output_is_preserved() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut journal = JournalWriter::new(
            directory.path(),
            1,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        journal.write(&audio(20))?;
        journal.checkpoint(20)?;
        let existing = part_path(directory.path(), 1, 0, OutputFormat::Wav);
        fs::write(&existing, b"existing recording")?;
        assert!(journal.finish(20, false).is_err());
        assert_eq!(fs::read(existing)?, b"existing recording");
        assert!(!load_state(directory.path(), 1)?.unwrap().published);
        Ok(())
    }

    #[test]
    fn recovery_keeps_committed_discard_intent() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut journal = JournalWriter::new(
            directory.path(),
            1,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        journal.write(&audio(20))?;
        journal.prepare_finish(600 * RATE, true)?;
        journal.prepare_finish(600 * RATE, true)?;
        assert!(journal.write(&audio(1)).is_err());
        drop(journal);
        assert_eq!(
            recover_all(directory.path(), Path::new("missing"))?.recovered,
            1
        );
        assert!(!directory.path().join("000001.complete.json").exists());
        assert!(load_state(directory.path(), 1)?.unwrap().published);
        Ok(())
    }

    #[test]
    #[ignore = "固定 FFmpeg が必要。内部 FLAC から Opus/FLAC/M4A への結合を検証"]
    fn lossless_parts_merge_to_requested_codec_with_exact_frames() -> Result<()> {
        let ffmpeg = std::env::var_os("TALKRECORDER_FFMPEG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("runtime/ffmpeg/bin/ffmpeg.exe"));
        let ffmpeg = ffmpeg.as_path();
        for format in [OutputFormat::Flac, OutputFormat::Opus, OutputFormat::M4a] {
            let directory = tempfile::tempdir()?;
            let mut journal = JournalWriter::new(directory.path(), 1, vec![1, 2], format, ffmpeg)?;
            journal.write(&audio(48_000))?;
            journal.checkpoint(48_000)?;
            journal.write(&audio(11_259))?;
            journal.checkpoint(59_259)?;
            let state = load_state(directory.path(), 1)?.unwrap();
            assert_eq!(state.internal_format, OutputFormat::Flac);
            let mut progress = Vec::new();
            assert!(
                journal.finish_with_progress(59_259, false, |done, total| progress
                    .push((done, total)))?
            );
            assert_eq!(progress, [(0, 2), (1, 2), (2, 2)]);
            if format == OutputFormat::M4a {
                let mut published = load_state(directory.path(), 1)?.unwrap();
                assert!(published.published);
                // A later WAV compaction changes the public manifest, not the
                // immutable original journal's publication format.
                published.format = OutputFormat::Wav;
                validate_public(directory.path(), &published)?;
                published.published = false;
                assert!(validate_public(directory.path(), &published).is_err());
            }
            for (track, channels) in [1usize, 2].into_iter().enumerate() {
                let mut command = Command::new(ffmpeg);
                hide_console(&mut command);
                let decoded = command
                    .args(["-hide_banner", "-loglevel", "error", "-i"])
                    .arg(part_path(directory.path(), 1, track, format))
                    .args(["-f", "f32le", "pipe:1"])
                    .output()?;
                assert!(decoded.status.success());
                assert_eq!(decoded.stdout.len(), 59_259 * channels * 4);
            }
        }
        Ok(())
    }
}
