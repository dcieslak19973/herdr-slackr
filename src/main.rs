fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();

    if args.get(1).map(String::as_str) == Some("--version") {
        println!("herdr-slackr {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }

    if std::env::var_os("HERDR_PLUGIN_CONFIG_DIR").is_none() {
        eprintln!("herdr-slackr: the pane must run inside herdr (set HERDR_PLUGIN_CONFIG_DIR)");
        return std::process::ExitCode::FAILURE;
    }

    if let Err(error) = herdr_slackr::run() {
        eprintln!("herdr-slackr: {error:#}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
