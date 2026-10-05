//! Main window: video preview on the left, settings sidebar on the right.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use elgato4k_linux::{
    AudioInput, DeviceModel, EdidRangePolicy, EdidSource, HdrToneMapping, ReadValue, VideoScaler,
};
use gst::prelude::*;
use gtk::{gio, glib};

use crate::config::Config;
use crate::controls::{self, Setting};
use crate::media::{self, VideoSource};

const COLOR_RANGES: [EdidRangePolicy; 3] = [EdidRangePolicy::Auto, EdidRangePolicy::Expand, EdidRangePolicy::Shrink];
const EDID_SOURCES: [EdidSource; 3] = [EdidSource::Display, EdidSource::Merged, EdidSource::Internal];
const AUDIO_INPUTS: [AudioInput; 2] = [AudioInput::Embedded, AudioInput::Analog];

/// Shown when the card's settings cannot be opened due to USB permissions.
const UDEV_HELP: &str = "The capture card's USB control interface is only writable by root.\n\
Install the udev rule shipped with this app, then unplug and replug the card:\n\n\
sudo cp gui/data/70-elgato4k.rules /etc/udev/rules.d/\n\
sudo udevadm control --reload-rules\n\
sudo udevadm trigger";

const CSS: &str = "
.video-area { background-color: black; color: white; }
.video-area statuspage { background: none; }
.level-bar trough { min-height: 8px; }
.level-bar block.filled { background-color: @success_color; }
.level-bar block.empty { background-color: alpha(currentColor, 0.15); }
";

/// A running pipeline plus its bus watch; stopping is tied to drop.
struct Running {
    pipeline: gst::Pipeline,
    _watch: gst::bus::BusWatchGuard,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

struct Window {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    toolbar: adw::ToolbarView,
    split: adw::OverlaySplitView,
    picture: gtk::Picture,
    status_page: adw::StatusPage,
    retry_button: gtk::Button,
    play_button: gtk::Button,
    mute_button: gtk::ToggleButton,

    mode_row: adw::ComboRow,
    hw_row: adw::SwitchRow,
    volume: gtk::Adjustment,
    level_bar: gtk::LevelBar,
    audio_row: adw::ActionRow,

    permission_banner: adw::Banner,
    card_group: adw::PreferencesGroup,
    device_row: adw::ActionRow,
    hdr_row: adw::SwitchRow,
    range_row: adw::ComboRow,
    edid_row: adw::ComboRow,
    audio_input_row: adw::ComboRow,
    scaler_row: adw::SwitchRow,
    /// Set while we update rows from device state, so the change handlers
    /// do not echo the values back to the card.
    syncing: Cell<bool>,

    source: RefCell<Option<VideoSource>>,
    audio_device: RefCell<Option<gst::Device>>,
    video: RefCell<Option<Running>>,
    audio: RefCell<Option<Running>>,
    config: RefCell<Config>,
}

pub fn build(app: &adw::Application) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().expect("no display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let config = Config::load();
    let win = Rc::new(Window::new(app, config));
    win.connect_signals();
    win.install_actions();
    win.discover();
    win.refresh_card();
    win.window.present();

    // Debug aid: ELGATO4K_SCREENSHOT=/path.png saves the window after a few
    // seconds (GNOME on Wayland does not let other tools capture it).
    if let Some(path) = std::env::var_os("ELGATO4K_SCREENSHOT") {
        let (window, picture) = (win.window.clone(), win.picture.clone());
        glib::timeout_add_seconds_local_once(4, move || save_screenshot(&window, &picture, &path));
    }
}

fn save_screenshot(window: &adw::ApplicationWindow, picture: &gtk::Picture, path: &std::ffi::OsStr) {
    let Some(renderer) = window.native().and_then(|n| n.renderer()) else { eprintln!("screenshot: no renderer"); return };
    let snapshot = gtk::Snapshot::new();
    gtk::WidgetPaintable::new(Some(window)).snapshot(&snapshot, window.width() as f64, window.height() as f64);
    // Unmapped windows snapshot to nothing; fall back to the current video
    // frame at its native size, still rendered by GTK.
    let node = snapshot.to_node().or_else(|| {
        let frame = picture.paintable()?;
        let snapshot = gtk::Snapshot::new();
        frame.snapshot(&snapshot, frame.intrinsic_width() as f64, frame.intrinsic_height() as f64);
        snapshot.to_node()
    });
    let Some(node) = node else { eprintln!("screenshot: nothing to render"); return };
    if let Err(e) = renderer.render_texture(&node, None).save_to_png(path) {
        eprintln!("screenshot failed: {e}");
    }
}

impl Window {
    fn new(app: &adw::Application, config: Config) -> Self {
        // --- Video area ----------------------------------------------------
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .hexpand(true)
            .vexpand(true)
            .build();
        let retry_button = gtk::Button::builder()
            .label("Retry")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        let status_page = adw::StatusPage::builder()
            .icon_name("camera-video-symbolic")
            .title("Starting…")
            .child(&retry_button)
            .build();
        let video_area = gtk::Overlay::builder().child(&picture).css_classes(["video-area"]).build();
        video_area.add_overlay(&status_page);

        // --- Header bar ----------------------------------------------------
        let play_button = gtk::Button::builder()
            .icon_name("media-playback-stop-symbolic")
            .tooltip_text("Stop preview")
            .action_name("win.toggle-playback")
            .build();
        let mute_button = gtk::ToggleButton::builder()
            .icon_name("audio-volume-high-symbolic")
            .tooltip_text("Mute (M)")
            .build();
        let fullscreen_button = gtk::Button::builder()
            .icon_name("view-fullscreen-symbolic")
            .tooltip_text("Fullscreen (F11)")
            .action_name("win.fullscreen")
            .build();
        let sidebar_button = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-right-symbolic")
            .tooltip_text("Settings (F9)")
            .build();
        let header = adw::HeaderBar::new();
        header.pack_start(&play_button);
        header.pack_start(&mute_button);
        header.pack_end(&sidebar_button);
        header.pack_end(&fullscreen_button);

        // --- Sidebar: preview ----------------------------------------------
        let mode_row = adw::ComboRow::builder().title("Video mode").use_subtitle(true).build();
        let hw_row = adw::SwitchRow::builder()
            .title("Hardware decoding")
            .subtitle("Decode MJPEG with NVIDIA NVJPEG (mainly helps at 4K)")
            .active(config.hardware_decode)
            .visible(media::hardware_jpeg_available())
            .build();
        let preview_group = adw::PreferencesGroup::builder().title("Preview").build();
        preview_group.add(&mode_row);
        preview_group.add(&hw_row);

        // --- Sidebar: audio ------------------------------------------------
        let volume = gtk::Adjustment::new(config.volume, 0.0, 1.0, 0.01, 0.1, 0.0);
        let volume_scale = gtk::Scale::builder()
            .adjustment(&volume)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .width_request(150)
            .build();
        let volume_row = adw::ActionRow::builder().title("Volume").build();
        volume_row.add_suffix(&volume_scale);
        let level_bar = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .width_request(150)
            .css_classes(["level-bar"])
            .build();
        level_bar.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_LOW));
        level_bar.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_HIGH));
        level_bar.remove_offset_value(Some(gtk::LEVEL_BAR_OFFSET_FULL));
        let audio_row = adw::ActionRow::builder().title("Input level").build();
        audio_row.add_suffix(&level_bar);
        let audio_group = adw::PreferencesGroup::builder()
            .title("Audio")
            .description("Plays the card's audio on your default output device")
            .build();
        audio_group.add(&volume_row);
        audio_group.add(&audio_row);

        // --- Sidebar: card settings ----------------------------------------
        let refresh_button = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Re-read settings from the card")
            .valign(gtk::Align::Center)
            .action_name("win.refresh-card")
            .css_classes(["flat"])
            .build();
        let card_group = adw::PreferencesGroup::builder()
            .title("Capture card")
            .header_suffix(&refresh_button)
            .sensitive(false)
            .build();
        let device_row = adw::ActionRow::builder().title("Device").subtitle("Not connected").build();
        device_row.add_css_class("property");
        let hdr_row = adw::SwitchRow::builder()
            .title("HDR tone mapping")
            .subtitle("Convert HDR input to SDR")
            .build();
        let range_row = combo_row("HDMI color range", &["Auto", "Expand (limited → full)", "Shrink (full → limited)"]);
        let edid_row = combo_row("EDID source", &["Display", "Merged", "Internal"]);
        let audio_input_row = combo_row("Audio input", &["HDMI", "Analog (line in)"]);
        let scaler_row = adw::SwitchRow::builder().title("Video scaler").build();
        for row in [device_row.upcast_ref::<gtk::Widget>(), hdr_row.upcast_ref(), range_row.upcast_ref(),
                    edid_row.upcast_ref(), audio_input_row.upcast_ref(), scaler_row.upcast_ref()] {
            card_group.add(row);
        }

        let permission_banner = adw::Banner::builder()
            .title("No permission to change card settings")
            .button_label("How to fix")
            .build();

        let page = adw::PreferencesPage::new();
        page.add(&preview_group);
        page.add(&audio_group);
        page.add(&card_group);
        let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
        sidebar.append(&permission_banner);
        sidebar.append(&page);
        page.set_vexpand(true);

        let split = adw::OverlaySplitView::builder()
            .content(&video_area)
            .sidebar(&sidebar)
            .sidebar_position(gtk::PackType::End)
            .min_sidebar_width(340.0)
            .max_sidebar_width(400.0)
            .show_sidebar(config.show_sidebar)
            .build();
        split.bind_property("show-sidebar", &sidebar_button, "active")
            .bidirectional()
            .sync_create()
            .build();

        let toolbar = adw::ToolbarView::builder().content(&split).build();
        toolbar.add_top_bar(&header);
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&toolbar));

        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Elgato 4K Capture")
            .default_width(1400)
            .default_height(820)
            .content(&toasts)
            .build();

        // Collapse the sidebar into an overlay on narrow windows.
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            900.0,
            adw::LengthUnit::Sp,
        ));
        breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(breakpoint);

        mute_button.set_active(config.muted);

        Self {
            window, toasts, toolbar, split, picture, status_page, retry_button, play_button, mute_button,
            mode_row, hw_row, volume, level_bar, audio_row,
            permission_banner, card_group, device_row, hdr_row, range_row, edid_row, audio_input_row, scaler_row,
            syncing: Cell::new(false),
            source: RefCell::new(None),
            audio_device: RefCell::new(None),
            video: RefCell::new(None),
            audio: RefCell::new(None),
            config: RefCell::new(config),
        }
    }

    fn connect_signals(self: &Rc<Self>) {
        let this = self;

        this.retry_button.connect_clicked(glib::clone!(#[weak] this, move |_| this.discover()));

        this.mode_row.connect_selected_notify(glib::clone!(#[weak] this, move |row| {
            if this.syncing.get() {
                return;
            }
            let label = this.source.borrow().as_ref()
                .and_then(|s| s.modes.get(row.selected() as usize))
                .map(|m| m.label());
            this.config.borrow_mut().video_mode = label;
            this.save_config();
            this.start_video();
        }));

        this.hw_row.connect_active_notify(glib::clone!(#[weak] this, move |row| {
            this.config.borrow_mut().hardware_decode = row.is_active();
            this.save_config();
            this.start_video();
        }));

        this.volume.connect_value_changed(glib::clone!(#[weak] this, move |adj| {
            this.config.borrow_mut().volume = adj.value();
            this.apply_volume();
        }));
        // The volume is persisted on close rather than on every slider tick.

        this.mute_button.connect_toggled(glib::clone!(#[weak] this, move |button| {
            let muted = button.is_active();
            button.set_icon_name(if muted { "audio-volume-muted-symbolic" } else { "audio-volume-high-symbolic" });
            this.config.borrow_mut().muted = muted;
            this.save_config();
            this.apply_volume();
        }));

        this.split.connect_show_sidebar_notify(glib::clone!(#[weak] this, move |split| {
            if !this.window.is_fullscreen() {
                this.config.borrow_mut().show_sidebar = split.shows_sidebar();
                this.save_config();
            }
        }));

        this.window.connect_fullscreened_notify(glib::clone!(#[weak] this, move |window| {
            let fullscreen = window.is_fullscreen();
            this.toolbar.set_reveal_top_bars(!fullscreen);
            this.split.set_show_sidebar(!fullscreen && this.config.borrow().show_sidebar);
        }));

        let double_click = gtk::GestureClick::new();
        double_click.connect_pressed(glib::clone!(#[weak] this, move |_, n_press, _, _| {
            if n_press == 2 {
                let _ = WidgetExt::activate_action(&this.window, "win.fullscreen", None);
            }
        }));
        this.picture.add_controller(double_click);

        let escape = gtk::EventControllerKey::new();
        escape.connect_key_pressed(glib::clone!(#[weak] this, #[upgrade_or] glib::Propagation::Proceed, move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape && this.window.is_fullscreen() {
                this.window.unfullscreen();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }));
        this.window.add_controller(escape);

        this.permission_banner.connect_button_clicked(glib::clone!(#[weak] this, move |_| {
            let dialog = adw::AlertDialog::builder()
                .heading("Allow access to the capture card")
                .body(UDEV_HELP)
                .build();
            dialog.add_response("ok", "OK");
            dialog.present(Some(&this.window));
        }));

        // Card settings → device.
        this.hdr_row.connect_active_notify(glib::clone!(#[weak] this, move |row| {
            let v = if row.is_active() { HdrToneMapping::On } else { HdrToneMapping::Off };
            this.apply_setting(Setting::HdrToneMapping(v));
        }));
        this.range_row.connect_selected_notify(glib::clone!(#[weak] this, move |row| {
            if let Some(&v) = COLOR_RANGES.get(row.selected() as usize) {
                this.apply_setting(Setting::ColorRange(v));
            }
        }));
        this.edid_row.connect_selected_notify(glib::clone!(#[weak] this, move |row| {
            if let Some(&v) = EDID_SOURCES.get(row.selected() as usize) {
                this.apply_setting(Setting::EdidSource(v));
            }
        }));
        this.audio_input_row.connect_selected_notify(glib::clone!(#[weak] this, move |row| {
            if let Some(&v) = AUDIO_INPUTS.get(row.selected() as usize) {
                this.apply_setting(Setting::AudioInput(v));
            }
        }));
        this.scaler_row.connect_active_notify(glib::clone!(#[weak] this, move |row| {
            let v = if row.is_active() { VideoScaler::On } else { VideoScaler::Off };
            this.apply_setting(Setting::VideoScaler(v));
        }));

        // The only strong reference besides `build`'s: keeps the state alive
        // for as long as the window is open.
        this.window.connect_close_request(glib::clone!(#[strong] this, move |_| {
            this.save_config();
            this.video.replace(None);
            this.audio.replace(None);
            glib::Propagation::Proceed
        }));
    }

    fn install_actions(self: &Rc<Self>) {
        let this = self;

        let fullscreen = gio::SimpleAction::new("fullscreen", None);
        fullscreen.connect_activate(glib::clone!(#[weak] this, move |_, _| {
            this.window.set_fullscreened(!this.window.is_fullscreen());
        }));

        let playback = gio::SimpleAction::new("toggle-playback", None);
        playback.connect_activate(glib::clone!(#[weak] this, move |_, _| {
            if this.video.borrow().is_some() || this.audio.borrow().is_some() {
                this.stop("Preview stopped", "Press play to resume.");
            } else {
                this.discover();
            }
        }));

        let mute = gio::SimpleAction::new("toggle-mute", None);
        mute.connect_activate(glib::clone!(#[weak] this, move |_, _| {
            this.mute_button.set_active(!this.mute_button.is_active());
        }));

        let sidebar = gio::SimpleAction::new("toggle-sidebar", None);
        sidebar.connect_activate(glib::clone!(#[weak] this, move |_, _| {
            this.split.set_show_sidebar(!this.split.shows_sidebar());
        }));

        let refresh = gio::SimpleAction::new("refresh-card", None);
        refresh.connect_activate(glib::clone!(#[weak] this, move |_, _| this.refresh_card()));

        for action in [fullscreen, playback, mute, sidebar, refresh] {
            this.window.add_action(&action);
        }
    }

    // -----------------------------------------------------------------------
    // Capture
    // -----------------------------------------------------------------------

    /// Find the card's video and audio devices and start both pipelines.
    fn discover(self: &Rc<Self>) {
        self.load_video_source();
        let audio_device = media::find_audio_device();

        self.audio_row.set_subtitle(
            &audio_device.as_ref().map(|d| gst::prelude::DeviceExt::display_name(d).to_string()).unwrap_or_else(|| "No audio device found".into()),
        );
        eprintln!(
            "video device: {}, audio device: {}",
            self.source.borrow().as_ref().map_or("none", |s| s.path.as_str()),
            audio_device.as_ref().map_or_else(|| "none".into(), |d| gst::prelude::DeviceExt::display_name(d).to_string()),
        );
        self.audio_device.replace(audio_device);

        if self.source.borrow().is_none() && self.audio_device.borrow().is_none() {
            self.stop("No capture card found", "Connect your Elgato 4K S or 4K X and press Retry.");
            return;
        }
        self.start_video();
        self.start_audio();
    }

    /// Find the video node and fill the mode list, keeping the user's mode
    /// when the card still offers it.
    fn load_video_source(&self) {
        let source = media::find_video_source();

        self.syncing.set(true);
        match &source {
            Some(source) => {
                let labels: Vec<String> = source.modes.iter().map(|m| m.label()).collect();
                let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
                self.mode_row.set_model(Some(&gtk::StringList::new(&labels)));
                let wanted = self.config.borrow().video_mode.clone();
                let selected = wanted
                    .and_then(|w| source.modes.iter().position(|m| m.label() == w))
                    .unwrap_or_else(|| media::default_mode(&source.modes));
                self.mode_row.set_selected(selected as u32);
                self.mode_row.set_sensitive(true);
            }
            None => {
                self.mode_row.set_model(None::<&gio::ListModel>);
                self.mode_row.set_sensitive(false);
            }
        }
        self.syncing.set(false);
        self.source.replace(source);
    }

    /// The scaler and EDID settings change which modes the card offers (it
    /// mirrors the HDMI input's timing), so re-probe once the card settles.
    fn reload_video_after_signal_change(self: &Rc<Self>) {
        self.video.replace(None);
        self.show_status("camera-video-symbolic", "Waiting for the card…", "");
        self.retry_button.set_visible(false);
        glib::timeout_add_local_once(
            std::time::Duration::from_millis(2000),
            glib::clone!(#[weak(rename_to = this)] self, move || {
                this.load_video_source();
                this.start_video();
            }),
        );
    }

    fn start_video(self: &Rc<Self>) {
        self.video.replace(None);

        let source = self.source.borrow();
        let Some(source) = source.as_ref() else {
            self.show_status("dialog-warning-symbolic", "No video device found", "The card's audio is still playing.");
            return;
        };
        let Some(mode) = source.modes.get(self.mode_row.selected() as usize) else { return };

        let result = media::VideoSink::new().and_then(|sink| {
            self.picture.set_paintable(Some(&sink.paintable));
            let hw = self.hw_row.is_visible() && self.hw_row.is_active();
            media::build_video_pipeline(&source.path, mode, &sink.element, hw)
        });
        let pipeline = match result {
            Ok(p) => p,
            Err(e) => {
                self.show_status("dialog-error-symbolic", "Could not build the video pipeline", &e.to_string());
                return;
            }
        };

        let watch = pipeline.bus().expect("pipeline has a bus").add_watch_local(glib::clone!(
            #[weak(rename_to = this)] self,
            #[upgrade_or] glib::ControlFlow::Break,
            move |_, msg| {
                this.on_video_message(msg);
                glib::ControlFlow::Continue
            }
        ));
        let Ok(watch) = watch else { return };

        self.show_status("camera-video-symbolic", "Starting…", "");
        self.retry_button.set_visible(false);
        if let Err(e) = pipeline.set_state(gst::State::Playing) {
            eprintln!("video pipeline failed to start: {e}");
        }
        self.video.replace(Some(Running { pipeline, _watch: watch }));
        self.update_play_button();
    }

    fn on_video_message(self: &Rc<Self>, msg: &gst::Message) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Error(err) => {
                let text = err.error().to_string();
                let debug = err.debug().map(|d| d.to_string()).unwrap_or_default();
                eprintln!("video error: {text}\n{debug}");
                self.video.replace(None);
                if text.contains("busy") || debug.contains("busy") {
                    self.show_status(
                        "dialog-warning-symbolic",
                        "Capture card is in use",
                        "Another app (OBS?) is using the video device. Close it and press Retry.\nAudio keeps playing.",
                    );
                } else {
                    self.show_status("dialog-error-symbolic", "Video stopped", &text);
                }
                self.update_play_button();
            }
            MessageView::StateChanged(change) => {
                let is_pipeline = msg.src().is_some_and(|s| s.type_() == gst::Pipeline::static_type());
                if is_pipeline && change.current() == gst::State::Playing {
                    self.status_page.set_visible(false);
                }
            }
            _ => {}
        }
    }

    fn start_audio(self: &Rc<Self>) {
        self.audio.replace(None);
        let device = self.audio_device.borrow();
        let Some(device) = device.as_ref() else { return };

        let pipeline = match media::build_audio_pipeline(device) {
            Ok(p) => p,
            Err(e) => {
                self.toast(&format!("Audio unavailable: {e}"));
                return;
            }
        };
        let watch = pipeline.bus().expect("pipeline has a bus").add_watch_local(glib::clone!(
            #[weak(rename_to = this)] self,
            #[upgrade_or] glib::ControlFlow::Break,
            move |_, msg| {
                this.on_audio_message(msg);
                glib::ControlFlow::Continue
            }
        ));
        let Ok(watch) = watch else { return };

        let _ = pipeline.set_state(gst::State::Playing);
        self.audio.replace(Some(Running { pipeline, _watch: watch }));
        self.apply_volume();
        self.update_play_button();
    }

    fn on_audio_message(self: &Rc<Self>, msg: &gst::Message) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Element(element) => {
                if let Some(db) = element.structure().and_then(media::level_peak_db) {
                    // Map -60 dBFS..0 dBFS onto the bar.
                    self.level_bar.set_value(((db + 60.0) / 60.0).clamp(0.0, 1.0));
                }
            }
            MessageView::Error(err) => {
                eprintln!("audio error: {} {:?}", err.error(), err.debug());
                self.audio.replace(None);
                self.level_bar.set_value(0.0);
                self.toast(&format!("Audio stopped: {}", err.error()));
                self.update_play_button();
            }
            _ => {}
        }
    }

    fn apply_volume(&self) {
        let audio = self.audio.borrow();
        let Some(audio) = audio.as_ref() else { return };
        let Some(volume) = audio.pipeline.by_name(media::VOLUME_ELEMENT) else { return };
        // A cubic curve makes the slider feel linear to the ear.
        let gain = self.volume.value().powi(3);
        volume.set_property("volume", gain);
        volume.set_property("mute", self.mute_button.is_active());
    }

    fn stop(&self, title: &str, description: &str) {
        self.video.replace(None);
        self.audio.replace(None);
        self.level_bar.set_value(0.0);
        self.picture.set_paintable(None::<&gtk::gdk::Paintable>);
        self.show_status("media-playback-stop-symbolic", title, description);
        self.update_play_button();
    }

    fn show_status(&self, icon: &str, title: &str, description: &str) {
        self.status_page.set_icon_name(Some(icon));
        self.status_page.set_title(title);
        self.status_page.set_description(Some(description).filter(|d| !d.is_empty()));
        self.status_page.set_visible(true);
        self.retry_button.set_visible(true);
    }

    fn update_play_button(&self) {
        let running = self.video.borrow().is_some() || self.audio.borrow().is_some();
        self.play_button.set_icon_name(if running { "media-playback-stop-symbolic" } else { "media-playback-start-symbolic" });
        self.play_button.set_tooltip_text(Some(if running { "Stop preview" } else { "Start preview" }));
    }

    // -----------------------------------------------------------------------
    // Card settings
    // -----------------------------------------------------------------------

    fn refresh_card(self: &Rc<Self>) {
        let this = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(controls::read).await.expect("device thread panicked");
            match result {
                Ok(snapshot) => {
                    this.permission_banner.set_revealed(false);
                    this.show_snapshot(&snapshot);
                }
                Err(e) => {
                    this.card_group.set_sensitive(false);
                    this.permission_banner.set_revealed(controls::is_permission_error(&e));
                    this.device_row.set_subtitle(&e.to_string());
                }
            }
        });
    }

    fn show_snapshot(&self, snapshot: &controls::Snapshot) {
        let status = &snapshot.status;
        let is_4ks = snapshot.model == DeviceModel::Elgato4KS;

        // The 4K S answers every HID read with the same status block, so the
        // per-setting values decoded from it are meaningless.  Only show what
        // the card reports on the 4K X; on the 4K S the rows are write-only.
        if is_4ks {
            self.device_row.set_subtitle("Elgato 4K S");
            self.card_group.set_description(Some(
                "The 4K S can't report its current settings, so these show defaults until you change them.",
            ));
            self.audio_input_row.set_visible(true);
            self.scaler_row.set_visible(true);
            self.card_group.set_sensitive(true);
            return;
        }

        self.syncing.set(true);
        self.device_row.set_subtitle(&format!("Elgato {} · firmware {}", snapshot.model, status.firmware_version));
        if let Some(ReadValue::Known(v)) = status.hdr_tone_mapping {
            self.hdr_row.set_active(v == HdrToneMapping::On);
        }
        if let Some(ReadValue::Known(v)) = status.hdmi_color_range {
            set_combo(&self.range_row, &COLOR_RANGES, v);
        }
        if let Some(ReadValue::Known(v)) = status.edid_source {
            set_combo(&self.edid_row, &EDID_SOURCES, v);
        }
        if let Some(ReadValue::Known(v)) = status.audio_input {
            set_combo(&self.audio_input_row, &AUDIO_INPUTS, v);
        }
        if let Some(ReadValue::Known(v)) = status.video_scaler {
            self.scaler_row.set_active(v == VideoScaler::On);
        }
        self.syncing.set(false);

        self.audio_input_row.set_visible(is_4ks);
        self.scaler_row.set_visible(is_4ks);
        self.card_group.set_sensitive(true);
    }

    fn apply_setting(self: &Rc<Self>, setting: Setting) {
        if self.syncing.get() {
            return;
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            this.card_group.set_sensitive(false);
            let result = gio::spawn_blocking(move || controls::apply(setting)).await.expect("device thread panicked");
            this.card_group.set_sensitive(true);
            match result {
                Ok(()) => {
                    if matches!(setting, Setting::VideoScaler(_) | Setting::EdidSource(_)) {
                        this.reload_video_after_signal_change();
                    }
                }
                Err(e) => {
                    this.permission_banner.set_revealed(controls::is_permission_error(&e));
                    this.toast(&format!("Could not apply setting: {e}"));
                    // Show what the card actually has now.
                    this.refresh_card();
                }
            }
        });
    }

    fn toast(&self, text: &str) {
        eprintln!("{text}");
        self.toasts.add_toast(adw::Toast::new(text));
    }

    fn save_config(&self) {
        self.config.borrow().save();
    }
}

fn combo_row(title: &str, items: &[&str]) -> adw::ComboRow {
    adw::ComboRow::builder().title(title).model(&gtk::StringList::new(items)).build()
}

fn set_combo<T: PartialEq>(row: &adw::ComboRow, values: &[T], value: T) {
    if let Some(i) = values.iter().position(|v| *v == value) {
        row.set_selected(i as u32);
    }
}
