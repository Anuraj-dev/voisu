//! The macOS privacy permissions Voisu needs, as the window shows them:
//! Microphone to record, Input Monitoring for the Trigger Key's event tap,
//! and Accessibility to post the paste keystroke. Windows asks for none of
//! these, so there [`current`] is `None` and the window shows nothing.

use serde::{Deserialize, Serialize};

/// Where the Microphone permission stands. macOS asks the first time a
/// Recording opens the mic, so "not asked" is not a problem yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MicAccess {
    Granted,
    NotAsked,
    /// Denied by the user, or restricted by a profile.
    Denied,
}

impl MicAccess {
    /// `AVAuthorizationStatus`: 0 not determined, 1 restricted, 2 denied,
    /// 3 authorized.
    pub fn from_status(status: isize) -> Self {
        match status {
            3 => Self::Granted,
            0 => Self::NotAsked,
            _ => Self::Denied,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Permissions {
    pub microphone: MicAccess,
    /// Without it the Transcript is copied but not pasted.
    pub accessibility: bool,
    /// Without it the Trigger Key does nothing.
    pub input_monitoring: bool,
}

/// One pane of System Settings → Privacy & Security.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum Pane {
    Microphone,
    Accessibility,
    InputMonitoring,
}

impl Pane {
    /// The fixed System Settings link for the pane; the webview never picks
    /// a URL.
    pub fn url(self) -> &'static str {
        match self {
            Self::Microphone => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"
            }
            Self::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            Self::InputMonitoring => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
            }
        }
    }
}

/// The permissions as they stand now; `None` off macOS.
pub fn current() -> Option<Permissions> {
    native::current()
}

/// Asks macOS for the pane's permission, which adds Voisu to that pane's
/// list (and shows the system prompt the first time). The caller opens the
/// pane after.
pub fn request(pane: Pane) {
    native::request(pane);
}

/// Whether Voisu may post the paste keystroke.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn accessibility_trusted() -> bool {
    native::accessibility_trusted()
}

/// Whether the user or a profile has refused Voisu the mic.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn microphone_denied() -> bool {
    native::microphone() == MicAccess::Denied
}

#[cfg(not(target_os = "macos"))]
mod native {
    use super::{MicAccess, Pane, Permissions};

    pub fn current() -> Option<Permissions> {
        None
    }

    pub fn request(_pane: Pane) {}

    pub fn accessibility_trusted() -> bool {
        true
    }

    pub fn microphone() -> MicAccess {
        MicAccess::Granted
    }
}

#[cfg(target_os = "macos")]
mod native {
    use std::ffi::c_void;

    use objc2::runtime::AnyClass;
    use objc2::{class, msg_send};
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};
    use objc2_core_graphics::{CGPreflightListenEventAccess, CGRequestListenEventAccess};
    use objc2_foundation::NSString;

    use super::{MicAccess, Pane, Permissions};

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> u8;
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    }

    #[link(name = "AVFoundation", kind = "framework")]
    unsafe extern "C" {
        static AVMediaTypeAudio: &'static NSString;
    }

    pub fn current() -> Option<Permissions> {
        Some(Permissions {
            microphone: microphone(),
            accessibility: accessibility_trusted(),
            input_monitoring: CGPreflightListenEventAccess(),
        })
    }

    pub fn request(pane: Pane) {
        match pane {
            Pane::Accessibility => {
                let key = CFString::from_static_str("AXTrustedCheckOptionPrompt");
                let options = CFDictionary::<CFString, CFBoolean>::from_slices(
                    &[&key],
                    &[CFBoolean::new(true)],
                );
                let options: *const CFDictionary = options.as_opaque();
                // SAFETY: a valid CFDictionary that outlives the call.
                unsafe { AXIsProcessTrustedWithOptions(options.cast()) };
            }
            Pane::InputMonitoring => {
                CGRequestListenEventAccess();
            }
            // Asked by the first Recording; a denial can only be undone in
            // System Settings.
            Pane::Microphone => {}
        }
    }

    pub fn accessibility_trusted() -> bool {
        // SAFETY: takes no arguments and only reads this process's trust.
        unsafe { AXIsProcessTrusted() != 0 }
    }

    pub fn microphone() -> MicAccess {
        let device: &AnyClass = class!(AVCaptureDevice);
        // SAFETY: `+[AVCaptureDevice authorizationStatusForMediaType:]` takes
        // an AVMediaType (an NSString) and returns an NSInteger;
        // `AVMediaTypeAudio` is a framework constant.
        let status: isize =
            unsafe { msg_send![device, authorizationStatusForMediaType: AVMediaTypeAudio] };
        MicAccess::from_status(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_authorized_mic_is_granted_and_only_an_unasked_one_waits() {
        assert_eq!(MicAccess::from_status(3), MicAccess::Granted);
        assert_eq!(MicAccess::from_status(0), MicAccess::NotAsked);
        assert_eq!(MicAccess::from_status(1), MicAccess::Denied);
        assert_eq!(MicAccess::from_status(2), MicAccess::Denied);
    }

    #[test]
    fn each_pane_opens_its_own_privacy_page() {
        let pane = |name: &str| serde_json::from_str::<Pane>(&format!("\"{name}\"")).unwrap();
        assert!(pane("microphone").url().ends_with("?Privacy_Microphone"));
        assert!(
            pane("accessibility")
                .url()
                .ends_with("?Privacy_Accessibility")
        );
        assert!(
            pane("inputMonitoring")
                .url()
                .ends_with("?Privacy_ListenEvent")
        );
        assert!(serde_json::from_str::<Pane>("\"https://example.com\"").is_err());
    }

    #[test]
    fn the_window_reads_permissions_in_camel_case() {
        let json = serde_json::to_string(&Permissions {
            microphone: MicAccess::NotAsked,
            accessibility: false,
            input_monitoring: true,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"microphone":"notAsked","accessibility":false,"inputMonitoring":true}"#
        );
    }
}
