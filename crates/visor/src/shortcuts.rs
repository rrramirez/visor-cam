//! System-wide keyboard shortcuts for recording, which work whichever app is
//! in front (macOS, Windows, and X11 on Linux).

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    /// Start a recording with the last settings, or stop the current one.
    StartStop,
    TogglePause,
    Discard,
}

const ALL: [(Shortcut, Code); 3] = [
    (Shortcut::StartStop, Code::KeyR),
    (Shortcut::TogglePause, Code::KeyP),
    (Shortcut::Discard, Code::Backspace),
];

const MODIFIERS: Modifiers = Modifiers::ALT.union(Modifiers::SHIFT);

fn hotkey(code: Code) -> HotKey {
    HotKey::new(Some(MODIFIERS), code)
}

/// How a shortcut is written on this platform, e.g. "⌥⇧R".
pub fn label(shortcut: Shortcut) -> String {
    let (_, code) = ALL.iter().find(|(s, _)| *s == shortcut).expect("listed");
    let key = match code {
        Code::Backspace if cfg!(target_os = "macos") => "⌫",
        Code::Backspace => "Backspace",
        Code::KeyR => "R",
        _ => "P",
    };
    if cfg!(target_os = "macos") {
        format!("⌥⇧{key}")
    } else {
        format!("Alt+Shift+{key}")
    }
}

/// Held only while recording, so the keys type as usual otherwise (⌥⇧P
/// is "∏" on a US Mac keyboard).
const RECORDING_ONLY: [Shortcut; 2] = [Shortcut::TogglePause, Shortcut::Discard];

pub struct Shortcuts {
    manager: GlobalHotKeyManager,
    registered: Vec<(Shortcut, HotKey)>,
    enabled: bool,
    recording: bool,
}

impl Shortcuts {
    /// Listens for the shortcuts once [`set_enabled`](Self::set_enabled) turns
    /// them on, calling `on_press` from any thread. Needs the event loop
    /// running (on the main thread on macOS).
    pub fn new(on_press: impl Fn(Shortcut) + Send + Sync + 'static) -> anyhow::Result<Self> {
        let manager = GlobalHotKeyManager::new()?;
        GlobalHotKeyEvent::set_event_handler(Some(move |event: GlobalHotKeyEvent| {
            if event.state() != HotKeyState::Pressed {
                return;
            }
            if let Some((shortcut, _)) = ALL
                .iter()
                .find(|(_, code)| hotkey(*code).id() == event.id())
            {
                on_press(*shortcut);
            }
        }));
        Ok(Self {
            manager,
            registered: Vec::new(),
            enabled: false,
            recording: false,
        })
    }

    /// Turns the shortcuts on or off. Returns the ones another app already
    /// uses, which then don't work.
    pub fn set_enabled(&mut self, enabled: bool) -> Vec<Shortcut> {
        self.enabled = enabled;
        self.update()
    }

    /// Pause and discard are registered only while a recording runs.
    /// Returns the shortcuts another app already uses.
    pub fn set_recording(&mut self, recording: bool) -> Vec<Shortcut> {
        self.recording = recording;
        self.update()
    }

    /// Registers what should be held now and lets go of the rest.
    fn update(&mut self) -> Vec<Shortcut> {
        let wanted: Vec<Shortcut> = ALL
            .iter()
            .map(|(s, _)| *s)
            .filter(|s| self.enabled && (self.recording || !RECORDING_ONLY.contains(s)))
            .collect();
        let manager = &self.manager;
        self.registered.retain(|(s, key)| {
            let keep = wanted.contains(s);
            if !keep {
                let _ = manager.unregister(*key);
            }
            keep
        });
        let mut taken = Vec::new();
        for (shortcut, code) in ALL {
            if !wanted.contains(&shortcut) || self.registered.iter().any(|(s, _)| *s == shortcut) {
                continue;
            }
            match self.manager.register(hotkey(code)) {
                Ok(()) => self.registered.push((shortcut, hotkey(code))),
                Err(e) => {
                    tracing::warn!("shortcut {} is unavailable: {e}", label(shortcut));
                    taken.push(shortcut);
                }
            }
        }
        taken
    }
}
