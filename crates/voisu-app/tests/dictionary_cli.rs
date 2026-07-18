use std::fs;
use std::process::{Command, Output};

use tempfile::TempDir;

struct DictionaryCli {
    home: TempDir,
}

impl DictionaryCli {
    fn new() -> Self {
        Self {
            home: TempDir::new().expect("temporary config home"),
        }
    }

    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_voisu"))
            .args(arguments)
            .env("XDG_CONFIG_HOME", self.home.path())
            .env("HOME", self.home.path())
            .output()
            .expect("dictionary CLI should run")
    }

    fn dictionary_path(&self) -> std::path::PathBuf {
        self.home.path().join("voisu").join("dictionary.txt")
    }
}

#[test]
fn dictionary_add_creates_a_user_term() {
    let cli = DictionaryCli::new();

    let added = cli.run(&["dictionary", "add", "Anuraj"]);

    assert!(added.status.success(), "{added:?}");
    assert_eq!(fs::read_to_string(cli.dictionary_path()).unwrap(), "Anuraj\n");
}

#[test]
fn dictionary_add_is_idempotent_and_preserves_comments_and_order() {
    let cli = DictionaryCli::new();
    fs::create_dir_all(cli.dictionary_path().parent().unwrap()).unwrap();
    fs::write(
        cli.dictionary_path(),
        "# Names\nAnuraj\nTokio # runtime\n\n# project terms\n",
    )
    .unwrap();

    let duplicate = cli.run(&["dictionary", "add", "anuraj"]);
    assert!(duplicate.status.success(), "{duplicate:?}");
    assert!(String::from_utf8_lossy(&duplicate.stdout).contains("already present"));

    let added = cli.run(&["dictionary", "add", "Voisu"]);
    assert!(added.status.success(), "{added:?}");
    assert_eq!(
        fs::read_to_string(cli.dictionary_path()).unwrap(),
        "# Names\nAnuraj\nTokio # runtime\n\n# project terms\nVoisu\n"
    );
}

#[test]
fn dictionary_remove_is_case_insensitive_and_preserves_everything_else() {
    let cli = DictionaryCli::new();
    fs::create_dir_all(cli.dictionary_path().parent().unwrap()).unwrap();
    fs::write(
        cli.dictionary_path(),
        "# Names\nAnuraj\nTokio # runtime\n\n# project terms\nVoisu\n",
    )
    .unwrap();

    let removed = cli.run(&["dictionary", "remove", "tOkIo"]);

    assert!(removed.status.success(), "{removed:?}");
    assert_eq!(
        fs::read_to_string(cli.dictionary_path()).unwrap(),
        "# Names\nAnuraj\n\n# project terms\nVoisu\n"
    );
}

#[test]
fn dictionary_add_rejects_a_comment_marker_term_without_writing_it() {
    let cli = DictionaryCli::new();

    // A leading '#' is a whole-line comment; adding it would store vocabulary the
    // parser silently drops. It must fail with a nonzero exit and write nothing.
    let leading = cli.run(&["dictionary", "add", "#project"]);
    assert_eq!(leading.status.code(), Some(4), "{leading:?}");
    assert!(String::from_utf8_lossy(&leading.stderr).contains("comment"));

    // An inline " # " boundary would store only "Tokio" while claiming the whole
    // string was added.
    let inline = cli.run(&["dictionary", "add", "Tokio # runtime"]);
    assert_eq!(inline.status.code(), Some(4), "{inline:?}");

    // Neither rejected term reached the file.
    assert!(!cli.dictionary_path().exists(), "no dictionary written");

    // "C#" (# not preceded by whitespace) is a real term and is accepted.
    let sharp = cli.run(&["dictionary", "add", "C#"]);
    assert!(sharp.status.success(), "{sharp:?}");
    assert_eq!(fs::read_to_string(cli.dictionary_path()).unwrap(), "C#\n");
}

#[test]
fn dictionary_remove_reports_a_missing_term_with_the_dispatcher_not_found_exit_code() {
    let cli = DictionaryCli::new();

    let removed = cli.run(&["dictionary", "remove", "absent"]);

    assert_eq!(removed.status.code(), Some(4), "{removed:?}");
    assert!(String::from_utf8_lossy(&removed.stderr).contains("not found"));
}

#[test]
fn dictionary_list_shows_only_stored_user_terms_as_plain_lines_or_json() {
    let cli = DictionaryCli::new();
    fs::create_dir_all(cli.dictionary_path().parent().unwrap()).unwrap();
    fs::write(
        cli.dictionary_path(),
        "# Names\nAnuraj\nTokio # runtime\n\nC#\n",
    )
    .unwrap();

    let plain = cli.run(&["dictionary", "list"]);
    assert!(plain.status.success(), "{plain:?}");
    assert_eq!(String::from_utf8_lossy(&plain.stdout), "Anuraj\nTokio\nC#\n");

    let json = cli.run(&["dictionary", "list", "--json"]);
    assert!(json.status.success(), "{json:?}");
    assert_eq!(
        serde_json::from_slice::<Vec<String>>(&json.stdout).unwrap(),
        vec!["Anuraj", "Tokio", "C#"]
    );
}
