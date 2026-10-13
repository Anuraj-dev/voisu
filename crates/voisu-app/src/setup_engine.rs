//! Setup engine screen: Cloud and Local suggestions, then an explicit choice.
//!
//! The core prints and reads through [`WizardIo`]. It does not persist ASR mode.

use voisu_core::{AsrMode, KeyDiagnosis, KeyLocation, Provider};

use crate::local_model::{
    ConsentedCatalogEntry, HostHardware, LocalSuggestion, consented_entry, suggest_local,
    suggestion_line, warnings,
};
use crate::setup::WizardIo;

const TITLE: &str = "Choose how Voisu transcribes:";
const CLOUD_TITLE: &str = "[1] Cloud — Deepgram + Groq (audio is sent to the providers)";
const LOCAL_TITLE: &str = "[2] Local — runs on this computer";
const GROQ_ONLY_HINT: &str =
    "No Deepgram key configured — run `voisu deepgram off` for the faster Groq-only path.";
const LOCAL_AVAILABILITY: &str =
    "Availability: not installed — no Local model can be installed by setup in this release yet.";
#[cfg(test)]
pub const INSTALLED_AVAILABILITY: &str =
    "Availability: installed — distil-large-v3.5-int8 (Consented Install, cuda:0)";
const LOCAL_SWITCH_CONFIRM: &str = "Local has no installed model yet; Voisu will refuse to dictate until one is installed. Switch to Local anyway?";
const MAX_ENGINE_ATTEMPTS: u32 = 3;

/// Injected Consented Install status. Tests never open the model store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsentedStatus {
    Installed { entry_id: String, device: String },
    NotInstalled,
    Unknown(String),
}

/// Outcome of the engine screen. The core does not persist the choice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineChoice {
    Keep(AsrMode),
    Switch(AsrMode),
}

/// Prints Cloud and Local together, then lets the user choose.
///
/// `interactive` is decided by the binary from `stdin().is_terminal()`.
/// `manifest_exists` and `consented` are injected so tests never touch the filesystem.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn run_engine_screen(
    io: &mut dyn WizardIo,
    current: AsrMode,
    hw: &HostHardware,
    deepgram: &KeyDiagnosis,
    groq: &KeyDiagnosis,
    interactive: bool,
    manifest_exists: impl Fn(&str) -> bool,
    consented: ConsentedStatus,
) -> EngineChoice {
    write_engine_screen(io, hw, deepgram, groq, &manifest_exists, &consented);
    let switch_needs_no_confirm = matches!(consented, ConsentedStatus::Installed { .. })
        || consent_confirms_local_switch(hw, &consented);
    choose_engine(io, current, interactive, switch_needs_no_confirm)
}

/// Whether choosing Local leads straight to the Consented Install prompt,
/// which then serves as the only confirmation of the switch: the suggested
/// model is consent-installable and known not to be installed. Setup must
/// keep Cloud if that prompt is declined.
#[must_use]
pub fn consent_confirms_local_switch(hw: &HostHardware, consented: &ConsentedStatus) -> bool {
    matches!(consented, ConsentedStatus::NotInstalled)
        && suggested_consented_entry(&suggest_local(hw)).is_some()
}

fn write_engine_screen(
    io: &mut dyn WizardIo,
    hw: &HostHardware,
    deepgram: &KeyDiagnosis,
    groq: &KeyDiagnosis,
    manifest_exists: &impl Fn(&str) -> bool,
    consented: &ConsentedStatus,
) {
    io.writeln(TITLE);
    io.writeln("");
    io.writeln(CLOUD_TITLE);
    io.writeln(&format!(
        "{}: {}",
        Provider::Deepgram.cli_label(),
        key_status(deepgram)
    ));
    io.writeln(&format!(
        "{}: {}",
        Provider::Groq.cli_label(),
        key_status(groq)
    ));
    if matches!(deepgram, KeyDiagnosis::Absent) && groq.is_configured() {
        io.writeln(GROQ_ONLY_HINT);
    }
    io.writeln("");
    io.writeln(LOCAL_TITLE);
    let suggestion = suggest_local(hw);
    io.writeln(&suggestion_line(&suggestion));
    for warning in warnings(hw) {
        io.writeln(&format!("Note: {warning}"));
    }
    io.writeln(&availability_line(&suggestion, consented));
    // The binary passes `manifest_exists` only under the developer Local pilot
    // flag, so production output never shows this line.
    let model_id = suggested_model_id(&suggestion);
    if manifest_exists(model_id) {
        io.writeln(&format!(
            "Developer pilot: `voisu local-dev select {model_id}` then `voisu-dev on` (stops voisu.service and enables voisu-dev.service)."
        ));
    }
}

fn availability_line(suggestion: &LocalSuggestion, consented: &ConsentedStatus) -> String {
    match consented {
        ConsentedStatus::Installed { entry_id, device } => {
            format!("Availability: installed — {entry_id} (Consented Install, {device})")
        }
        ConsentedStatus::Unknown(reason) => format!("Availability: unknown — {reason}"),
        ConsentedStatus::NotInstalled if suggested_consented_entry(suggestion).is_some() => {
            format!(
                "Availability: not installed — setup can download it ({}) after you agree.",
                format_size(
                    suggested_consented_entry(suggestion)
                        .expect("catalog entry")
                        .total_download_bytes
                )
            )
        }
        ConsentedStatus::NotInstalled => LOCAL_AVAILABILITY.to_owned(),
    }
}

pub(crate) fn suggested_consented_entry(
    suggestion: &LocalSuggestion,
) -> Option<&'static ConsentedCatalogEntry> {
    consented_entry(suggested_model_id(suggestion)).filter(|entry| entry.consent_installable)
}

/// Human byte size in binary units, one decimal: `2.8 GiB`, `25.3 MiB`.
#[must_use]
pub(crate) fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KIB * KIB * KIB {
        format!("{:.1} GiB", value / (KIB * KIB * KIB))
    } else if value >= KIB * KIB {
        format!("{:.1} MiB", value / (KIB * KIB))
    } else if value >= KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn choose_engine(
    io: &mut dyn WizardIo,
    current: AsrMode,
    interactive: bool,
    switch_needs_no_confirm: bool,
) -> EngineChoice {
    if !interactive {
        io.writeln(&format!(
            "Non-interactive: keeping {}; use `voisu mode local|cloud` to change.",
            current.as_str()
        ));
        return EngineChoice::Keep(current);
    }
    let prompt = format!(
        "Engine [1 Cloud / 2 Local] (Enter keeps {}): ",
        mode_label(current)
    );
    for _ in 0..MAX_ENGINE_ATTEMPTS {
        match io.prompt_line(&prompt) {
            None => return EngineChoice::Keep(current),
            Some(answer) => match answer.trim() {
                "" => return EngineChoice::Keep(current),
                "1" => {
                    return if current == AsrMode::Cloud {
                        EngineChoice::Keep(AsrMode::Cloud)
                    } else {
                        EngineChoice::Switch(AsrMode::Cloud)
                    };
                }
                "2" => {
                    if current == AsrMode::Local {
                        return EngineChoice::Keep(AsrMode::Local);
                    }
                    if switch_needs_no_confirm {
                        return EngineChoice::Switch(AsrMode::Local);
                    }
                    return match ask_yes_no(io, LOCAL_SWITCH_CONFIRM, false) {
                        Some(true) => EngineChoice::Switch(AsrMode::Local),
                        Some(false) => EngineChoice::Keep(AsrMode::Cloud),
                        None => {
                            io.writeln(&format!(
                                "Keeping {}; no valid confirmation after 3 attempts.",
                                current.as_str()
                            ));
                            EngineChoice::Keep(current)
                        }
                    };
                }
                _ => io.writeln("Please answer 1 or 2."),
            },
        }
    }
    io.writeln(&format!(
        "Keeping {}; no valid engine choice after 3 attempts.",
        current.as_str()
    ));
    EngineChoice::Keep(current)
}

fn key_status(diagnosis: &KeyDiagnosis) -> &'static str {
    match diagnosis {
        KeyDiagnosis::Found {
            location: KeyLocation::EnvOverride,
            ..
        } => "set via env",
        KeyDiagnosis::Found { .. } => "configured",
        KeyDiagnosis::EnvOverrideInvalid => "env override invalid",
        KeyDiagnosis::Absent => "not configured",
        KeyDiagnosis::Locked => "keyring locked — status unknown",
        KeyDiagnosis::Unavailable => "keyring unavailable — status unknown",
        KeyDiagnosis::ToolMissing => "secret-tool missing — status unknown",
    }
}

fn suggested_model_id(suggestion: &LocalSuggestion) -> &'static str {
    match suggestion {
        LocalSuggestion::Gpu { model_id, .. } | LocalSuggestion::Cpu { model_id, .. } => model_id,
        LocalSuggestion::GpuUnknown {
            cpu_fallback_model_id,
            ..
        } => cpu_fallback_model_id,
    }
}

fn mode_label(mode: AsrMode) -> &'static str {
    match mode {
        AsrMode::Cloud => "Cloud",
        AsrMode::Local => "Local",
    }
}

/// Asks a yes/no question. `None` means three invalid answers; EOF and Enter
/// still take `default_yes` (No on the Local switch confirm).
pub(crate) fn ask_yes_no(io: &mut dyn WizardIo, question: &str, default_yes: bool) -> Option<bool> {
    let suffix = if default_yes { " [Y/n]" } else { " [y/N]" };
    for _ in 0..MAX_ENGINE_ATTEMPTS {
        match io.prompt_line(&format!("{question}{suffix} ")) {
            None => return Some(default_yes),
            Some(answer) => match answer.trim().to_ascii_lowercase().as_str() {
                "" => return Some(default_yes),
                "y" | "yes" => return Some(true),
                "n" | "no" => return Some(false),
                _ => io.writeln("Please answer y or n."),
            },
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::{CONSENTED_ENTRY, DISTIL_MODEL_ID};
    use crate::local_model::{GpuProbe, NvidiaGpu};
    use voisu_core::Credential;

    struct FakeIo {
        lines: Vec<Option<String>>,
        output: Vec<String>,
        prompts: usize,
    }

    impl FakeIo {
        fn new(lines: Vec<Option<&str>>) -> Self {
            Self {
                lines: lines
                    .into_iter()
                    .map(|line| line.map(str::to_owned))
                    .collect(),
                output: Vec::new(),
                prompts: 0,
            }
        }

        fn transcript(&self) -> String {
            self.output.join("\n")
        }
    }

    impl WizardIo for FakeIo {
        fn writeln(&mut self, line: &str) {
            self.output.push(line.to_owned());
        }

        fn prompt_line(&mut self, prompt: &str) -> Option<String> {
            self.prompts += 1;
            self.output.push(format!("PROMPT {prompt}"));
            if self.lines.is_empty() {
                None
            } else {
                self.lines.remove(0)
            }
        }

        fn prompt_secret(&mut self, _prompt: &str) -> Option<String> {
            panic!("engine screen must not prompt for Cloud keys");
        }
    }

    fn gpu(index: u32, total_vram_mib: u64) -> NvidiaGpu {
        NvidiaGpu {
            index,
            name: "RTX 5050 Laptop".into(),
            total_vram_mib,
            driver_version: "580.95".into(),
        }
    }

    fn host(gpu: GpuProbe) -> HostHardware {
        HostHardware {
            gpu,
            ram_bytes: Some(16 * 1024 * 1024 * 1024),
            cpu_cores: 16,
        }
    }

    fn absent() -> KeyDiagnosis {
        KeyDiagnosis::Absent
    }

    fn found(location: KeyLocation) -> KeyDiagnosis {
        KeyDiagnosis::Found {
            location,
            credential: Credential::new("test-key".into()).unwrap(),
        }
    }

    fn run(
        hw: &HostHardware,
        current: AsrMode,
        deepgram: &KeyDiagnosis,
        groq: &KeyDiagnosis,
        interactive: bool,
        answers: Vec<Option<&str>>,
        manifest_exists: impl Fn(&str) -> bool,
    ) -> (EngineChoice, FakeIo) {
        run_with(
            hw,
            current,
            deepgram,
            groq,
            interactive,
            answers,
            manifest_exists,
            ConsentedStatus::NotInstalled,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_with(
        hw: &HostHardware,
        current: AsrMode,
        deepgram: &KeyDiagnosis,
        groq: &KeyDiagnosis,
        interactive: bool,
        answers: Vec<Option<&str>>,
        manifest_exists: impl Fn(&str) -> bool,
        consented: ConsentedStatus,
    ) -> (EngineChoice, FakeIo) {
        let mut io = FakeIo::new(answers);
        let choice = run_engine_screen(
            &mut io,
            current,
            hw,
            deepgram,
            groq,
            interactive,
            manifest_exists,
            consented,
        );
        (choice, io)
    }

    #[test]
    fn rtx_5050_laptop_suggests_distil_and_shows_cloud() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (choice, io) = run(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            true,
            vec![Some("")],
            |_| false,
        );
        let text = io.transcript();
        assert_eq!(choice, EngineChoice::Keep(AsrMode::Cloud));
        assert!(text.contains("Choose how Voisu transcribes:"), "{text}");
        assert!(
            text.contains("[1] Cloud — Deepgram + Groq (audio is sent to the providers)"),
            "{text}"
        );
        assert!(
            text.contains(
                "\n[2] Local — runs on this computer\nRecommended for this computer: distil-large-v3.5-int8 (RTX 5050 Laptop)\nAvailability: not installed — setup can download it (2.8 GiB) after you agree.\nPROMPT "
            ),
            "{text}"
        );
        for removed in [
            "WER",
            "Invented",
            "Hinglish",
            "Development evidence",
            "Developer pilot:",
            "Suggested",
        ] {
            assert!(!text.contains(removed), "{removed}: {text}");
        }
    }

    #[test]
    fn small_gpu_zero_suggests_parakeet_on_cpu() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 2048)]));
        let (_, io) = run(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            false,
            vec![],
            |_| false,
        );
        let text = io.transcript();
        assert!(
            text.contains("Recommended for this computer: parakeet-tdt-0.6b-v3 on CPU (RTX 5050 Laptop has 2048 MiB; 3584 MiB needed for the GPU model)"),
            "{text}"
        );
    }

    #[test]
    fn absent_nvidia_suggests_parakeet() {
        let hw = host(GpuProbe::NoneFound);
        let (_, io) = run(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            false,
            vec![],
            |_| false,
        );
        let text = io.transcript();
        assert!(
            text.contains(
                "Recommended for this computer: parakeet-tdt-0.6b-v3 on CPU (no NVIDIA GPU detected)\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn probe_failure_shows_gpu_status_unknown() {
        let hw = host(GpuProbe::Failed("driver failed".into()));
        let (_, io) = run(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            false,
            vec![],
            |_| false,
        );
        let text = io.transcript();
        assert!(text.contains("GPU status unknown"), "{text}");
        assert!(text.contains("parakeet-tdt-0.6b-v3"), "{text}");
    }

    #[test]
    fn enter_and_eof_keep_the_current_mode() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (enter, _) = run(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            true,
            vec![Some("")],
            |_| false,
        );
        assert_eq!(enter, EngineChoice::Keep(AsrMode::Cloud));
        let (eof, io) = run(
            &hw,
            AsrMode::Local,
            &absent(),
            &absent(),
            true,
            vec![],
            |_| false,
        );
        assert_eq!(eof, EngineChoice::Keep(AsrMode::Local));
        assert_eq!(io.prompts, 1);
    }

    #[test]
    fn switching_to_local_before_a_consented_install_has_no_extra_confirm() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (choice, io) = run(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            true,
            vec![Some("2"), Some("n")],
            |_| false,
        );
        // The Consented Install prompt that follows is the confirmation.
        assert_eq!(choice, EngineChoice::Switch(AsrMode::Local));
        assert_eq!(io.prompts, 1);
        assert_eq!(
            io.lines,
            vec![Some("n".to_owned())],
            "the answer is left for consent"
        );
        assert!(
            !io.transcript().contains("Switch to Local anyway?"),
            "{}",
            io.transcript()
        );
        assert!(consent_confirms_local_switch(
            &hw,
            &ConsentedStatus::NotInstalled
        ));
        assert!(!consent_confirms_local_switch(
            &hw,
            &ConsentedStatus::Installed {
                entry_id: CONSENTED_ENTRY.id.into(),
                device: "cuda:0".into()
            }
        ));
        assert!(!consent_confirms_local_switch(
            &hw,
            &ConsentedStatus::Unknown("busy".into())
        ));
        assert!(consent_confirms_local_switch(
            &host(GpuProbe::NoneFound),
            &ConsentedStatus::NotInstalled
        ));
    }

    #[test]
    fn switching_to_local_with_unknown_install_status_requires_confirm() {
        let hw = host(GpuProbe::NoneFound);
        let (yes, io) = run_with(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            true,
            vec![Some("2"), Some("y")],
            |_| false,
            ConsentedStatus::Unknown("store unavailable".into()),
        );
        assert_eq!(yes, EngineChoice::Switch(AsrMode::Local));
        assert!(
            io.transcript().contains(
                "Local has no installed model yet; Voisu will refuse to dictate until one is installed. Switch to Local anyway?"
            ),
            "{}",
            io.transcript()
        );
        let (no, _) = run_with(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            true,
            vec![Some("2"), Some("n")],
            |_| false,
            ConsentedStatus::Unknown("store unavailable".into()),
        );
        assert_eq!(no, EngineChoice::Keep(AsrMode::Cloud));
        let (already_local, io) = run_with(
            &hw,
            AsrMode::Local,
            &absent(),
            &absent(),
            true,
            vec![Some("2")],
            |_| false,
            ConsentedStatus::Unknown("store unavailable".into()),
        );
        assert_eq!(already_local, EngineChoice::Keep(AsrMode::Local));
        assert_eq!(io.prompts, 1);
    }

    #[test]
    fn choosing_cloud_from_local_switches_without_confirm() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (choice, io) = run(
            &hw,
            AsrMode::Local,
            &absent(),
            &absent(),
            true,
            vec![Some("1")],
            |_| false,
        );
        assert_eq!(choice, EngineChoice::Switch(AsrMode::Cloud));
        assert_eq!(io.prompts, 1);
        assert!(
            !io.transcript().contains("Switch to Local anyway?"),
            "{}",
            io.transcript()
        );
    }

    #[test]
    fn three_invalid_answers_keep_the_current_mode() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (choice, io) = run(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            true,
            vec![Some("nope"), Some("3"), Some("local")],
            |_| false,
        );
        assert_eq!(choice, EngineChoice::Keep(AsrMode::Cloud));
        assert_eq!(io.prompts, 3);
        assert!(
            io.transcript()
                .contains("Keeping cloud; no valid engine choice after 3 attempts."),
            "{}",
            io.transcript()
        );
    }

    #[test]
    fn non_interactive_prints_and_keeps_without_prompting() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let leftover = vec![Some("2"), Some("y")];
        let (choice, io) = run(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            false,
            leftover,
            |_| false,
        );
        assert_eq!(choice, EngineChoice::Keep(AsrMode::Cloud));
        assert_eq!(io.prompts, 0);
        assert!(
            io.transcript().contains(
                "Non-interactive: keeping cloud; use `voisu mode local|cloud` to change."
            ),
            "{}",
            io.transcript()
        );
        assert_eq!(io.lines.len(), 2, "piped answers must remain unconsumed");
    }

    #[test]
    fn developer_pilot_line_follows_the_suggested_id() {
        let rtx = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (choice, io) = run(
            &rtx,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            false,
            vec![],
            |id| id == "distil-large-v3.5-int8",
        );
        assert_eq!(choice, EngineChoice::Keep(AsrMode::Cloud));
        assert!(
            io.transcript().contains(
                "Developer pilot: `voisu local-dev select distil-large-v3.5-int8` then `voisu-dev on` (stops voisu.service and enables voisu-dev.service)."
            ),
            "{}",
            io.transcript()
        );
        let (_, io) = run(
            &rtx,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            false,
            vec![],
            |id| id == "parakeet-tdt-0.6b-v3",
        );
        assert!(
            !io.transcript().contains("Developer pilot:"),
            "{}",
            io.transcript()
        );
        let unknown = host(GpuProbe::Failed("driver failed".into()));
        let (_, io) = run(
            &unknown,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            false,
            vec![],
            |id| id == "parakeet-tdt-0.6b-v3",
        );
        assert!(
            io.transcript().contains(
                "Developer pilot: `voisu local-dev select parakeet-tdt-0.6b-v3` then `voisu-dev on`"
            ),
            "{}",
            io.transcript()
        );
    }

    #[test]
    fn local_confirm_three_invalid_answers_keep_current_and_leave_later_input() {
        let hw = host(GpuProbe::NoneFound);
        let (choice, io) = run_with(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            true,
            vec![
                Some("2"),
                Some("maybe"),
                Some("sure"),
                Some("ok"),
                Some("y"),
            ],
            |_| false,
            ConsentedStatus::Unknown("store unavailable".into()),
        );
        assert_eq!(choice, EngineChoice::Keep(AsrMode::Cloud));
        assert_eq!(
            io.prompts, 4,
            "engine choice plus three confirmation attempts"
        );
        assert_eq!(
            io.lines,
            vec![Some("y".to_owned())],
            "the later y must remain unread"
        );
        assert!(
            io.transcript()
                .contains("Keeping cloud; no valid confirmation after 3 attempts."),
            "{}",
            io.transcript()
        );
    }

    #[test]
    fn cloud_key_status_and_groq_only_hint() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (_, io) = run(
            &hw,
            AsrMode::Cloud,
            &found(KeyLocation::Keyring),
            &found(KeyLocation::EnvOverride),
            false,
            vec![],
            |_| false,
        );
        let text = io.transcript();
        assert!(text.contains("Deepgram: configured"), "{text}");
        assert!(text.contains("Groq: set via env"), "{text}");
        assert!(!text.contains(GROQ_ONLY_HINT), "{text}");
        let (_, io) = run(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &found(KeyLocation::Keyring),
            false,
            vec![],
            |_| false,
        );
        let text = io.transcript();
        assert!(text.contains("Deepgram: not configured"), "{text}");
        assert!(text.contains("Groq: configured"), "{text}");
        assert!(text.contains(GROQ_ONLY_HINT), "{text}");
    }

    #[test]
    fn key_status_covers_every_diagnosis_and_hint_only_when_deepgram_absent() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let groq_ok = found(KeyLocation::Keyring);
        let cases = [
            (found(KeyLocation::Keyring), "configured"),
            (found(KeyLocation::PlaintextFile), "configured"),
            (found(KeyLocation::EnvOverride), "set via env"),
            (KeyDiagnosis::EnvOverrideInvalid, "env override invalid"),
            (KeyDiagnosis::Absent, "not configured"),
            (KeyDiagnosis::Locked, "keyring locked — status unknown"),
            (
                KeyDiagnosis::Unavailable,
                "keyring unavailable — status unknown",
            ),
            (
                KeyDiagnosis::ToolMissing,
                "secret-tool missing — status unknown",
            ),
        ];
        for (diagnosis, status) in &cases {
            let (_, io) = run(
                &hw,
                AsrMode::Cloud,
                diagnosis,
                &absent(),
                false,
                vec![],
                |_| false,
            );
            let text = io.transcript();
            assert!(
                text.contains(&format!("Deepgram: {status}")),
                "{status}: {text}"
            );
            assert!(
                !text.contains(GROQ_ONLY_HINT),
                "hint with Groq absent ({status}): {text}"
            );

            let (_, io) = run(
                &hw,
                AsrMode::Cloud,
                diagnosis,
                &groq_ok,
                false,
                vec![],
                |_| false,
            );
            let text = io.transcript();
            assert!(
                text.contains(&format!("Deepgram: {status}")),
                "{status} with Groq: {text}"
            );
            assert_eq!(
                text.contains(GROQ_ONLY_HINT),
                matches!(diagnosis, KeyDiagnosis::Absent),
                "hint condition ({status}): {text}"
            );
        }
    }

    #[test]
    fn distil_availability_names_the_consented_install_and_size() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (_, io) = run(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            false,
            vec![],
            |_| false,
        );
        let text = io.transcript();
        assert!(
            text.contains(
                "Availability: not installed — setup can download it (2.8 GiB) after you agree."
            ),
            "{text}"
        );
        assert_eq!(
            format_size(CONSENTED_ENTRY.total_download_bytes),
            "2.8 GiB",
            "2,980,914,661 bytes"
        );
        assert!(text.contains(DISTIL_MODEL_ID), "{text}");
        assert!(!text.contains(LOCAL_AVAILABILITY), "{text}");
    }

    #[test]
    fn format_size_uses_binary_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(25 * 1024 * 1024 + 300 * 1024), "25.3 MiB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn parakeet_offers_consented_download_with_real_size() {
        for hw in [
            host(GpuProbe::Nvidia(vec![gpu(0, 2048)])),
            host(GpuProbe::NoneFound),
        ] {
            let (_, io) = run(
                &hw,
                AsrMode::Cloud,
                &absent(),
                &absent(),
                false,
                vec![],
                |_| false,
            );
            let text = io.transcript();
            assert!(
                text.contains("setup can download it (734.1 MiB) after you agree"),
                "{text}"
            );
        }
    }

    #[test]
    fn verified_install_shows_consented_availability_and_skips_the_switch_warning() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (choice, io) = run_with(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            true,
            vec![Some("2")],
            |_| false,
            ConsentedStatus::Installed {
                entry_id: CONSENTED_ENTRY.id.into(),
                device: "cuda:0".into(),
            },
        );
        assert_eq!(choice, EngineChoice::Switch(AsrMode::Local));
        let text = io.transcript();
        assert!(text.contains(INSTALLED_AVAILABILITY), "{text}");
        assert!(!text.contains("Switch to Local anyway?"), "{text}");
        assert_eq!(io.prompts, 1);
    }

    #[test]
    fn unknown_consented_status_is_printed_and_still_confirms_a_switch() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (choice, io) = run_with(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            true,
            vec![Some("2"), Some("n")],
            |_| false,
            ConsentedStatus::Unknown("the model store is busy or unavailable".into()),
        );
        assert_eq!(choice, EngineChoice::Keep(AsrMode::Cloud));
        let text = io.transcript();
        assert!(
            text.contains("Availability: unknown — the model store is busy or unavailable"),
            "{text}"
        );
        assert!(text.contains("Switch to Local anyway?"), "{text}");
    }
}
