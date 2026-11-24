//! Attach a one-process job before the normal Windows job resumes the child.
use process_wrap::tokio::{CommandWrap, CommandWrapper};
use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::{Path, PathBuf},
};
use tokio::process::{Child, Command};
use windows::Win32::{
    Foundation::HANDLE,
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject,
        },
        Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED},
    },
};

#[derive(Debug, Default)]
pub(super) struct Limit {
    job: Option<OwnedHandle>,
}

impl CommandWrapper for Limit {
    fn pre_spawn(&mut self, command: &mut Command, _core: &CommandWrap) -> io::Result<()> {
        command.creation_flags((CREATE_NO_WINDOW | CREATE_SUSPENDED).0);
        let handle = unsafe { CreateJobObjectW(None, None) }?;
        // The unnamed job handle is owned here and is never inherited.
        let job = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_ACTIVE_PROCESS | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        info.BasicLimitInformation.ActiveProcessLimit = 1;
        unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        }?;
        self.job = Some(job);
        Ok(())
    }

    fn post_spawn(
        &mut self,
        _command: &mut Command,
        child: &mut Child,
        _core: &CommandWrap,
    ) -> io::Result<()> {
        let handle = self
            .job
            .as_ref()
            .ok_or_else(|| io::Error::other("Missing Git job"))?;
        let child_handle = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("Missing Git process"))?;
        // process-wrap runs post_spawn before JobObject::wrap_child resumes the child.
        unsafe { AssignProcessToJobObject(HANDLE(handle.as_raw_handle()), HANDLE(child_handle)) }?;
        Ok(())
    }
}

// Let Git discover and validate its metadata as usual. Fix the worktree so
// repository configuration cannot redirect these reports to another directory.
pub(super) fn worktree(cwd: &Path) -> &Path {
    cwd.ancestors()
        .find(|dir| match std::fs::symlink_metadata(dir.join(".git")) {
            Ok(_) => true,
            Err(error) => error.kind() != io::ErrorKind::NotFound,
        })
        .unwrap_or(cwd)
}

pub(super) fn git_executable(program: &Path, cwd: &Path) -> Option<PathBuf> {
    let directory = program.parent()?;
    if !["cmd", "bin"].iter().any(|name| {
        directory
            .file_name()
            .is_some_and(|entry| entry.eq_ignore_ascii_case(name))
    }) {
        return Some(program.to_owned());
    }
    let root = directory.parent()?;
    // Git for Windows installs a launcher in cmd/ and bin/. Run the actual
    // executable for this same installation, since the launcher needs a child.
    for arch in ["ucrt64", "mingw64", "mingw32", "clangarm64"] {
        let bin = root.join(arch).join("bin");
        if let Ok(candidate) = bin.join("git.exe").canonicalize() {
            if candidate.starts_with(root) && !candidate.starts_with(cwd) && candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    Some(program.to_owned())
}
