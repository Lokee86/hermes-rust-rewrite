//! Immutable package directories selected by the installed plugin record.

use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

pub(super) const LEGACY: &str = "python-packages";
const GENERATIONS: &str = "python-package-generations";
const PREFIX: &str = "generation-";
const RANDOM_BYTES: usize = 16;

pub(super) fn stage(data: &Path) -> Result<TempDir> {
    checked_data_directory(data)?;
    let base = data.join(GENERATIONS);
    match fs::create_dir(&base) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    checked_directory(&base)?;
    Ok(tempfile::Builder::new()
        .prefix(PREFIX)
        .rand_bytes(RANDOM_BYTES)
        .tempdir_in(base)?)
}

pub(super) fn selector(staged: &TempDir) -> Result<String> {
    let value = staged
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .context("Could not identify the new Python package generation")?;
    validate(value)?;
    Ok(value.into())
}

pub(super) fn selected(data: &Path, selector: Option<&str>) -> Result<PathBuf> {
    let Some(selector) = selector else {
        return Ok(data.join(LEGACY));
    };
    validate(selector)?;
    checked_data_directory(data)?;
    let base = data.join(GENERATIONS);
    checked_directory(&base)?;
    let path = base.join(selector);
    checked_directory(&path)?;
    Ok(path)
}

fn validate(value: &str) -> Result<()> {
    ensure!(
        value.strip_prefix(PREFIX).is_some_and(|suffix| {
            suffix.len() == RANDOM_BYTES && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
        }),
        "Invalid Python package generation"
    );
    Ok(())
}

fn checked_data_directory(data: &Path) -> Result<()> {
    checked_directory(data.parent().context("Missing plugin data root")?)?;
    checked_directory(data)
}

fn checked_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).context("Python package directory is unavailable")?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const REPARSE_POINT: u32 = 0x400;
        ensure!(
            metadata.file_attributes() & REPARSE_POINT == 0,
            "Python package directory must not be a reparse point"
        );
    }
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Python package directory must be a regular directory"
    );
    Ok(())
}
