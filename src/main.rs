use raven::{CompositorError, Result, state::Raven};
use smithay::reexports::{
    calloop::{
        EventLoop,
        timer::{TimeoutAction, Timer},
    },
    wayland_server::Display,
};
use std::{backtrace::Backtrace, fs, path::PathBuf};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

fn main() -> Result<()> {
    init_backtrace_defaults();
    init_logging()?;
    std::panic::set_hook(Box::new(|panic_info| {
        let backtrace = Backtrace::force_capture();
        tracing::error!("panic: {panic_info}\n{backtrace}");
        eprintln!("panic: {panic_info}\n{backtrace}");
    }));

    let mut event_loop: EventLoop<Raven> =
        EventLoop::try_new().map_err(|e| CompositorError::EventLoop(e.to_string()))?;

    let display = Display::new().map_err(|e| CompositorError::Backend(e.to_string()))?;
    let mut state = Raven::new(display, event_loop.handle(), event_loop.get_signal())?;

    let args: Vec<String> = std::env::args().collect();
    let force_winit = args.iter().any(|a| a == "--winit");

    if force_winit || is_nested() {
        tracing::info!("Starting with Winit backend");
        raven::backend::winit::init_winit(&mut event_loop, &mut state)?;
    } else {
        tracing::info!("Starting with DRM/KMS backend");
        raven::backend::udev::init_udev(&mut event_loop, &mut state)?;
    }

    event_loop
        .handle()
        .insert_source(
            Timer::from_duration(std::time::Duration::from_millis(700)),
            |_, _, state| {
                state.run_startup_tasks();
                TimeoutAction::Drop
            },
        )
        .map_err(|err| {
            CompositorError::EventLoop(format!("failed to schedule startup tasks: {err}"))
        })?;

    // Spawn a command if provided (skip --winit flag)
    let spawn_cmd = args.iter().skip(1).find(|a| !a.starts_with("--"));
    if let Some(cmd) = spawn_cmd {
        state.spawn_command(cmd);
    }

    event_loop
        .run(None, &mut state, |_| {})
        .map_err(|e| CompositorError::EventLoop(e.to_string()))?;

    Ok(())
}

fn init_backtrace_defaults() {
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        // Safety: called at startup before creating any threads.
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }
    if std::env::var_os("RUST_LIB_BACKTRACE").is_none() {
        // Safety: called at startup before creating any threads.
        unsafe { std::env::set_var("RUST_LIB_BACKTRACE", "0") };
    }
}

/// Check if we're running inside an existing display server
fn is_nested() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok() || std::env::var("DISPLAY").is_ok()
}

const DEFAULT_LOG_FILTER: &str = "raven=debug,smithay::backend::renderer::gles=error";

fn init_logging() -> Result<()> {
    let log_dir: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("log");
    fs::create_dir_all(&log_dir).map_err(|err| {
        CompositorError::Backend(format!(
            "failed to create log directory {}: {err}",
            log_dir.display()
        ))
    })?;

    let file_appender = tracing_appender::rolling::never(&log_dir, "raven.log");
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(true)
                .with_writer(std::io::stderr),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file_appender),
        )
        .init();

    let log_file = log_dir.join("raven.log");
    tracing::info!(path = %log_file.display(), "logging initialized");

    Ok(())
}
