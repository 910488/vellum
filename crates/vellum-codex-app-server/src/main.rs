fn main() {
    if let Err(error) = vellum_lib::enhanced_runtime::app_server_bridge::run_from_env() {
        eprintln!("vellum-codex-app-server: {error}");
        std::process::exit(1);
    }
}
