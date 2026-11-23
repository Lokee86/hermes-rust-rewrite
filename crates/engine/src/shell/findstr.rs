//! Windows FINDSTR file discovery for a single standalone file operand.
//!
//! FINDSTR owns text matching, filenames in results, and exit status. The /F
//! list contains only checked, pinned paths. Lists use ASCII because FINDSTR
//! reads narrow path names with legacy codepage behavior.
use super::{
    Error,
    policy::{FindstrSearch, findstr_candidate},
};
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::Write,
    os::windows::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
    time::{Duration, Instant},
};
use tempfile::{NamedTempFile, TempDir};
use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, ERROR_PATH_NOT_FOUND, HANDLE},
        Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_ATTRIBUTE_SYSTEM, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FindClose, FindFirstFileW, FindNextFileW,
            WIN32_FIND_DATAW,
        },
    },
    core::{HRESULT, PCWSTR},
};

const MAX_ENTRIES: usize = 16_384;
const MAX_HANDLES: usize = 1024;
const MAX_DEPTH: usize = 32;
const MAX_OUTPUT: u64 = 64 * 1024 * 1024;
const EXCLUDED: u32 =
    FILE_ATTRIBUTE_HIDDEN.0 | FILE_ATTRIBUTE_SYSTEM.0 | FILE_ATTRIBUTE_REPARSE_POINT.0;

pub(super) struct FileList {
    list: NamedTempFile,
    _handles: Vec<File>,
    empty: Option<TempDir>,
}

impl FileList {
    pub(super) fn build(cwd: &Path, search: &FindstrSearch<'_>) -> Result<Self, Error> {
        let (prefix, pattern) = search
            .operand
            .rsplit_once('\\')
            .unwrap_or(("", search.operand));
        let list = NamedTempFile::new().map_err(|_| Error::Failed)?;
        let mut scanner = Scanner {
            cwd,
            pattern,
            recursive: search.recursive,
            list,
            handles: Vec::new(),
            entries: 0,
            matched: false,
            output_budget: MAX_OUTPUT,
            started: Instant::now(),
        };
        // Keep enumerated paths fixed while FINDSTR opens its list. Each open
        // excludes sharing for writes and deletes and opens the reparse point
        // itself, allowing a post-open check before descent or list insertion.
        if !scanner.pin(cwd, true)? {
            return Err(Error::CwdUnavailable);
        }
        let mut directory = cwd.to_path_buf();
        for part in prefix
            .split('\\')
            .filter(|part| !part.is_empty() && *part != ".")
        {
            directory.push(part);
            if !scanner.pin(&directory, true)? {
                return scanner.finish();
            }
        }
        scanner.scan(&directory, prefix, 0)?;
        scanner.finish()
    }

    pub(super) fn arguments(&self, search: FindstrSearch<'_>) -> Result<Vec<String>, Error> {
        let mut args = Vec::new();
        if self.empty.is_none() {
            let path = self
                .list
                .path()
                .to_str()
                .filter(|path| path.is_ascii())
                .ok_or(Error::Unsupported)?;
            args.push(format!("/f:{path}"));
        }
        args.extend(search.args.into_iter().map(|arg| {
            arg.get(..4)
                .filter(|prefix| prefix.eq_ignore_ascii_case("/c:\""))
                .and_then(|_| arg[4..].strip_suffix('"'))
                .map(|pattern| format!("/c:{pattern}"))
                .unwrap_or_else(|| arg.to_owned())
        }));
        if self.empty.is_some() {
            args.push(search.operand.to_owned());
        }
        Ok(args)
    }

    pub(super) fn directory<'a>(&'a self, cwd: &'a Path) -> &'a Path {
        self.empty.as_ref().map_or(cwd, |dir| dir.path())
    }
}

struct Scanner<'a> {
    cwd: &'a Path,
    pattern: &'a str,
    recursive: bool,
    list: NamedTempFile,
    handles: Vec<File>,
    entries: usize,
    matched: bool,
    output_budget: u64,
    started: Instant,
}

impl Scanner<'_> {
    fn finish(self) -> Result<FileList, Error> {
        // An unmatched nonrecursive wildcard is diagnosed by native FINDSTR.
        // Let it format that diagnostic in a private empty directory.
        let empty = (!self.recursive && !self.matched)
            .then(TempDir::new)
            .transpose()
            .map_err(|_| Error::Failed)?;
        Ok(FileList {
            list: self.list,
            _handles: self.handles,
            empty,
        })
    }

    fn pin(&mut self, path: &Path, directory: bool) -> Result<bool, Error> {
        if self.handles.len() == MAX_HANDLES {
            return Err(Error::Unsupported);
        }
        // Read-attributes access is enough to hold the path in place.
        let handle = match OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES.0)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
        {
            Ok(handle) => handle,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && directory => {
                return Ok(false);
            }
            Err(_) => return Err(Error::Failed),
        };
        let metadata = handle.metadata().map_err(|_| Error::Failed)?;
        if metadata.file_attributes() & EXCLUDED != 0 || metadata.is_dir() != directory {
            return Ok(false);
        }
        if !directory {
            let path_len = path
                .strip_prefix(self.cwd)
                .map_err(|_| Error::Failed)?
                .as_os_str()
                .len();
            let estimate = metadata
                .len()
                .saturating_add(1)
                .saturating_mul(path_len as u64 + 24);
            self.output_budget = self
                .output_budget
                .checked_sub(estimate)
                .ok_or(Error::Unsupported)?;
        }
        self.handles.push(handle);
        Ok(true)
    }

    fn inspect(&mut self) -> Result<(), Error> {
        self.entries += 1;
        if self.entries > MAX_ENTRIES || self.started.elapsed() > Duration::from_secs(5) {
            return Err(Error::Unsupported);
        }
        Ok(())
    }

    fn scan(&mut self, directory: &Path, prefix: &str, depth: usize) -> Result<(), Error> {
        if depth > MAX_DEPTH {
            return Err(Error::Unsupported);
        }
        if !self.recursive {
            return enumerate(&directory.join(self.pattern), |entry| {
                self.inspect()?;
                if entry.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | EXCLUDED)
                    == FILE_ATTRIBUTE_DIRECTORY.0
                {
                    self.matched |= relative_name(prefix, entry)?.is_some();
                }
                self.file(directory, prefix, entry)
            });
        }
        // Win32 matches DOS wildcards and returns the long name even when the
        // pattern matches the short name. FINDSTR descends at each directory's
        // position in the enumeration, among ordinary files at that level.
        let mut matches = HashSet::new();
        enumerate(&directory.join(self.pattern), |entry| {
            self.inspect()?;
            if entry.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | EXCLUDED) == 0 {
                matches.insert(name(entry).to_vec());
            }
            Ok(())
        })?;
        enumerate(&directory.join("*"), |entry| {
            self.inspect()?;
            if entry.dwFileAttributes & EXCLUDED != 0 {
                return Ok(());
            }
            if entry.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
                let Some((name, relative)) = relative_name(prefix, entry)? else {
                    return Ok(());
                };
                let child = directory.join(name);
                if self.pin(&child, true)? {
                    self.scan(&child, &relative, depth + 1)?;
                }
            } else if matches.contains(name(entry)) {
                self.file(directory, prefix, entry)?;
            }
            Ok(())
        })
    }

    fn file(
        &mut self,
        directory: &Path,
        prefix: &str,
        entry: &WIN32_FIND_DATAW,
    ) -> Result<(), Error> {
        if entry.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY.0 | EXCLUDED) != 0 {
            return Ok(());
        }
        let Some((name, relative)) = relative_name(prefix, entry)? else {
            return Ok(());
        };
        let file = directory.join(name);
        if file.as_os_str().encode_wide().count() >= 260 {
            return Err(Error::Unsupported);
        }
        if self.pin(&file, false)? {
            self.matched = true;
            write!(self.list, "{relative}\r\n").map_err(|_| Error::Failed)?;
        }
        Ok(())
    }
}

fn name(entry: &WIN32_FIND_DATAW) -> &[u16] {
    let length = entry
        .cFileName
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(entry.cFileName.len());
    &entry.cFileName[..length]
}

fn relative_name(
    prefix: &str,
    entry: &WIN32_FIND_DATAW,
) -> Result<Option<(String, String)>, Error> {
    let name = String::from_utf16(name(entry)).map_err(|_| Error::Unsupported)?;
    if name == "." || name == ".." {
        return Ok(None);
    }
    let relative = if prefix.is_empty() {
        name.clone()
    } else {
        format!("{prefix}\\{name}")
    };
    if !findstr_candidate(&relative) {
        return Ok(None);
    }
    if !relative.is_ascii() {
        return Err(Error::Unsupported);
    }
    Ok(Some((name, relative)))
}

struct FindHandle(HANDLE);
impl Drop for FindHandle {
    fn drop(&mut self) {
        // SAFETY: This is the live handle returned by FindFirstFileW.
        let _ = unsafe { FindClose(self.0) };
    }
}

fn enumerate(
    path: &Path,
    mut visit: impl FnMut(&WIN32_FIND_DATAW) -> Result<(), Error>,
) -> Result<(), Error> {
    let query = path
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect::<Vec<_>>();
    let mut entry = WIN32_FIND_DATAW::default();
    // SAFETY: query is terminated and lives through the call. entry is writable.
    let handle = match unsafe { FindFirstFileW(PCWSTR(query.as_ptr()), &mut entry) } {
        Ok(handle) => FindHandle(handle),
        Err(error)
            if [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]
                .iter()
                .any(|missing| error.code() == HRESULT::from_win32(missing.0)) =>
        {
            return Ok(());
        }
        Err(_) => return Err(Error::Failed),
    };
    loop {
        visit(&entry)?;
        // SAFETY: handle is live and entry remains writable.
        match unsafe { FindNextFileW(handle.0, &mut entry) } {
            Ok(()) => {}
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) => {
                return Ok(());
            }
            Err(_) => return Err(Error::Failed),
        }
    }
}
