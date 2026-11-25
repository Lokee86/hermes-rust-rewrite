//! Settings › Gateways › SSH: reach Hermes on another machine the way the
//! desktop's SSH mode does (hermes_cli/gateway_connections_ssh_remote.py):
//! upload a one-use session token under ~/.hermes/desktop-ssh/<ownership>/,
//! start `hermes serve --isolated --host 127.0.0.1 --port 0` with that token
//! file and an owner nonce, read the port from its HERMES_*_READY log line,
//! then forward a local port to it with `ssh -L`. Chats then use the remote
//! gateway path (remote.rs) over http://127.0.0.1:<local port>.
use super::connections::Ssh;
use std::{io::Write, path::Path, process::{Child, Command, Stdio}, sync::{Arc, Mutex}, time::{Duration, Instant}};

/// A live tunnel: the local URL, its session token, and what to tear down.
#[derive(Clone)]
pub(super) struct Tunnel {
    pub url: String,
    pub token: String,
    pub host: String,
    _guard: Arc<Guard>,
}
/// Owns the forward and the remote serve; closes both when the last Tunnel copy goes away.
struct Guard { remote_pid: String, files: String, ssh: Ssh, forward: Mutex<Option<Child>> }
impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(mut forward) = self.forward.lock() && let Some(mut child) = forward.take() { let _ = child.kill(); let _ = child.wait(); }
        // The token is worth nothing once its server is gone, and neither is the log.
        let _ = run(&self.ssh, &format!("kill {} 2>/dev/null; rm -f \"{files}.token\" \"{files}.log\"; true", self.remote_pid, files = self.files), None);
    }
}

fn hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
fn getrandom(buf: &mut [u8]) {
    // uuid v4 is backed by the OS RNG.
    for chunk in buf.chunks_mut(16) { let id = uuid::Uuid::new_v4(); chunk.copy_from_slice(&id.as_bytes()[..chunk.len()]); }
}

/// ssh argv for `target`, with the saved user, port and identity file. Batch
/// mode never prompts; the first host key is pinned (accept-new).
fn base(ssh: &Ssh) -> Result<Command, String> {
    let host = ssh.host.trim();
    if host.is_empty() { return Err("Enter an SSH host before connecting.".into()); }
    if host.starts_with('-') || ssh.user.starts_with('-') || ssh.key.starts_with('-') { return Err("That host is not valid.".into()); }
    let mut command = Command::new("ssh");
    command.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "StrictHostKeyChecking=accept-new", "-o", "ServerAliveInterval=15"]);
    #[cfg(test)]
    if let Some(path) = tests::KNOWN_HOSTS.get() { command.args(["-o", &format!("UserKnownHostsFile={}", path.display()), "-F", "/dev/null"]); }
    if !ssh.port.trim().is_empty() { command.args(["-p", ssh.port.trim()]); }
    if !ssh.key.trim().is_empty() { command.args(["-i", ssh.key.trim()]); }
    Ok(command)
}
fn target(ssh: &Ssh) -> String {
    if ssh.user.trim().is_empty() { ssh.host.trim().to_owned() } else { format!("{}@{}", ssh.user.trim(), ssh.host.trim()) }
}
fn run(ssh: &Ssh, script: &str, stdin: Option<&str>) -> Result<String, String> {
    let mut command = base(ssh)?;
    command.arg("--").arg(target(ssh)).arg(script).stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|_| "ssh is not available on this computer.".to_owned())?;
    if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) { let _ = pipe.write_all(input.as_bytes()); }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        return Err(if stderr.contains("timed out") { "SSH connection timed out. The host may be unreachable or asleep.".into() }
            else if stderr.contains("permission denied") { "The host refused this key. Check the user and identity file.".into() }
            else if stderr.contains("could not resolve") { "That host name could not be found.".into() }
            else { format!("The remote command failed: {}", String::from_utf8_lossy(&output.stderr).trim()) });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The 32-hex ownership id for this machine's connections (stable per library).
fn ownership_id(root: &Path) -> String {
    let path = root.join("ssh-ownership-id");
    if let Ok(id) = std::fs::read_to_string(&path) && id.trim().len() == 32 && id.trim().bytes().all(|b| b.is_ascii_hexdigit()) { return id.trim().to_owned(); }
    let id = hex(16); let _ = std::fs::write(&path, &id); id
}

/// The port in a serve log's ready line (READY_RE).
pub(super) fn ready_port(log: &str) -> Option<u16> {
    let regex = regex::Regex::new(r"(?:^|[^\w])HERMES_(?:BACKEND|DASHBOARD)_READY port=(\d+)").ok()?;
    regex.captures_iter(log).last().and_then(|c| c[1].parse().ok()).filter(|p| *p > 0)
}

/// Start the remote serve and the local forward. Blocking: run off the UI thread.
pub(super) fn connect(root: &Path, ssh: Ssh) -> Result<Tunnel, String> {
    let oid = ownership_id(root);
    let nonce = hex(8);
    let token = hex(32);
    let dir = format!("$HOME/.hermes/desktop-ssh/{oid}");
    let hermes = if ssh.hermes_path.trim().is_empty() { "\"$(command -v hermes || echo $HOME/.local/bin/hermes)\"".to_owned() }
        else { format!("'{}'", ssh.hermes_path.trim().replace('\'', "")) };
    // 1. The token file: owner-only, written from stdin so the secret is never in argv.
    run(&ssh, &format!("umask 077 && mkdir -p \"{dir}\" && cat > \"{dir}/{nonce}.token\""), Some(&token))?;
    // 2. The serve, detached, logging its ready line.
    // `cd` stands alone: in `cd && nohup … &` a subshell would hold the session open until the serve
    // exits, and `$!` would be that subshell.
    let spawn = format!("cd \"$HOME\" || exit 1; nohup env HERMES_DESKTOP=1 {hermes} serve --isolated --host 127.0.0.1 --port 0 \
        --ssh-session-token-file \"{dir}/{nonce}.token\" --ssh-owner-nonce {nonce} </dev/null >> \"{dir}/{nonce}.log\" 2>&1 & echo $!");
    let remote_pid = run(&ssh, &spawn, None)?.trim().to_owned();
    if remote_pid.is_empty() || !remote_pid.bytes().all(|b| b.is_ascii_digit()) { return Err("The remote Hermes did not start.".into()); }
    // From here on, giving up also stops what was started.
    let guard = Guard { remote_pid: remote_pid.clone(), files: format!("{dir}/{nonce}"), ssh: ssh.clone(), forward: Mutex::new(None) };
    // 3. Wait for the port announcement.
    let deadline = Instant::now() + Duration::from_secs(120);
    let remote_port = loop {
        let log = run(&ssh, &format!("cat \"{dir}/{nonce}.log\" 2>/dev/null; kill -0 {remote_pid} 2>/dev/null || echo HERMES_SERVE_EXITED"), None)?;
        if let Some(port) = ready_port(&log) { break port; }
        if log.contains("HERMES_SERVE_EXITED") {
            let last = log.lines().rev().find(|l| !l.trim().is_empty() && !l.contains("HERMES_SERVE_EXITED")).unwrap_or("no output");
            return Err(format!("The remote Hermes serve exited before announcing its port: {last}"));
        }
        if Instant::now() > deadline { return Err("Timed out waiting for the remote Hermes serve to announce its port (120s).".into()); }
        std::thread::sleep(Duration::from_millis(700));
    };
    // 4. A free local port and the forward.
    let local = std::net::TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).map_err(|e| e.to_string())?.port();
    let mut forward = base(&ssh)?;
    forward.args(["-N", "-o", "ExitOnForwardFailure=yes", "-L", &format!("127.0.0.1:{local}:127.0.0.1:{remote_port}"), "--", &target(&ssh)])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let child = forward.spawn().map_err(|_| "Could not start the SSH port forward.".to_owned())?;
    if let Ok(mut slot) = guard.forward.lock() { *slot = Some(child); }
    let deadline = Instant::now() + Duration::from_secs(15);
    while std::net::TcpStream::connect(("127.0.0.1", local)).is_err() {
        if Instant::now() > deadline { return Err("The SSH port forward did not come up.".into()); }
        std::thread::sleep(Duration::from_millis(200));
    }
    let host = ssh.host.clone();
    Ok(Tunnel { url: format!("http://127.0.0.1:{local}"), token, host, _guard: Arc::new(guard) })
}

impl std::fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("Tunnel([redacted])") }
}
impl Tunnel {
    /// Close the forward and stop the serve we started (only that pid).
    pub fn close(self) { drop(self) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// Keeps the test's host key out of the user's own known_hosts.
    pub(super) static KNOWN_HOSTS: OnceLock<std::path::PathBuf> = OnceLock::new();

    /// A real OpenSSH server for this user only, on a loopback port, with its own keys and home.
    #[cfg(unix)]
    #[test]
    fn connects_through_a_real_ssh_server_and_cleans_up_after_itself() {
        use std::{fs, io::Read, net::TcpListener, os::unix::fs::PermissionsExt};
        let sshd = Path::new("/usr/sbin/sshd");
        if !sshd.is_file() { eprintln!("skipped: no sshd"); return; }
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str| dir.path().canonicalize().unwrap().join(name);
        for name in ["host", "user"] {
            assert!(Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(at(name)).status().unwrap().success());
        }
        fs::copy(at("user.pub"), at("authorized")).unwrap();
        fs::create_dir(at("home")).unwrap();
        // Stands in for `hermes serve`: announces a port this test listens on, and stays up.
        let remote = TcpListener::bind("127.0.0.1:0").unwrap();
        fs::write(at("hermes"), format!("#!/bin/sh\necho \"$@\" > \"$HOME/args\"\necho HERMES_BACKEND_READY port={}\nexec sleep 300\n", remote.local_addr().unwrap().port())).unwrap();
        fs::set_permissions(at("hermes"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(at("command"), format!("#!/bin/sh\nexport HOME='{}'\nexec /bin/sh -c \"$SSH_ORIGINAL_COMMAND\"\n", at("home").display())).unwrap();
        fs::set_permissions(at("command"), fs::Permissions::from_mode(0o755)).unwrap();
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        fs::write(at("config"), format!("ListenAddress 127.0.0.1:{port}\nHostKey {}\nAuthorizedKeysFile {}\nPidFile none\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nForceCommand {}\n",
            at("host").display(), at("authorized").display(), at("command").display())).unwrap();
        struct Server(Child);
        impl Drop for Server { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
        let server = Command::new(sshd).args(["-D", "-e", "-f"]).arg(at("config")).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let mut server = Server(server);
        let started = Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            if server.0.try_wait().unwrap().is_some() || started.elapsed() > Duration::from_secs(10) {
                eprintln!("skipped: sshd does not run here without privileges");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        KNOWN_HOSTS.set(at("known_hosts")).unwrap();
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap().trim().to_owned();
        let ssh = Ssh { host: "127.0.0.1".into(), user, port: port.to_string(), key: at("user").display().to_string(), hermes_path: at("hermes").display().to_string(), ..Default::default() };
        let connecting = Instant::now();
        let tunnel = connect(dir.path(), ssh.clone()).unwrap();
        assert!(connecting.elapsed() < Duration::from_secs(30), "starting the remote server must not wait for it to exit");
        // Bytes sent to the local end arrive at the "remote" server.
        let local = tunnel.url.rsplit(':').next().unwrap().parse::<u16>().unwrap();
        let mut sent = std::net::TcpStream::connect(("127.0.0.1", local)).unwrap();
        sent.write_all(b"hello").unwrap();
        let mut got = [0; 5];
        // Connecting probed the forward first, which arrives as an empty connection.
        while remote.accept().unwrap().0.read_exact(&mut got).is_err() {}
        assert_eq!(&got, b"hello");
        let folder = at("home").join(".hermes/desktop-ssh").join(ownership_id(dir.path()));
        let files = || fs::read_dir(&folder).unwrap().flatten().collect::<Vec<_>>();
        let token = files().into_iter().find(|f| f.path().extension().is_some_and(|e| e == "token")).unwrap();
        assert_eq!(token.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(token.path()).unwrap(), tunnel.token);
        let args = fs::read_to_string(at("home").join("args")).unwrap();
        assert!(args.contains("serve --isolated") && !args.contains(&tunnel.token), "the secret stays out of argv");
        let pid = tunnel._guard.remote_pid.clone();
        tunnel.close();
        assert!(files().is_empty(), "the token and log are removed");
        let gone = (0..40).any(|_| { std::thread::sleep(Duration::from_millis(50)); !Command::new("kill").args(["-0", &pid]).stderr(Stdio::null()).status().unwrap().success() });
        assert!(gone, "the remote server is stopped");
        // A wrong key is reported as such.
        assert!(Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(at("other")).status().unwrap().success());
        let refused = connect(dir.path(), Ssh { key: at("other").display().to_string(), ..ssh }).unwrap_err();
        assert!(refused.contains("refused this key"), "{refused}");
    }

    #[test]
    fn reads_the_ready_port_and_rejects_bad_hosts() {
        assert_eq!(ready_port("starting\nHERMES_BACKEND_READY port=48211\n"), Some(48211));
        assert_eq!(ready_port("x HERMES_DASHBOARD_READY port=9119"), Some(9119));
        assert_eq!(ready_port("XHERMES_BACKEND_READY port=1"), None, "the marker must stand alone");
        assert_eq!(ready_port("HERMES_BACKEND_READY port=0"), None);
        assert!(base(&Ssh { host: "-oProxyCommand=x".into(), ..Default::default() }).is_err());
        assert_eq!(hex(16).len(), 32);
    }
}
