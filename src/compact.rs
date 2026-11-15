//! 公開済み WAV 一式を検証してから M4A へ置換する、再実行可能な容量削減。
//! journal/raw とメタデータのない音声には触れない。
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    config::OutputFormat,
    journal,
    recovery::{
        SessionLock, ensure_plain_dir, ensure_plain_file, is_lock_busy, read_json,
        validate_complete, write_atomic,
    },
    storage::hide_console,
};

const RATE: u64 = 48_000;
const MAX_DEPTH: usize = 4;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompactReport {
    pub converted_groups: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub skipped_active: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Group {
    number: u64,
    frames: u64,
    sample_rate: u32,
    channels: Vec<u16>,
    #[serde(default)]
    format: Option<OutputFormat>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Transaction {
    version: u32,
    group: Group,
    /// 変換開始時の公開確定記録。未知の追加フィールドも維持する。
    original_manifest: Value,
    wav_sizes: Vec<u64>,
    wav_digests: Vec<u64>,
    #[serde(default)]
    kept_wav: bool,
}

pub fn compact_recordings(root: &Path, ffmpeg: &Path) -> Result<CompactReport> {
    let mut report = CompactReport::default();
    if !root.exists() {
        return Ok(report);
    }
    ensure_plain_dir(root)?;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let session = entry.path();
        if !entry.file_type()?.is_dir() || !session.join("session.json").exists() {
            continue;
        }
        let result = (|| -> Result<()> {
            ensure_plain_dir(&session)?;
            let _lock = match SessionLock::acquire(&session) {
                Ok(lock) => lock,
                Err(error) if is_lock_busy(&error) => {
                    report.skipped_active += 1;
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            let metadata: Value = read_json(&session.join("session.json"))?;
            let devices = metadata
                .get("devices")
                .and_then(Value::as_array)
                .context("セッションのデバイス情報がありません")?;
            ensure!((1..=4).contains(&devices.len()), "トラック数が範囲外です");
            let channels: Vec<u16> = devices
                .iter()
                .map(|device| {
                    let ch = device
                        .get("channels")
                        .and_then(Value::as_u64)
                        .context("セッションのチャンネル数がありません")?;
                    ensure!((1..=8).contains(&ch), "チャンネル数が範囲外です");
                    Ok(ch as u16)
                })
                .collect::<Result<_>>()?;
            let fallback_format: OutputFormat = serde_json::from_value(
                metadata
                    .pointer("/config/format")
                    .cloned()
                    .unwrap_or(Value::String("Wav".into())),
            )?;
            compact_directory(
                &session,
                &session,
                &channels,
                fallback_format,
                ffmpeg,
                &mut report,
            )?;
            let recovered = session.join("recovered");
            if recovered.exists() {
                visit_recovered(&session, &recovered, &channels, ffmpeg, 0, &mut report)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            report
                .errors
                .push(format!("{}: {error:#}", session.display()));
        }
    }
    Ok(report)
}

fn visit_recovered(
    session: &Path,
    dir: &Path,
    channels: &[u16],
    ffmpeg: &Path,
    depth: usize,
    report: &mut CompactReport,
) -> Result<()> {
    ensure_plain_dir(dir)?;
    ensure!(depth <= MAX_DEPTH, "復旧音声の階層が深すぎます");
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let child = entry.path();
        ensure_plain_dir(&child)?;
        if child.join("source.json").exists() {
            let source: Value = read_json(&child.join("source.json"))?;
            let start = source.get("source_start").and_then(Value::as_u64);
            let end = source.get("source_end").and_then(Value::as_u64);
            let frames = source.get("frames").and_then(Value::as_u64);
            ensure!(
                source.get("version").and_then(Value::as_u64) == Some(1)
                    && source.get("channels") == Some(&serde_json::to_value(channels)?)
                    && start.zip(end).and_then(|(a, b)| b.checked_sub(a)) == frames
                    && frames.is_some_and(|n| n > 0),
                "復旧元の範囲が不正です"
            );
            let proof: Group = read_json(&child.join("000001.complete.json"))?;
            ensure!(
                Some(proof.frames) == frames && proof.number == 1,
                "復旧元と公開音声の長さが一致しません"
            );
            compact_directory(session, &child, channels, OutputFormat::Wav, ffmpeg, report)?;
        } else if depth < MAX_DEPTH {
            // .compact-* など、変換処理自身の作業ディレクトリを再走査しない。
            if !entry.file_name().to_string_lossy().starts_with('.') {
                visit_recovered(session, &child, channels, ffmpeg, depth + 1, report)?;
            }
        }
    }
    Ok(())
}

fn compact_directory(
    session: &Path,
    dir: &Path,
    channels: &[u16],
    fallback_format: OutputFormat,
    ffmpeg: &Path,
    report: &mut CompactReport,
) -> Result<()> {
    let mut numbers = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(number) = name
            .strip_suffix(".complete.json")
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|n| n.parse::<u64>().ok())
        {
            numbers.push(number);
        }
    }
    numbers.sort_unstable();
    numbers.dedup();
    for number in numbers {
        let result = (|| -> Result<Option<(u64, u64)>> {
            if dir == session {
                let journals = session.join("journal");
                if journals.exists() {
                    ensure_plain_dir(&journals)?;
                    let journal_dir = journals.join(format!("{number:06}"));
                    if journal_dir.exists() {
                        ensure_plain_dir(&journal_dir)?;
                        for entry in fs::read_dir(&journal_dir)? {
                            let entry = entry?;
                            let name = entry.file_name();
                            let name = name.to_string_lossy();
                            if name.starts_with("state-") && name.ends_with(".json") {
                                ensure_plain_file(&entry.path())?;
                            }
                        }
                    }
                }
                if let Some(state) = journal::load_state(session, number)? {
                    ensure!(state.published, "内部確定音声の公開が完了していません");
                    let group: Group = read_json(&dir.join(format!("{number:06}.complete.json")))?;
                    ensure!(
                        state.frames == group.frames && state.channels == channels,
                        "公開音声と内部確定記録が一致しません"
                    );
                }
            }
            compact_group(dir, number, channels, fallback_format, ffmpeg)
        })();
        match result {
            Ok(Some((before, after))) => {
                report.converted_groups += 1;
                report.bytes_before += before;
                report.bytes_after += after;
            }
            Ok(None) => {}
            Err(error) => report
                .errors
                .push(format!("{} / {number:06}: {error:#}", dir.display())),
        }
    }
    Ok(())
}

fn compact_group(
    dir: &Path,
    number: u64,
    channels: &[u16],
    fallback_format: OutputFormat,
    ffmpeg: &Path,
) -> Result<Option<(u64, u64)>> {
    let manifest_path = dir.join(format!("{number:06}.complete.json"));
    let manifest: Value = read_json(&manifest_path)?;
    let group: Group = serde_json::from_value(manifest.clone())?;
    ensure!(
        group.number == number
            && group.channels == channels
            && group.sample_rate == RATE as u32
            && group.frames > 0,
        "公開確定記録がセッションと一致しません"
    );
    let transaction_path = dir.join(format!("{number:06}.compact.json"));
    let format = group.format.unwrap_or(fallback_format);
    if format != OutputFormat::Wav && !transaction_path.exists() {
        return Ok(None);
    }
    ensure!(
        matches!(format, OutputFormat::Wav | OutputFormat::M4a),
        "変換途中の形式が不正です"
    );
    let stage = dir.join(format!(".compact-{number:06}"));
    let mut tx = if transaction_path.exists() {
        let tx: Transaction = read_json(&transaction_path)?;
        let mut original = group.clone();
        original.format = tx.group.format;
        ensure!(
            tx.version == 1
                && tx.group == original
                && tx.wav_sizes.len() == channels.len()
                && tx.wav_digests.len() == channels.len()
                && serde_json::from_value::<Group>(tx.original_manifest.clone())? == tx.group,
            "変換の再開記録が公開音声と一致しません"
        );
        tx
    } else {
        validate_complete(dir, number, channels, group.frames, OutputFormat::Wav)?;
        ensure!(
            !stage.exists(),
            "所有を確認できない変換作業フォルダーがあります"
        );
        let mut sizes = Vec::new();
        let mut digests = Vec::new();
        for (i, &ch) in channels.iter().enumerate() {
            sizes.push(validate_wav(
                &track_path(dir, number, i, "wav"),
                ch,
                group.frames,
            )?);
            digests.push(digest(&track_path(dir, number, i, "wav"))?);
            ensure!(
                !track_path(dir, number, i, "m4a").exists(),
                "同名の M4A が既にあります"
            );
        }
        let tx = Transaction {
            version: 1,
            group: group.clone(),
            original_manifest: manifest.clone(),
            wav_sizes: sizes,
            wav_digests: digests,
            kept_wav: false,
        };
        write_atomic(&transaction_path, &tx)?;
        tx
    };
    let bytes_before = tx
        .wav_sizes
        .iter()
        .try_fold(0u64, |total, size| total.checked_add(*size))
        .context("変換前サイズの合計が上限を超えています")?;
    if format == OutputFormat::M4a {
        let mut expected_manifest = tx.original_manifest.clone();
        expected_manifest["format"] = serde_json::to_value(OutputFormat::M4a)?;
        ensure!(
            manifest == expected_manifest,
            "変換後の公開確定記録が変更されました"
        );
        // manifest 切替後に停止した場合も、全出力を再検証してから残った WAV だけ削除する。
        let bytes_after = validate_outputs(dir, &group, ffmpeg)?;
        delete_sources(dir, &tx)?;
        clean_stage(&stage, number, channels.len())?;
        fs::remove_file(transaction_path)?;
        return Ok(Some((bytes_before, bytes_after)));
    }
    ensure!(
        manifest == tx.original_manifest,
        "変換中に公開確定記録が変更されました"
    );
    let mut source_missing = false;
    for i in 0..channels.len() {
        source_missing |= !has_plain_file(&track_path(dir, number, i, "wav"))?;
    }
    if source_missing {
        // 電源断で manifest の rename だけが以前の状態へ戻った場合を救う。
        // 部分的な変換結果や kept_wav から推測せず、確定先の全 M4A が実復号できることを要求。
        ensure!(!tx.kept_wav, "保持予定の WAV が欠落しています");
        let bytes_after = validate_outputs(dir, &group, ffmpeg)?;
        ensure!(
            bytes_after < bytes_before,
            "WAV 欠落後の変換結果に容量削減の根拠がありません"
        );
        validate_remaining_sources(dir, &tx)?;
        let mut committed = manifest;
        committed["format"] = serde_json::to_value(OutputFormat::M4a)?;
        write_atomic(&manifest_path, &committed)?;
        delete_sources(dir, &tx)?;
        clean_stage(&stage, number, channels.len())?;
        fs::remove_file(transaction_path)?;
        return Ok(Some((bytes_before, bytes_after)));
    }
    for (i, &ch) in channels.iter().enumerate() {
        let size = validate_wav(&track_path(dir, number, i, "wav"), ch, group.frames)?;
        ensure!(
            size == tx.wav_sizes[i]
                && digest(&track_path(dir, number, i, "wav"))? == tx.wav_digests[i],
            "変換元 WAV が変更されました"
        );
    }
    if tx.kept_wav {
        remove_converted(dir, &stage, number, channels.len())?;
        return Ok(None);
    }
    if stage.exists() {
        ensure_plain_dir(&stage)?;
    } else {
        fs::create_dir(&stage)?;
    }
    // 確定先に既にあるファイルは、当取引の前回実行が検証して移動したものだけ。
    for (i, &ch) in channels.iter().enumerate() {
        let destination = track_path(dir, number, i, "m4a");
        if destination.exists() {
            validate_m4a(&destination, ch, group.frames, ffmpeg, &stage, i)?;
            continue;
        }
        let staged = track_path(&stage, number, i, "m4a");
        if staged.exists() {
            validate_m4a(&staged, ch, group.frames, ffmpeg, &stage, i)?;
        } else {
            encode(
                &track_path(dir, number, i, "wav"),
                &staged,
                ch,
                group.frames,
                ffmpeg,
                &stage,
                i,
            )?;
            validate_m4a(&staged, ch, group.frames, ffmpeg, &stage, i)?;
        }
    }
    let mut bytes_after = 0;
    for i in 0..channels.len() {
        let final_path = track_path(dir, number, i, "m4a");
        let path = if final_path.exists() {
            final_path
        } else {
            track_path(&stage, number, i, "m4a")
        };
        bytes_after += fs::metadata(path)?.len();
    }
    if bytes_after >= bytes_before {
        // 数サンプルの極短音声などはコンテナーの方が大きい。原音保持を明記して再変換を避ける。
        tx.kept_wav = true;
        write_atomic(&transaction_path, &tx)?;
        remove_converted(dir, &stage, number, channels.len())?;
        return Ok(None);
    }
    for i in 0..channels.len() {
        let destination = track_path(dir, number, i, "m4a");
        if !destination.exists() {
            fs::rename(track_path(&stage, number, i, "m4a"), &destination)?;
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(destination)?
            .sync_all()?;
    }
    let bytes_after = validate_outputs(dir, &group, ffmpeg)?;
    validate_remaining_sources(dir, &tx)?;
    // 全 track の検証・同期後に一式の公開形式を切り替える。削除の開始はこの後だけ。
    let mut committed = manifest;
    committed["format"] = serde_json::to_value(OutputFormat::M4a)?;
    write_atomic(&manifest_path, &committed)?;
    delete_sources(dir, &tx)?;
    clean_stage(&stage, number, channels.len())?;
    fs::remove_file(transaction_path)?;
    Ok(Some((bytes_before, bytes_after)))
}

fn track_path(dir: &Path, number: u64, track: usize, extension: &str) -> PathBuf {
    dir.join(format!("{number:06}_track{}.{}", track + 1, extension))
}

fn validate_wav(path: &Path, channels: u16, frames: u64) -> Result<u64> {
    ensure_plain_file(path)?;
    let bytes = frames
        .checked_mul(u64::from(channels))
        .and_then(|n| n.checked_mul(4))
        .context("WAV サイズが上限を超えています")?;
    let mut header = [0; 80];
    File::open(path)?.read_exact(&mut header)?;
    let size = fs::metadata(path)?.len();
    ensure!(
        &header[..4] == b"RF64"
            && &header[8..16] == b"WAVEds64"
            && &header[48..52] == b"fmt "
            && &header[72..76] == b"data"
            && u16::from_le_bytes(header[56..58].try_into()?) == 3
            && u16::from_le_bytes(header[58..60].try_into()?) == channels
            && u32::from_le_bytes(header[60..64].try_into()?) == RATE as u32
            && u16::from_le_bytes(header[70..72].try_into()?) == 32
            && u64::from_le_bytes(header[28..36].try_into()?) == bytes
            && u64::from_le_bytes(header[36..44].try_into()?) == frames
            && bytes.checked_add(80) == Some(size),
        "変換元 WAV の形式・長さが一致しません"
    );
    Ok(size)
}

fn encode(
    input: &Path,
    output: &Path,
    ch: u16,
    frames: u64,
    ffmpeg: &Path,
    stage: &Path,
    index: usize,
) -> Result<()> {
    let partial = output.with_extension("m4a.partial");
    if partial.exists() {
        ensure_plain_file(&partial)?;
        fs::remove_file(&partial)?;
    }
    let log = owned_file(&stage.join(format!("track{index}.encode.log")))?;
    let mut command = Command::new(ffmpeg);
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-xerror",
            "-i",
        ])
        .arg(input)
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-c:a",
            "aac",
            "-profile:a",
            "aac_low",
            "-b:a",
            if ch == 1 { "64000" } else { "128000" },
            "-movie_timescale",
            "48000",
            "-use_editlist",
            "1",
            "-movflags",
            "+faststart",
            "-f",
            "ipod",
        ])
        .arg(&partial)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    run(&mut command, frames)?;
    ensure_plain_file(&partial)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&partial)?
        .sync_all()?;
    fs::rename(partial, output)?;
    Ok(())
}

fn validate_outputs(dir: &Path, group: &Group, ffmpeg: &Path) -> Result<u64> {
    let stage = dir.join(format!(".compact-{:06}", group.number));
    if stage.exists() {
        ensure_plain_dir(&stage)?;
    } else {
        fs::create_dir(&stage)?;
    }
    let mut bytes = 0;
    for (i, &ch) in group.channels.iter().enumerate() {
        let path = track_path(dir, group.number, i, "m4a");
        validate_m4a(&path, ch, group.frames, ffmpeg, &stage, i)?;
        bytes += fs::metadata(path)?.len();
    }
    Ok(bytes)
}

fn validate_m4a(
    path: &Path,
    ch: u16,
    frames: u64,
    ffmpeg: &Path,
    stage: &Path,
    index: usize,
) -> Result<()> {
    ensure_plain_file(path)?;
    let probe = ffmpeg.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    let info_path = stage.join(format!("track{index}.probe.json"));
    let output = owned_file(&info_path)?;
    let log = owned_file(&stage.join(format!("track{index}.probe.log")))?;
    let mut command = Command::new(probe);
    command
        .args([
            "-v",
            "error",
            "-select_streams",
            "a",
            "-show_entries",
            "stream=codec_name,sample_rate,channels,time_base,duration_ts",
            "-of",
            "json",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(output)
        .stderr(log);
    run(&mut command, frames)?;
    let info: Value = read_json(&info_path)?;
    let streams = info
        .get("streams")
        .and_then(Value::as_array)
        .context("M4A ストリームがありません")?;
    ensure!(streams.len() == 1, "M4A の音声数が一致しません");
    let stream = &streams[0];
    ensure!(
        stream.get("codec_name").and_then(Value::as_str) == Some("aac")
            && stream.get("sample_rate").and_then(Value::as_str) == Some("48000")
            && stream.get("channels").and_then(Value::as_u64) == Some(u64::from(ch))
            && stream.get("time_base").and_then(Value::as_str) == Some("1/48000")
            && stream.get("duration_ts").and_then(Value::as_u64) == Some(frames),
        "M4A のコーデック・チャンネル・保存時間が一致しません"
    );
    let log = owned_file(&stage.join(format!("track{index}.decode.log")))?;
    let mut command = Command::new(ffmpeg);
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-xerror",
            "-err_detect",
            "explode",
            "-i",
        ])
        .arg(path)
        .args([
            "-map",
            "0:a:0",
            "-f",
            "f32le",
            "-c:a",
            "pcm_f32le",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    hide_console(&mut command);
    let mut child = command
        .spawn()
        .context("M4A 検証用 FFmpeg を起動できません")?;
    let mut output = child.stdout.take().context("復号パイプがありません")?;
    // PCM はメモリにもディスクにも蓄積せず、復号済みのバイト数だけ数える。
    let reader = thread::spawn(move || std::io::copy(&mut output, &mut std::io::sink()));
    let result = wait_child(&mut child, frames);
    let decoded = reader
        .join()
        .map_err(|_| anyhow::anyhow!("復号検証スレッドが停止しました"))??;
    result?;
    let expected = frames
        .checked_mul(u64::from(ch))
        .and_then(|n| n.checked_mul(4))
        .context("復号サイズが上限を超えています")?;
    ensure!(
        decoded == expected,
        "M4A の復号フレーム数が一致しません: {decoded} / {expected} bytes"
    );
    Ok(())
}

fn owned_file(path: &Path) -> Result<File> {
    match fs::symlink_metadata(path) {
        Ok(_) => ensure_plain_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?)
}

fn run(command: &mut Command, frames: u64) -> Result<()> {
    hide_console(command);
    let mut child = command
        .spawn()
        .context("音声変換・検証プログラムを起動できません")?;
    wait_child(&mut child, frames)
}

fn wait_child(child: &mut std::process::Child, frames: u64) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(60 + frames / RATE);
    loop {
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        };
        if let Some(status) = status {
            ensure!(
                status.success(),
                "音声変換・検証プログラムが失敗しました: {status}"
            );
            return Ok(());
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("音声変換・検証が制限時間を超えたため元の WAV を保持しました");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn delete_sources(dir: &Path, tx: &Transaction) -> Result<()> {
    // 一部削除後の再開でも、残った全 WAV を先に確認し、関係のないファイルを削除しない。
    validate_remaining_sources(dir, tx)?;
    for i in 0..tx.group.channels.len() {
        let path = track_path(dir, tx.group.number, i, "wav");
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn validate_remaining_sources(dir: &Path, tx: &Transaction) -> Result<()> {
    for (i, &ch) in tx.group.channels.iter().enumerate() {
        let path = track_path(dir, tx.group.number, i, "wav");
        if has_plain_file(&path)? {
            ensure!(
                validate_wav(&path, ch, tx.group.frames)? == tx.wav_sizes[i]
                    && digest(&path)? == tx.wav_digests[i],
                "削除前に WAV が変更されました"
            );
        }
    }
    Ok(())
}

fn has_plain_file(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            ensure_plain_file(path)?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn digest(path: &Path) -> Result<u64> {
    ensure_plain_file(path)?;
    let mut file = File::open(path)?;
    let mut hash = 0xcbf29ce484222325u64;
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        for &byte in &buffer[..n] {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
    }
    Ok(hash)
}

fn remove_converted(dir: &Path, stage: &Path, number: u64, tracks: usize) -> Result<()> {
    for i in 0..tracks {
        let path = track_path(dir, number, i, "m4a");
        if path.exists() {
            ensure_plain_file(&path)?;
            fs::remove_file(path)?;
        }
    }
    clean_stage(stage, number, tracks)
}

fn clean_stage(stage: &Path, number: u64, tracks: usize) -> Result<()> {
    if !stage.exists() {
        return Ok(());
    }
    ensure_plain_dir(stage)?;
    for entry in fs::read_dir(stage)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let owned_log = (0..tracks).any(|i| {
            ["encode.log", "probe.log", "probe.json", "decode.log"]
                .iter()
                .any(|suffix| name == format!("track{i}.{suffix}"))
        });
        let own_audio = (0..tracks).any(|i| {
            name == format!("{number:06}_track{}.m4a", i + 1)
                || name == format!("{number:06}_track{}.m4a.partial", i + 1)
        });
        ensure!(
            owned_log || own_audio,
            "変換作業内に所有不明なファイルがあります"
        );
        ensure_plain_file(&entry.path())?;
        fs::remove_file(entry.path())?;
    }
    fs::remove_dir(stage)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{SegmentWriter, Spool};

    fn fixture(frames: usize) -> Result<tempfile::TempDir> {
        let root = tempfile::tempdir()?;
        let session = root.path().join("session");
        fs::create_dir(&session)?;
        fs::write(
            session.join("session.json"),
            serde_json::to_vec(&serde_json::json!({
                "devices":[{"channels":1},{"channels":2},{"channels":1},{"channels":2}],
                "config":{"format":"Wav"}
            }))?,
        )?;
        let tracks = [1, 2, 1, 2]
            .into_iter()
            .map(|ch| {
                (0..frames)
                    .flat_map(|i| {
                        let sample = ((i as f32) * 0.043).sin() * 0.1;
                        (0..ch).map(move |_| sample)
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let mut writer = SegmentWriter::new(
            &session,
            1,
            vec![1, 2, 1, 2],
            OutputFormat::Wav,
            Path::new("missing"),
        )?;
        writer.write(&tracks)?;
        writer.finish(false)?;
        Ok(root)
    }

    fn ffmpeg() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/ffmpeg/bin/ffmpeg.exe")
    }

    #[test]
    fn active_sessions_and_metadata_free_audio_are_untouched() -> Result<()> {
        let root = fixture(100)?;
        let session = root.path().join("session");
        let _lock = SessionLock::acquire(&session)?;
        fs::write(root.path().join("standalone.wav"), b"original")?;
        let report = compact_recordings(root.path(), Path::new("missing"))?;
        assert_eq!(report.skipped_active, 1);
        assert!(report.errors.is_empty());
        assert!(track_path(&session, 1, 0, "wav").exists());
        assert_eq!(fs::read(root.path().join("standalone.wav"))?, b"original");
        assert!(!session.join("000001.compact.json").exists());
        Ok(())
    }

    #[test]
    fn encoding_failure_preserves_all_original_tracks_and_manifest() -> Result<()> {
        let root = fixture(1024)?;
        let session = root.path().join("session");
        let original = fs::read(session.join("000001.complete.json"))?;
        let report = compact_recordings(root.path(), Path::new("definitely-missing-ffmpeg"))?;
        assert_eq!(report.converted_groups, 0);
        assert_eq!(report.errors.len(), 1);
        for i in 0..4 {
            assert!(track_path(&session, 1, i, "wav").exists());
        }
        assert_eq!(fs::read(session.join("000001.complete.json"))?, original);
        assert!(session.join("000001.compact.json").exists());
        Ok(())
    }

    #[test]
    fn source_modification_after_failure_is_rejected_without_deletion() -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let root = fixture(1024)?;
        let session = root.path().join("session");
        compact_recordings(root.path(), Path::new("missing"))?;
        let wav = track_path(&session, 1, 0, "wav");
        let mut file = OpenOptions::new().write(true).open(&wav)?;
        file.seek(SeekFrom::Start(80))?;
        file.write_all(&0.9f32.to_le_bytes())?;
        drop(file);
        let report = compact_recordings(root.path(), Path::new("missing"))?;
        assert!(report.errors.iter().any(|e| e.contains("WAV が変更")));
        for i in 0..4 {
            assert!(track_path(&session, 1, i, "wav").exists());
        }
        Ok(())
    }

    #[test]
    fn invalid_wav_group_never_starts_an_encoder() -> Result<()> {
        let root = fixture(100)?;
        let session = root.path().join("session");
        fs::write(track_path(&session, 1, 2, "wav"), b"broken")?;
        let report = compact_recordings(root.path(), Path::new("missing"))?;
        assert_eq!(report.errors.len(), 1);
        assert!(!session.join("000001.compact.json").exists());
        assert!(track_path(&session, 1, 0, "wav").exists());
        Ok(())
    }

    #[test]
    fn legacy_opus_and_flac_without_manifest_format_are_untouched() -> Result<()> {
        for format in [OutputFormat::Opus, OutputFormat::Flac] {
            let root = fixture(100)?;
            let session = root.path().join("session");
            let manifest = session.join("000001.complete.json");
            let mut proof: Value = read_json(&manifest)?;
            proof.as_object_mut().unwrap().remove("format");
            write_atomic(&manifest, &proof)?;
            let config = session.join("session.json");
            let mut metadata: Value = read_json(&config)?;
            metadata["config"]["format"] = serde_json::to_value(format)?;
            write_atomic(&config, &metadata)?;
            for i in 0..4 {
                fs::rename(
                    track_path(&session, 1, i, "wav"),
                    track_path(&session, 1, i, format.extension()),
                )?;
            }
            let original = fs::read(&manifest)?;
            let report = compact_recordings(root.path(), Path::new("missing"))?;
            assert!(report.errors.is_empty(), "{:?}", report.errors);
            assert_eq!(report.converted_groups, 0);
            assert_eq!(fs::read(&manifest)?, original);
            for i in 0..4 {
                assert!(track_path(&session, 1, i, format.extension()).exists());
            }
            assert!(!session.join("000001.compact.json").exists());
        }
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn linked_session_is_never_followed() -> Result<()> {
        let root = tempfile::tempdir()?;
        let outside = fixture(100)?;
        let link = root.path().join("linked");
        if let Err(error) = std::os::windows::fs::symlink_dir(outside.path().join("session"), &link)
        {
            if error.raw_os_error() == Some(1314) {
                return Ok(());
            }
            return Err(error.into());
        }
        let report = compact_recordings(root.path(), Path::new("missing"))?;
        assert_eq!(report.converted_groups, 0);
        assert!(track_path(&outside.path().join("session"), 1, 0, "wav").exists());
        assert!(!outside.path().join("session/000001.compact.json").exists());
        Ok(())
    }

    #[test]
    #[ignore = "固定 FFmpeg/FFprobe が必要。4トラックの実変換・復号長と再実行を検証"]
    fn four_track_m4a_conversion_and_failed_attempt_resume() -> Result<()> {
        let root = fixture(59_259)?;
        let session = root.path().join("session");
        compact_recordings(root.path(), Path::new("missing"))?;
        let report = compact_recordings(root.path(), &ffmpeg())?;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.converted_groups, 1);
        assert!(report.bytes_after < report.bytes_before);
        for i in 0..4 {
            assert!(!track_path(&session, 1, i, "wav").exists());
            assert!(track_path(&session, 1, i, "m4a").exists());
        }
        let proof: Value = read_json(&session.join("000001.complete.json"))?;
        assert_eq!(proof["format"], "M4a");
        assert_eq!(proof["frames"], 59_259);
        assert!(!session.join("000001.compact.json").exists());
        let second = compact_recordings(root.path(), &ffmpeg())?;
        assert_eq!(second.converted_groups, 0);
        assert!(second.errors.is_empty());
        Ok(())
    }

    #[test]
    #[ignore = "固定 FFmpeg/FFprobe が必要。manifest切替・原音の一部削除後の再開を検証"]
    fn committed_conversion_finishes_partial_wav_deletion() -> Result<()> {
        let root = fixture(59_259)?;
        let session = root.path().join("session");
        // 最初の失敗が変換前の原音 fingerprint を残す。
        compact_recordings(root.path(), Path::new("missing"))?;
        let tx = fs::read(session.join("000001.compact.json"))?;
        let wavs = (0..4)
            .map(|i| fs::read(track_path(&session, 1, i, "wav")))
            .collect::<std::io::Result<Vec<_>>>()?;
        let first = compact_recordings(root.path(), &ffmpeg())?;
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        // 実変換済み M4A と切替済み manifest を維持し、途中削除の状態だけ再現。
        for (i, wav) in wavs.iter().enumerate().skip(1) {
            fs::write(track_path(&session, 1, i, "wav"), wav)?;
        }
        fs::write(session.join("000001.compact.json"), tx)?;
        let report = compact_recordings(root.path(), &ffmpeg())?;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.converted_groups, 1);
        for i in 0..4 {
            assert!(!track_path(&session, 1, i, "wav").exists());
        }
        assert!(!session.join("000001.compact.json").exists());
        Ok(())
    }

    #[test]
    #[ignore = "固定 FFmpeg/FFprobe が必要。電源断でmanifestだけ旧状態へ戻った場合の再検証を検証"]
    fn old_manifest_with_missing_wav_is_recommitted_from_all_verified_m4a() -> Result<()> {
        let root = fixture(59_259)?;
        let session = root.path().join("session");
        let manifest = fs::read(session.join("000001.complete.json"))?;
        compact_recordings(root.path(), Path::new("missing"))?;
        let tx = fs::read(session.join("000001.compact.json"))?;
        let wavs = (0..4)
            .map(|i| fs::read(track_path(&session, 1, i, "wav")))
            .collect::<std::io::Result<Vec<_>>>()?;
        let first = compact_recordings(root.path(), &ffmpeg())?;
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        fs::write(session.join("000001.complete.json"), manifest)?;
        fs::write(session.join("000001.compact.json"), tx)?;
        for (i, wav) in wavs.iter().enumerate().skip(1) {
            fs::write(track_path(&session, 1, i, "wav"), wav)?;
        }
        let damaged_path = track_path(&session, 1, 2, "m4a");
        let undamaged = fs::read(&damaged_path)?;
        fs::write(&damaged_path, b"damaged")?;
        let rejected = compact_recordings(root.path(), &ffmpeg())?;
        assert_eq!(rejected.converted_groups, 0);
        assert!(!rejected.errors.is_empty());
        for i in 1..4 {
            assert!(track_path(&session, 1, i, "wav").exists());
        }
        fs::write(&damaged_path, undamaged)?;
        let resumed = compact_recordings(root.path(), &ffmpeg())?;
        assert!(resumed.errors.is_empty(), "{:?}", resumed.errors);
        assert_eq!(resumed.converted_groups, 1);
        let proof: Value = read_json(&session.join("000001.complete.json"))?;
        assert_eq!(proof["format"], "M4a");
        for i in 0..4 {
            assert!(!track_path(&session, 1, i, "wav").exists());
        }
        assert!(!session.join("000001.compact.json").exists());
        Ok(())
    }

    #[test]
    #[ignore = "固定 FFmpeg/FFprobe が必要。極短音声で圧縮が大きくなる場合は WAV を保持"]
    fn tiny_group_is_retained_when_m4a_is_larger() -> Result<()> {
        let root = fixture(1)?;
        let session = root.path().join("session");
        let report = compact_recordings(root.path(), &ffmpeg())?;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.converted_groups, 0);
        assert_eq!(report.bytes_before, 0);
        for i in 0..4 {
            assert!(track_path(&session, 1, i, "wav").exists());
            assert!(!track_path(&session, 1, i, "m4a").exists());
        }
        assert!(!session.join(".compact-000001").exists());
        let tx: Transaction = read_json(&session.join("000001.compact.json"))?;
        assert!(tx.kept_wav);
        assert!(
            compact_recordings(root.path(), Path::new("missing"))?
                .errors
                .is_empty()
        );
        Ok(())
    }

    #[test]
    #[ignore = "固定 FFmpeg/FFprobe が必要。復旧音声の圧縮後も raw 範囲を重複復旧しない"]
    fn recovered_tail_remains_a_valid_recovery_proof_after_compaction() -> Result<()> {
        let root = fixture(RATE as usize)?;
        let session = root.path().join("session");
        let mut spool = Spool::create(&session.join("raw"), vec![1, 2, 1, 2])?;
        spool.append(&[
            vec![0.1; RATE as usize],
            vec![0.1; RATE as usize * 2],
            vec![0.1; RATE as usize],
            vec![0.1; RATE as usize * 2],
        ])?;
        fs::write(
            spool.dir.join("00000000000000000000_valid.json"),
            serde_json::to_vec(&serde_json::json!({"frames":RATE,"valid":[true,true,true,true]}))?,
        )?;
        let recovery = crate::recovery::recover_pending(root.path(), &ffmpeg())?;
        assert!(recovery.errors.is_empty(), "{:?}", recovery.errors);
        assert_eq!(recovery.recovered, 1);
        let converted = compact_recordings(root.path(), &ffmpeg())?;
        assert!(converted.errors.is_empty(), "{:?}", converted.errors);
        assert_eq!(converted.converted_groups, 2); // 公開分と復旧分。
        let after = crate::recovery::recover_pending(root.path(), &ffmpeg())?;
        assert!(after.errors.is_empty(), "{:?}", after.errors);
        assert_eq!(after.recovered, 0);
        assert_eq!(fs::read_dir(session.join("recovered"))?.count(), 1);
        Ok(())
    }

    #[test]
    #[ignore = "固定 FFmpeg/FFprobe が必要。公開済み内部記録を残したまま圧縮しても復旧可能"]
    fn published_journal_accepts_compacted_public_format() -> Result<()> {
        let root = fixture(RATE as usize)?;
        let session = root.path().join("session");
        for i in 0..4 {
            fs::remove_file(track_path(&session, 1, i, "wav"))?;
        }
        fs::remove_file(session.join("000001.complete.json"))?;
        let mut journal = journal::JournalWriter::new(
            &session,
            1,
            vec![1, 2, 1, 2],
            OutputFormat::Wav,
            &ffmpeg(),
        )?;
        journal.write(&[
            vec![0.1; RATE as usize],
            vec![0.1; RATE as usize * 2],
            vec![0.1; RATE as usize],
            vec![0.1; RATE as usize * 2],
        ])?;
        journal.finish(RATE, false)?;
        let report = compact_recordings(root.path(), &ffmpeg())?;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.converted_groups, 1);
        let state = journal::load_state(&session, 1)?.unwrap();
        assert!(state.published);
        assert_eq!(state.format, OutputFormat::Wav); // 元形式の履歴を維持。
        let recovery = journal::recover_all(&session, &ffmpeg())?;
        assert_eq!(recovery.source_until, RATE);
        assert_eq!(recovery.recovered, 0);
        let next = crate::recovery::recover_pending(root.path(), &ffmpeg())?;
        assert!(next.errors.is_empty(), "{:?}", next.errors);
        assert_eq!(next.recovered, 0);
        Ok(())
    }
}
