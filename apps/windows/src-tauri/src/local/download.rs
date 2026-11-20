//! The Local model download: HTTPS only, resumable, cancellable, and
//! checked against its pinned SHA-256 and size before it is used.
//!
//! Each file downloads to `<name>.part`. A later download asks for the rest
//! with `Range` (206 appends; a server that answers 200 starts it over).
//! Only a file that matches its pin is renamed into place; a mismatch is
//! deleted, so the next try starts clean.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::{PINS, Pin};

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
            Self::Disk(_) => "Couldn't save the Local model. Check that the drive has 300 MB free.",
            Self::Damaged => {
                "The downloaded model didn't match its checksum, so Voisu deleted it. Try again."
            }
        }
    }
}

/// Downloads every pinned file missing from `dir`. `progress` gets the bytes
/// of the whole install that are on disk so far.
pub async fn install(
    dir: &Path,
    cancel: &AtomicBool,
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
    for pin in PINS {
        let target = dir.join(pin.name);
        if !fs::metadata(&target).is_ok_and(|meta| meta.len() == pin.bytes) {
            fetch(&client, pin, &target, cancel, |bytes| {
                progress(done + bytes)
            })
            .await?;
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

/// Where the body of a response to a `Range: bytes=<have>-` request starts.
fn resume_offset(status: u16, have: u64) -> Option<u64> {
    match status {
        206 if have > 0 => Some(have),
        200 => Some(0),
        _ => None,
    }
}

async fn fetch(
    client: &reqwest::Client,
    pin: Pin,
    target: &Path,
    cancel: &AtomicBool,
    mut progress: impl FnMut(u64),
) -> Result<(), DownloadError> {
    let part = part_path(target);
    let mut have = fs::metadata(&part).map_or(0, |meta| meta.len());
    if have > pin.bytes {
        fs::remove_file(&part).map_err(DownloadError::Disk)?;
        have = 0;
    }
    if have < pin.bytes {
        let mut request = client.get(pin.url);
        if have > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let mut response = request
            .send()
            .await
            .map_err(|error| DownloadError::Network(error.to_string()))?;
        let status = response.status().as_u16();
        let Some(start) = resume_offset(status, have) else {
            if status == 416 {
                // The part no longer fits the file; start over next time.
                let _ = fs::remove_file(&part);
            }
            return Err(DownloadError::Network(format!("HTTP {status}")));
        };
        let mut file = if start == 0 {
            File::create(&part)
        } else {
            OpenOptions::new().append(true).open(&part)
        }
        .map_err(DownloadError::Disk)?;
        let mut written = start;
        progress(written);
        loop {
            let chunk = tokio::select! {
                chunk = response.chunk() => chunk,
                () = cancelled(cancel) => return Err(DownloadError::Cancelled),
            };
            let Some(chunk) = chunk.map_err(|error| DownloadError::Network(error.to_string()))?
            else {
                break;
            };
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
    fs::rename(&part, target).map_err(DownloadError::Disk)
}

/// Resolves once `cancel` is set; checked a few times a second so a stalled
/// connection still cancels promptly.
async fn cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn matches_pin(path: &Path, pin: Pin) -> io::Result<bool> {
    Ok(fs::metadata(path)?.len() == pin.bytes && super::worker::sha256_file(path)? == pin.sha256)
}

/// Deletes the downloaded files, any partial ones and the manifest.
pub fn remove(dir: &Path) -> io::Result<()> {
    let mut paths: Vec<PathBuf> = PINS
        .iter()
        .flat_map(|pin| {
            let target = dir.join(pin.name);
            [part_path(&target), target]
        })
        .collect();
    paths.push(dir.join("manifest.json"));
    for path in paths {
        fs::remove_file(&path).or_else(|error| match error.kind() {
            io::ErrorKind::NotFound => Ok(()),
            _ => Err(error),
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_file_resumes_only_on_206_and_restarts_on_200() {
        assert_eq!(resume_offset(206, 1000), Some(1000));
        assert_eq!(resume_offset(200, 1000), Some(0));
        assert_eq!(resume_offset(200, 0), Some(0));
        // 206 to a request that asked for no range, and any error.
        assert_eq!(resume_offset(206, 0), None);
        assert_eq!(resume_offset(404, 0), None);
        assert_eq!(resume_offset(416, 1000), None);
    }

    #[test]
    fn only_the_pinned_bytes_pass_verification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.part");
        let pin = Pin {
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
        for pin in PINS {
            let target = dir.path().join(pin.name);
            fs::write(&target, b"x").unwrap();
            fs::write(part_path(&target), b"x").unwrap();
        }
        fs::write(dir.path().join("manifest.json"), b"{}").unwrap();
        remove(dir.path()).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        // Nothing left to remove is not an error.
        remove(dir.path()).unwrap();
    }
}
