mod capture_command;

fn print_help() {
    println!(
        "TalkRecorder {}\n\
         --record --config PATH [--stop-after-secs 秒]\n\
         --preview --config PATH [--stop-after-secs 秒]\n\
         --list-devices\n\
         --write-default-config / --self-check / --check-storage / --recover / --compact-wav --config PATH\n\
         --check-denoise PLUGIN_ID --config PATH\n\
         入力確認と録音は標準入力の stop または Ctrl+C で正常停止します。",
        talk_recorder::APP_VERSION
    );
}

fn check_capture_options(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.iter().map(String::as_str);
    let mut seen = std::collections::HashSet::new();
    while let Some(flag) = args.next() {
        anyhow::ensure!(seen.insert(flag), "オプションが重複しています: {flag}");
        match flag {
            "--record" | "--preview" => {}
            "--config" | "--stop-after-secs" => {
                anyhow::ensure!(
                    args.next().is_some_and(|value| !value.starts_with("--")),
                    "オプションの値を指定してください: {flag}"
                );
            }
            _ => anyhow::bail!("録音や入力確認に使用できないオプションです: {flag}"),
        }
    }
    Ok(())
}

pub fn run(console: bool) -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|arg| arg == "--speech-worker") {
        return talk_recorder::speech::run_worker();
    }
    if console && (args.len() == 1 || args.iter().any(|arg| arg == "--help")) {
        print_help();
        return Ok(());
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
    let specified_path = args
        .windows(2)
        .find(|p| p[0] == "--config")
        .map(|p| {
            if console {
                std::path::absolute(&p[1])
            } else {
                Ok(std::path::PathBuf::from(&p[1]))
            }
        })
        .transpose()?;
    std::env::set_current_dir(base)?;
    let stop_after = if let Some(index) = args.iter().position(|arg| arg == "--stop-after-secs") {
        let seconds: f64 = args
            .get(index + 1)
            .ok_or_else(|| anyhow::anyhow!("停止までの秒数を指定してください"))?
            .parse()?;
        anyhow::ensure!(
            seconds.is_finite() && (1.0..=3600.0).contains(&seconds),
            "停止までの秒数は1〜3600秒で指定してください"
        );
        Some(seconds)
    } else {
        None
    };
    let record = args.iter().any(|arg| arg == "--record");
    let preview = args.iter().any(|arg| arg == "--preview");
    if record || preview {
        anyhow::ensure!(
            console,
            "録音や入力確認は TalkRecorder-cli.exe で実行してください"
        );
        anyhow::ensure!(
            !(record && preview),
            "--record と --preview はどちらか一方を指定してください"
        );
        check_capture_options(&args[1..])?;
        let path = specified_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("--config で使用する設定ファイルを指定してください"))?;
        return capture_command::run(path, preview, stop_after);
    }
    let path = specified_path.unwrap_or_else(talk_recorder::ui::config_path_default);
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
    anyhow::ensure!(!console, "利用できるコマンドは --help で確認してください");
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
