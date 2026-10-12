//! Setup engine screen: Cloud and Local suggestions, then an explicit choice.
//!
//! The core prints and reads through [`WizardIo`]. It does not persist ASR mode.

use voisu_core::{AsrMode, KeyDiagnosis, KeyLocation, Provider};

use crate::local_model::{
    HostHardware, LocalSuggestion, suggest_local, suggestion_lines, warnings,
};
use crate::setup::WizardIo;

const TITLE: &str = "Choose how Voisu transcribes:";
const CLOUD_TITLE: &str = "[1] Cloud — Deepgram + Groq (audio is sent to the providers)";
const LOCAL_TITLE: &str = "[2] Local — runs on this computer";
const CLOUD_SUGGESTION: &str = "Suggested for Hinglish, and whenever no Local model is installed.";
const GROQ_ONLY_HINT: &str =
    "No Deepgram key configured — run `voisu deepgram off` for the faster Groq-only path.";
const LOCAL_AVAILABILITY: &str =
    "Availability: not installed — no Local model can be installed by setup in this release yet.";
const LOCAL_SWITCH_CONFIRM: &str = "Local has no installed model yet; Voisu will refuse to dictate until one is installed. Switch to Local anyway?";
const MAX_ENGINE_ATTEMPTS: u32 = 3;

/// Outcome of the engine screen. The core does not persist the choice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EngineChoice {
    Keep(AsrMode),
    Switch(AsrMode),
}

/// Prints Cloud and Local together, then lets the user choose.
///
/// `interactive` is decided by the binary from `stdin().is_terminal()`.
/// `manifest_exists` is injected so tests never touch the filesystem.
#[must_use]
pub fn run_engine_screen(
    io: &mut dyn WizardIo,
    current: AsrMode,
    hw: &HostHardware,
    deepgram: &KeyDiagnosis,
    groq: &KeyDiagnosis,
    interactive: bool,
    manifest_exists: impl Fn(&str) -> bool,
) -> EngineChoice {
    write_engine_screen(io, hw, deepgram, groq, &manifest_exists);
    choose_engine(io, current, interactive)
}

fn write_engine_screen(
    io: &mut dyn WizardIo,
    hw: &HostHardware,
    deepgram: &KeyDiagnosis,
    groq: &KeyDiagnosis,
    manifest_exists: &impl Fn(&str) -> bool,
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
    io.writeln(CLOUD_SUGGESTION);
    if matches!(deepgram, KeyDiagnosis::Absent) && groq.is_configured() {
        io.writeln(GROQ_ONLY_HINT);
    }
    io.writeln("");
    io.writeln(LOCAL_TITLE);
    let suggestion = suggest_local(hw);
    for line in suggestion_lines(&suggestion) {
        io.writeln(&line);
    }
    for warning in warnings(hw) {
        io.writeln(&format!("Note: {warning}"));
    }
    io.writeln(LOCAL_AVAILABILITY);
    let model_id = suggested_model_id(&suggestion);
    if manifest_exists(model_id) {
        io.writeln(&format!(
            "Developer pilot: `voisu local-dev select {model_id}` then `voisu-dev on` (stops voisu.service and enables voisu-dev.service)."
        ));
    }
}

fn choose_engine(io: &mut dyn WizardIo, current: AsrMode, interactive: bool) -> EngineChoice {
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
fn ask_yes_no(io: &mut dyn WizardIo, question: &str, default_yes: bool) -> Option<bool> {
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
        let mut io = FakeIo::new(answers);
        let choice = run_engine_screen(
            &mut io,
            current,
            hw,
            deepgram,
            groq,
            interactive,
            manifest_exists,
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
        assert!(text.contains("distil-large-v3.5-int8"), "{text}");
        assert!(text.contains("Suggested for this hardware"), "{text}");
        assert!(
            text.contains("Suggested for Hinglish, and whenever no Local model is installed."),
            "{text}"
        );
        assert!(!text.contains("Recommended"), "{text}");
        assert!(!text.contains("Developer pilot:"), "no manifest: {text}");
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
        assert!(text.contains("parakeet-tdt-0.6b-v3 on CPU"), "{text}");
        assert!(
            text.contains("GPU 0 has 2048 MiB, below 3584 MiB"),
            "{text}"
        );
        assert!(!text.contains("Recommended"), "{text}");
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
        assert!(text.contains("parakeet-tdt-0.6b-v3"), "{text}");
        assert!(text.contains("No NVIDIA GPU detected"), "{text}");
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
    fn switching_to_local_from_cloud_requires_confirm() {
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (yes, io) = run(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            true,
            vec![Some("2"), Some("y")],
            |_| false,
        );
        assert_eq!(yes, EngineChoice::Switch(AsrMode::Local));
        assert!(
            io.transcript().contains(
                "Local has no installed model yet; Voisu will refuse to dictate until one is installed. Switch to Local anyway?"
            ),
            "{}",
            io.transcript()
        );
        let (no, _) = run(
            &hw,
            AsrMode::Cloud,
            &absent(),
            &absent(),
            true,
            vec![Some("2"), Some("n")],
            |_| false,
        );
        assert_eq!(no, EngineChoice::Keep(AsrMode::Cloud));
        let (already_local, io) = run(
            &hw,
            AsrMode::Local,
            &absent(),
            &absent(),
            true,
            vec![Some("2")],
            |_| false,
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
        let hw = host(GpuProbe::Nvidia(vec![gpu(0, 8151)]));
        let (choice, io) = run(
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
}
