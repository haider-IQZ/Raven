use raven::{CompositorError, Result, state::Raven};
use smithay::reexports::{
    calloop::{
        EventLoop,
        timer::{TimeoutAction, Timer},
    },
    wayland_server::Display,
};
use std::{
    backtrace::Backtrace,
    fs,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

fn main() -> Result<()> {
    init_backtrace_defaults();
    init_logging()?;
    std::panic::set_hook(Box::new(|panic_info| {
        let backtrace = Backtrace::force_capture();
        tracing::error!("panic: {panic_info}\n{backtrace}");
        eprintln!("panic: {panic_info}\n{backtrace}");
    }));

    let args: Vec<String> = std::env::args().collect();
    if let Some(command) = args.get(1).map(String::as_str)
        && matches!(command, "clients" | "reload")
    {
        let output = run_ipc_command(command)?;
        print!("{output}");
        return Ok(());
    }

    let mut event_loop: EventLoop<Raven> =
        EventLoop::try_new().map_err(|e| CompositorError::EventLoop(e.to_string()))?;

    let display = Display::new().map_err(|e| CompositorError::Backend(e.to_string()))?;
    let mut state = Raven::new(display, event_loop.handle(), event_loop.get_signal())?;

    let force_winit = args.iter().any(|a| a == "--winit");
    let force_drm = args.iter().any(|a| a == "--drm" || a == "--tty");

    if force_winit && force_drm {
        return Err(CompositorError::Backend(
            "cannot pass both --winit and --drm/--tty".to_owned(),
        ));
    }

    if force_winit || (!force_drm && is_nested()) {
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

    // Present any redraws queued during backend initialization before entering the loop.
    raven::backend::udev::drain_queued_redraws(&mut state);

    event_loop
        .run(None, &mut state, |state| {
            // Refresh compositor state before rendering: clean up dead surfaces,
            // stale popup grabs, etc.
            state.space.refresh();
            state.popups.cleanup();

            // Keep pointer focus in sync even when client surface trees change without
            // pointer motion events (common with complex Xwayland clients like Steam).
            state.refresh_pointer_contents();

            raven::backend::udev::drain_queued_redraws(state);

            // Flush protocol messages to clients.  Without this, frame callbacks,
            // configure events, and other protocol traffic pile up and get burst-
            // delivered, causing clients like Brave to stutter.
            if let Err(err) = state.display_handle.flush_clients() {
                tracing::warn!("failed to flush clients: {err}");
            }
        })
        .map_err(|e| CompositorError::EventLoop(e.to_string()))?;

    Ok(())
}

fn run_ipc_command(command: &str) -> Result<String> {
    let socket_path = ipc_socket_path_from_env()?;
    let mut stream = UnixStream::connect(&socket_path).map_err(|err| {
        CompositorError::Backend(format!(
            "failed to connect to Raven ipc socket {}: {err}",
            socket_path.display()
        ))
    })?;

    stream
        .write_all(command.as_bytes())
        .map_err(|err| CompositorError::Backend(format!("failed to send ipc command: {err}")))?;
    stream.shutdown(std::net::Shutdown::Write).map_err(|err| {
        CompositorError::Backend(format!("failed to finalize ipc command write: {err}"))
    })?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|err| CompositorError::Backend(format!("failed to read ipc response: {err}")))?;

    if response.is_empty() {
        return Err(CompositorError::Backend(
            "empty response from compositor ipc".to_owned(),
        ));
    }

    Ok(response)
}

fn ipc_socket_path_from_env() -> Result<PathBuf> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| CompositorError::Backend("XDG_RUNTIME_DIR is not set".to_owned()))?;

    let runtime_dir_path = PathBuf::from(runtime_dir);
    if let Ok(wayland_display) = std::env::var("WAYLAND_DISPLAY") {
        let path = runtime_dir_path.join(format!("raven-{wayland_display}.sock"));
        if path.exists() {
            return Ok(path);
        }
    }

    let mut candidates = Vec::new();
    let entries = std::fs::read_dir(&runtime_dir_path).map_err(|err| {
        CompositorError::Backend(format!(
            "failed to scan runtime dir {}: {err}",
            runtime_dir_path.display()
        ))
    })?;

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if name.starts_with("raven-wayland-") && name.ends_with(".sock") {
            let candidate = runtime_dir_path.join(name.as_ref());
            if candidate.exists() {
                candidates.push(candidate);
            }
        }
    }

    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => Err(CompositorError::Backend(
            "Raven ipc socket not found (is Raven running?)".to_owned(),
        )),
        _ => Err(CompositorError::Backend(
            "multiple Raven sessions detected; set WAYLAND_DISPLAY to select one".to_owned(),
        )),
    }
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

const DEFAULT_LOG_FILTER: &str = concat!(
    "raven=debug,",
    "raven::backend::udev=trace,",
    "raven::handlers=debug,",
    "smithay::backend::drm=debug,",
    "smithay::backend::renderer::gles=error"
);

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
