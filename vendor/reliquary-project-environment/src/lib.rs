//! Read-only access to repository-owned project history.
//!
//! Git remains authoritative for repository identity, revisions, trees, and file
//! bytes. This crate validates an existing association and returns bytes from an
//! exact Git tree; it does not create or persist project history.
//!
//! Modified for Mercury: Reliquary can also read history kept in Lore. Mercury does not use that, and
//! Lore brings a network stack with it, so a reference to a Lore project is refused here. The Git
//! files are Reliquary's, unchanged.

mod git_history;
mod git_repository;

pub use git_history::{read_git_revision_file, read_project_file_reference};
pub use git_repository::{current_git_revision, snapshot_git_project_file};

use reliquary_archive::{ProjectFileRef, ProjectRevisionRef};
use std::path::Path;

const NO_LORE: &str = "Lore project history is not available in this build";

pub fn read_lore_project_file(_project_dir: &Path, _reference: &ProjectFileRef) -> Result<Vec<u8>, String> {
    Err(NO_LORE.into())
}

pub fn checkpoint_lore_project(_project_dir: &Path) -> Result<ProjectRevisionRef, String> {
    Err(NO_LORE.into())
}
