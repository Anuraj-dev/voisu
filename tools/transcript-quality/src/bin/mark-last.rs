fn main() {
    if let Err(err) = transcript_quality::run_mark_last(std::env::args().skip(1)) {
        eprintln!("mark-last: {err}");
        std::process::exit(1);
    }
}
