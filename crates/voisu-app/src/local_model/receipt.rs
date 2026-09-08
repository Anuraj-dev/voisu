//! Private active receipt: pins catalog entry, ABI, and exact file digests.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::catalog::{CatalogEntry, sha256_hex};
use super::safe_fs::{self, SafeFsError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveReceipt {
    pub catalog_id: String,
    pub catalog_revision: String,
    pub runtime_abi: String,
    pub protocol: u32,
    pub language: String,
    pub artifact_id: String,
    pub files: BTreeMap<String, FilePin>,
    pub receipt_hash: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePin {
    pub sha256_hex: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReceiptError {
    Missing,
    Invalid(String),
    Io(String),
}

pub fn receipt_path(store: &Path) -> PathBuf {
    store.join("private").join("active.receipt")
}

pub fn retained_receipt_path(store: &Path) -> PathBuf {
    store.join("private").join("retained.receipt")
}

pub fn encode(receipt: &ActiveReceipt) -> String {
    let mut files = String::new();
    for (name, pin) in &receipt.files {
        files.push_str(&format!("{}:{}:{}\n", name, pin.bytes, pin.sha256_hex));
    }
    format!(
        "catalog_id={}\nrevision={}\nabi={}\nprotocol={}\nlanguage={}\nartifact={}\nfiles:\n{files}",
        receipt.catalog_id,
        receipt.catalog_revision,
        receipt.runtime_abi,
        receipt.protocol,
        receipt.language,
        receipt.artifact_id,
    )
}

pub fn parse(text: &str) -> Result<ActiveReceipt, ReceiptError> {
    let mut catalog_id = None;
    let mut revision = None;
    let mut abi = None;
    let mut protocol = None;
    let mut language = None;
    let mut artifact = None;
    let mut files = BTreeMap::new();
    let mut in_files = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line == "files:" {
            in_files = true;
            continue;
        }
        if in_files {
            let mut parts = line.split(':');
            let name = parts
                .next()
                .ok_or_else(|| ReceiptError::Invalid("file pin".into()))?;
            let bytes = parts
                .next()
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| ReceiptError::Invalid("file size".into()))?;
            let sha = parts
                .next()
                .ok_or_else(|| ReceiptError::Invalid("file digest".into()))?;
            if parts.next().is_some() {
                return Err(ReceiptError::Invalid("file pin extra field".into()));
            }
            files.insert(
                name.to_owned(),
                FilePin {
                    sha256_hex: sha.to_owned(),
                    bytes,
                },
            );
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ReceiptError::Invalid("malformed receipt".into()));
        };
        match key {
            "catalog_id" => catalog_id = Some(value.to_owned()),
            "revision" => revision = Some(value.to_owned()),
            "abi" => abi = Some(value.to_owned()),
            "protocol" => {
                protocol = Some(
                    value
                        .parse()
                        .map_err(|_| ReceiptError::Invalid("protocol".into()))?,
                );
            }
            "language" => language = Some(value.to_owned()),
            "artifact" => artifact = Some(value.to_owned()),
            _ => return Err(ReceiptError::Invalid(format!("unknown field {key}"))),
        }
    }
    let mut receipt = ActiveReceipt {
        catalog_id: catalog_id.ok_or_else(|| ReceiptError::Invalid("catalog_id".into()))?,
        catalog_revision: revision.ok_or_else(|| ReceiptError::Invalid("revision".into()))?,
        runtime_abi: abi.ok_or_else(|| ReceiptError::Invalid("abi".into()))?,
        protocol: protocol.ok_or_else(|| ReceiptError::Invalid("protocol".into()))?,
        language: language.ok_or_else(|| ReceiptError::Invalid("language".into()))?,
        artifact_id: artifact.ok_or_else(|| ReceiptError::Invalid("artifact".into()))?,
        files,
        receipt_hash: String::new(),
    };
    receipt.receipt_hash = hash_body(&encode(&receipt));
    Ok(receipt)
}

pub fn from_entry(entry: &CatalogEntry, artifact_id: &str) -> ActiveReceipt {
    let mut files = BTreeMap::new();
    for file in entry.files {
        files.insert(
            file.name.to_owned(),
            FilePin {
                sha256_hex: file.sha256_hex.to_owned(),
                bytes: file.bytes,
            },
        );
    }
    let mut receipt = ActiveReceipt {
        catalog_id: entry.id.to_owned(),
        catalog_revision: entry.revision.to_owned(),
        runtime_abi: entry.runtime_abi.abi_id.to_owned(),
        protocol: entry.runtime_abi.protocol,
        language: entry.language.to_owned(),
        artifact_id: artifact_id.to_owned(),
        files,
        receipt_hash: String::new(),
    };
    receipt.receipt_hash = hash_body(&encode(&receipt));
    receipt
}

pub fn load(store: &Path) -> Result<ActiveReceipt, ReceiptError> {
    match fs::read_to_string(receipt_path(store)) {
        Ok(text) => parse(&text),
        Err(error) if error.kind() == ErrorKind::NotFound => Err(ReceiptError::Missing),
        Err(error) => Err(ReceiptError::Io(error.to_string())),
    }
}

pub fn store_atomic(store: &Path, receipt: &ActiveReceipt) -> Result<(), ReceiptError> {
    let private = store.join("private");
    safe_fs::ensure_private_dir(&private).map_err(map_fs)?;
    let encoded = encode(receipt);
    let path = receipt_path(store);
    let parent = path
        .parent()
        .ok_or_else(|| ReceiptError::Io("receipt parent".into()))?;
    let mut staged = tempfile::Builder::new()
        .prefix(".receipt.")
        .tempfile_in(parent)
        .map_err(|error| ReceiptError::Io(error.to_string()))?;
    staged
        .write_all(encoded.as_bytes())
        .and_then(|()| staged.as_file().sync_all())
        .map_err(|error| ReceiptError::Io(error.to_string()))?;
    let persist = staged
        .persist(&path)
        .map_err(|error| ReceiptError::Io(error.error.to_string()))?;
    persist
        .sync_all()
        .map_err(|error| ReceiptError::Io(error.to_string()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|error| ReceiptError::Io(error.to_string()))?;
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| ReceiptError::Io(error.to_string()))?;
    Ok(())
}

pub fn retain_current(store: &Path) -> Result<(), ReceiptError> {
    let active = receipt_path(store);
    if !active.exists() {
        return Ok(());
    }
    let retained = retained_receipt_path(store);
    fs::copy(&active, &retained).map_err(|error| ReceiptError::Io(error.to_string()))?;
    File::open(&retained)
        .and_then(|file| file.sync_all())
        .map_err(|error| ReceiptError::Io(error.to_string()))?;
    Ok(())
}

fn hash_body(body: &str) -> String {
    sha256_hex(body.as_bytes())
}

fn map_fs(error: SafeFsError) -> ReceiptError {
    ReceiptError::Io(format!("{error:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model::catalog::ci_fixture_entry;

    #[test]
    fn receipt_pins_catalog_entry_and_abi() {
        let receipt = from_entry(ci_fixture_entry(), "abc");
        assert_eq!(receipt.catalog_id, "l3-health-fixture");
        assert_eq!(receipt.runtime_abi, "voisu-l3-test-abi");
        let parsed = parse(&encode(&receipt)).unwrap();
        assert_eq!(parsed.catalog_id, receipt.catalog_id);
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.receipt_hash.len(), 64);
    }
}
