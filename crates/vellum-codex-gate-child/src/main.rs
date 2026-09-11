//! App Server child used by the Enhanced Runtime bridge integration gate.
//!
//! It is a test peer, not a product runtime: it speaks the real protocol and
//! runs the real portable Enhanced ports, so the gate can prove port behaviour
//! over the wire without a Codex fork build on every CI machine.
fn main() {
    if let Err(error) = vellum_lib::eval::enhanced_gate::child::run() {
        eprintln!("vellum-codex-gate-child: {error}");
        std::process::exit(1);
    }
}
