// Windows release 版不開主控台視窗
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// The App Server bridge used to live behind a mode flag on this binary
// (`VELLUM_CODEX_APP_SERVER_BRIDGE=1`). It is now the packaged
// `vellum-codex-app-server` sidecar, because `CODEX_CLI_PATH` has to name a
// real standalone executable: Codex Desktop starts it directly, and a flag on
// the Desktop binary only works when whoever starts Codex also happens to be
// Vellum.
fn main() {
    vellum_lib::run()
}
