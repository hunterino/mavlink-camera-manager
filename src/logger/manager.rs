use crate::cli;

use tracing::{metadata::LevelFilter, *};
use tracing_log::LogTracer;
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter, Layer};

// Start logger, should be done inside main
pub fn init() {
    // Redirect all logs from libs using "Log"
    LogTracer::init_with_filter(tracing::log::LevelFilter::Trace).expect("Failed to set logger");

    // Configure the console log
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if cli::manager::is_verbose() {
            EnvFilter::new(LevelFilter::DEBUG.to_string())
        } else {
            EnvFilter::new(LevelFilter::INFO.to_string())
        }
    });
    let console_layer = fmt::Layer::new()
        .with_writer(std::io::stdout)
        .with_ansi(true)
        .with_file(true)
        .with_line_number(true)
        .with_span_events(fmt::format::FmtSpan::NONE)
        .with_target(false)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_filter(env_filter);

    // Configure file log
    let dir = cli::manager::log_path();
    let file_appender = tracing_appender::rolling::daily(dir, "mavlink-camera-manager.log");
    let file_layer = fmt::Layer::new()
        .with_writer(file_appender)
        .with_ansi(false)
        .with_file(true)
        .with_line_number(true)
        .with_span_events(fmt::format::FmtSpan::NONE)
        .with_target(false)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_filter(LevelFilter::DEBUG);

    // Configure the default subscriber
    let subscriber = tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer);
    tracing::subscriber::set_global_default(subscriber).expect("Unable to set a global subscriber");

    info!(
        "{}, version: {}-{}, build date: {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        env!("VERGEN_GIT_SHA_SHORT"),
        env!("VERGEN_BUILD_DATE")
    );
    info!(
        "Starting at {}",
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S"),
    );
    debug!("Command line call: {}", cli::manager::command_line_string());
    debug!(
        "Command line input struct call: {:#?}",
        cli::manager::matches().args
    );
}
