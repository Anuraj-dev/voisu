//! L0-DEFECT: exact Trigger Key uniqueness is not enforced on the portal path.
//!
//! Hyprland setup already fails closed on a duplicate default-submap binding.
//! Fedora Global Shortcuts bind records the desktop-approved description and
//! does not reject a chord already occupied on the desktop. L6 host gates must
//! fail closed. This file does not implement portal or Hyprland bind work.

use voisu_app::hyprland_bindings::{
    CAPS_LOCK, VOISU_TOGGLE_COMMAND, VOISU_TRIGGER_DESCRIPTION, hyprland_binding_is_installed,
};
use voisu_core::TriggerKeyBinding;

#[test]
fn hyprland_setup_rejects_a_duplicate_same_key_binding() {
    let duplicate = serde_json::json!([
        {
            "key": "",
            "modmask": 0,
            "description": VOISU_TRIGGER_DESCRIPTION,
            "dispatcher": "__lua",
            "arg": "66"
        },
        {
            "key": CAPS_LOCK.code,
            "modmask": 0,
            "dispatcher": "exec",
            "arg": "kitty"
        }
    ]);
    assert!(
        !hyprland_binding_is_installed(&duplicate, CAPS_LOCK.code, VOISU_TOGGLE_COMMAND),
        "Hyprland verification must fail closed when another default-submap binding claims the Trigger Key"
    );
}

#[test]
fn portal_trigger_key_binding_cannot_reject_an_occupied_chord() {
    // Current: the portal path's public bind artifact is a description string.
    // Constructing it cannot fail when the chord is already bound elsewhere.
    // Required future contract (R8 / L6): fail closed unless the Trigger Key
    // chord is unique on the desktop.
    let occupied_desktop_chords = ["Super+Alt+V", "Caps Lock"];
    let requested = "Super+Alt+V";
    let binding = TriggerKeyBinding::new(requested);
    assert_eq!(binding.description, requested);
    assert!(
        occupied_desktop_chords.contains(&binding.description.as_str()),
        "L0-DEFECT: a Trigger Key chord already present on the desktop still constructs a binding"
    );
}
