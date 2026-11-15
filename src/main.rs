#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--speech-worker") {
        return talk_recorder::speech::run_worker();
    }
    if args.iter().any(|arg| arg == "--list-devices") {
        println!(
            "{}",
            serde_json::to_string_pretty(&talk_recorder::capture::enumerate_devices()?)?
        );
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let base = exe
        .ancestors()
        .find(|p| p.join("runtime").is_dir() && p.join("Cargo.toml").is_file())
        .map(std::path::Path::to_path_buf)
        .unwrap_or(exe.parent().unwrap().to_path_buf());
    std::env::set_current_dir(base)?;
    let path = args
        .windows(2)
        .find(|p| p[0] == "--config")
        .map(|p| std::path::PathBuf::from(&p[1]))
        .unwrap_or_else(talk_recorder::ui::config_path_default);
    if let Some(index) = args.iter().position(|arg| arg == "--stop-after-secs") {
        let seconds: f64 = args
            .get(index + 1)
            .ok_or_else(|| anyhow::anyhow!("停止までの秒数を指定してください"))?
            .parse()?;
        anyhow::ensure!(
            seconds.is_finite() && (1.0..=3600.0).contains(&seconds),
            "停止までの秒数は1〜3600秒で指定してください"
        );
    }
    if args.iter().any(|arg| arg == "--write-default-config") {
        talk_recorder::config::Config::default().save_atomic(&path)?;
        println!("{}", path.display());
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--self-check") {
        let config = talk_recorder::config::Config::load(&path)?;
        let mut client = talk_recorder::speech::SpeechClient::spawn(&config.speech)?;
        let analysis = client.process(
            0,
            48000,
            1,
            &vec![0.; 48000],
            talk_recorder::speech::DenoiseMode::Off,
        )?;
        println!(
            "speech.cpp {}: {:?}, {} samples",
            client.version,
            analysis.state,
            analysis.samples.len()
        );
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--check-storage") {
        let config = talk_recorder::config::Config::load(&path)?;
        let mut monitor = talk_recorder::disk_space::SpaceMonitor::start(
            &config.output_dir,
            config.stop_free_space_mb,
        )?;
        let reading = monitor.check_now()?;
        println!(
            "{}",
            serde_json::json!({
                "success": reading.is_none_or(|reading| !reading.is_low()),
                "stop_free_space_mb":config.stop_free_space_mb,
                "available_bytes":reading.map(|reading|reading.available_bytes),
                "recorded_audio":false
            })
        );
        if let Some(reading) = reading {
            anyhow::ensure!(!reading.is_low(), "{}", reading.message());
        }
        return Ok(());
    }
    if let Some(index) = args.iter().position(|arg| arg == "--check-denoise") {
        let id = args
            .get(index + 1)
            .ok_or_else(|| anyhow::anyhow!("プラグインIDを指定してください"))?;
        let config = talk_recorder::config::Config::load(&path)?;
        let mode = talk_recorder::speech::DenoiseMode::Plugin(id.clone());
        mode.validate()?;
        let mut client = talk_recorder::speech::SpeechClient::spawn(&config.speech)?;
        client.prepare_denoise(std::slice::from_ref(&mode))?;
        let start = std::time::Instant::now();
        client.check_denoise(
            &[mode],
            std::time::Duration::from_secs_f64(config.analysis_timeout_secs),
        )?;
        println!(
            "{}",
            serde_json::json!({"plugin":id,"success":true,"seconds":start.elapsed().as_secs_f64(),"recorded_audio":false})
        );
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--recover") {
        let mut config = talk_recorder::config::Config::load(&path)?;
        config.auto_recover = true;
        let report = talk_recorder::maintenance::run(&config, true)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        anyhow::ensure!(
            report.errors().next().is_none(),
            "復旧できない範囲があります。元データを保持しました"
        );
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--compact-wav") {
        let config = talk_recorder::config::Config::load(&path)?;
        let report =
            talk_recorder::compact::compact_recordings(&config.output_dir, &config.ffmpeg_path)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        anyhow::ensure!(report.errors.is_empty(), "圧縮できないWAVは保持しました");
        return Ok(());
    }
    let origin = match args
        .windows(2)
        .find(|pair| pair[0] == "--launch-origin")
        .map(|pair| pair[1].as_str())
    {
        Some("host_manual") => "host_manual",
        Some("via_kotorintoys") => "via_kotorintoys",
        Some("direct_task") => "direct_task",
        _ => "standalone",
    };
    talk_recorder::ui::run(path, args.iter().any(|arg| arg == "--startup"), origin)
}
