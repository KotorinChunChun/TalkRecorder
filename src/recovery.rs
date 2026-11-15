//! 排他保護されたセッションの、確定済み音声と未確定原音の復旧。
//! 元の raw/partial は削除せず、復旧済み範囲は出力一式の確定記録で識別する。
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::OutputFormat,
    journal,
    storage::{SegmentWriter, Spool},
};

const RATE: u64 = 48_000;
const MAX_JSON_BYTES: u64 = 2 * 1024 * 1024;

/// 開いている間だけ有効な OS の排他。ファイルの存在を稼働中の根拠にしない。
pub struct SessionLock {
    file: File,
}

impl SessionLock {
    pub fn acquire(session: &Path) -> Result<Self> {
        ensure_plain_dir(session)?;
        let path = session.join("session.lock");
        match fs::symlink_metadata(&path) {
            Ok(_) => ensure_plain_file(&path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0);
        }
        let file = options.open(&path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe extern "C" {
                fn flock(fd: i32, operation: i32) -> i32;
            }
            // LOCK_EX | LOCK_NB。別の open file description とは同一プロセスでも排他。
            if unsafe { flock(file.as_raw_fd(), 2 | 4) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(any(windows, unix)))]
        bail!("この OS ではセッションの排他ロックに対応していません");
        Ok(Self { file })
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        // Windows の共有モードも Unix の flock もハンドル終了で解除される。
        let _ = &self.file;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub recovered: usize,
    pub skipped_active: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceRange {
    version: u32,
    source_start: u64,
    source_end: u64,
    frames: u64,
    channels: Vec<u16>,
    state: String,
}

#[derive(Deserialize)]
struct Complete {
    number: u64,
    frames: u64,
    sample_rate: u32,
    channels: Vec<u16>,
    #[serde(default)]
    format: Option<OutputFormat>,
}

pub fn recover_pending(output_root: &Path, ffmpeg: &Path) -> Result<RecoveryReport> {
    let mut report = RecoveryReport::default();
    if !output_root.exists() {
        return Ok(report);
    }
    ensure_plain_dir(output_root)?;
    // 前版は SessionLock を持たない。旧版形式についてだけ、同名の
    // 別プロセスが動作中なら保守的に復旧を保留する。
    let legacy_process_guard = other_recorder_processes();
    let mut sessions: Vec<_> = fs::read_dir(output_root)?.collect::<std::io::Result<Vec<_>>>()?;
    sessions.sort_by_key(|entry| entry.file_name());
    for entry in sessions {
        let path = entry.path();
        if !entry.file_type()?.is_dir() || !path.join("session.json").exists() {
            continue;
        }
        if let Err(error) = ensure_plain_dir(&path) {
            report.errors.push(format!("{}: {error:#}", path.display()));
            continue;
        }
        if !path.join("journal").exists() {
            match &legacy_process_guard {
                Ok(processes) if !processes.is_empty() => {
                    report.skipped_active += 1;
                    report.errors.push(format!("{}: 別の録音プロセス（PID {:?}）が動作中のため旧版形式の復旧を保留しました",path.display(),processes));
                    continue;
                }
                Err(error) => {
                    report.errors.push(format!(
                        "{}: 旧版の稼働状態を確認できず復旧を保留しました: {error:#}",
                        path.display()
                    ));
                    continue;
                }
                _ => {}
            }
        }
        let _lock = match SessionLock::acquire(&path) {
            Ok(lock) => lock,
            Err(error) if is_lock_busy(&error) => {
                report.skipped_active += 1;
                continue;
            }
            Err(error) => {
                report
                    .errors
                    .push(format!("{}: ロックできません: {error:#}", path.display()));
                continue;
            }
        };
        match recover_session(&path, ffmpeg) {
            Ok((changed, issues)) => {
                report.recovered += usize::from(changed);
                report.errors.extend(
                    issues
                        .into_iter()
                        .map(|issue| format!("{}: {issue}", path.display())),
                );
            }
            Err(error) => report.errors.push(format!("{}: {error:#}", path.display())),
        }
    }
    Ok(report)
}

fn is_other_recorder(pid: u32, parent: u32, name: &str, own_pid: u32, own_name: &str) -> bool {
    pid != own_pid
        && parent != own_pid
        && ["talk-recorder.exe", "talk-recorder", own_name]
            .iter()
            .any(|expected| name.eq_ignore_ascii_case(expected))
}

fn other_recorder_processes() -> Result<Vec<u32>> {
    let own = std::env::current_exe()?;
    let own_name = own
        .file_name()
        .context("実行ファイル名がありません")?
        .to_string_lossy();
    platform_recorder_processes(std::process::id(), &own_name)
}

#[cfg(windows)]
fn platform_recorder_processes(own_pid: u32, own_name: &str) -> Result<Vec<u32>> {
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };
    struct Snapshot(HANDLE);
    impl Drop for Snapshot {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
    let snapshot = Snapshot(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }?);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut found = Vec::new();
    match unsafe { Process32FirstW(snapshot.0, &mut entry) } {
        Ok(()) => {}
        Err(error) if error.code().0 as u32 == 0x80070012 => return Ok(found), // ERROR_NO_MORE_FILES
        Err(error) => return Err(error.into()),
    }
    loop {
        let end = entry
            .szExeFile
            .iter()
            .position(|&ch| ch == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
        if is_other_recorder(
            entry.th32ProcessID,
            entry.th32ParentProcessID,
            &name,
            own_pid,
            own_name,
        ) {
            found.push(entry.th32ProcessID);
        }
        match unsafe { Process32NextW(snapshot.0, &mut entry) } {
            Ok(()) => {}
            Err(error) if error.code().0 as u32 == 0x80070012 => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(found)
}

#[cfg(not(windows))]
fn platform_recorder_processes(own_pid: u32, own_name: &str) -> Result<Vec<u32>> {
    // Linux の非録音テスト用経路。/proc がない OS は不明を空配列にせず保留する。
    let proc = Path::new("/proc");
    ensure!(
        proc.is_dir(),
        "この OS では旧版録音プロセスの稼働確認に対応していません"
    );
    let mut found = Vec::new();
    for entry in fs::read_dir(proc)? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(exe) = fs::read_link(entry.path().join("exe")) else {
            continue;
        };
        let Some(name) = exe.file_name() else {
            continue;
        };
        let stat = fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
        let parent = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|parent| parent.parse::<u32>().ok())
            .unwrap_or(0);
        if is_other_recorder(pid, parent, &name.to_string_lossy(), own_pid, own_name) {
            found.push(pid);
        }
    }
    Ok(found)
}

pub(crate) fn is_lock_busy(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|error| {
            error.kind() == std::io::ErrorKind::WouldBlock
                || matches!(error.raw_os_error(), Some(32 | 33))
        })
}

fn recover_session(session: &Path, ffmpeg: &Path) -> Result<(bool, Vec<String>)> {
    let metadata: Value = read_json(&session.join("session.json"))?;
    let channels: Vec<u16> = metadata
        .get("devices")
        .and_then(Value::as_array)
        .context("取得デバイスの情報がありません")?
        .iter()
        .map(|device| {
            let value = device
                .get("channels")
                .and_then(Value::as_u64)
                .context("チャンネル数がありません")?;
            ensure!((1..=8).contains(&value), "チャンネル数が範囲外です");
            Ok(value as u16)
        })
        .collect::<Result<_>>()?;
    ensure!((1..=4).contains(&channels.len()), "トラック数が範囲外です");
    let format: OutputFormat = serde_json::from_value(
        metadata
            .pointer("/config/format")
            .cloned()
            .unwrap_or(Value::String("Wav".into())),
    )?;
    let mut source_until = split_watermark(session, &channels, format)?;
    let mut changed = false;
    let mut issues = Vec::new();
    let journals = session.join("journal");
    if journals.exists() {
        ensure_plain_dir(&journals)?;
        let mut numbers = BTreeSet::new();
        for entry in fs::read_dir(&journals)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                ensure_plain_dir(&entry.path())?;
                if let Some(number) = parse_number(&entry.file_name().to_string_lossy()) {
                    numbers.insert(number);
                }
            }
        }
        for number in numbers {
            let Some(state) = journal::load_state(session, number)
                .with_context(|| format!("内部区切り {number} の確定記録を検証できません"))?
            else {
                continue;
            };
            ensure!(
                state.channels == channels,
                "内部区切り {number} のチャンネル構成が異なります"
            );
            source_until = source_until.max(state.source_until);
            if !state.published {
                match journal::finalize_journal(session, number, ffmpeg, false) {
                    Ok(()) => changed = true,
                    Err(error) => issues.push(format!(
                        "内部区切り {number} の公開を復旧できません: {error:#}"
                    )),
                }
            }
        }
    }
    let recovered_root = session.join("recovered");
    if recovered_root.exists() {
        ensure_plain_dir(&recovered_root)?;
    }
    let covered = recovered_ranges(&recovered_root, &channels)?;
    let raw = session.join("raw");
    if raw.exists() {
        ensure_plain_dir(&raw)?;
        let (made, raw_issues) =
            recover_raw(&raw, &recovered_root, &channels, source_until, &covered)?;
        changed |= made;
        issues.extend(raw_issues);
    }
    if changed {
        write_atomic(
            &session.join("recovery.json"),
            &serde_json::json!({
                "version":1,"journal_source_until":source_until,"issues":issues,
                "recovered_at":chrono::Local::now().to_rfc3339(),"source_audio_deleted":false,
            }),
        )?;
    }
    Ok((changed, issues))
}

fn split_watermark(session: &Path, channels: &[u16], format: OutputFormat) -> Result<u64> {
    let path = session.join("timeline.jsonl");
    if !path.exists() {
        return Ok(0);
    }
    ensure_plain_file(&path)?;
    let mut reader = BufReader::new(File::open(path)?);
    let mut at = 0;
    loop {
        let mut line = Vec::new();
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                break;
            }
            let n = available
                .iter()
                .position(|&b| b == b'\n')
                .map_or(available.len(), |n| n + 1);
            ensure!(
                line.len() + n <= MAX_JSON_BYTES as usize,
                "時刻対応表の行が大きすぎます"
            );
            line.extend_from_slice(&available[..n]);
            reader.consume(n);
            if line.last() == Some(&b'\n') {
                break;
            }
        }
        if line.is_empty() {
            break;
        }
        let value: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(_) if reader.fill_buf()?.is_empty() => break,
            Err(error) => return Err(error).context("時刻対応表の途中が破損しています"),
        };
        if value.get("event").and_then(Value::as_str) != Some("edit") {
            continue;
        }
        let Some(split) = value.pointer("/action/Split") else {
            continue;
        };
        let next = split
            .get("at")
            .and_then(Value::as_u64)
            .context("分割時刻がありません")?;
        ensure!(next >= at, "時刻対応表の分割位置が逆行しています");
        let number = value
            .get("segment")
            .and_then(Value::as_u64)
            .context("分割番号がありません")?;
        let frames = value
            .get("segment_frames")
            .and_then(Value::as_u64)
            .context("分割フレーム数がありません")?;
        let discard = split
            .get("discard")
            .and_then(Value::as_bool)
            .context("分割の除外状態がありません")?;
        if !discard && frames > 0 {
            if session.join(format!("{number:06}.complete.json")).exists() {
                validate_complete(session, number, channels, frames, format)?;
            } else if let Some(state) = journal::load_state(session, number)? {
                ensure!(
                    state.channels == channels && state.source_until >= next,
                    "未公開の分割に対応する内部確定記録が不足しています"
                );
                // 公開は別スレッドで行う。この Split 自体では
                // watermark を進めず、後段の journal 検証・公開へ委ねる。
                continue;
            } else {
                bail!("分割 {number} の公開確定記録がありません");
            }
        }
        at = next;
    }
    Ok(at)
}

fn recovered_ranges(root: &Path, channels: &[u16]) -> Result<Vec<(u64, u64)>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut ranges = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        ensure_plain_dir(&entry.path())?;
        if !entry.path().join("000001.complete.json").exists() {
            continue;
        }
        let info: SourceRange = read_json(&entry.path().join("source.json"))?;
        ensure!(
            info.version == 1
                && info.channels == channels
                && info.source_end >= info.source_start
                && info.frames == info.source_end - info.source_start,
            "復旧済み範囲の記録が不正です"
        );
        validate_complete(&entry.path(), 1, channels, info.frames, OutputFormat::Wav)?;
        ranges.push((info.source_start, info.source_end));
    }
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        ensure!(pair[0].1 <= pair[1].0, "復旧済み範囲が重複しています");
    }
    Ok(ranges)
}

pub(crate) fn validate_complete(
    dir: &Path,
    number: u64,
    channels: &[u16],
    frames: u64,
    format: OutputFormat,
) -> Result<()> {
    let complete: Complete = read_json(&dir.join(format!("{number:06}.complete.json")))?;
    // 公開後の圧縮変換ではセッション開始時の設定より実ファイルの形式を優先する。
    // format のない旧版と raw 復旧結果には従来の指定を適用する。
    let format = complete.format.unwrap_or(format);
    ensure!(
        complete.number == number
            && complete.frames == frames
            && complete.sample_rate == RATE as u32
            && complete.channels == channels,
        "音声一式の確定記録が一致しません"
    );
    for (i, &ch) in channels.iter().enumerate() {
        let path = dir.join(format!("{number:06}_track{}.{}", i + 1, format.extension()));
        ensure_plain_file(&path)?;
        if matches!(format, OutputFormat::Wav) {
            let mut file = File::open(&path)?;
            let mut header = [0; 80];
            file.read_exact(&mut header)?;
            let expected_bytes = frames
                .checked_mul(u64::from(ch))
                .and_then(|value| value.checked_mul(4))
                .and_then(|value| value.checked_add(80))
                .context("WAV の期待サイズが上限を超えています")?;
            ensure!(
                &header[0..4] == b"RF64"
                    && u64::from_le_bytes(header[36..44].try_into()?) == frames
                    && fs::metadata(path)?.len() == expected_bytes,
                "復旧済み WAV の長さが一致しません"
            );
        } else {
            ensure!(fs::metadata(path)?.len() > 0, "確定音声ファイルが空です");
        }
    }
    Ok(())
}

struct RawGroup {
    dir: PathBuf,
    writer: SegmentWriter,
    start: u64,
    end: u64,
}

fn recover_raw(
    raw: &Path,
    output: &Path,
    channels: &[u16],
    watermark: u64,
    covered: &[(u64, u64)],
) -> Result<(bool, Vec<String>)> {
    let mut bases = BTreeSet::new();
    for entry in fs::read_dir(raw)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some((base, suffix)) = name.split_once('_')
            && base.len() == 20
            && base.bytes().all(|v| v.is_ascii_digit())
            && (suffix == "valid.json" || suffix.ends_with(".pcm"))
        {
            ensure_plain_file(&entry.path())?;
            bases.insert(base.parse::<u64>()?);
        }
    }
    let mut spool = Spool::open(raw, channels.to_vec())?;
    let mut group: Option<RawGroup> = None;
    let mut changed = false;
    let mut issues = Vec::new();
    let mut expected = watermark;
    for base in bases {
        ensure!(base.is_multiple_of(RATE), "原音の区切り位置が不正です");
        let info = raw_block_frames(raw, base, channels);
        let (frames, warning) = match info {
            Ok(info) => info,
            Err(error) => {
                finish_group(&mut group, channels)?;
                issues.push(format!("原音 {base} の復旧を保留: {error:#}"));
                continue;
            }
        };
        let end = base
            .checked_add(frames)
            .context("原音の位置が上限を超えました")?;
        if end <= watermark {
            continue;
        }
        if base > expected {
            issues.push(format!("原音の欠落範囲: {expected}..{base} サンプル"));
        }
        expected = expected.max(end);
        if let Some(warning) = warning {
            issues.push(format!("原音 {base}: {warning}"));
        }
        let mut pieces = vec![(base.max(watermark), end)];
        for &(a, b) in covered {
            pieces = pieces
                .into_iter()
                .flat_map(|(start, end)| {
                    let mut rest = Vec::new();
                    if b <= start || a >= end {
                        rest.push((start, end));
                    } else {
                        if start < a {
                            rest.push((start, a));
                        }
                        if b < end {
                            rest.push((b, end));
                        }
                    }
                    rest
                })
                .collect();
        }
        for (start, end) in pieces {
            if start == end {
                continue;
            }
            if group.as_ref().is_some_and(|g| g.end != start) {
                finish_group(&mut group, channels)?;
            }
            let samples = match spool.read(start, end) {
                Ok(samples) if samples.iter().flatten().all(|sample| sample.is_finite()) => samples,
                Ok(_) => {
                    finish_group(&mut group, channels)?;
                    issues.push(format!(
                        "原音 {start}..{end}: 非有限サンプルを含むため保持して保留"
                    ));
                    continue;
                }
                Err(error) => {
                    finish_group(&mut group, channels)?;
                    issues.push(format!("原音 {start}..{end}: {error:#}"));
                    continue;
                }
            };
            if group.is_none() {
                fs::create_dir_all(output)?;
                ensure_plain_dir(output)?;
                let dir = new_group_dir(output, start)?;
                let writer = SegmentWriter::new(
                    &dir,
                    1,
                    channels.to_vec(),
                    OutputFormat::Wav,
                    Path::new("unused"),
                )?;
                group = Some(RawGroup {
                    dir,
                    writer,
                    start,
                    end: start,
                });
            }
            let active = group.as_mut().unwrap();
            active.writer.write(&samples)?;
            active.end = end;
            changed = true;
        }
    }
    finish_group(&mut group, channels)?;
    Ok((changed, issues))
}

fn raw_block_frames(raw: &Path, base: u64, channels: &[u16]) -> Result<(u64, Option<String>)> {
    let metadata_path = raw.join(format!("{base:020}_valid.json"));
    let declared = if metadata_path.exists() {
        let value: Value = read_json(&metadata_path)?;
        let frames = value
            .get("frames")
            .and_then(Value::as_u64)
            .context("原音フレーム数がありません")?;
        ensure!((1..=RATE).contains(&frames), "原音フレーム数が範囲外です");
        ensure!(
            value
                .get("valid")
                .and_then(Value::as_array)
                .is_some_and(
                    |valid| valid.len() == channels.len() && valid.iter().all(|v| v.is_boolean())
                ),
            "原音の有効性記録が不正です"
        );
        Some(frames)
    } else {
        None
    };
    let mut common = declared.unwrap_or(RATE);
    let mut mismatch = false;
    for (i, &ch) in channels.iter().enumerate() {
        let path = raw.join(format!("{base:020}_{i}.pcm"));
        ensure_plain_file(&path)?;
        let bytes = fs::metadata(path)?.len();
        let unit = u64::from(ch) * 4;
        ensure!(bytes <= RATE * unit, "原音ブロックが1秒を超えています");
        let frames = bytes / unit;
        common = common.min(frames);
        mismatch |= bytes % unit != 0 || declared.is_some_and(|wanted| wanted != frames);
    }
    ensure!(common > 0, "全トラックに共通する完全フレームがありません");
    let warning = if declared.is_none() {
        Some(format!(
            "確定メタデータがなく、共通する {common} フレームを判定不能として復旧"
        ))
    } else if mismatch {
        Some(format!(
            "記録と原音長が一致しないため共通する {common} フレームだけ復旧し、元データを保持"
        ))
    } else {
        None
    };
    Ok((common, warning))
}

fn new_group_dir(output: &Path, start: u64) -> Result<PathBuf> {
    for attempt in 0..100_000u32 {
        let path = output.join(format!("tail-{start:020}-{attempt:05}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    bail!("復旧先の一意なフォルダーを作成できません")
}

fn finish_group(group: &mut Option<RawGroup>, channels: &[u16]) -> Result<()> {
    if let Some(group) = group.take() {
        let source = SourceRange {
            version: 1,
            source_start: group.start,
            source_end: group.end,
            frames: group.end - group.start,
            channels: channels.to_vec(),
            state: "Unknown".into(),
        };
        write_atomic(&group.dir.join("source.json"), &source)?;
        group.writer.finish(false)?;
    }
    Ok(())
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    ensure_plain_file(path)?;
    ensure!(
        fs::metadata(path)?.len() <= MAX_JSON_BYTES,
        "JSON が大きすぎます: {}",
        path.display()
    );
    serde_json::from_slice(&fs::read(path)?)
        .with_context(|| format!("JSON を読めません: {}", path.display()))
}

pub(crate) fn write_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let name = path
        .file_name()
        .context("保存ファイル名がありません")?
        .to_string_lossy();
    let temporary = path.with_file_name(format!("{name}.{}.{suffix}.partial", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, path)?;
    Ok(())
}

fn parse_number(name: &str) -> Option<u64> {
    if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
        name.parse().ok()
    } else {
        None
    }
}

pub(crate) fn ensure_plain_dir(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        !is_link(&meta) && meta.is_dir(),
        "通常のフォルダーではありません: {}",
        path.display()
    );
    Ok(())
}
pub(crate) fn ensure_plain_file(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        !is_link(&meta) && meta.is_file(),
        "通常のファイルではありません: {}",
        path.display()
    );
    Ok(())
}
fn is_link(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Result<tempfile::TempDir> {
        let root = tempfile::tempdir()?;
        let session = root.path().join("session");
        fs::create_dir(&session)?;
        fs::write(
            session.join("session.json"),
            serde_json::to_vec(
                &serde_json::json!({"devices":[{"channels":1},{"channels":2}],"config":{"format":"Wav"}}),
            )?,
        )?;
        let mut spool = Spool::create(&session.join("raw"), vec![1, 2])?;
        for _ in 0..2 {
            let start = spool.frames;
            spool.append(&[vec![0.1; RATE as usize], vec![-0.2; RATE as usize * 2]])?;
            fs::write(
                spool.dir.join(format!("{start:020}_valid.json")),
                format!("{{\"frames\":{RATE},\"valid\":[true,true]}}"),
            )?;
        }
        Ok(root)
    }
    #[test]
    fn legacy_guard_excludes_self_and_own_workers_but_not_another_recorder() {
        assert!(!is_other_recorder(
            100,
            50,
            "talk-recorder.exe",
            100,
            "renamed.exe"
        ));
        assert!(!is_other_recorder(
            101,
            100,
            "talk-recorder.exe",
            100,
            "renamed.exe"
        ));
        assert!(is_other_recorder(
            102,
            50,
            "TALK-RECORDER.EXE",
            100,
            "renamed.exe"
        ));
        assert!(is_other_recorder(
            103,
            50,
            "renamed.exe",
            100,
            "renamed.exe"
        ));
        assert!(!is_other_recorder(104, 50, "other.exe", 100, "renamed.exe"));
    }
    #[test]
    fn legacy_recovers_all_tracks_and_second_run_does_not_duplicate() -> Result<()> {
        let root = fixture()?;
        let session = root.path().join("session");
        let report = recover_pending(root.path(), Path::new("missing"))?;
        assert_eq!(report.recovered, 1);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let groups =
            fs::read_dir(session.join("recovered"))?.collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(groups.len(), 1);
        validate_complete(&groups[0].path(), 1, &[1, 2], 2 * RATE, OutputFormat::Wav)?;
        fs::remove_file(session.join("recovery.json"))?; // catalogue 書込み前の異常終了でも一式の確定が根拠になる。
        assert_eq!(
            recover_pending(root.path(), Path::new("missing"))?.recovered,
            0
        );
        assert_eq!(fs::read_dir(session.join("recovered"))?.count(), 1);
        assert_eq!(fs::read_dir(session.join("raw"))?.count(), 6);
        Ok(())
    }
    #[test]
    fn active_session_is_skipped_and_lock_file_can_be_reused() -> Result<()> {
        let root = fixture()?;
        let session = root.path().join("session");
        let lock = SessionLock::acquire(&session)?;
        let report = recover_pending(root.path(), Path::new("missing"))?;
        assert_eq!(report.skipped_active, 1);
        assert_eq!(report.recovered, 0);
        drop(lock);
        assert!(SessionLock::acquire(&session).is_ok());
        Ok(())
    }
    #[test]
    fn trailing_partial_timeline_is_ignored_but_middle_corruption_is_rejected() -> Result<()> {
        let root = fixture()?;
        let session = root.path().join("session");
        fs::write(
            session.join("timeline.jsonl"),
            b"{\"event\":\"test\"}\n{\"partial\":",
        )?;
        assert_eq!(
            recover_pending(root.path(), Path::new("missing"))?.recovered,
            1
        );
        fs::write(
            session.join("timeline.jsonl"),
            b"not json\n{\"event\":\"test\"}\n",
        )?;
        let report = recover_pending(root.path(), Path::new("missing"))?;
        assert_eq!(report.errors.len(), 1);
        assert_eq!(fs::read_dir(session.join("raw"))?.count(), 6);
        Ok(())
    }
    #[test]
    fn incomplete_raw_recovers_common_prefix_and_reports_the_shortfall() -> Result<()> {
        let root = fixture()?;
        let session = root.path().join("session");
        let tail = session.join("raw").join(format!("{RATE:020}_1.pcm"));
        OpenOptions::new()
            .write(true)
            .open(&tail)?
            .set_len(RATE / 2 * 2 * 4)?;
        let report = recover_pending(root.path(), Path::new("missing"))?;
        assert_eq!(report.recovered, 1);
        assert!(!report.errors.is_empty());
        let ranges = recovered_ranges(&session.join("recovered"), &[1, 2])?;
        assert_eq!(ranges, vec![(0, RATE + RATE / 2)]);
        assert!(tail.exists());
        Ok(())
    }
    #[test]
    fn committed_split_is_excluded_from_recovered_source() -> Result<()> {
        let root = fixture()?;
        let session = root.path().join("session");
        let mut writer = SegmentWriter::new(
            &session,
            1,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        writer.write(&[vec![0.; RATE as usize], vec![0.; RATE as usize * 2]])?;
        writer.finish(false)?;
        fs::write(
            session.join("timeline.jsonl"),
            serde_json::to_vec(
                &serde_json::json!({"event":"edit","segment":1,"segment_frames":RATE,"action":{"Split":{"at":RATE,"discard":false}}}),
            )?,
        )?;
        let report = recover_pending(root.path(), Path::new("missing"))?;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(
            recovered_ranges(&session.join("recovered"), &[1, 2])?,
            vec![(RATE, 2 * RATE)]
        );
        Ok(())
    }
    #[test]
    fn journal_prefix_and_raw_tail_recover_without_duplicate_audio() -> Result<()> {
        let root = fixture()?;
        let session = root.path().join("session");
        let mut journal = crate::journal::JournalWriter::new(
            &session,
            1,
            vec![1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        journal.write(&[vec![0.1; RATE as usize], vec![-0.2; RATE as usize * 2]])?;
        journal.checkpoint(RATE)?;
        drop(journal);
        // 公開が終わる前に Split が記録されて異常終了した状態。
        fs::write(
            session.join("timeline.jsonl"),
            serde_json::to_vec(
                &serde_json::json!({"event":"edit","segment":1,"segment_frames":RATE,"action":{"Split":{"at":RATE,"discard":false}}}),
            )?,
        )?;
        let report = recover_pending(root.path(), Path::new("missing"))?;
        assert_eq!(report.recovered, 1);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        validate_complete(&session, 1, &[1, 2], RATE, OutputFormat::Wav)?;
        assert_eq!(
            recovered_ranges(&session.join("recovered"), &[1, 2])?,
            vec![(RATE, 2 * RATE)]
        );
        assert_eq!(
            recover_pending(root.path(), Path::new("missing"))?.recovered,
            0
        );
        Ok(())
    }
    #[test]
    fn absent_metadata_only_recovers_complete_common_frames_as_unknown() -> Result<()> {
        let root = fixture()?;
        let session = root.path().join("session");
        fs::remove_file(session.join("raw").join("00000000000000000000_valid.json"))?;
        let report = recover_pending(root.path(), Path::new("missing"))?;
        assert_eq!(report.recovered, 1);
        assert!(report.errors.iter().any(|s| s.contains("確定メタデータ")));
        Ok(())
    }
    #[test]
    fn corrupt_recovered_proof_is_not_overwritten() -> Result<()> {
        let root = fixture()?;
        let session = root.path().join("session");
        recover_pending(root.path(), Path::new("missing"))?;
        let entry = fs::read_dir(session.join("recovered"))?.next().unwrap()?;
        let file = entry.path().join("source.json");
        fs::write(&file, b"bad")?;
        assert!(
            !recover_pending(root.path(), Path::new("missing"))?
                .errors
                .is_empty()
        );
        assert_eq!(fs::read(&file)?, b"bad");
        Ok(())
    }
    #[cfg(windows)]
    #[test]
    fn junction_or_symlink_session_is_not_followed() -> Result<()> {
        use std::os::windows::fs::symlink_dir;
        let root = fixture()?;
        let link = root.path().join("linked");
        if let Err(error) = symlink_dir(root.path().join("session"), &link) {
            if error.raw_os_error() == Some(1314) {
                return Ok(());
            }
            return Err(error.into());
        }
        let report = recover_pending(root.path(), Path::new("missing"))?;
        assert_eq!(report.recovered, 1); // 元セッション一式だけ。
        assert!(report.errors.is_empty());
        Ok(())
    }
}
