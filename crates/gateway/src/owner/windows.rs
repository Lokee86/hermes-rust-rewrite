use std::{
    ffi::c_void,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::Path,
    ptr,
};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::{
    Foundation::{HANDLE, LocalFree},
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        },
        GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER,
        TokenElevation, TokenSessionId, TokenUser,
    },
    System::{
        Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId},
        Threading::{OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION},
    },
};

fn token_word(token: HANDLE, field: TOKEN_INFORMATION_CLASS) -> io::Result<u32> {
    let mut value = 0u32;
    let mut size = 0;
    if unsafe { GetTokenInformation(token, field, ptr::from_mut(&mut value).cast(), 4, &mut size) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

#[derive(PartialEq, Eq)]
struct PeerIdentity {
    sid: String,
    elevated: u32,
    session: u32,
}

fn process_identity(pid: u32) -> io::Result<PeerIdentity> {
    // Process and token are pinned for the entire SID read. No impersonation is performed.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = OwnedHandle::from_raw_handle(process);
        let mut token = ptr::null_mut();
        if OpenProcessToken(process.as_raw_handle(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(token);
        let mut bytes = 0;
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            ptr::null_mut(),
            0,
            &mut bytes,
        );
        if bytes == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut buf = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            bytes,
            &mut bytes,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let mut sid = ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0;
        while *sid.add(len) != 0 {
            len += 1
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
        LocalFree(sid.cast());
        Ok(PeerIdentity {
            sid: s,
            elevated: token_word(token.as_raw_handle(), TokenElevation)?,
            session: token_word(token.as_raw_handle(), TokenSessionId)?,
        })
    }
}

fn pipe_name(home: &Path) -> io::Result<String> {
    let path = super::profile_id(home)
        .map_err(io::Error::other)?
        .replace('/', "\\")
        .to_lowercase();
    let digest = ring::digest::digest(&ring::digest::SHA256, path.as_bytes());
    let hash: String = digest.as_ref()[..8]
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect();
    Ok(format!(r"\\.\pipe\hermes-gateway-{hash}"))
}

fn same_user(handle: HANDLE, server: bool) -> io::Result<()> {
    let mut pid = 0;
    let result = unsafe {
        if server {
            GetNamedPipeServerProcessId(handle, &mut pid)
        } else {
            GetNamedPipeClientProcessId(handle, &mut pid)
        }
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    if process_identity(pid)? != process_identity(std::process::id())? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Foreign gateway control peer",
        ));
    }
    Ok(())
}
pub async fn connect(home: &Path) -> io::Result<NamedPipeClient> {
    let name = pipe_name(home)?;
    loop {
        // Identification level allows the stock server to authenticate, never to act as us.
        match ClientOptions::new().security_qos_flags(0x10000).open(&name) {
            Ok(client) => {
                same_user(client.as_raw_handle(), true)?;
                return Ok(client);
            }
            Err(e) if e.raw_os_error() == Some(231) => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await
            }
            Err(e) => return Err(e),
        }
    }
}

pub struct Listener {
    name: String,
    sid: String,
    next: NamedPipeServer,
}
impl Listener {
    pub fn bind(home: &Path) -> io::Result<Self> {
        let name = pipe_name(home)?;
        let sid = process_identity(std::process::id())?.sid;
        let next = Self::instance(&name, &sid, true)?;
        Ok(Self { name, sid, next })
    }
    fn instance(name: &str, sid: &str, first: bool) -> io::Result<NamedPipeServer> {
        // Protected owner-only DACL is in force from creation, including the first instance.
        let sddl: Vec<u16> = format!("O:{sid}D:P(A;;GA;;;{sid})\0")
            .encode_utf16()
            .collect();
        unsafe {
            let mut sd = ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut sd,
                ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut attrs = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            };
            let result = ServerOptions::new()
                .reject_remote_clients(true)
                .first_pipe_instance(first)
                .create_with_security_attributes_raw(
                    name,
                    ptr::from_mut(&mut attrs).cast::<c_void>(),
                );
            LocalFree(sd);
            result
        }
    }
    pub async fn accept(&mut self) -> io::Result<NamedPipeServer> {
        loop {
            match self.next.connect().await {
                Err(e) if matches!(e.raw_os_error(), Some(232 | 233)) => {}
                Err(e) => return Err(e),
                Ok(()) => {}
            }
            let new = Self::instance(&self.name, &self.sid, false)?;
            let connected = std::mem::replace(&mut self.next, new);
            if same_user(connected.as_raw_handle(), false).is_ok() {
                return Ok(connected);
            }
        }
    }
}
