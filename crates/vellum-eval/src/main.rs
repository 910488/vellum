#[tokio::main]
async fn main() {
    if let Err(error) = vellum_lib::eval::run_cli().await {
        eprintln!("vellum-eval: {error}");
        std::process::exit(1);
    }
}
