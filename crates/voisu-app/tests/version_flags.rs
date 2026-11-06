//! The released binaries must answer the standard `--version`/`--help` probes
//! with exit 0, so packaging smoke gates (and users) can identify them without
//! accidentally launching the daemon or printing a usage error. Invalid
//! arguments still fail with the usage text and exit 2.

use std::process::{Command, Output, Stdio};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const EXPECTED_HELP: &str = "\
usage: voisu <command> [args]
       voisu                    open the interface (on a real terminal)

commands:
  setup                         set up Voisu step by step
  start                         start a Recording
  stop                          stop the Recording and deliver the text
  toggle                        start or stop a Recording
  status                        show what Voisu is doing
  shortcut                      show your Trigger Key
  history [--json|--verbose]    list recent Recordings
  export <recording-id>         print one Recording's details for a bug report
  doctor [--verbose|-v]         check that everything works
  key set <groq|deepgram>       save a key (reads it from standard input)
  key check <groq|deepgram>     check that a saved key works
  deepgram <on|off>             use Deepgram alongside Groq
  engine <local|cloud>          transcribe on this computer or online
  delivery [paste|copy]         paste the text for you, or only copy it
  writing [clean|exact]         fix grammar and misheard words, or keep your words as spoken
  layout [plain|auto|sections]  punctuation only, organize when you speak in parts, or add headings
  tone [formal|informal]        full words, or abbreviations like btw, no final full stop
  dictionary add <term>         add a word Voisu should spell your way
  dictionary remove <term>      remove a word
  dictionary list [--json]      list your words
  service <install|start|stop|restart|status|uninstall>
                                manage the Voisu service
  local remove [--yes]          delete the Local model
  update [--check|--dry-run] [--switch-to-bin]
                                update Voisu

options:
  -h, --help, help              show this help
  -V, --version                 show the version
";

fn run(bin: &str, arguments: &[&str]) -> Output {
    Command::new(bin)
        .args(arguments)
        .output()
        .expect("binary under test should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn voisu_version_flag_prints_name_and_version() {
    for flag in ["--version", "-V"] {
        let output = run(env!("CARGO_BIN_EXE_voisu"), &[flag]);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert_eq!(stdout(&output), format!("voisu {VERSION}\n"), "{flag}");
    }
}

#[test]
fn voisu_help_flag_prints_usage_and_exits_zero() {
    for flag in ["--help", "-h", "help"] {
        let output = run(env!("CARGO_BIN_EXE_voisu"), &[flag]);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert!(output.stderr.is_empty(), "{flag}: {output:?}");
        assert_eq!(stdout(&output), EXPECTED_HELP, "{flag}");
    }
}

#[test]
fn voisu_invalid_arguments_still_fail_with_usage_and_exit_two() {
    let output = run(env!("CARGO_BIN_EXE_voisu"), &["definitely-not-a-command"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("usage: voisu"),
        "{output:?}"
    );
}

#[test]
fn bare_voisu_off_a_terminal_prints_usage_and_exits_two() {
    let output = Command::new(env!("CARGO_BIN_EXE_voisu"))
        .stdin(Stdio::null())
        .output()
        .expect("binary under test should run");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim_end(),
        EXPECTED_HELP.trim_end()
    );
}

#[test]
fn voisu_daemon_version_flag_prints_name_and_version() {
    for flag in ["--version", "-V"] {
        let output = run(env!("CARGO_BIN_EXE_voisu-daemon"), &[flag]);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert_eq!(
            stdout(&output),
            format!("voisu-daemon {VERSION}\n"),
            "{flag}"
        );
    }
}

#[test]
fn voisu_daemon_help_flag_describes_the_daemon_and_exits_zero() {
    for flag in ["--help", "-h"] {
        let output = run(env!("CARGO_BIN_EXE_voisu-daemon"), &[flag]);
        assert!(output.status.success(), "{flag}: {output:?}");
        assert!(stdout(&output).contains("voisu-daemon"), "{flag}");
    }
}
