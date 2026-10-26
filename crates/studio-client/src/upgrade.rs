//! Narrow migration of the legacy 0.1.0 management process. Workers are never signalled.
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    fs::File,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::net::UnixStream;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Peer {
    pub pid: i32,
    pub uid: u32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub peer: Peer,
    birth: (u64, u64),
    executable: PathBuf,
    arguments: Vec<Vec<u8>>,
}

#[cfg(target_os = "macos")]
pub(crate) fn peer(stream: &UnixStream) -> Result<Peer> {
    let mut pid: libc::pid_t = 0;
    let mut length = std::mem::size_of_val(&pid) as libc::socklen_t;
    let mut uid = 0;
    let mut gid = 0;
    unsafe {
        if libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut length,
        ) != 0
            || length as usize != std::mem::size_of_val(&pid)
            || pid <= 0
        {
            bail!("Cannot verify the management socket's process");
        }
        if libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) != 0 || uid != libc::geteuid() {
            bail!("The management service is not owned by this user");
        }
    }
    Ok(Peer { pid, uid })
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn peer(_: &UnixStream) -> Result<Peer> {
    bail!("Automatic management updates require macOS peer credentials")
}

#[cfg(target_os = "macos")]
pub(crate) fn identity(peer: &Peer) -> Result<Identity> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let length = std::mem::size_of_val(&info);
    if unsafe {
        libc::proc_pidinfo(
            peer.pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            length as i32,
        )
    } != length as i32
        || info.pbi_uid != peer.uid
    {
        bail!("Cannot verify management process identity");
    }
    let mut maximum: libc::c_int = 0;
    let mut maximum_size = std::mem::size_of_val(&maximum);
    let mut maximum_mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    if unsafe {
        libc::sysctl(
            maximum_mib.as_mut_ptr(),
            2,
            (&mut maximum as *mut libc::c_int).cast(),
            &mut maximum_size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || !(1..=2 * 1024 * 1024).contains(&maximum)
    {
        bail!("Cannot determine the management argument inspection limit");
    }
    let mut bytes = vec![0u8; maximum as usize];
    let mut size = bytes.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, peer.pid];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        bail!(
            "Cannot inspect management process arguments: {}",
            std::io::Error::last_os_error()
        );
    }
    bytes.truncate(size);
    let (executable, arguments) = parse_arguments(&bytes)?;
    Ok(Identity {
        peer: peer.clone(),
        birth: (info.pbi_start_tvsec, info.pbi_start_tvusec),
        executable,
        arguments,
    })
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn identity(_: &Peer) -> Result<Identity> {
    bail!("Automatic management updates require macOS process identity")
}

fn parse_arguments(bytes: &[u8]) -> Result<(PathBuf, Vec<Vec<u8>>)> {
    use std::os::unix::ffi::OsStringExt;
    let argc = i32::from_ne_bytes(
        bytes
            .get(..4)
            .context("Missing process argument count")?
            .try_into()?,
    );
    if !(1..=4096).contains(&argc) {
        bail!("Invalid process argument count");
    }
    let bytes = &bytes[4..];
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .context("Missing process executable")?;
    let executable = PathBuf::from(std::ffi::OsString::from_vec(bytes[..end].to_vec()));
    let mut cursor = end;
    while bytes.get(cursor) == Some(&0) {
        cursor += 1;
    }
    let mut arguments = Vec::new();
    for _ in 0..argc {
        let tail = bytes.get(cursor..).context("Truncated process arguments")?;
        let length = tail
            .iter()
            .position(|byte| *byte == 0)
            .context("Truncated process argument")?;
        arguments.push(tail[..length].to_vec());
        cursor += length + 1;
    }
    Ok((executable, arguments))
}
pub(crate) fn verify_daemon(identity: &Identity) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    if identity.peer.pid == std::process::id() as i32
        || identity.executable.file_name() != Some(std::ffi::OsStr::new("locals-studio"))
        || identity.arguments.len() != 2
        || identity.arguments[1] != b"daemon"
        || Path::new(std::ffi::OsStr::from_bytes(&identity.arguments[0])).file_name()
            != Some(std::ffi::OsStr::new("locals-studio"))
    {
        bail!("The socket process is not a verified locals-studio daemon");
    }
    Ok(())
}

pub(crate) fn quiescent(snapshot: &Value) -> Result<()> {
    let servers = snapshot["servers"]
        .as_array()
        .context("Cannot verify running models")?;
    let downloads = snapshot["downloads"]
        .as_array()
        .context("Cannot verify downloads")?;
    if downloads.iter().any(|job| {
        !matches!(
            job.get("status")
                .or_else(|| job.get("state"))
                .and_then(Value::as_str),
            Some("paused" | "completed" | "failed" | "cancelled" | "error")
        )
    }) {
        bail!("Model downloads are still active or their state is unavailable");
    }
    for server in servers {
        if matches!(
            server["state"].as_str(),
            Some("starting" | "loading" | "draining" | "stopping")
        ) || server["active_requests"]
            .as_u64()
            .is_none_or(|count| count > 0)
        {
            bail!("Model requests or server operations are still active");
        }
        if server["pid"].is_null() {
            continue;
        }
        if !matches!(server["state"].as_str(), Some("ready" | "running")) {
            bail!("A model worker is not yet stable");
        }
        let row = snapshot["telemetry"]["workers"]
            .as_array()
            .and_then(|workers| {
                workers.iter().find(|worker| {
                    worker["profile_id"] == server["profile_id"] && worker["pid"] == server["pid"]
                })
            })
            .context("A running worker has no current telemetry")?;
        for key in ["started_at", "port"] {
            if !row[key].is_null() && !server[key].is_null() && row[key] != server[key] {
                bail!("A running worker's telemetry belongs to a previous process instance");
            }
        }
        if row["stats"]["active"]
            .as_array()
            .is_none_or(|active| !active.is_empty())
        {
            bail!("A model request is active or its activity is unavailable");
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn children_are_workers(process: &Identity, snapshot: &Value) -> Result<()> {
    let mut children = [0i32; 1024];
    let result = unsafe {
        libc::proc_listchildpids(
            process.peer.pid,
            children.as_mut_ptr().cast(),
            std::mem::size_of_val(&children) as i32,
        )
    };
    if result < 0 || children[children.len() - 1] != 0 {
        bail!("Cannot verify all management subprocesses");
    }
    let servers = snapshot["servers"]
        .as_array()
        .context("Cannot verify worker subprocesses")?;
    if children.iter().filter(|pid| **pid > 0).any(|pid| {
        !servers
            .iter()
            .any(|server| server["pid"].as_i64() == Some(*pid as i64))
    }) {
        bail!("Runtime installation or another management operation is still active");
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
pub(crate) fn children_are_workers(_: &Identity, _: &Value) -> Result<()> {
    bail!("Automatic management updates require macOS")
}

pub(crate) async fn lock(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    let started = tokio::time::Instant::now();
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(error.into());
        }
        if started.elapsed() > Duration::from_secs(2) {
            bail!("Another client is updating the management service");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn idle() -> Value {
        json!({"servers":[{"profile_id":"one","pid":123,"state":"ready","active_requests":0}],"downloads":[],"telemetry":{"workers":[{"profile_id":"one","pid":123,"stats":{"active":[]}}]}})
    }
    #[test]
    fn refuses_busy_and_unverifiable_work_without_unloading_ready_workers() {
        assert!(quiescent(&idle()).is_ok());
        for state in ["starting", "loading", "draining", "stopping"] {
            let mut value = idle();
            value["servers"][0]["state"] = json!(state);
            assert!(quiescent(&value).is_err());
        }
        for status in ["queued", "running", "verifying"] {
            let mut value = idle();
            value["downloads"] = json!([{"status":status}]);
            assert!(quiescent(&value).is_err());
        }
        for status in ["paused", "completed", "failed", "cancelled"] {
            let mut value = idle();
            value["downloads"] = json!([{"status":status}]);
            assert!(quiescent(&value).is_ok());
        }
        let mut value = idle();
        value["servers"][0]["active_requests"] = json!(1);
        assert!(quiescent(&value).is_err());
        value = idle();
        value["telemetry"]["workers"][0]["stats"]["active"] = json!([{}]);
        assert!(quiescent(&value).is_err());
        value = idle();
        value["telemetry"]["workers"][0]["pid"] = json!(124);
        assert!(quiescent(&value).is_err());
        value = idle();
        value["telemetry"]["workers"][0]["stats"] = Value::Null;
        assert!(quiescent(&value).is_err());
        for job in [
            json!({}),
            json!({"status":"unknown"}),
            json!({"state":"downloading"}),
            json!({"status":"resolving"}),
        ] {
            let mut value = idle();
            value["downloads"] = json!([job]);
            assert!(quiescent(&value).is_err());
        }
        let mut value = idle();
        value["downloads"] = json!([{"state":"paused"}]);
        assert!(quiescent(&value).is_ok());
        let mut value = idle();
        value["servers"][0]["started_at"] = json!("new");
        value["telemetry"]["workers"][0]["started_at"] = json!("old");
        assert!(quiescent(&value).is_err());
        let mut value = idle();
        value["servers"][0]["port"] = json!(1234);
        value["telemetry"]["workers"][0]["port"] = json!(5678);
        assert!(quiescent(&value).is_err());
    }
    #[test]
    fn process_arguments_and_identity_are_exact_and_preserve_paths_with_spaces() {
        let mut bytes = 2i32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/Applications/Locals Studio.app/Contents/MacOS/locals-studio\0\0/Applications/Locals Studio.app/Contents/MacOS/locals-studio\0daemon\0TOKEN=not-an-argument\0");
        let (executable, arguments) = parse_arguments(&bytes).unwrap();
        let mut process = Identity {
            peer: Peer { pid: 123, uid: 42 },
            birth: (1, 2),
            executable,
            arguments,
        };
        assert!(verify_daemon(&process).is_ok());
        process.arguments[1] = b"server".to_vec();
        assert!(verify_daemon(&process).is_err());
        process.arguments[1] = b"daemon".to_vec();
        process.executable = PathBuf::from("/tmp/python3");
        assert!(verify_daemon(&process).is_err());
        assert!(parse_arguments(b"short").is_err());
    }
    #[tokio::test]
    async fn filesystem_lock_serializes_independent_handles() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("upgrade.lock");
        let first = lock(&path).await.unwrap();
        let second = tokio::spawn({
            let path = path.clone();
            async move { lock(&path).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!second.is_finished());
        drop(first);
        assert!(second.await.unwrap().is_ok());
    }
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn socket_peer_credentials_come_from_the_kernel() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("peer.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let client = UnixStream::connect(&path).await.unwrap();
        let _server = listener.accept().await.unwrap();
        let peer = peer(&client).unwrap();
        assert_eq!(peer.pid, std::process::id() as i32);
        assert_eq!(peer.uid, unsafe { libc::geteuid() });
        let process = identity(&peer).unwrap();
        assert!(
            verify_daemon(&process).is_err(),
            "never signal our own test/client process"
        );
    }
}
