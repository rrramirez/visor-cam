//! The application controller: tray, recorder panel, device previews, the
//! recording session (countdown → recording → stop) and preferences.
//!
//! Everything here runs on the UI thread. Slow work (starting capture,
//! opening the camera, finalizing files) runs on worker threads that report
//! back with `slint::invoke_from_event_loop`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use slint::{
    ComponentHandle, LogicalPosition, LogicalSize, ModelRc, SharedString, Timer, TimerMode,
    VecModel,
};
use visor_capture::camera::{self, CameraDevice, CameraStream};
use visor_capture::mic::{self, MicDevice, MicStream};
use visor_capture::{Backend, DisplayInfo, Shareable};
use visor_core::project::BubbleShape;
use visor_core::{AudioConfig, CameraOverlay, CaptureTarget, RecordingConfig, Rect};
use visor_recorder::{Recorder, RecorderOptions, new_project_dir};

use crate::editor::EditorWindow;
use crate::platform::{self, Overlay};
use crate::settings::Settings;
use crate::tray::{Action, Tray};
use crate::util;
use crate::{
    AreaSelector, CameraBubble, ControlBar, Countdown, Preferences, RecorderPanel, ScreenHighlight,
};

thread_local! {
    static APP: RefCell<Option<Rc<App>>> = const { RefCell::new(None) };
}

/// The app singleton. Panics if called before [`App::install`].
pub fn app() -> Rc<App> {
    APP.with(|a| a.borrow().clone().expect("app not installed"))
}

/// Runs `f` on the UI thread with the app, from any thread.
pub fn post(f: impl FnOnce(&App) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || f(&app()));
}

const BUBBLE_SIZES: [f32; 3] = [160.0, 220.0, 300.0];

pub struct App {
    backend: Arc<dyn Backend>,
    pub settings: RefCell<Settings>,
    tray: RefCell<Option<Tray>>,
    panel: RefCell<Option<Panel>>,
    devices: RefCell<Devices>,
    session: RefCell<Session>,
    editors: RefCell<Vec<Rc<EditorWindow>>>,
    prefs: RefCell<Option<Preferences>>,
    area_window: RefCell<Option<AreaSelector>>,
    /// Border around the display chosen in the panel, with several displays.
    screen_highlight: RefCell<Option<Highlight>>,
    next_start_id: std::cell::Cell<u64>,
    /// Quit once the recording being finalized is saved.
    quit_pending: std::cell::Cell<bool>,
}

struct Highlight {
    display: u32,
    ui: ScreenHighlight,
    /// Keeps it over the whole display (see `update_screen_highlight`).
    _keep_placed: Timer,
}

struct Panel {
    ui: RecorderPanel,
    sources: Shareable,
    cameras: Vec<CameraDevice>,
    mics: Vec<MicDevice>,
    /// Chosen capture area: display id and global logical rect.
    area: Option<(u32, Rect)>,
    _meter: Timer,
}

#[derive(Default)]
struct Devices {
    mic: Option<(String, Arc<MicStream>)>,
    camera: Option<(String, Arc<CameraStream>)>,
    /// Camera id being opened in the background.
    camera_pending: Option<String>,
    bubble: Option<Bubble>,
}

struct Bubble {
    ui: CameraBubble,
    _timer: Timer,
    size_step: usize,
}

#[derive(Default)]
enum Session {
    #[default]
    Idle,
    Starting {
        /// Identifies this start so a late result from an abandoned one is ignored.
        id: u64,
        countdown: Option<(Countdown, Timer)>,
        remaining: u32,
        recorder: Option<Recorder>,
        display: DisplayInfo,
        /// What is being captured, in global logical coordinates, when that
        /// is a fixed region (a display or an area).
        captured: Option<Rect>,
    },
    Recording {
        recorder: Recorder,
        bar: ControlBar,
        captured: Option<Rect>,
        _timer: Timer,
    },
    Stopping,
}

impl App {
    pub fn install(backend: Arc<dyn Backend>) -> Rc<App> {
        let app = Rc::new(App {
            backend,
            settings: RefCell::new(Settings::load()),
            tray: RefCell::default(),
            panel: RefCell::default(),
            devices: RefCell::default(),
            session: RefCell::default(),
            editors: RefCell::default(),
            prefs: RefCell::default(),
            area_window: RefCell::default(),
            screen_highlight: RefCell::default(),
            next_start_id: std::cell::Cell::new(1),
            quit_pending: std::cell::Cell::new(false),
        });
        APP.with(|a| *a.borrow_mut() = Some(app.clone()));
        app
    }

    // ---- tray -------------------------------------------------------------

    /// Recovers recordings left unfinished by a crash, in the background.
    /// Call at launch, before a recording can start: one in progress would
    /// look unfinished too.
    pub fn recover_interrupted(&self) {
        let root = self.settings.borrow().save_dir.clone();
        let dirs = visor_media::recover::find_interrupted(&root);
        if dirs.is_empty() {
            return;
        }
        std::thread::spawn(move || {
            for dir in dirs {
                if let Err(e) = visor_media::recover::recover(&dir) {
                    tracing::warn!("could not recover {}: {e:#}", dir.display());
                }
            }
            post(|app| app.refresh_tray());
        });
    }

    pub fn create_tray(&self) {
        match Tray::new(&self.recent()) {
            Ok(tray) => *self.tray.borrow_mut() = Some(tray),
            Err(e) => tracing::error!("could not create the menu bar icon: {e:#}"),
        }
        tray_icon::menu::MenuEvent::set_event_handler(Some(|event: tray_icon::menu::MenuEvent| {
            post(move |app| {
                let action = app.tray.borrow().as_ref().and_then(|t| t.action(&event));
                if let Some(action) = action {
                    app.on_tray(action);
                }
            });
        }));
    }

    fn on_tray(&self, action: Action) {
        match action {
            Action::NewRecording => self.open_panel(),
            Action::TogglePause => self.toggle_pause(),
            Action::StopRecording => self.stop_recording(false),
            Action::OpenRecent(dir) => self.open_editor(dir),
            Action::OpenFolder => platform::open_folder(&self.settings.borrow().save_dir),
            Action::Preferences => self.open_preferences(),
            Action::Quit => self.quit(),
        }
    }

    fn recent(&self) -> Vec<(String, PathBuf)> {
        let root = self.settings.borrow().save_dir.clone();
        visor_core::project::list_projects(&root)
            .unwrap_or_default()
            .into_iter()
            .take(8)
            .map(|dir| {
                let title = visor_core::Project::load(&dir)
                    .map(|p| p.title)
                    .unwrap_or_else(|_| {
                        dir.file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    });
                (title, dir)
            })
            .collect()
    }

    fn refresh_tray(&self) {
        let recent = self.recent();
        let (recording, paused) = match &*self.session.borrow() {
            Session::Recording { recorder, .. } => (true, recorder.is_paused()),
            Session::Starting { .. } | Session::Stopping => (true, false),
            Session::Idle => (false, false),
        };
        if let Some(tray) = &mut *self.tray.borrow_mut() {
            tray.set_recent(&recent);
            tray.set_recording(recording, paused);
        }
    }

    fn quit(&self) {
        if matches!(*self.session.borrow(), Session::Stopping) {
            // A recording is being finalized; quit once it's saved.
            self.quit_pending.set(true);
            return;
        }
        let session = std::mem::take(&mut *self.session.borrow_mut());
        match session {
            // Finish the recording rather than losing it.
            Session::Recording { recorder, .. } => {
                if let Err(e) = recorder.stop() {
                    tracing::error!("saving the recording on quit failed: {e:#}");
                }
            }
            // Nothing was recorded yet.
            Session::Starting {
                recorder: Some(recorder),
                ..
            } => {
                let _ = recorder.discard();
            }
            _ => {}
        }
        let _ = slint::quit_event_loop();
    }

    // ---- recorder panel ---------------------------------------------------

    pub fn open_panel(&self) {
        if !matches!(*self.session.borrow(), Session::Idle) {
            return;
        }
        let existing = self.panel.borrow().as_ref().map(|p| p.ui.clone_strong());
        if let Some(ui) = existing {
            // Devices are released after each recording and when the panel
            // closes; reopen the chosen ones.
            self.refresh_sources();
            self.restore_device_choices();
            ui.set_busy(false);
            let _ = ui.show();
            platform::activate();
            self.update_screen_highlight();
            return;
        }
        let ui = match RecorderPanel::new() {
            Ok(ui) => ui,
            Err(e) => return tracing::error!("could not create the recorder panel: {e}"),
        };
        self.wire_panel(&ui);
        let meter = Timer::default();
        {
            let weak = ui.as_weak();
            meter.start(TimerMode::Repeated, Duration::from_millis(60), move || {
                let Some(ui) = weak.upgrade() else { return };
                let level = app()
                    .devices
                    .borrow()
                    .mic
                    .as_ref()
                    .map_or(0.0, |(_, m)| m.level());
                ui.set_mic_level(level.max(ui.get_mic_level() * 0.8));
            });
        }
        *self.panel.borrow_mut() = Some(Panel {
            ui: ui.clone_strong(),
            sources: Shareable::default(),
            cameras: Vec::new(),
            mics: Vec::new(),
            area: None,
            _meter: meter,
        });
        self.refresh_sources();
        self.restore_device_choices();
        self.update_hint();

        // Top-right of the main display, under the menu bar.
        if let Some(d) = self
            .panel
            .borrow()
            .as_ref()
            .and_then(|p| p.sources.displays.first().cloned())
        {
            ui.window().set_position(LogicalPosition::new(
                (d.frame.x + d.frame.width - 380.0 - 16.0) as f32,
                (d.frame.y + 38.0) as f32,
            ));
        }
        let _ = ui.show();
        platform::activate();
        self.update_screen_highlight();
    }

    /// Outlines the display chosen in the panel while it is open in Screen
    /// mode, so it's clear which one will be recorded. Only with several
    /// displays; hidden otherwise.
    fn update_screen_highlight(&self) {
        let target = {
            let panel = self.panel.borrow();
            panel.as_ref().and_then(|p| {
                let choosing = p.ui.window().is_visible()
                    && p.ui.get_mode() == 0
                    && matches!(*self.session.borrow(), Session::Idle)
                    && p.sources.displays.len() > 1;
                choosing
                    .then(|| usize::try_from(p.ui.get_display_index()).ok())
                    .flatten()
                    .and_then(|i| p.sources.displays.get(i).cloned())
            })
        };
        let current = self.screen_highlight.borrow().as_ref().map(|h| h.display);
        if target.as_ref().map(|d| d.id) == current {
            return;
        }
        // One window per display: moving a window between displays with
        // different scale factors leaves it mis-sized and unpainted.
        if let Some(old) = self.screen_highlight.borrow_mut().take() {
            let _ = old.ui.hide();
        }
        let Some(d) = target else { return };
        let Ok(ui) = ScreenHighlight::new() else {
            return;
        };
        ui.set_display_name(d.name.clone().into());
        ui.set_display_width(d.frame.width as f32);
        ui.set_display_height(d.frame.height as f32);
        // Created on its display, so it has that display's scale.
        ui.window()
            .set_position(LogicalPosition::new(d.frame.x as f32, d.frame.y as f32));
        let _ = ui.show();
        platform::configure_overlay(&ui, Overlay::Shield);
        // Keep keyboard focus in the panel.
        if let Some(p) = &*self.panel.borrow() {
            platform::focus(p.ui.window());
        }
        // The native window only appears once the event loop runs, and
        // AppKit and Slint then nudge it below the menu bar. Keep putting it
        // over the whole display (a no-op once it is there) while shown.
        let timer = Timer::default();
        {
            let (weak, frame) = (ui.as_weak(), d.frame);
            timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
                if let Some(ui) = weak.upgrade() {
                    platform::set_frame(ui.window(), frame);
                    platform::set_click_through(ui.window());
                }
            });
        }
        *self.screen_highlight.borrow_mut() = Some(Highlight {
            display: d.id,
            ui,
            _keep_placed: timer,
        });
    }

    fn wire_panel(&self, ui: &RecorderPanel) {
        ui.on_mode_changed(|mode| {
            let app = app();
            if mode == 3
                && app
                    .panel
                    .borrow()
                    .as_ref()
                    .is_some_and(|p| p.area.is_none())
            {
                app.select_area();
            }
            app.update_hint();
            app.update_screen_highlight();
        });
        ui.on_display_changed(|_| app().update_screen_highlight());
        ui.on_select_area(|| app().select_area());
        ui.on_refresh(|| {
            let app = app();
            app.refresh_sources();
            app.restore_device_choices();
            app.update_screen_highlight();
        });
        ui.on_camera_changed(|i| app().choose_camera(i as usize));
        ui.on_mic_changed(|i| app().choose_mic(i as usize));
        ui.on_start(|| app().start_recording());
        ui.on_open_preferences(|| app().open_preferences());
        ui.on_open_folder(|| platform::open_folder(&app().settings.borrow().save_dir));
        ui.on_open_permission_settings(|| {
            #[cfg(target_os = "macos")]
            {
                use visor_capture::macos_permissions::{Kind, open_settings, request};
                request(Kind::ScreenRecording, |_| {});
                open_settings(Kind::ScreenRecording);
            }
        });
        ui.window().on_close_requested(|| {
            // Closing the panel releases the camera and mic.
            let app = app();
            if matches!(*app.session.borrow(), Session::Idle) {
                app.release_devices();
            }
            // Runs after the panel is hidden.
            slint::Timer::single_shot(Duration::ZERO, || {
                crate::app::app().update_screen_highlight()
            });
            slint::CloseRequestResponse::HideWindow
        });
    }

    fn refresh_sources(&self) {
        let sources = match self.backend.shareable() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("listing capture sources failed: {e:#}");
                Shareable::default()
            }
        };
        let cameras = camera::list();
        let mics = mic::list();
        let mut panel = self.panel.borrow_mut();
        let Some(panel) = panel.as_mut() else { return };
        let ui = &panel.ui;
        ui.set_displays(strings(sources.displays.iter().map(|d| {
            format!(
                "{} — {}×{}",
                d.name, d.frame.width as u32, d.frame.height as u32
            )
        })));
        ui.set_windows(strings(
            sources
                .windows
                .iter()
                .map(|w| format!("{} — {}", w.app, w.title)),
        ));
        ui.set_apps(strings(sources.apps.iter().map(|a| a.name.clone())));
        ui.set_audio_apps(strings(
            std::iter::once("All apps".to_string())
                .chain(sources.apps.iter().map(|a| format!("Only {}", a.name))),
        ));
        ui.set_cameras(strings(
            std::iter::once("No camera".to_string()).chain(cameras.iter().map(|c| c.name.clone())),
        ));
        ui.set_mics(strings(
            std::iter::once("No microphone".to_string()).chain(mics.iter().map(|m| m.name.clone())),
        ));
        clamp_index(ui.get_display_index(), sources.displays.len(), |i| {
            ui.set_display_index(i)
        });
        clamp_index(ui.get_window_index(), sources.windows.len(), |i| {
            ui.set_window_index(i)
        });
        clamp_index(ui.get_app_index(), sources.apps.len(), |i| {
            ui.set_app_index(i)
        });
        clamp_index(ui.get_audio_app_index(), sources.apps.len() + 1, |i| {
            ui.set_audio_app_index(i)
        });

        #[cfg(target_os = "macos")]
        {
            use visor_capture::macos_permissions::{Kind, Status, status};
            let granted = status(Kind::ScreenRecording) == Status::Granted;
            ui.set_screen_permission(granted);
            ui.set_permission_note(
                "Visor needs Screen Recording permission. Grant it in System Settings, then restart Visor.".into(),
            );
        }
        panel.sources = sources;
        panel.cameras = cameras;
        panel.mics = mics;
    }

    fn restore_device_choices(&self) {
        let (last_camera, last_mic, system_audio) = {
            let s = self.settings.borrow();
            (s.last_camera.clone(), s.last_mic.clone(), s.system_audio)
        };
        let (camera_index, mic_index) = {
            let panel = self.panel.borrow();
            let Some(p) = panel.as_ref() else { return };
            p.ui.set_system_audio(system_audio);
            let cam = last_camera
                .and_then(|id| p.cameras.iter().position(|c| c.id == id))
                .map_or(0, |i| i + 1);
            let mic = last_mic
                .and_then(|id| p.mics.iter().position(|m| m.id == id))
                .map_or(0, |i| i + 1);
            p.ui.set_camera_index(cam as i32);
            p.ui.set_mic_index(mic as i32);
            (cam, mic)
        };
        self.choose_camera(camera_index);
        self.choose_mic(mic_index);
    }

    fn update_hint(&self) {
        let secs = self.settings.borrow().countdown_secs;
        if let Some(p) = &*self.panel.borrow() {
            let hint = if secs > 0 {
                format!("Recording starts after a {secs}-second countdown")
            } else {
                String::new()
            };
            p.ui.set_hint(hint.into());
        }
    }

    fn set_panel_error(&self, error: &str) {
        if let Some(p) = &*self.panel.borrow() {
            p.ui.set_error(error.into());
        }
    }

    // ---- devices ----------------------------------------------------------

    fn choose_mic(&self, index: usize) {
        let id = self.panel.borrow().as_ref().and_then(|p| {
            index
                .checked_sub(1)
                .and_then(|i| p.mics.get(i))
                .map(|m| m.id.clone())
        });
        self.settings.borrow_mut().last_mic = id.clone();
        self.settings.borrow().save();
        let Some(id) = id else {
            self.devices.borrow_mut().mic = None;
            return;
        };
        if self
            .devices
            .borrow()
            .mic
            .as_ref()
            .is_some_and(|(cur, _)| *cur == id)
        {
            return;
        }
        if !self.ensure_permission(Permission::Microphone, move |app| app.choose_mic(index)) {
            return;
        }
        match MicStream::open(Some(&id)) {
            Ok(stream) => {
                self.devices.borrow_mut().mic = Some((id, Arc::new(stream)));
                self.set_panel_error("");
            }
            Err(e) => {
                self.devices.borrow_mut().mic = None;
                self.set_panel_error(&format!("Microphone unavailable: {e:#}"));
            }
        }
    }

    fn choose_camera(&self, index: usize) {
        let id = self.panel.borrow().as_ref().and_then(|p| {
            index
                .checked_sub(1)
                .and_then(|i| p.cameras.get(i))
                .map(|c| c.id.clone())
        });
        self.settings.borrow_mut().last_camera = id.clone();
        self.settings.borrow().save();
        let Some(id) = id else {
            self.close_bubble();
            let mut d = self.devices.borrow_mut();
            d.camera = None;
            d.camera_pending = None;
            return;
        };
        if self
            .devices
            .borrow()
            .camera
            .as_ref()
            .is_some_and(|(cur, _)| *cur == id)
        {
            return;
        }
        if !self.ensure_permission(Permission::Camera, move |app| app.choose_camera(index)) {
            return;
        }
        self.devices.borrow_mut().camera_pending = Some(id.clone());
        std::thread::spawn(move || {
            let result = CameraStream::open(&id);
            post(move |app| app.on_camera_opened(id, result));
        });
    }

    fn on_camera_opened(&self, id: String, result: anyhow::Result<CameraStream>) {
        if self.devices.borrow().camera_pending.as_deref() != Some(id.as_str())
            || !matches!(*self.session.borrow(), Session::Idle)
        {
            return; // A different camera was chosen, or recording began, meanwhile.
        }
        self.devices.borrow_mut().camera_pending = None;
        match result {
            Ok(stream) => {
                let stream = Arc::new(stream);
                self.devices.borrow_mut().camera = Some((id, stream.clone()));
                self.show_bubble(&stream);
                self.set_panel_error("");
            }
            Err(e) => {
                self.close_bubble();
                self.devices.borrow_mut().camera = None;
                self.set_panel_error(&format!("Camera unavailable: {e:#}"));
            }
        }
    }

    fn show_bubble(&self, stream: &Arc<CameraStream>) {
        self.close_bubble();
        let Ok(ui) = CameraBubble::new() else { return };
        let rx = stream.subscribe();
        let size_step = 1;
        let size = BUBBLE_SIZES[size_step];
        ui.window().set_size(LogicalSize::new(size, size));
        if let Some(d) = self.main_display() {
            ui.window().set_position(LogicalPosition::new(
                (d.frame.x + 32.0) as f32,
                (d.frame.y + d.frame.height - f64::from(size) - 72.0) as f32,
            ));
        }
        let timer = Timer::default();
        {
            let weak = ui.as_weak();
            timer.start(TimerMode::Repeated, Duration::from_millis(33), move || {
                let Some(ui) = weak.upgrade() else { return };
                let Some(frame) = rx.try_iter().last() else {
                    return;
                };
                let px = ui.window().size().width.clamp(2, 600);
                if let Some(buf) = util::nv12_square_preview(&frame, px) {
                    ui.set_frame(slint::Image::from_rgba8(buf));
                }
            });
        }
        ui.on_drag_by(|dx, dy| {
            if let Some(b) = &app().devices.borrow().bubble {
                move_window(b.ui.window(), dx, dy);
            }
        });
        ui.on_cycle_size(|| {
            let app = app();
            let mut devices = app.devices.borrow_mut();
            if let Some(b) = &mut devices.bubble {
                b.size_step = (b.size_step + 1) % BUBBLE_SIZES.len();
                let s = BUBBLE_SIZES[b.size_step];
                b.ui.window().set_size(LogicalSize::new(s, s));
            }
        });
        ui.on_cycle_shape(|| {
            if let Some(b) = &app().devices.borrow().bubble {
                b.ui.set_shape(1 - b.ui.get_shape());
            }
        });
        let _ = ui.show();
        platform::configure_overlay(&ui, Overlay::Floating);
        self.devices.borrow_mut().bubble = Some(Bubble {
            ui,
            _timer: timer,
            size_step,
        });
    }

    /// The camera bubble's on-screen placement relative to `captured`, as the
    /// project's overlay. `None` without a bubble or when it lies outside.
    fn bubble_overlay(&self, captured: Rect) -> Option<CameraOverlay> {
        let devices = self.devices.borrow();
        let bubble = devices.bubble.as_ref()?;
        let window = bubble.ui.window();
        let scale = window.scale_factor();
        let pos = window.position().to_logical(scale);
        let size = window.size().to_logical(scale);
        let (x, y, w) = (f64::from(pos.x), f64::from(pos.y), f64::from(size.width));
        let overlaps = x < captured.x + captured.width
            && x + w > captured.x
            && y < captured.y + captured.height
            && y + w > captured.y;
        if !overlaps || captured.width <= 0.0 || captured.height <= 0.0 {
            return None;
        }
        Some(CameraOverlay {
            visible: true,
            shape: if bubble.ui.get_shape() == 0 {
                BubbleShape::Circle
            } else {
                BubbleShape::RoundedRect
            },
            x: ((x - captured.x) / captured.width).clamp(0.0, 1.0),
            y: ((y - captured.y) / captured.height).clamp(0.0, 1.0),
            // The editor's size slider range.
            size: (w / captured.width).clamp(0.08, 0.45),
            // The live bubble is a mirror view.
            mirror: true,
        })
    }

    fn release_devices(&self) {
        self.close_bubble();
        *self.devices.borrow_mut() = Devices::default();
    }

    /// Hides and drops the camera bubble. A shown Slint window outlives its
    /// handle, so dropping it alone would leave it on screen.
    fn close_bubble(&self) {
        let bubble = self.devices.borrow_mut().bubble.take();
        if let Some(b) = bubble {
            let _ = b.ui.hide();
        }
    }

    /// Returns true if access is granted now. Otherwise asks (once) and calls
    /// `retry` on the UI thread if the user allows it.
    fn ensure_permission(
        &self,
        which: Permission,
        retry: impl FnOnce(&App) + Send + 'static,
    ) -> bool {
        #[cfg(target_os = "macos")]
        {
            use visor_capture::macos_permissions::{Kind, Status, request, status};
            let kind = match which {
                Permission::Camera => Kind::Camera,
                Permission::Microphone => Kind::Microphone,
            };
            match status(kind) {
                Status::Granted => true,
                Status::NotDetermined => {
                    let retry = std::sync::Mutex::new(Some(retry));
                    request(kind, move |granted| {
                        let retry = retry.lock().unwrap().take();
                        post(move |app| {
                            let panel_open = app
                                .panel
                                .borrow()
                                .as_ref()
                                .is_some_and(|p| p.ui.window().is_visible());
                            if granted {
                                if let Some(retry) = retry
                                    && panel_open
                                    && matches!(*app.session.borrow(), Session::Idle)
                                {
                                    retry(app);
                                }
                            } else {
                                app.set_panel_error(&format!("{which:?} access was denied."));
                            }
                        });
                    });
                    false
                }
                Status::Denied => {
                    self.set_panel_error(&format!(
                        "{which:?} access is off for Visor. Enable it in System Settings › Privacy & Security."
                    ));
                    false
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (which, retry);
            true
        }
    }

    fn main_display(&self) -> Option<DisplayInfo> {
        let from_panel = self
            .panel
            .borrow()
            .as_ref()
            .and_then(|p| p.sources.displays.first().cloned());
        from_panel.or_else(|| {
            self.backend
                .shareable()
                .ok()
                .and_then(|s| s.displays.into_iter().next())
        })
    }

    // ---- area selection ---------------------------------------------------

    fn select_area(&self) {
        let display = {
            let panel = self.panel.borrow();
            let Some(p) = panel.as_ref() else { return };
            let i = p.ui.get_display_index().max(0) as usize;
            p.sources.displays.get(i).cloned()
        };
        let Some(display) = display else { return };
        let Ok(ui) = AreaSelector::new() else { return };
        ui.window().set_position(LogicalPosition::new(
            display.frame.x as f32,
            display.frame.y as f32,
        ));
        ui.window().set_size(LogicalSize::new(
            display.frame.width as f32,
            display.frame.height as f32,
        ));
        ui.set_scale(display.scale as f32);
        let frame = display.frame;
        let id = display.id;
        ui.on_confirm(move || {
            let app = app();
            if let Some(area) = app.area_window.borrow_mut().take() {
                let rect = Rect {
                    x: frame.x + f64::from(area.get_sel_x()),
                    y: frame.y + f64::from(area.get_sel_y()),
                    width: f64::from(area.get_sel_w()),
                    height: f64::from(area.get_sel_h()),
                };
                let _ = area.hide();
                if let Some(p) = &mut *app.panel.borrow_mut() {
                    p.area = Some((id, rect));
                    p.ui.set_area_label(
                        format!(
                            "{}×{} at {}, {}",
                            rect.width as u32, rect.height as u32, rect.x as i32, rect.y as i32
                        )
                        .into(),
                    );
                    let _ = p.ui.show();
                }
            }
            platform::activate();
        });
        ui.on_cancel(|| {
            let app = app();
            if let Some(area) = app.area_window.borrow_mut().take() {
                let _ = area.hide();
            }
            if let Some(p) = &*app.panel.borrow() {
                if p.area.is_none() {
                    p.ui.set_mode(0);
                }
                let _ = p.ui.show();
            }
            app.update_screen_highlight();
        });
        if let Some(p) = &*self.panel.borrow() {
            let _ = p.ui.hide();
        }
        self.update_screen_highlight();
        let _ = ui.show();
        platform::configure_overlay(&ui, Overlay::Shield);
        platform::activate();
        *self.area_window.borrow_mut() = Some(ui);
    }

    // ---- recording session --------------------------------------------------

    pub fn start_recording(&self) {
        if !matches!(*self.session.borrow(), Session::Idle) {
            return;
        }
        if self.devices.borrow().camera_pending.is_some() {
            return self.set_panel_error("The camera is still starting; try again in a moment.");
        }
        let built = self.build_config();
        let (config, display) = match built {
            Ok(v) => v,
            Err(e) => return self.set_panel_error(&e.to_string()),
        };
        {
            let mut s = self.settings.borrow_mut();
            s.system_audio = config.audio.system;
            s.save();
        }
        let captured = match &config.target {
            CaptureTarget::Display { .. } => Some(display.frame),
            CaptureTarget::Area { rect, .. } => Some(*rect),
            CaptureTarget::Window { .. } | CaptureTarget::App { .. } => None,
        };
        let save_dir = self.settings.borrow().save_dir.clone();
        let dir = new_project_dir(&save_dir, "Recording");
        let title = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let (mic, camera) = {
            let d = self.devices.borrow();
            (
                d.mic.as_ref().map(|(_, m)| m.clone()),
                d.camera.as_ref().map(|(_, c)| c.clone()),
            )
        };
        if let Some(p) = &*self.panel.borrow() {
            p.ui.set_error("".into());
            p.ui.set_busy(true);
            let _ = p.ui.hide();
        }
        self.update_screen_highlight();

        // Capture warms up (paused) during the countdown.
        let backend = self.backend.clone();
        let options = RecorderOptions {
            config,
            dir,
            title,
            mic,
            camera,
            exclude_self: true,
            start_paused: true,
        };
        let id = self.next_start_id.get();
        self.next_start_id.set(id + 1);
        std::thread::spawn(move || {
            let result = Recorder::start(backend.as_ref(), options);
            post(move |app| app.on_recorder_ready(id, result));
        });

        let secs = self.settings.borrow().countdown_secs;
        let countdown = (secs > 0)
            .then(|| self.show_countdown(&display, secs))
            .flatten();
        *self.session.borrow_mut() = Session::Starting {
            id,
            remaining: if countdown.is_some() { secs } else { 0 },
            countdown,
            recorder: None,
            display,
            captured,
        };
        self.refresh_tray();
    }

    fn build_config(&self) -> anyhow::Result<(RecordingConfig, DisplayInfo)> {
        let panel = self.panel.borrow();
        let p = panel.as_ref().ok_or_else(|| anyhow::anyhow!("no panel"))?;
        let ui = &p.ui;
        let pick = |i: i32| usize::try_from(i).ok();
        let main = p
            .sources
            .displays
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No display available to record."))?;
        let selected_display = pick(ui.get_display_index())
            .and_then(|i| p.sources.displays.get(i).cloned())
            .unwrap_or(main.clone());
        let (target, display) = match ui.get_mode() {
            1 => {
                let w = pick(ui.get_window_index())
                    .and_then(|i| p.sources.windows.get(i))
                    .ok_or_else(|| anyhow::anyhow!("Choose a window to record."))?;
                (CaptureTarget::Window { id: w.id }, main)
            }
            2 => {
                let a = pick(ui.get_app_index())
                    .and_then(|i| p.sources.apps.get(i))
                    .ok_or_else(|| anyhow::anyhow!("Choose an app to record."))?;
                (CaptureTarget::App { pid: a.pid }, main)
            }
            3 => {
                let (display_id, rect) = p
                    .area
                    .ok_or_else(|| anyhow::anyhow!("Select an area to record first."))?;
                let display = p
                    .sources
                    .displays
                    .iter()
                    .find(|d| d.id == display_id)
                    .cloned()
                    .unwrap_or(main);
                (CaptureTarget::Area { display_id, rect }, display)
            }
            _ => (
                CaptureTarget::Display {
                    id: selected_display.id,
                },
                selected_display,
            ),
        };
        let system = ui.get_system_audio();
        let app_pid = if system {
            pick(ui.get_audio_app_index())
                .and_then(|i| i.checked_sub(1))
                .and_then(|i| p.sources.apps.get(i))
                .map(|a| a.pid)
        } else {
            None
        };
        let settings = self.settings.borrow();
        let devices = self.devices.borrow();
        let config = RecordingConfig {
            target,
            fps: settings.fps,
            show_cursor: settings.show_cursor,
            audio: AudioConfig {
                mic: devices.mic.as_ref().map(|(id, _)| id.clone()),
                system,
                app_pid,
            },
            camera: devices.camera.as_ref().map(|(id, _)| id.clone()),
            countdown_secs: settings.countdown_secs,
        };
        Ok((config, display))
    }

    fn show_countdown(&self, display: &DisplayInfo, secs: u32) -> Option<(Countdown, Timer)> {
        let ui = Countdown::new().ok()?;
        ui.set_value(secs as i32);
        ui.window().set_position(LogicalPosition::new(
            (display.frame.x + (display.frame.width - 180.0) / 2.0) as f32,
            (display.frame.y + (display.frame.height - 180.0) / 2.0) as f32,
        ));
        ui.on_skip(|| app().countdown_tick(true));
        let _ = ui.show();
        platform::configure_overlay(&ui, Overlay::Shield);
        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_secs(1), || {
            app().countdown_tick(false)
        });
        Some((ui, timer))
    }

    fn countdown_tick(&self, skip: bool) {
        let ready = {
            let mut session = self.session.borrow_mut();
            let Session::Starting {
                countdown,
                remaining,
                recorder,
                ..
            } = &mut *session
            else {
                return;
            };
            *remaining = if skip { 0 } else { remaining.saturating_sub(1) };
            if let Some((ui, _)) = countdown {
                ui.set_value(*remaining as i32);
            }
            if *remaining == 0
                && let Some((ui, _)) = countdown.take()
            {
                let _ = ui.hide();
            }
            *remaining == 0 && recorder.is_some()
        };
        if ready {
            self.begin_recording();
        }
    }

    fn on_recorder_ready(&self, start_id: u64, result: anyhow::Result<Recorder>) {
        let current =
            matches!(&*self.session.borrow(), Session::Starting { id, .. } if *id == start_id);
        if !current {
            // This start was cancelled; throw away what it created.
            if let Ok(r) = result {
                discard_in_background(r);
            }
            return;
        }
        let recorder = match result {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("could not start recording: {e:#}");
                if let Session::Starting {
                    countdown: Some((ui, _)),
                    ..
                } = std::mem::take(&mut *self.session.borrow_mut())
                {
                    let _ = ui.hide();
                }
                self.return_to_panel(Some(&format!("Could not start recording: {e:#}")));
                return;
            }
        };
        let ready = {
            let mut session = self.session.borrow_mut();
            let Session::Starting {
                recorder: slot,
                remaining,
                ..
            } = &mut *session
            else {
                return;
            };
            *slot = Some(recorder);
            *remaining == 0
        };
        if ready {
            self.begin_recording();
        }
    }

    /// Back to Idle with the panel showing, e.g. after a cancelled start.
    fn return_to_panel(&self, error: Option<&str>) {
        *self.session.borrow_mut() = Session::Idle;
        self.refresh_tray();
        self.open_panel();
        if let Some(e) = error {
            self.set_panel_error(e);
        }
    }

    fn begin_recording(&self) {
        let session = std::mem::take(&mut *self.session.borrow_mut());
        let Session::Starting {
            recorder: Some(recorder),
            display,
            captured,
            ..
        } = session
        else {
            *self.session.borrow_mut() = session;
            return;
        };
        recorder.resume();

        let Ok(bar) = ControlBar::new() else { return };
        bar.window().set_position(LogicalPosition::new(
            (display.frame.x + (display.frame.width - 236.0) / 2.0) as f32,
            (display.frame.y + display.frame.height - 110.0) as f32,
        ));
        bar.on_toggle_pause(|| app().toggle_pause());
        bar.on_stop(|| app().stop_recording(false));
        bar.on_discard(|| app().stop_recording(true));
        bar.on_drag_by(|dx, dy| {
            if let Session::Recording { bar, .. } = &*app().session.borrow() {
                move_window(bar.window(), dx, dy);
            }
        });
        let _ = bar.show();
        platform::configure_overlay(&bar, Overlay::Floating);

        // The portal can't hide our windows on Linux; keep the bubble out of
        // display recordings there.
        #[cfg(all(unix, not(target_os = "macos")))]
        if let Some(b) = &self.devices.borrow().bubble {
            let _ = b.ui.hide();
        }

        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_millis(250), || {
            if let Session::Recording { recorder, bar, .. } = &*app().session.borrow() {
                bar.set_elapsed(util::clock(recorder.elapsed()).into());
                bar.set_paused(recorder.is_paused());
            }
        });
        *self.session.borrow_mut() = Session::Recording {
            recorder,
            bar,
            captured,
            _timer: timer,
        };
        if let Some(p) = &*self.panel.borrow() {
            p.ui.set_busy(false);
        }
        self.refresh_tray();
    }

    fn toggle_pause(&self) {
        if let Session::Recording { recorder, bar, .. } = &*self.session.borrow() {
            if recorder.is_paused() {
                recorder.resume();
            } else {
                recorder.pause();
            }
            bar.set_paused(recorder.is_paused());
        }
        self.refresh_tray();
    }

    /// Whether the chosen devices are open, so a recording can start.
    pub fn devices_ready(&self) -> bool {
        self.devices.borrow().camera_pending.is_none()
    }

    /// Recorded time of the running recording, if one is running.
    pub fn recorded(&self) -> Option<Duration> {
        match &*self.session.borrow() {
            Session::Recording { recorder, .. } => Some(Duration::from_micros(recorder.elapsed())),
            _ => None,
        }
    }

    pub fn stop_recording(&self, discard: bool) {
        let session = std::mem::replace(&mut *self.session.borrow_mut(), Session::Stopping);
        let (recorder, overlay) = match session {
            Session::Recording {
                recorder,
                bar,
                captured,
                ..
            } => {
                let _ = bar.hide();
                (recorder, captured.and_then(|c| self.bubble_overlay(c)))
            }
            Session::Starting {
                recorder,
                countdown,
                ..
            } => {
                // Nothing has been recorded yet: cancel and go back to the panel.
                if let Some((ui, _)) = countdown {
                    let _ = ui.hide();
                }
                if let Some(r) = recorder {
                    discard_in_background(r);
                }
                // A recorder still starting is discarded by on_recorder_ready.
                self.return_to_panel(None);
                return;
            }
            other => {
                *self.session.borrow_mut() = other;
                return;
            }
        };
        // Free the camera (and its light) and mic now that we're done.
        self.release_devices();
        self.refresh_tray();
        std::thread::spawn(move || {
            if discard {
                let result = recorder.discard();
                post(move |app| app.on_discarded(result));
            } else {
                let result = recorder.stop().map(|(dir, mut project)| {
                    // Start the editor with the bubble where it was on screen.
                    if let Some(overlay) = overlay {
                        project.camera_overlay = overlay;
                        if let Err(e) = project.save(&dir) {
                            tracing::warn!("could not save the bubble position: {e}");
                        }
                    }
                    dir
                });
                post(move |app| app.on_stopped(result));
            }
        });
    }

    fn on_stopped(&self, result: anyhow::Result<PathBuf>) {
        *self.session.borrow_mut() = Session::Idle;
        if self.quit_pending.get() {
            if let Err(e) = &result {
                tracing::error!("finishing the recording failed: {e:#}");
            }
            return self.quit();
        }
        self.refresh_tray();
        match result {
            Ok(dir) => self.open_editor(dir),
            Err(e) => {
                tracing::error!("finishing the recording failed: {e:#}");
                self.open_panel();
                self.set_panel_error(&format!("Saving the recording failed: {e:#}"));
            }
        }
    }

    fn on_discarded(&self, result: anyhow::Result<()>) {
        if let Err(e) = result {
            tracing::warn!("discarding the recording failed: {e:#}");
        }
        if self.quit_pending.get() {
            *self.session.borrow_mut() = Session::Idle;
            return self.quit();
        }
        self.return_to_panel(None);
    }

    // ---- editor & preferences ---------------------------------------------

    pub fn open_editor(&self, dir: PathBuf) {
        if let Some(existing) = self.editors.borrow().iter().find(|e| e.dir() == dir) {
            existing.show();
            platform::activate();
            return;
        }
        match EditorWindow::open(dir.clone()) {
            Ok(editor) => {
                self.editors.borrow_mut().push(editor);
                self.update_dock();
                platform::activate();
            }
            Err(e) => {
                tracing::error!("could not open {}: {e:#}", dir.display());
                let _ = rfd::MessageDialog::new()
                    .set_title("Visor")
                    .set_description(format!("Could not open the recording:\n{e:#}"))
                    .show();
            }
        }
    }

    pub fn editor_closed(&self, dir: &std::path::Path) {
        self.editors.borrow_mut().retain(|e| e.dir() != dir);
        self.update_dock();
        self.refresh_tray();
    }

    fn update_dock(&self) {
        let regular = !self.editors.borrow().is_empty()
            || self
                .prefs
                .borrow()
                .as_ref()
                .is_some_and(|p| p.window().is_visible());
        platform::set_dock_visible(regular);
    }

    pub fn open_preferences(&self) {
        if self.prefs.borrow().is_none() {
            let Ok(ui) = Preferences::new() else { return };
            ui.set_version(env!("CARGO_PKG_VERSION").into());
            ui.on_changed(|| app().preferences_changed());
            ui.on_choose_folder(|| {
                let app = app();
                let current = app.settings.borrow().save_dir.clone();
                if let Some(dir) = rfd::FileDialog::new().set_directory(&current).pick_folder() {
                    app.settings.borrow_mut().save_dir = dir;
                    app.settings.borrow().save();
                    app.sync_preferences();
                    app.refresh_tray();
                }
            });
            ui.window().on_close_requested(|| {
                post(|app| app.update_dock());
                slint::CloseRequestResponse::HideWindow
            });
            *self.prefs.borrow_mut() = Some(ui);
        }
        self.sync_preferences();
        if let Some(ui) = &*self.prefs.borrow() {
            let _ = ui.show();
        }
        self.update_dock();
        platform::activate();
    }

    fn sync_preferences(&self) {
        let s = self.settings.borrow().clone();
        if let Some(ui) = &*self.prefs.borrow() {
            ui.set_save_dir(s.save_dir.display().to_string().into());
            ui.set_fps_index(if s.fps >= 60 { 1 } else { 0 });
            ui.set_countdown_index(match s.countdown_secs {
                0 => 0,
                1..=3 => 1,
                _ => 2,
            });
            ui.set_show_cursor(s.show_cursor);
            ui.set_export_format(s.export_format);
        }
    }

    fn preferences_changed(&self) {
        let Some(ui) = self.prefs.borrow().as_ref().map(|p| p.clone_strong()) else {
            return;
        };
        {
            let mut s = self.settings.borrow_mut();
            s.fps = if ui.get_fps_index() == 1 { 60 } else { 30 };
            s.countdown_secs = [0, 3, 5][ui.get_countdown_index().clamp(0, 2) as usize];
            s.show_cursor = ui.get_show_cursor();
            s.export_format = ui.get_export_format();
            s.save();
        }
        self.update_hint();
    }
}

#[derive(Debug, Clone, Copy)]
enum Permission {
    Camera,
    Microphone,
}

/// Discards a recorder without blocking the UI thread on its writer threads.
fn discard_in_background(recorder: Recorder) {
    std::thread::spawn(move || {
        if let Err(e) = recorder.discard() {
            tracing::warn!("discarding an abandoned recording failed: {e:#}");
        }
    });
}

fn strings(items: impl Iterator<Item = String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        items.map(SharedString::from).collect::<Vec<_>>(),
    ))
}

fn clamp_index(current: i32, len: usize, set: impl FnOnce(i32)) {
    if len == 0 {
        set(-1);
    } else if current < 0 || current as usize >= len {
        set(0);
    }
}

/// Moves a frameless window by a drag delta (logical pixels).
fn move_window(window: &slint::Window, dx: f32, dy: f32) {
    let scale = window.scale_factor();
    let pos = window.position().to_logical(scale);
    window.set_position(LogicalPosition::new(pos.x + dx, pos.y + dy));
}
