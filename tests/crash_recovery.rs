use anyhow::{Result, ensure};
use serde_json::json;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};
use talk_recorder::{
    config::OutputFormat,
    journal::JournalWriter,
    recovery::{SessionLock, recover_pending},
    storage::{Spool, hide_console},
};

#[test]
fn crash_fixture_child() -> Result<()> {
    let Some(root) = std::env::var_os("TALKRECORDER_CRASH_FIXTURE") else {
        return Ok(());
    };
    let session = Path::new(&root).join("session");
    fs::create_dir(&session)?;
    let _lock = SessionLock::acquire(&session)?;
    fs::write(
        session.join("session.json"),
        serde_json::to_vec(
            &json!({"app":"TalkRecorder","version":talk_recorder::APP_VERSION,"devices":[{"channels":1},{"channels":2}],"config":{"format":"Wav"}}),
        )?,
    )?;
    let mut raw = Spool::create(&session.join("raw"), vec![1, 2])?;
    let mut journal = JournalWriter::new(
        &session,
        1,
        vec![1, 2],
        OutputFormat::Wav,
        Path::new("not-needed"),
    )?;
    for second in 0..12_u64 {
        let samples = vec![vec![0.25; 48000], vec![-0.5; 96000]];
        raw.append(&samples)?;
        fs::write(
            raw.dir.join(format!("{:020}_valid.json", second * 48000)),
            serde_json::to_vec(&json!({"frames":48000,"valid":[true,true]}))?,
        )?;
        journal.write(&samples)?;
        if second == 9 {
            journal.checkpoint(10 * 48000)?;
            raw.discard_before(journal.durable_source())?;
        }
    }
    // Dropを通さず終了し、未確定WAV・ロック・記録末尾の実プロセス障害を再現。
    std::process::exit(77)
}

#[test]
fn process_exit_recovers_checkpoint_and_raw_tail_exactly_once() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut child = Command::new(std::env::current_exe()?);
    child
        .args(["--exact", "crash_fixture_child", "--nocapture"])
        .env("TALKRECORDER_CRASH_FIXTURE", root.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_console(&mut child);
    ensure!(
        child.status()?.code() == Some(77),
        "合成録音プロセスの途中終了を確認できません"
    );
    let session = root.path().join("session");
    assert!(!session.join("000001.complete.json").exists());
    let report = recover_pending(root.path(), Path::new("not-needed"))?;
    assert_eq!(report.recovered, 1);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let prefix: serde_json::Value =
        serde_json::from_slice(&fs::read(session.join("000001.complete.json"))?)?;
    assert_eq!(prefix["frames"], 10 * 48000);
    let tails: Vec<_> = fs::read_dir(session.join("recovered"))?.collect::<std::io::Result<_>>()?;
    assert_eq!(tails.len(), 1);
    let tail: serde_json::Value =
        serde_json::from_slice(&fs::read(tails[0].path().join("000001.complete.json"))?)?;
    assert_eq!(tail["frames"], 2 * 48000);
    for (i, ch) in [1, 2].into_iter().enumerate() {
        let bytes = fs::read(session.join(format!("000001_track{}.wav", i + 1)))?;
        assert_eq!(bytes.len(), 80 + 10 * 48000 * ch * 4);
        let sample = f32::from_le_bytes(bytes[80..84].try_into()?);
        assert_eq!(sample, if i == 0 { 0.25 } else { -0.5 });
    }
    let again = recover_pending(root.path(), Path::new("not-needed"))?;
    assert_eq!(again.recovered, 0);
    assert!(again.errors.is_empty());
    assert_eq!(fs::read_dir(session.join("recovered"))?.count(), 1);
    Ok(())
}
