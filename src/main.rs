//! Command line entry point.

use std::collections::BTreeSet;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use tracing::Level;
use tracing_subscriber::fmt::time::ChronoLocal;

use opencode_bridge::app::create_app;
use opencode_bridge::config::{
    default_config_path, load_config, load_or_bootstrap, public_base_url, Bootstrap, ConfigStore,
    UPSTREAM_KEY_ENV,
};
use opencode_bridge::daemon;
use opencode_bridge::keygen::{generate_api_key_with_prefix, DEFAULT_KEY_PREFIX};
use opencode_bridge::proxy::UpstreamRelay;
use opencode_bridge::registry::{all_wires, parse_protocol_selection, Wire};
use opencode_bridge::{APP, VERSION};

/// How long a graceful shutdown may take before the process is forced out,
/// matching uvicorn's `timeout_graceful_shutdown`.
const GRACEFUL_SHUTDOWN_SECONDS: u64 = 3;

#[derive(Parser)]
#[command(name = APP, version = VERSION, about = "Kimi Code -> OpenCode Go API bridge")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the bridge in the foreground (the default when no subcommand is given).
    Serve(ServeArgs),
    /// Start the bridge in the background (daemon mode).
    Start(StartArgs),
    /// Stop a background bridge started with `start`.
    Stop(ConfigArg),
    /// Show whether a background bridge is running.
    Status(ConfigArg),
    /// Restart a background bridge (stop, then start).
    Restart(StartArgs),
    /// Generate bridge API keys in the format the config expects.
    GenKey(GenKeyArgs),
}

impl Default for Command {
    fn default() -> Self {
        Command::Serve(ServeArgs::default())
    }
}

#[derive(Args)]
struct ServeArgs {
    /// Path to the YAML config file (default: $OPENCODE_BRIDGE_CONFIG, else
    /// ~/.config/ocbk/config.yaml)
    #[arg(short, long, value_name = "PATH")]
    config: Option<String>,
    /// Host (or host:port, or a full http(s) URL) published in the registry.
    /// Defaults to the host in `listen`; a wildcard listen publishes
    /// 127.0.0.1.
    #[arg(long, value_name = "HOST")]
    host: Option<String>,
    #[arg(
        long,
        value_name = "LEVEL",
        default_value = "info",
        value_parser = ["debug", "info", "warning", "error"]
    )]
    log_level: String,
    /// Protocols published by /registry/api.json: a comma-separated subset of
    /// chat, responses, anthropic, or all (default: all)
    #[arg(long, value_name = "LIST", value_parser = parse_protocols)]
    registry_protocols: Option<BTreeSet<Wire>>,
    /// Written by `start`: the daemon records its pid here once it is bound.
    #[arg(long, value_name = "PATH", hide = true)]
    pid_file: Option<String>,
}

impl Default for ServeArgs {
    fn default() -> Self {
        Self {
            config: None,
            host: None,
            log_level: "info".to_string(),
            registry_protocols: None,
            pid_file: None,
        }
    }
}

#[derive(Args)]
struct StartArgs {
    #[command(flatten)]
    serve: ServeArgs,
}

#[derive(Args)]
struct ConfigArg {
    /// Path to the YAML config file (default: $OPENCODE_BRIDGE_CONFIG, else
    /// ~/.config/ocbk/config.yaml)
    #[arg(short, long, value_name = "PATH")]
    config: Option<String>,
}

#[derive(Args)]
struct GenKeyArgs {
    /// Number of keys to generate
    #[arg(value_name = "COUNT", default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    count: u32,
    /// Key prefix
    #[arg(long, value_name = "PREFIX", default_value = DEFAULT_KEY_PREFIX)]
    prefix: String,
    /// Emit a JSON array instead of one key per line
    #[arg(long)]
    json: bool,
}

/// clap needs the parser's output type to match the field's inner type, so
/// `all` is expanded here and the `None` case stays reserved for "no flag".
fn parse_protocols(value: &str) -> Result<BTreeSet<Wire>, String> {
    parse_protocol_selection(value).map(|selection| selection.unwrap_or_else(all_wires))
}

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().command.unwrap_or_default() {
        Command::GenKey(args) => run_gen_key(&args),
        Command::Serve(args) => run_serve(args).await,
        Command::Start(args) => run_start(&args.serve),
        Command::Stop(args) => daemon::stop(&resolve_config_path(args.config)),
        Command::Status(args) => daemon::status(&resolve_config_path(args.config)),
        Command::Restart(args) => {
            let config = resolve_config_path(args.serve.config.clone());
            let code = daemon::stop(&config);
            if code == ExitCode::SUCCESS {
                daemon::start(&config, &serve_extra_args(&args.serve))
            } else {
                code
            }
        }
    }
}

/// `-c` flag, else $OPENCODE_BRIDGE_CONFIG, else `~/.config/ocbk/config.yaml`.
fn resolve_config_path(flag: Option<String>) -> String {
    flag.or_else(|| std::env::var("OPENCODE_BRIDGE_CONFIG").ok())
        .filter(|path| !path.is_empty())
        .unwrap_or_else(default_config_path)
}

/// Flags the daemonized `serve` child needs beyond `--config`/`--pid-file`.
fn serve_extra_args(args: &ServeArgs) -> Vec<String> {
    let mut extra = Vec::new();
    if let Some(host) = &args.host {
        extra.push("--host".to_string());
        extra.push(host.clone());
    }
    extra.push("--log-level".to_string());
    extra.push(args.log_level.clone());
    if let Some(selected) = &args.registry_protocols {
        let mut wires: Vec<&str> = selected.iter().map(|wire| wire.as_str()).collect();
        wires.sort_unstable();
        extra.push("--registry-protocols".to_string());
        extra.push(wires.join(","));
    }
    extra
}

fn run_start(args: &ServeArgs) -> ExitCode {
    let config = resolve_config_path(args.config.clone());
    daemon::start(&config, &serve_extra_args(args))
}

fn run_gen_key(args: &GenKeyArgs) -> ExitCode {
    let keys: Vec<String> = (0..args.count)
        .map(|_| generate_api_key_with_prefix(&args.prefix))
        .collect();
    if args.json {
        println!(
            "{}",
            serde_json::to_string(&keys).unwrap_or_else(|_| "[]".to_string())
        );
    } else {
        for key in keys {
            println!("{key}");
        }
    }
    ExitCode::SUCCESS
}

async fn run_serve(args: ServeArgs) -> ExitCode {
    init_logging(&args.log_level);

    let config_path = resolve_config_path(args.config);

    let config = match load_or_bootstrap(&config_path) {
        Ok(Bootstrap::Ready(config)) => config,
        Ok(Bootstrap::Created(config)) => {
            let key = config
                .proxy_api_keys
                .first()
                .map(String::as_str)
                .unwrap_or("<none>");
            eprintln!(
                "created new config {config_path}\n\
                 1. set 'upstream_api_key' to your OpenCode Go API key\n\
                 2. restart the bridge\n\
                 the published bridge API key is: {key}"
            );
            return ExitCode::from(2);
        }
        Err(error) => {
            eprintln!("config error: {error}");
            return ExitCode::from(2);
        }
    };

    // Fixed at startup: `listen` cannot move without a restart, so the
    // published URL must not drift from the socket it describes either.
    let public = match public_base_url(&config.listen, args.host.as_deref()) {
        Ok(public) => public,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };

    let keys = if config.proxy_api_keys.is_empty() {
        vec!["<none>".to_string()]
    } else {
        config.proxy_api_keys.clone()
    };
    let continuation = " ".repeat("  published api keys: ".len());
    let protocols = match &args.registry_protocols {
        None => "all (chat, responses, anthropic)".to_string(),
        Some(selected) => {
            let mut wires: Vec<&str> = selected.iter().map(|wire| wire.as_str()).collect();
            wires.sort_unstable();
            wires.join(", ")
        }
    };
    let upstream_source =
        if std::env::var(UPSTREAM_KEY_ENV).is_ok_and(|value| !value.trim().is_empty()) {
            format!("{UPSTREAM_KEY_ENV} (environment override)")
        } else {
            "the config file".to_string()
        };

    println!(
        "{APP} v{VERSION}\n\
         \x20 listening         : http://{listen}\n\
         \x20 upstream          : {upstream}\n\
         \x20 upstream key      : {upstream_source}\n\
         \x20 published api keys: {keys}\n\
         \x20 client env var    : export {env}={first}\n\
         \x20 registry          : kimi provider add {public}/registry/api.json\n\
         \x20 registry protocols: {protocols}\n\
         \x20 config            : {path} (SIGHUP to reload)",
        listen = config.listen,
        upstream = config.upstream_base_url,
        keys = keys.join(&format!("\n{continuation}")),
        env = config.api_key_env_name,
        first = keys[0],
        public = public,
        path = config_path,
    );

    let store = ConfigStore::new(config.clone());
    let relay = Arc::new(UpstreamRelay::new(store.clone()));
    if let Err(error) = relay.start(config.upstream_timeout_seconds) {
        eprintln!("cannot build the upstream HTTP client: {error}");
        return ExitCode::from(1);
    }

    install_reload_handler(store.clone(), relay.clone(), config_path.clone());

    let (host, port) = config.listen_addr();
    let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot listen on {}: {error}", config.listen);
            return ExitCode::from(1);
        }
    };

    // Written only after the bind succeeds, so `start` can treat its
    // appearance as the readiness signal.
    if let Some(path) = &args.pid_file {
        if let Err(error) = std::fs::write(path, format!("{}\n", std::process::id())) {
            eprintln!("cannot write pid file {path}: {error}");
            return ExitCode::from(1);
        }
    }

    let app = create_app(store, relay, args.registry_protocols, public);
    let code = match axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("server error: {error}");
            ExitCode::from(1)
        }
    };
    if let Some(path) = &args.pid_file {
        let _ = std::fs::remove_file(path);
    }
    code
}

fn install_reload_handler(store: ConfigStore, relay: Arc<UpstreamRelay>, path: String) {
    tokio::spawn(async move {
        let mut hangup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        {
            Ok(signal) => signal,
            Err(error) => {
                tracing::warn!("cannot install the SIGHUP handler: {error}");
                return;
            }
        };
        while hangup.recv().await.is_some() {
            match load_config(&path) {
                Ok(config) => {
                    // reqwest has no per-request idle timeout, so a changed
                    // read timeout means a new connection pool.
                    if let Err(error) = relay.apply_read_timeout(config.upstream_timeout_seconds) {
                        tracing::error!("cannot rebuild the relay client: {error}");
                    }
                    store.replace(config);
                    tracing::info!("config reloaded from {path}");
                }
                Err(error) => {
                    tracing::error!("config reload failed ({error}); keeping previous config");
                }
            }
        }
    });
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(GRACEFUL_SHUTDOWN_SECONDS)).await;
        std::process::exit(0);
    });
}

/// Local wall-clock timestamps, in the `%(asctime)s` shape the Python
/// original logs with.
const LOG_TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S,%3f";

fn init_logging(level: &str) {
    let level = match level {
        "debug" => Level::DEBUG,
        "warning" => Level::WARN,
        "error" => Level::ERROR,
        _ => Level::INFO,
    };
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_timer(ChronoLocal::new(LOG_TIME_FORMAT.to_string()))
        .with_writer(std::io::stderr)
        .with_target(true)
        .init();
}
