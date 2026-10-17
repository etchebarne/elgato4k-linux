//! Elgato 4K Capture — live preview, audio monitoring and card settings.

mod config;
mod controls;
mod framegen;
mod media;
mod window;

use std::ffi::OsString;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "io.github.etchebarne.Elgato4kCapture";

static RESTART: AtomicBool = AtomicBool::new(false);

/// The environment variables `main` overrides, as the app was started with.
static INHERITED_ENV: OnceLock<Vec<(&str, Option<OsString>)>> = OnceLock::new();

/// Relaunch the app once it quits, e.g. to switch GTK renderers.
pub fn request_restart() {
    RESTART.store(true, Ordering::Relaxed);
}

/// End the process right away, relaunching it if that was requested.
///
/// With lsfg-vk in the swapchain, GTK's renderer teardown can wait forever
/// for a present that never completes (seen when closing the window).  The
/// process then lingers without a window and swallows every new launch, so
/// skip the teardown once the window's own cleanup has run.
pub fn exit_now() -> ! {
    if RESTART.load(Ordering::Relaxed) {
        relaunch();
    }
    std::process::exit(0)
}

fn relaunch() {
    use std::os::unix::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else { return };
    let mut command = std::process::Command::new(exe);
    command.args(std::env::args_os().skip(1));
    for (key, value) in INHERITED_ENV.get().into_iter().flatten() {
        match value {
            Some(value) => command.env(key, value),
            None => command.env_remove(key),
        };
    }
    let e = command.exec();
    eprintln!("failed to restart: {e}");
}

fn main() -> glib::ExitCode {
    INHERITED_ENV.get_or_init(|| {
        ["GSK_RENDERER", "LSFG_PROCESS", "DISABLE_LSFG"].into_iter().map(|k| (k, std::env::var_os(k))).collect()
    });

    // GTK's default Vulkan renderer has to copy every GL video frame the
    // GStreamer sink hands it, which costs a CPU core at 60 fps.  The GL
    // renderer takes them as-is, so only use Vulkan when frame generation
    // needs it (lsfg-vk is a Vulkan layer).
    let settings = config::Config::load().frame_gen;
    let available = framegen::unavailable_reason().is_none();
    if available {
        // lsfg-vk reads its profile when the window's swapchain is created.
        framegen::write_profile(&settings);
    }
    let frame_gen = settings.enabled && available;
    // SAFETY: no other threads exist yet.
    unsafe {
        if std::env::var_os("GSK_RENDERER").is_none() {
            std::env::set_var("GSK_RENDERER", if frame_gen { "vulkan" } else { "gl" });
        }
        std::env::set_var("LSFG_PROCESS", framegen::PROFILE);
        // GTK creates a Vulkan device even with the GL renderer; keep the
        // layer (and Lossless.dll) out of it when it can't do anything.
        if !frame_gen {
            std::env::set_var("DISABLE_LSFG", "1");
        }
    }

    if let Err(e) = gst::init() {
        eprintln!("Failed to initialize GStreamer: {e}");
        return glib::ExitCode::FAILURE;
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(window::build);
    app.set_accels_for_action("win.fullscreen", &["F11"]);
    app.set_accels_for_action("win.toggle-mute", &["m"]);
    app.set_accels_for_action("win.toggle-sidebar", &["F9"]);
    let code = app.run();

    if RESTART.load(Ordering::Relaxed) {
        relaunch();
    }
    code
}
