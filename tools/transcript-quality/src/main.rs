fn main() {
    if let Err(err) = transcript_quality::run(std::env::args().skip(1)) {
        eprintln!("transcript-quality: {err}");
        std::process::exit(1);
    }
}
