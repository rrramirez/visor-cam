//! App updates through Sparkle on macOS.
//!
//! The packaged app carries `Sparkle.framework` and the feed URL and public
//! key in its Info.plist; Sparkle asks the user once whether to check
//! automatically and verifies every update's signature before installing it.
//! Development builds and other platforms have no updater.

pub struct Updater {
    #[cfg(target_os = "macos")]
    controller: objc2::rc::Retained<objc2::runtime::AnyObject>,
}

impl Updater {
    /// Starts Sparkle's updater if Visor is running from its app bundle with
    /// Sparkle inside. Must run on the main thread.
    pub fn start() -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            let controller = macos::start()?;
            tracing::info!("updates: Sparkle started");
            Some(Self { controller })
        }
        #[cfg(not(target_os = "macos"))]
        None
    }

    /// Checks for an update now and shows the result.
    pub fn check_now(&self) {
        #[cfg(target_os = "macos")]
        macos::check_now(&self.controller);
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use objc2::rc::{Allocated, Retained};
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::{MainThreadMarker, msg_send};
    use objc2_foundation::{NSBundle, NSString};

    pub fn start() -> Option<Retained<AnyObject>> {
        MainThreadMarker::new()?;
        let app = NSBundle::mainBundle().bundlePath().to_string();
        if !app.ends_with(".app") {
            return None;
        }
        let framework = format!("{app}/Contents/Frameworks/Sparkle.framework");
        let bundle = NSBundle::bundleWithPath(&NSString::from_str(&framework))?;
        // SAFETY: loads the Sparkle framework shipped (and signed) inside
        // Visor's own bundle; its initializers have no preconditions.
        if !unsafe { bundle.load() } {
            tracing::warn!("updates: could not load {framework}");
            return None;
        }
        let class = AnyClass::get(c"SPUStandardUpdaterController")?;
        // SAFETY: Sparkle's documented initializer; nil delegates use the
        // standard behaviour. Called on the main thread (checked above).
        unsafe {
            let allocated: Allocated<AnyObject> = msg_send![class, alloc];
            msg_send![
                allocated,
                initWithStartingUpdater: Bool::YES,
                updaterDelegate: Option::<&AnyObject>::None,
                userDriverDelegate: Option::<&AnyObject>::None
            ]
        }
    }

    pub fn check_now(controller: &AnyObject) {
        // SAFETY: an action method taking an optional sender, on the main thread.
        unsafe {
            let _: () = msg_send![controller, checkForUpdates: Option::<&AnyObject>::None];
        }
    }
}
