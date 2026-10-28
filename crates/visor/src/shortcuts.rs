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

pub struct Shortcuts {
    manager: GlobalHotKeyManager,
    registered: Vec<HotKey>,
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
        })
    }

    /// Registers or unregisters the shortcuts. Returns the ones another app
    /// already uses, which then don't work.
    pub fn set_enabled(&mut self, enabled: bool) -> Vec<Shortcut> {
        for key in self.registered.drain(..) {
            let _ = self.manager.unregister(key);
        }
        if !enabled {
            return Vec::new();
        }
        let mut taken = Vec::new();
        for (shortcut, code) in ALL {
            match self.manager.register(hotkey(code)) {
                Ok(()) => self.registered.push(hotkey(code)),
                Err(e) => {
                    tracing::warn!("shortcut {} is unavailable: {e}", label(shortcut));
                    taken.push(shortcut);
                }
            }
        }
        taken
    }
}
