//! Listening TCP ports for a pid.
//!
//! Node's inspector port is 9229 unless `--inspect-port` moved it, and a busy
//! default port makes Node pick the next free one. Discovery is what tells
//! those cases apart. An explicit `--inspect` skips this scan.

#[cfg(any(target_os = "linux", test))]
use std::collections::HashSet;

/// Ports the process is listening on, or an empty list when the OS query fails.
///
/// Failure is not fatal: the caller can still try the default inspector port.
pub(crate) fn listening_ports(pid: u32) -> Vec<u16> {
    match listening_ports_result(pid) {
        Ok(ports) => ports,
        Err(err) => {
            tracing::debug!(pid, %err, "inspector port discovery failed");
            Vec::new()
        }
    }
}

/// Prefer 9229, then the remaining ports in ascending order, with duplicates removed.
pub(crate) fn prefer_inspector_port(mut ports: Vec<u16>) -> Vec<u16> {
    ports.sort_unstable();
    ports.dedup();
    if let Some(index) = ports.iter().position(|port| *port == 9229) {
        let preferred = ports.remove(index);
        ports.insert(0, preferred);
    }
    ports
}

fn listening_ports_result(pid: u32) -> Result<Vec<u16>, String> {
    #[cfg(target_os = "linux")]
    {
        linux_ports(pid)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        lsof_ports(pid)
    }
    #[cfg(windows)]
    {
        netstat_ports(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        Ok(Vec::new())
    }
}

#[cfg(target_os = "linux")]
fn linux_ports(pid: u32) -> Result<Vec<u16>, String> {
    let inodes = socket_inodes(pid)?;
    if inodes.is_empty() {
        return Ok(Vec::new());
    }
    let mut ports = Vec::new();
    for name in ["tcp", "tcp6"] {
        let path = format!("/proc/{pid}/net/{name}");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                tracing::debug!(%path, %err, "skipping proc net table");
                continue;
            }
        };
        ports.extend(parse_proc_net(&text, &inodes));
    }
    Ok(ports)
}

#[cfg(target_os = "linux")]
fn socket_inodes(pid: u32) -> Result<HashSet<u64>, String> {
    let dir = format!("/proc/{pid}/fd");
    let entries = std::fs::read_dir(&dir).map_err(|err| format!("reading {dir}: {err}"))?;
    let mut inodes = HashSet::new();
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(link) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let text = link.to_string_lossy();
        let Some(rest) = text.strip_prefix("socket:[") else {
            continue;
        };
        let Some(inode) = rest.strip_suffix(']') else {
            continue;
        };
        if let Ok(inode) = inode.parse() {
            inodes.insert(inode);
        }
    }
    Ok(inodes)
}

/// `inodes` are the process's `socket:[inode]` fds. `/proc/net/tcp` is
/// namespace-wide, so a listen socket is kept only when its inode matches.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_proc_net(text: &str, inodes: &HashSet<u64>) -> Vec<u16> {
    let mut ports = Vec::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 10 {
            continue;
        }
        if !fields[3].eq_ignore_ascii_case("0A") {
            continue;
        }
        let Ok(inode) = fields[9].parse::<u64>() else {
            continue;
        };
        if !inodes.contains(&inode) {
            continue;
        }
        let Some((_, port_hex)) = fields[1].rsplit_once(':') else {
            continue;
        };
        if let Ok(port) = u16::from_str_radix(port_hex, 16) {
            if port != 0 {
                ports.push(port);
            }
        }
    }
    ports
}

#[cfg(all(unix, not(target_os = "linux")))]
fn lsof_ports(pid: u32) -> Result<Vec<u16>, String> {
    let output = std::process::Command::new("lsof")
        .args(["-nP", "-a", "-p", &pid.to_string(), "-iTCP", "-sTCP:LISTEN"])
        .output()
        .map_err(|err| format!("running lsof: {err}"))?;
    if !output.status.success() && output.stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("lsof exited {}: {stderr}", output.status));
    }
    Ok(parse_lsof(&String::from_utf8_lossy(&output.stdout)))
}

#[cfg(any(all(unix, not(target_os = "linux")), test))]
pub(crate) fn parse_lsof(text: &str) -> Vec<u16> {
    let mut ports = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        for (index, field) in fields.iter().enumerate() {
            let listening = fields
                .get(index + 1)
                .is_some_and(|next| *next == "(LISTEN)")
                || field.ends_with("(LISTEN)");
            if !listening {
                continue;
            }
            let addr = field.trim_end_matches("(LISTEN)");
            if let Some(port) = port_from_addr(addr) {
                ports.push(port);
            }
        }
    }
    ports
}

#[cfg(windows)]
fn netstat_ports(pid: u32) -> Result<Vec<u16>, String> {
    let output = std::process::Command::new("netstat")
        .args(["-ano", "-p", "tcp"])
        .output()
        .map_err(|err| format!("running netstat: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("netstat exited {}: {stderr}", output.status));
    }
    Ok(parse_netstat(&String::from_utf8_lossy(&output.stdout), pid))
}

#[cfg(any(windows, test))]
pub(crate) fn parse_netstat(text: &str, pid: u32) -> Vec<u16> {
    let wanted = pid.to_string();
    let mut ports = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 5 {
            continue;
        }
        if !fields[0].eq_ignore_ascii_case("TCP") {
            continue;
        }
        if !fields[3].eq_ignore_ascii_case("LISTENING") {
            continue;
        }
        if fields[4] != wanted {
            continue;
        }
        if let Some(port) = port_from_addr(fields[1]) {
            ports.push(port);
        }
    }
    ports
}

#[cfg(any(not(target_os = "linux"), test))]
pub(crate) fn port_from_addr(addr: &str) -> Option<u16> {
    let addr = addr.trim();
    let port = if let Some(rest) = addr.strip_prefix('[') {
        rest.split_once(']')?.1.trim_start_matches(':')
    } else {
        addr.rsplit_once(':')?.1
    };
    let port = port.trim();
    if port.is_empty() {
        return None;
    }
    port.parse().ok().filter(|port| *port != 0)
}

#[cfg(test)]
mod tests {
    use super::{parse_lsof, parse_netstat, parse_proc_net, port_from_addr, prefer_inspector_port};
    use std::collections::HashSet;

    #[test]
    fn proc_net_keeps_only_this_process_listen_sockets() {
        let text = "\
  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:241D 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 42 1 0000000000000000 100 0 0 10 0
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 99 1 0000000000000000 100 0 0 10 0
   2: 0100007F:0050 0100007F:C000 01 00000000:00000000 00:00000000 00000000     0        0 42 1 0000000000000000 100 0 0 10 0
";
        let mut inodes = HashSet::new();
        inodes.insert(42);
        assert_eq!(parse_proc_net(text, &inodes), vec![0x241D]);
    }

    #[test]
    fn lsof_and_netstat_extract_listen_ports() {
        let lsof = "\
COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME
node 9 user 14u IPv4 0x1 0t0 TCP 127.0.0.1:9229 (LISTEN)
node 9 user 15u IPv6 0x2 0t0 TCP *:9230 (LISTEN)
node 9 user 16u IPv4 0x3 0t0 TCP 127.0.0.1:80 (ESTABLISHED)
";
        assert_eq!(parse_lsof(lsof), vec![9229, 9230]);

        let netstat = "\
  Proto  Local Address          Foreign Address        State           PID
  TCP    127.0.0.1:9229         0.0.0.0:0              LISTENING       9
  TCP    [::1]:9231             [::]:0                 LISTENING       9
  TCP    0.0.0.0:80             0.0.0.0:0              LISTENING       3
  TCP    127.0.0.1:443          1.2.3.4:5              ESTABLISHED     9
";
        assert_eq!(parse_netstat(netstat, 9), vec![9229, 9231]);
    }

    #[test]
    fn inspector_port_is_listed_first() {
        assert_eq!(
            prefer_inspector_port(vec![9230, 9229, 9229, 3000]),
            vec![9229, 3000, 9230]
        );
        assert_eq!(prefer_inspector_port(vec![3000]), vec![3000]);
        assert_eq!(port_from_addr("[::1]:9229"), Some(9229));
        assert_eq!(port_from_addr("*:0"), None);
    }
}
