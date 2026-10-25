use anyhow::Result;

#[cfg(any(target_os = "macos", test))]
use anyhow::{Context, bail};

/// Keep the public API's port unambiguous across loopback, LAN, and wildcard bindings.
pub(crate) async fn ensure_public_port_available(port: u16) -> Result<()> {
    if port == 0 {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::process::Command::new("/usr/sbin/lsof")
                .args([
                    "-nP",
                    "-a",
                    &format!("-iTCP:{port}"),
                    "-sTCP:LISTEN",
                    "-Fpcn",
                ])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .with_context(|| {
            format!("Checking TCP listeners for Studio API port {port} timed out after 3 seconds")
        })?
        .with_context(|| format!("Cannot inspect TCP listeners for Studio API port {port}"))?;
        let listeners = parse_lsof_output(output.status.code(), &output.stdout, &output.stderr)
            .with_context(|| format!("Cannot inspect TCP listeners for Studio API port {port}"))?;
        if !listeners.is_empty() {
            let details = listeners
                .iter()
                .map(|listener| {
                    format!(
                        "{} (PID {}, listening on {})",
                        listener.command,
                        listener.pid,
                        listener.addresses.join(", ")
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            bail!(
                "Studio API port {port} is already in use by {details}. Choose another Studio API port, or explicitly stop the external server before starting Studio."
            );
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
#[derive(Debug, PartialEq, Eq)]
struct Listener {
    pid: u32,
    command: String,
    addresses: Vec<String>,
}

#[cfg(any(target_os = "macos", test))]
fn parse_lsof_output(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Result<Vec<Listener>> {
    // lsof uses exit 1 without output when its selection matches no open files.
    if code == Some(1) && stdout.is_empty() && stderr.is_empty() {
        return Ok(Vec::new());
    }
    if code != Some(0) {
        bail!(
            "lsof failed (exit code {code:?}): {}",
            String::from_utf8_lossy(stderr).trim()
        );
    }
    if !stderr.is_empty() {
        bail!(
            "lsof reported an inspection error: {}",
            String::from_utf8_lossy(stderr).trim()
        );
    }
    let text = std::str::from_utf8(stdout).context("lsof returned invalid UTF-8")?;
    let mut listeners: Vec<Listener> = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        if let Some(pid) = line.strip_prefix('p') {
            let pid = pid
                .parse::<u32>()
                .ok()
                .filter(|pid| *pid != 0)
                .context("lsof returned an invalid listener PID")?;
            listeners.push(Listener {
                pid,
                command: String::new(),
                addresses: Vec::new(),
            });
        } else if let Some(command) = line.strip_prefix('c') {
            listeners
                .last_mut()
                .context("lsof returned a command without a PID")?
                .command = command.to_owned();
        } else if let Some(address) = line.strip_prefix('n') {
            if address.is_empty() {
                bail!("lsof returned an empty listener address");
            }
            listeners
                .last_mut()
                .context("lsof returned an address without a PID")?
                .addresses
                .push(address.to_owned());
        } else if !line.starts_with('f') {
            bail!("lsof returned an unexpected listener field");
        }
    }
    if listeners.is_empty()
        || listeners
            .iter()
            .any(|listener| listener.command.is_empty() || listener.addresses.is_empty())
    {
        bail!("lsof returned incomplete listener information");
    }
    Ok(listeners)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_processes_and_ipv4_ipv6_listener_addresses() {
        let listeners = parse_lsof_output(
            Some(0),
            b"p42\ncPython\nf7\nn127.0.0.1:8321\nf8\nn192.168.1.10:8321\np84\nclocals-studio\nn[::1]:8321\nn*:8321\n",
            b"",
        )
        .unwrap();
        assert_eq!(
            listeners,
            vec![
                Listener {
                    pid: 42,
                    command: "Python".into(),
                    addresses: vec!["127.0.0.1:8321".into(), "192.168.1.10:8321".into()],
                },
                Listener {
                    pid: 84,
                    command: "locals-studio".into(),
                    addresses: vec!["[::1]:8321".into(), "*:8321".into()],
                },
            ]
        );
    }

    #[test]
    fn only_empty_exit_one_means_no_listeners() {
        assert!(parse_lsof_output(Some(1), b"", b"").unwrap().is_empty());
        assert!(parse_lsof_output(Some(0), b"", b"").is_err());
        assert!(parse_lsof_output(Some(1), b"\n", b"").is_err());
        assert!(parse_lsof_output(Some(1), b"", b"permission denied").is_err());
        assert!(parse_lsof_output(Some(2), b"", b"").is_err());
        assert!(parse_lsof_output(None, b"", b"").is_err());
    }

    #[test]
    fn warnings_and_malformed_records_do_not_report_a_free_port() {
        let valid = b"p42\ncPython\nn127.0.0.1:8321\n";
        assert!(parse_lsof_output(Some(0), valid, b"cannot inspect all files").is_err());
        for malformed in [
            &b"pnot-a-pid\ncPython\nn*:8321\n"[..],
            &b"p0\ncPython\nn*:8321\n"[..],
            &b"cPython\nn*:8321\n"[..],
            &b"n*:8321\n"[..],
            &b"p42\nn*:8321\n"[..],
            &b"p42\ncPython\n"[..],
            &b"p42\ncPython\nn\n"[..],
            &b"p42\ncPython\nxunexpected\nn*:8321\n"[..],
            &b"p42\nc\xff\nn*:8321\n"[..],
        ] {
            assert!(parse_lsof_output(Some(0), malformed, b"").is_err());
        }
    }

    #[tokio::test]
    async fn ephemeral_ports_do_not_require_listener_inspection() {
        ensure_public_port_available(0).await.unwrap();
    }
}
