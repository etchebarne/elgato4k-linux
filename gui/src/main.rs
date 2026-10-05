//! Elgato 4K Capture — live preview, audio monitoring and card settings.

mod config;
mod controls;
mod media;
mod window;

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "io.github.etchebarne.Elgato4kCapture";

fn main() -> glib::ExitCode {
    // GTK's default Vulkan renderer has to copy every GL video frame the
    // GStreamer sink hands it, which costs a CPU core at 60 fps.  The GL
    // renderer takes them as-is.
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: no other threads exist yet.
        unsafe { std::env::set_var("GSK_RENDERER", "gl") };
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
    app.run()
}
