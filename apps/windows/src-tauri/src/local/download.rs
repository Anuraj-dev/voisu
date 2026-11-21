//! The Local model download: HTTPS only, resumable, cancellable, and
//! checked against its pinned SHA-256 and size before it is used.
//!
//! Each file downloads to `<name>.part`. A later download asks for the rest
//! with `Range`; a 206 appends only when its `Content-Range` continues the
//! part exactly, and anything else starts the file over. Only a file that
//! matches its pin is renamed into place, and only while the download is
//! not cancelled; a mismatch is deleted, so the next try starts clean.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use voisu_core::local_tier::{LocalTier, ModelFile};

use super::{HEALTH_CLIP, pins};

#[derive(Debug)]
pub enum DownloadError {
    Cancelled,
    Network(String),
    Disk(io::Error),
    /// The file did not match its pin and was deleted.
    Damaged,
}

impl DownloadError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Cancelled => "Download cancelled.",
            Self::Network(_) => {
                "Couldn't download the Local model. Check your connection and try again; it picks up where it stopped."
            }
            Self::Disk(_) => {
                "Couldn't save the Local model. Check that the drive has room for it and try again."
            }
            Self::Damaged => {
                "The downloaded model didn't match its checksum, so Voisu deleted it. Try again."
            }
        }
    }
}

/// For the log: what failed, never any file content.
impl fmt::Display for DownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Network(detail) => write!(f, "network: {detail}"),
            Self::Disk(error) => write!(f, "disk ({:?}): {error}", error.kind()),
            Self::Damaged => f.write_str("checksum mismatch"),
        }
    }
}

/// One download's cancel switch. Cancelling and moving a verified file into
/// place take the same lock, so once [`Cancel::cancel`] returns no file of
/// that download lands.
#[derive(Debug, Default)]
pub struct Cancel(Mutex<bool>);

impl Cancel {
    pub fn cancel(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
    }

    pub fn is_cancelled(&self) -> bool {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Renames the verified `part` to `target` unless cancelled first.
    fn publish(&self, part: &Path, target: &Path) -> Result<(), DownloadError> {
        let cancelled = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if *cancelled {
            return Err(DownloadError::Cancelled);
        }
        fs::rename(part, target).map_err(DownloadError::Disk)
    }
}

/// Downloads every file `tier` needs that is missing from `dir`. `progress`
/// gets the bytes of the whole install that are on disk so far.
pub async fn install(
    dir: &Path,
    tier: LocalTier,
    cancel: &Cancel,
    mut progress: impl FnMut(u64),
) -> Result<(), DownloadError> {
    fs::create_dir_all(dir).map_err(DownloadError::Disk)?;
    voisu_https();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        // Hugging Face redirects to its CDN; the checksum, not the host,
        // decides what is kept.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() == "https" && attempt.previous().len() < 10 {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|error| DownloadError::Network(error.to_string()))?;
    let mut done = 0;
    for pin in pins(tier) {
        let target = dir.join(pin.name);
        if !fs::metadata(&target).is_ok_and(|meta| meta.len() == pin.bytes) {
            // Cancelling ends the fetch wherever it waits, a stalled
            // connect or response included.
            tokio::select! {
                fetched = fetch(&client, pin, &target, cancel, |bytes| progress(done + bytes)) => {
                    fetched?;
                }
                () = cancelled(cancel) => return Err(DownloadError::Cancelled),
            }
        }
        done += pin.bytes;
        progress(done);
    }
    Ok(())
}

fn voisu_https() {
    // reqwest is built without a provider; ring, as everywhere in Voisu.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn part_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

/// What to do with the response to a request for a `total`-byte file, sent
/// with `Range: bytes=<have>-` when `have > 0`.
#[derive(Debug, PartialEq, Eq)]
enum Start {
    /// Write the body from this offset.
    At(u64),
    /// The server will not continue this part: delete it and ask again.
    Restart,
    /// The download failed.
    Refused,
}

fn body_start(
    status: u16,
    have: u64,
    content_range: Option<&str>,
    content_length: Option<u64>,
    total: u64,
) -> Start {
    match status {
        200 if content_length.is_none_or(|length| length == total) => Start::At(0),
        206 if have > 0 => {
            let continues =
                content_range
                    .and_then(parse_content_range)
                    .is_some_and(|(first, last, whole)| {
                        first == have
                            && Some(last) == total.checked_sub(1)
                            && whole.is_none_or(|whole| whole == total)
                    });
            let length_fits =
                content_length.is_none_or(|length| Some(length) == total.checked_sub(have));
            if continues && length_fits {
                Start::At(have)
            } else {
                Start::Restart
            }
        }
        // The part no longer fits the file.
        416 if have > 0 => Start::Restart,
        _ => Start::Refused,
    }
}

/// `bytes <first>-<last>/<total>`, with `*` as an unknown total.
fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let (span, whole) = value.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (first, last) = span.split_once('-')?;
    let first: u64 = first.trim().parse().ok()?;
    let last: u64 = last.trim().parse().ok()?;
    let whole = match whole.trim() {
        "*" => None,
        whole => Some(whole.parse().ok()?),
    };
    (first <= last).then_some((first, last, whole))
}

async fn fetch(
    client: &reqwest::Client,
    pin: ModelFile,
    target: &Path,
    cancel: &Cancel,
    mut progress: impl FnMut(u64),
) -> Result<(), DownloadError> {
    let part = part_path(target);
    let mut have = fs::metadata(&part).map_or(0, |meta| meta.len());
    if have > pin.bytes {
        remove_file(&part).map_err(DownloadError::Disk)?;
        have = 0;
    }
    let mut restarted = false;
    while have < pin.bytes {
        let mut request = client.get(pin.url);
        if have > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let mut response = request
            .send()
            .await
            .map_err(|error| DownloadError::Network(error.to_string()))?;
        let status = response.status().as_u16();
        let content_range = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok());
        let start = match body_start(
            status,
            have,
            content_range,
            response.content_length(),
            pin.bytes,
        ) {
            Start::At(start) => start,
            Start::Restart if !restarted => {
                restarted = true;
                remove_file(&part).map_err(DownloadError::Disk)?;
                have = 0;
                continue;
            }
            Start::Restart | Start::Refused => {
                return Err(DownloadError::Network(format!("HTTP {status}")));
            }
        };
        let mut file = if start == 0 {
            File::create(&part)
        } else {
            OpenOptions::new().append(true).open(&part)
        }
        .map_err(DownloadError::Disk)?;
        let mut written = start;
        progress(written);
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| DownloadError::Network(error.to_string()))?
        {
            written += chunk.len() as u64;
            if written > pin.bytes {
                drop(file);
                let _ = fs::remove_file(&part);
                return Err(DownloadError::Damaged);
            }
            file.write_all(&chunk).map_err(DownloadError::Disk)?;
            progress(written);
        }
        file.sync_all().map_err(DownloadError::Disk)?;
        if written < pin.bytes {
            // A short body: keep the part, so the next try resumes it.
            return Err(DownloadError::Network(format!(
                "body ended at {written} of {} bytes",
                pin.bytes
            )));
        }
        have = written;
    }
    let checked = part.clone();
    let matches = tokio::task::spawn_blocking(move || matches_pin(&checked, pin))
        .await
        .map_err(|error| DownloadError::Disk(io::Error::other(error)))?
        .map_err(DownloadError::Disk)?;
    if !matches {
        let _ = fs::remove_file(&part);
        return Err(DownloadError::Damaged);
    }
    // A cancel during the hash still wins.
    cancel.publish(&part, target)
}

/// Resolves once `cancel` is set; checked a few times a second so a stalled
/// connection still cancels promptly.
async fn cancelled(cancel: &Cancel) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn remove_file(path: &Path) -> io::Result<()> {
    fs::remove_file(path).or_else(|error| match error.kind() {
        io::ErrorKind::NotFound => Ok(()),
        _ => Err(error),
    })
}

fn matches_pin(path: &Path, pin: ModelFile) -> io::Result<bool> {
    Ok(fs::metadata(path)?.len() == pin.bytes && super::worker::sha256_file(path)? == pin.sha256)
}

/// Deletes both tiers' models, the health clip, any partial files and any
/// manifest a launch left behind. Call it only while no download runs and
/// the worker is stopped.
pub fn remove(dir: &Path) -> io::Result<()> {
    let files = [
        HEALTH_CLIP,
        LocalTier::Accurate.model(),
        LocalTier::Fast.model(),
    ];
    for pin in files {
        let target = dir.join(pin.name);
        remove_file(&part_path(&target))?;
        remove_file(&target)?;
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(super::worker::MANIFEST_PREFIX)
            && (name.ends_with(".json") || name.ends_with(".json.tmp"))
        {
            remove_file(&entry.path())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_206_resumes_only_when_it_continues_the_part_exactly() {
        let total = 1000;
        let resume =
            |range: &str, length: Option<u64>| body_start(206, 400, Some(range), length, total);
        assert_eq!(resume("bytes 400-999/1000", Some(600)), Start::At(400));
        assert_eq!(resume("bytes 400-999/1000", None), Start::At(400));
        assert_eq!(resume("bytes 400-999/*", None), Start::At(400));
        // Another offset, end, total or body length starts the file over.
        assert_eq!(resume("bytes 0-999/1000", None), Start::Restart);
        assert_eq!(resume("bytes 400-899/1000", None), Start::Restart);
        assert_eq!(resume("bytes 400-999/2000", None), Start::Restart);
        assert_eq!(resume("bytes 400-999/1000", Some(599)), Start::Restart);
        assert_eq!(resume("items 400-999/1000", None), Start::Restart);
        assert_eq!(resume("bytes 999-400/1000", None), Start::Restart);
        assert_eq!(body_start(206, 400, None, None, total), Start::Restart);
        // 206 to a request that asked for no range.
        assert_eq!(
            body_start(206, 0, Some("bytes 0-999/1000"), None, total),
            Start::Refused
        );
    }

    #[test]
    fn a_200_starts_over_and_errors_fail() {
        assert_eq!(body_start(200, 400, None, Some(1000), 1000), Start::At(0));
        assert_eq!(body_start(200, 0, None, None, 1000), Start::At(0));
        assert_eq!(body_start(200, 0, None, Some(999), 1000), Start::Refused);
        assert_eq!(body_start(416, 400, None, None, 1000), Start::Restart);
        assert_eq!(body_start(416, 0, None, None, 1000), Start::Refused);
        assert_eq!(body_start(404, 0, None, None, 1000), Start::Refused);
    }

    #[test]
    fn a_cancel_during_the_hash_keeps_the_file_out_of_place() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("model.part");
        let target = dir.path().join("model");
        fs::write(&part, b"verified").unwrap();
        let cancel = Cancel::default();
        cancel.cancel();
        assert!(matches!(
            cancel.publish(&part, &target),
            Err(DownloadError::Cancelled)
        ));
        assert!(part.exists() && !target.exists());

        Cancel::default().publish(&part, &target).unwrap();
        assert!(!part.exists() && target.exists());
    }

    #[test]
    fn only_the_pinned_bytes_pass_verification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.part");
        let pin = ModelFile {
            name: "hello",
            url: "https://example.invalid/hello",
            // sha256("hello")
            sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            bytes: 5,
        };
        fs::write(&path, b"hello").unwrap();
        assert!(matches_pin(&path, pin).unwrap());
        fs::write(&path, b"hellp").unwrap();
        assert!(!matches_pin(&path, pin).unwrap());
        fs::write(&path, b"hello!").unwrap();
        assert!(!matches_pin(&path, pin).unwrap());
    }

    #[test]
    fn remove_deletes_finished_and_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        for pin in [
            HEALTH_CLIP,
            LocalTier::Accurate.model(),
            LocalTier::Fast.model(),
        ] {
            let target = dir.path().join(pin.name);
            fs::write(&target, b"x").unwrap();
            fs::write(part_path(&target), b"x").unwrap();
        }
        fs::write(dir.path().join("manifest.json"), b"{}").unwrap();
        fs::write(dir.path().join("manifest-12-0.json"), b"{}").unwrap();
        fs::write(dir.path().join("manifest.json.tmp"), b"{}").unwrap();
        fs::write(dir.path().join("notes.txt"), b"mine").unwrap();
        remove(dir.path()).unwrap();
        let left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left, ["notes.txt"]);
        // Nothing left to remove is not an error.
        remove(dir.path()).unwrap();
    }
}
