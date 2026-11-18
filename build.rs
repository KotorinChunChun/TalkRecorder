fn main() {
    println!("cargo:rerun-if-changed=packaging/version.rc");
    println!("cargo:rerun-if-changed=packaging/cli-version.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let major = std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap();
        let minor = std::env::var("CARGO_PKG_VERSION_MINOR").unwrap();
        let patch = std::env::var("CARGO_PKG_VERSION_PATCH").unwrap();
        let version = format!("APP_VERSION={major},{minor},{patch},0");
        let text = format!("APP_VERSION_TEXT=\"{major}.{minor}.{patch}\"");
        embed_resource::compile_for("packaging/version.rc", ["TalkRecorder"], [&version, &text])
            .manifest_required()
            .expect("Windows の製品名と版を埋め込めません");
        embed_resource::compile_for(
            "packaging/cli-version.rc",
            ["TalkRecorder-cli"],
            [&version, &text],
        )
        .manifest_required()
        .expect("CLI の製品名と版を埋め込めません");
    }
}
