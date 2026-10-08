fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=../../data/icons/tidemark.ico");
        winresource::WindowsResource::new()
            .set_icon("../../data/icons/tidemark.ico")
            .set("ProductName", "Tidemark")
            .set("FileDescription", "Tidemark desktop client")
            .set("OriginalFilename", "tidemark.exe")
            .compile()
            .expect("the Windows icon and version resources compile");
    }
    let config = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    slint_build::compile_with_config("ui/app.slint", config).expect("the Slint markup compiles");

    // The Slint the About dialog's troubleshooting page names. Statically linked, so the
    // version the lockfile resolved is the one running; Slint exports no constant for it.
    let lock = "../../Cargo.lock";
    println!("cargo:rerun-if-changed={lock}");
    let version = std::fs::read_to_string(lock)
        .ok()
        .and_then(|lock| slint_version(&lock))
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=TIDEMARK_SLINT_VERSION={version}");
}

fn slint_version(lock: &str) -> Option<String> {
    let mut lines = lock.lines();
    while let Some(line) = lines.next() {
        if line == "name = \"slint\"" {
            let version = lines.next()?.strip_prefix("version = \"")?;
            return Some(version.trim_end_matches('"').to_owned());
        }
    }
    None
}
