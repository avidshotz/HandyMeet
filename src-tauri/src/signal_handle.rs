use crate::TranscriptionCoordinator;
#[cfg(unix)]
use log::debug;
use log::warn;
use tauri::{AppHandle, Manager};

#[cfg(target_os = "macos")]
use signal_hook::consts::SIGUSR1;
#[cfg(unix)]
use signal_hook::consts::{SIGINT, SIGTERM, SIGUSR2};
#[cfg(unix)]
use signal_hook::iterator::Signals;
#[cfg(unix)]
use std::thread;

/// Send a transcription input to the coordinator.
/// Used by signal handlers, CLI flags, and any other external trigger.
pub fn send_transcription_input(app: &AppHandle, binding_id: &str, source: &str) {
    if let Some(c) = app.try_state::<TranscriptionCoordinator>() {
        c.send_external_input(binding_id, source);
    } else {
        warn!("TranscriptionCoordinator not initialized");
    }
}

/// Listen for Unix signals that remotely toggle transcription.
///
/// SIGUSR2 toggles plain transcription on all Unix platforms. SIGUSR1
/// (transcription with post-processing) is only handled on macOS: on Linux,
/// WebKitGTK's JavaScriptCore garbage collector sends SIGUSR1 to its own
/// threads to suspend them, so handling it caused phantom recordings on every
/// GC cycle (#1660). Linux users should use `handy --toggle-post-process`
/// instead.
#[cfg(unix)]
pub fn setup_signal_handler(app_handle: AppHandle) {
    #[cfg(target_os = "macos")]
    let mut signals =
        Signals::new([SIGUSR1, SIGUSR2]).expect("failed to register transcription signal handlers");
    #[cfg(not(target_os = "macos"))]
    let mut signals =
        Signals::new([SIGUSR2]).expect("failed to register transcription signal handlers");
    #[cfg(target_os = "macos")]
    debug!("Signal handlers registered (SIGUSR1, SIGUSR2)");
    #[cfg(not(target_os = "macos"))]
    debug!("Signal handler registered (SIGUSR2; SIGUSR1 is left to WebKitGTK)");
    thread::spawn(move || {
        for sig in signals.forever() {
            let (binding_id, signal_name) = match sig {
                #[cfg(target_os = "macos")]
                SIGUSR1 => ("transcribe_with_post_process", "SIGUSR1"),
                SIGUSR2 => ("transcribe", "SIGUSR2"),
                _ => continue,
            };
            debug!("Received {signal_name}");
            send_transcription_input(&app_handle, binding_id, signal_name);
        }
    });
}

/// Listen for SIGTERM/SIGINT and shut down through Tauri's normal exit path
/// (`AppHandle::exit`, which fires `RunEvent::Exit` — stops an in-progress
/// meeting cleanly, unloads the transcription model) instead of letting the
/// OS's default disposition just kill the process outright.
///
/// This matters regardless of how the process was started: a plain
/// `kill <pid>` / `killall handy` / Activity Monitor "Quit" all send SIGTERM,
/// and none of those go through `tauri_plugin_single_instance` (that path
/// only works between two instances the OS itself recognizes as "the same
/// app" via LaunchServices, i.e. both launched through `open`/Finder/Dock —
/// it does nothing for a directly-run binary, as `--quit` still does via
/// the single-instance plugin for that specific case).
#[cfg(unix)]
pub fn setup_termination_handler(app_handle: AppHandle) {
    let mut signals =
        Signals::new([SIGTERM, SIGINT]).expect("failed to register termination signal handlers");
    debug!("Termination signal handlers registered (SIGTERM, SIGINT)");
    thread::spawn(move || {
        if let Some(sig) = signals.forever().next() {
            let name = if sig == SIGTERM { "SIGTERM" } else { "SIGINT" };
            log::info!("Received {name}, shutting down cleanly");
            app_handle.exit(0);
        }
    });
}
