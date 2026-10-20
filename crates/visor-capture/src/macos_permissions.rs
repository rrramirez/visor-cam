//! macOS privacy permissions (TCC) for screen recording, camera and mic.

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureDevice, AVMediaType, AVMediaTypeAudio, AVMediaTypeVideo,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Granted,
    Denied,
    NotDetermined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    ScreenRecording,
    Camera,
    Microphone,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

pub fn status(kind: Kind) -> Status {
    match kind {
        // SAFETY: no preconditions.
        Kind::ScreenRecording => {
            if unsafe { CGPreflightScreenCaptureAccess() } {
                Status::Granted
            } else {
                Status::NotDetermined
            }
        }
        Kind::Camera | Kind::Microphone => {
            let Some(media) = media_type(kind) else {
                return Status::Denied;
            };
            match unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) } {
                AVAuthorizationStatus::Authorized => Status::Granted,
                AVAuthorizationStatus::NotDetermined => Status::NotDetermined,
                _ => Status::Denied,
            }
        }
    }
}

/// Asks for access, showing the system prompt if the user hasn't decided yet.
/// `done` runs on an arbitrary thread with whether access was granted.
pub fn request(kind: Kind, done: impl Fn(bool) + Send + 'static) {
    match kind {
        Kind::ScreenRecording => {
            // SAFETY: no preconditions. Only prompts once per app install.
            done(unsafe { CGRequestScreenCaptureAccess() });
        }
        Kind::Camera | Kind::Microphone => {
            let Some(media) = media_type(kind) else {
                return done(false);
            };
            let block = RcBlock::new(move |granted: Bool| done(granted.as_bool()));
            unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &block) };
        }
    }
}

/// Opens the matching pane of System Settings.
pub fn open_settings(kind: Kind) {
    let pane = match kind {
        Kind::ScreenRecording => "Privacy_ScreenCapture",
        Kind::Camera => "Privacy_Camera",
        Kind::Microphone => "Privacy_Microphone",
    };
    let url = format!("x-apple.systempreferences:com.apple.preference.security?{pane}");
    if let Err(e) = std::process::Command::new("open").arg(url).spawn() {
        tracing::warn!("could not open System Settings: {e}");
    }
}

fn media_type(kind: Kind) -> Option<&'static AVMediaType> {
    // SAFETY: framework-provided constant strings.
    unsafe {
        match kind {
            Kind::Camera => AVMediaTypeVideo,
            Kind::Microphone => AVMediaTypeAudio,
            Kind::ScreenRecording => None,
        }
    }
}
