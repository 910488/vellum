//! `vellum-app-server`: the Harness Manager behind a Codex App Server surface.
//!
//! Speaks newline-delimited JSON-RPC on stdio. stdout carries protocol only;
//! everything diagnostic goes to stderr, so a client can attach directly to
//! this process.
//!
//! ```text
//! vellum-app-server --harness grok-build=<path to grok> [--harness ...]
//!                   [--bindings <sqlite path>]
//! ```
//!
//! Each `--harness` names an installed native harness and where to find it. A
//! harness that is not passed is simply not offered; the facade refuses a
//! thread that selects it rather than substituting another one.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use vellum_codex_facade::{serve, CodexAppServerFacade, ManagedHarnessBroker};
use vellum_harness_protocol::HarnessId;
use vellum_harness_runtime::adapters::{
    deepseek::DeepSeekHarnessDriver, grok::GrokBuildDriver, qwen::QwenCodeDriver,
    zcode_desktop::ZcodeDesktopDriver,
};
use vellum_harness_runtime::{
    HarnessBindingStore, HarnessDriver, HarnessRegistry, NativeHarnessSupervisor,
};

struct Options {
    harnesses: BTreeMap<String, PathBuf>,
    bindings: PathBuf,
}

fn default_bindings_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Vellum")
        .join("harness-bindings.sqlite")
}

fn parse_options() -> Result<Options, String> {
    let mut harnesses = BTreeMap::new();
    let mut bindings = default_bindings_path();
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--harness" => {
                let value = args
                    .next()
                    .ok_or("--harness needs <id>=<executable path>")?;
                if value == HarnessId::ZCODE_DESKTOP {
                    harnesses.insert(value, PathBuf::new());
                } else {
                    let (id, path) = value
                        .split_once('=')
                        .ok_or_else(|| format!("--harness expects <id>=<path>, got `{value}`"))?;
                    harnesses.insert(id.to_owned(), PathBuf::from(path));
                }
            }
            "--bindings" => {
                bindings = PathBuf::from(args.next().ok_or("--bindings needs a path")?);
            }
            "--help" | "-h" => return Err(usage()),
            other => return Err(format!("unrecognized argument `{other}`\n\n{}", usage())),
        }
    }
    if harnesses.is_empty() {
        return Err(format!("at least one --harness is required\n\n{}", usage()));
    }
    Ok(Options {
        harnesses,
        bindings,
    })
}

fn usage() -> String {
    format!(
        "vellum-app-server --harness <id>=<path> [--harness ...] [--bindings <sqlite path>]\n\n\
         ZCode Desktop uses: --harness {}\n\
         Known executable harness ids: {}, {}, {}",
        HarnessId::ZCODE_DESKTOP,
        HarnessId::GROK_BUILD,
        HarnessId::QWEN_CODE,
        HarnessId::DEEPSEEK_HARNESS
    )
}

fn build_registry(options: &Options) -> Result<HarnessRegistry, String> {
    let supervisor = Arc::new(NativeHarnessSupervisor::default());
    let mut registry = HarnessRegistry::default();
    for (id, executable) in &options.harnesses {
        let driver: Arc<dyn HarnessDriver> = match id.as_str() {
            HarnessId::GROK_BUILD => Arc::new(GrokBuildDriver::new(
                executable.clone(),
                Arc::clone(&supervisor),
            )),
            HarnessId::QWEN_CODE => Arc::new(QwenCodeDriver::new(
                executable.clone(),
                Arc::clone(&supervisor),
            )),
            HarnessId::DEEPSEEK_HARNESS => Arc::new(DeepSeekHarnessDriver::new(
                executable.clone(),
                Arc::clone(&supervisor),
            )),
            HarnessId::ZCODE_DESKTOP => Arc::new(ZcodeDesktopDriver::from_env()),
            other => return Err(format!("unknown harness id `{other}`\n\n{}", usage())),
        };
        registry
            .register(driver)
            .map_err(|error| error.to_string())?;
    }
    Ok(registry)
}

#[tokio::main]
async fn main() -> ExitCode {
    let options = match parse_options() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    let registry = match build_registry(&options) {
        Ok(registry) => registry,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(parent) = options.bindings.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            eprintln!("cannot create binding store directory: {error}");
            return ExitCode::FAILURE;
        }
    }
    let bindings = match HarnessBindingStore::open(&options.bindings) {
        Ok(bindings) => Arc::new(bindings),
        Err(error) => {
            eprintln!("cannot open binding store: {error}");
            return ExitCode::FAILURE;
        }
    };

    for id in options.harnesses.keys() {
        eprintln!("vellum-app-server: offering harness {id}");
    }

    let facade = Arc::new(CodexAppServerFacade::new(Arc::new(
        ManagedHarnessBroker::new(Arc::new(registry), bindings),
    )));

    match serve(facade, tokio::io::stdin(), tokio::io::stdout()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("vellum-app-server: transport ended: {error}");
            ExitCode::FAILURE
        }
    }
}
