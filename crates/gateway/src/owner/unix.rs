use std::{
    fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::net::{UnixListener, UnixStream};

fn user() -> u32 {
    unsafe { libc::geteuid() }
}

fn private(path: &Path, strict: bool) -> io::Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    if meta.uid() != user() || meta.file_type().is_symlink() || (strict && meta.mode() & 0o077 != 0)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Untrusted gateway control path",
        ));
    }
    Ok(meta)
}

fn client_path(home: &Path) -> io::Result<PathBuf> {
    let direct = home.join("gateway.sock");
    match private(&direct, false) {
        Ok(meta) if meta.file_type().is_socket() => return Ok(direct),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid control socket",
            ));
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let pointer = home.join("gateway.sock.path");
    let meta = private(&pointer, true)?;
    if !meta.is_file() || meta.len() > 8192 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid control pointer",
        ));
    }
    let path = PathBuf::from(
        fs::read_to_string(pointer)?
            .trim_start_matches('\u{feff}')
            .trim(),
    );
    if !path.is_absolute() || !private(&path, true)?.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Invalid fallback socket",
        ));
    }
    Ok(path)
}

pub async fn connect(home: &Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(client_path(home)?).await?;
    if stream.peer_cred()?.uid() != user() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Foreign control socket owner",
        ));
    }
    Ok(stream)
}

pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    pointer: Option<PathBuf>,
    _fallback: Option<tempfile::TempDir>,
}

impl Listener {
    pub fn bind(home: &Path) -> io::Result<Self> {
        let path = home.join("gateway.sock");
        // Caller holds the native owner lock and checked for a live stock owner.
        if let Ok(meta) = private(&path, true) {
            if meta.file_type().is_socket() {
                match std::os::unix::net::UnixStream::connect(&path) {
                    Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
                        fs::remove_file(&path)?
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "Control socket is in use",
                        ));
                    }
                }
            }
        }
        let (listener, path, pointer, _fallback) = match UnixListener::bind(&path) {
            Ok(l) => (l, path, None, None),
            Err(e)
                if path.as_os_str().as_encoded_bytes().len() >= 100
                    || e.raw_os_error() == Some(libc::EOPNOTSUPP) =>
            {
                let dir = tempfile::Builder::new().prefix("hermes-gw-").tempdir()?;
                let path = dir.path().join("control.sock");
                let l = UnixListener::bind(&path)?;
                let pointer = home.join("gateway.sock.path");
                (l, path, Some(pointer), Some(dir))
            }
            Err(e) => return Err(e),
        };
        let result = Self {
            listener,
            path,
            pointer,
            _fallback,
        };
        fs::set_permissions(&result.path, fs::Permissions::from_mode(0o600))?;
        if let Some(pointer) = &result.pointer {
            use std::io::Write;
            let mut f = tempfile::NamedTempFile::new_in(home)?;
            write!(f, "{}", result.path.display())?;
            f.persist(pointer)?;
        }
        Ok(result)
    }
    pub async fn accept(&mut self) -> io::Result<UnixStream> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            if stream.peer_cred().is_ok_and(|p| p.uid() == user()) {
                return Ok(stream);
            }
        }
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(p) = &self.pointer {
            let _ = fs::remove_file(p);
        }
        let _ = fs::remove_file(&self.path);
    }
}
