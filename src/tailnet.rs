//! The optional Tailscale (or Headscale) integration.
//!
//! Everything here is a hint for the person, never a reason to trust anything:
//!
//! - the tailnet address of this machine can be put into a pairing code,
//! - the machines of the tailnet can be listed (online or not, and whether rustsync answers there),
//! - while someone pairs over the tailnet, Tailscale says which tailnet machine it is.
//!
//! It works by running the local `tailscale` program (`status --json`, `whois --json`); no account,
//! API key or internet access is involved, and it behaves the same with Headscale. Nothing is
//! stored: switch it off and nothing is left (a device you paired by a tailnet host name or address
//! simply keeps that host name or address, as for any other host name).
//!
//! Care taken with the program: it is only run from its usual install places or from the path
//! written in the config file (never found through the PATH, because the daemon may run as root),
//! only if the file is not writable by other users, with no input, a time limit and a cap on how
//! much it may print. What it prints is untrusted text: control and direction-changing characters
//! are removed and lengths are limited before anything is shown.

use crate::proto::{TailPeer, TailnetInfo};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::process::Command;

const RUN_TIMEOUT: Duration = Duration::from_secs(5);
const WHOIS_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const MAX_PEERS: usize = 1000;
const MAX_PROBES: usize = 128;
const MAX_TEXT: usize = 100;

/// Where the `tailscale` program is looked for when the config file does not say.
const USUAL_PLACES: &[&str] = &[
    "/usr/bin/tailscale",
    "/usr/sbin/tailscale",
    "/usr/local/bin/tailscale",
    "/usr/local/sbin/tailscale",
    "/opt/homebrew/bin/tailscale",
    "/snap/bin/tailscale",
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
];

// ---------------------------------------------------------------- finding and running the program

/// Is this file something we are willing to run?
fn check_program(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("the Tailscale program must be given with its full path ({})", path.display()));
    }
    let md = std::fs::metadata(path).map_err(|_| format!("{} does not exist", path.display()))?;
    if !md.is_file() {
        return Err(format!("{} is not a program", path.display()));
    }
    let mode = md.permissions().mode();
    if mode & 0o111 == 0 {
        return Err(format!("{} cannot be run", path.display()));
    }
    if mode & 0o022 != 0 {
        return Err(format!("{} can be changed by other users, so it is not run", path.display()));
    }
    Ok(())
}

/// The `tailscale` program: the one named in the config file, else the first usual place.
pub fn find_command(configured: &str) -> Result<PathBuf, String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        return check_program(&p).map(|_| p);
    }
    let mut refused: Option<String> = None;
    for place in USUAL_PLACES {
        let p = Path::new(place);
        if !p.exists() {
            continue;
        }
        match check_program(p) {
            Ok(()) => return Ok(p.to_path_buf()),
            Err(e) => refused = Some(e),
        }
    }
    Err(refused.unwrap_or_else(|| {
        "the Tailscale program was not found in its usual places (install Tailscale, or give its path as `command` in the [tailscale] section of the config file)".to_string()
    }))
}

/// Run the program and return what it printed. Errors are short sentences for the person.
async fn run(cmd: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run {}: {}", cmd.display(), e))?;
    let Some(mut out) = child.stdout.take() else {
        return Err("cannot read the answer of the Tailscale program".to_string());
    };
    let done = tokio::time::timeout(timeout, async {
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = vec![0u8; 8192];
        loop {
            let n = out.read(&mut chunk).await.map_err(|e| format!("cannot read the answer of tailscale: {}", e))?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > MAX_OUTPUT {
                return Err("tailscale printed far too much".to_string());
            }
        }
        let status = child.wait().await.map_err(|e| format!("tailscale: {}", e))?;
        Ok((buf, status))
    })
    .await;
    match done {
        Err(_) => {
            let _ = child.start_kill();
            Err("tailscale did not answer in time".to_string())
        }
        Ok(Err(e)) => {
            let _ = child.start_kill();
            Err(e)
        }
        Ok(Ok((buf, status))) => {
            if !status.success() {
                return Err("tailscale could not reach its background service (is Tailscale running?)".to_string());
            }
            String::from_utf8(buf).map_err(|_| "tailscale printed something that is not text".to_string())
        }
    }
}

// ---------------------------------------------------------------- reading what it says

/// Text from outside: no control or direction-changing characters, limited length.
fn clean(s: &str) -> String {
    let t: String = s
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
        })
        .take(MAX_TEXT)
        .collect();
    t.trim().to_string()
}

#[derive(Deserialize, Default)]
struct RawStatus {
    #[serde(rename = "BackendState")]
    backend_state: Option<String>,
    #[serde(rename = "Self")]
    me: Option<RawNode>,
    #[serde(rename = "Peer")]
    peers: Option<HashMap<String, Option<RawNode>>>,
    #[serde(rename = "CurrentTailnet")]
    current: Option<RawTailnet>,
}

#[derive(Deserialize, Default)]
struct RawTailnet {
    #[serde(rename = "Name")]
    name: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawNode {
    #[serde(rename = "HostName")]
    host_name: Option<String>,
    #[serde(rename = "DNSName")]
    dns_name: Option<String>,
    #[serde(rename = "TailscaleIPs")]
    ips: Option<Vec<String>>,
    #[serde(rename = "Online")]
    online: Option<bool>,
    #[serde(rename = "LastSeen")]
    last_seen: Option<String>,
    #[serde(rename = "OS")]
    os: Option<String>,
}

fn addresses(list: Option<Vec<String>>) -> Vec<String> {
    list.unwrap_or_default()
        .iter()
        .filter_map(|s| s.parse::<IpAddr>().ok())
        .map(|ip| ip.to_string())
        .take(8)
        .collect()
}

fn dns_name(s: Option<String>) -> String {
    clean(s.unwrap_or_default().trim_end_matches('.'))
}

fn problem(msg: impl Into<String>) -> TailnetInfo {
    TailnetInfo { enabled: true, problem: msg.into(), ..TailnetInfo::default() }
}

/// Understand the output of `tailscale status --json`.
pub fn parse_status(json: &str) -> Result<TailnetInfo, String> {
    let raw: RawStatus = serde_json::from_str(json).map_err(|e| format!("cannot read what tailscale says: {}", e))?;
    let state = raw.backend_state.unwrap_or_default();
    if state != "Running" {
        return Ok(problem(match state.as_str() {
            "Stopped" => "Tailscale is switched off on this machine".to_string(),
            "NeedsLogin" => "Tailscale needs you to log in".to_string(),
            "NeedsMachineAuth" => "this machine still has to be approved on the tailnet".to_string(),
            "Starting" => "Tailscale is still starting".to_string(),
            "" => "Tailscale did not say whether it is connected".to_string(),
            other => format!("Tailscale is not connected ({})", clean(other)),
        }));
    }
    let mut info = TailnetInfo { enabled: true, ..TailnetInfo::default() };
    info.tailnet = clean(&raw.current.and_then(|c| c.name).unwrap_or_default());
    if let Some(me) = raw.me {
        info.self_name = clean(&me.host_name.unwrap_or_default());
        info.self_dns = dns_name(me.dns_name);
        info.self_ips = addresses(me.ips);
    }
    let mut peers: Vec<TailPeer> = raw
        .peers
        .unwrap_or_default()
        .into_values()
        .flatten()
        .take(MAX_PEERS)
        .map(|n| TailPeer {
            name: clean(&n.host_name.unwrap_or_default()),
            dns: dns_name(n.dns_name),
            ips: addresses(n.ips),
            online: n.online.unwrap_or(false),
            last_seen: n.last_seen.as_deref().map(parse_rfc3339).unwrap_or(0),
            os: clean(&n.os.unwrap_or_default()),
            paired: String::new(),
            reachable: None,
        })
        .collect();
    // online machines first, then by name
    peers.sort_by(|a, b| b.online.cmp(&a.online).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    info.peers = peers;
    Ok(info)
}

/// Days since 1970-01-01 of a calendar date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// "2026-11-14T10:00:00.123Z" or "...+01:00" as seconds since 1970; 0 if it can't be read or is
/// the "never" date Tailscale uses (year 1).
pub fn parse_rfc3339(s: &str) -> u64 {
    fn go(s: &str) -> Option<i64> {
        let num = |from: usize, len: usize| -> Option<i64> {
            let part = s.get(from..from + len)?;
            if !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            part.parse().ok()
        };
        let b = s.as_bytes();
        if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') || b[13] != b':' || b[16] != b':' {
            return None;
        }
        let (y, mo, d, h, mi, se) = (num(0, 4)?, num(5, 2)?, num(8, 2)?, num(11, 2)?, num(14, 2)?, num(17, 2)?);
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
            return None;
        }
        let mut i = 19;
        if b.get(i) == Some(&b'.') {
            i += 1;
            while b.get(i).map_or(false, |c| c.is_ascii_digit()) {
                i += 1;
            }
        }
        let offset = match b.get(i)? {
            b'Z' | b'z' => 0,
            sign @ (b'+' | b'-') => {
                let (oh, om) = (num(i + 1, 2)?, num(i + 4, 2)?);
                if b.get(i + 3) != Some(&b':') || oh > 23 || om > 59 {
                    return None;
                }
                let v = oh * 3600 + om * 60;
                if *sign == b'+' {
                    v
                } else {
                    -v
                }
            }
            _ => return None,
        };
        let secs = days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se - offset;
        (secs > 0).then_some(secs)
    }
    go(s).map_or(0, |v| v as u64)
}

/// Is this address inside the tailnet's usual ranges (100.64.0.0/10 and fd7a:115c:a1e0::/48)?
/// Used to decide whether it is worth asking Tailscale about a connection.
pub fn in_tailnet_range(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xC0) == 64
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => in_tailnet_range(IpAddr::V4(v4)),
            None => {
                let s = v6.segments();
                s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
            }
        },
    }
}

#[derive(Deserialize, Default)]
struct RawWhois {
    #[serde(rename = "Node")]
    node: Option<RawWhoisNode>,
    #[serde(rename = "UserProfile")]
    user: Option<RawUser>,
}

#[derive(Deserialize, Default)]
struct RawWhoisNode {
    #[serde(rename = "Name")]
    name: Option<String>,
    #[serde(rename = "ComputedName")]
    computed_name: Option<String>,
    #[serde(rename = "Tags")]
    tags: Option<Vec<String>>,
    #[serde(rename = "Hostinfo")]
    host_info: Option<RawHostInfo>,
}

#[derive(Deserialize, Default)]
struct RawHostInfo {
    #[serde(rename = "Hostname")]
    hostname: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawUser {
    #[serde(rename = "LoginName")]
    login_name: Option<String>,
    #[serde(rename = "DisplayName")]
    display_name: Option<String>,
}

/// Understand the output of `tailscale whois --json`: "laptop (me@example.com)", or for a tagged
/// machine "build-1 (tag:ci, tag:linux)".
pub fn parse_whois(json: &str) -> Option<String> {
    let raw: RawWhois = serde_json::from_str(json).ok()?;
    let node = raw.node?;
    let first_label = |s: &str| s.trim_end_matches('.').split('.').next().unwrap_or("").to_string();
    let name = clean(
        &node
            .host_info
            .and_then(|h| h.hostname)
            .filter(|h| !h.trim().is_empty())
            .or(node.computed_name.filter(|n| !n.trim().is_empty()))
            .or(node.name.as_deref().map(first_label))
            .unwrap_or_default(),
    );
    if name.is_empty() {
        return None;
    }
    let tags: Vec<String> = node.tags.unwrap_or_default().iter().map(|t| clean(t)).filter(|t| !t.is_empty()).take(6).collect();
    let who = if !tags.is_empty() {
        tags.join(", ")
    } else {
        raw.user
            .map(|u| {
                let login = clean(&u.login_name.unwrap_or_default());
                if login.is_empty() {
                    clean(&u.display_name.unwrap_or_default())
                } else {
                    login
                }
            })
            .unwrap_or_default()
    };
    Some(if who.is_empty() { name } else { format!("{} ({})", name, who) })
}

// ---------------------------------------------------------------- asking Tailscale

/// Ask the local Tailscale client about the tailnet. Never fails: if there is nothing to show,
/// `problem` says why.
pub async fn fetch(configured: &str) -> TailnetInfo {
    fetch_with(configured, RUN_TIMEOUT).await
}

pub async fn fetch_with(configured: &str, timeout: Duration) -> TailnetInfo {
    let cmd = match find_command(configured) {
        Ok(c) => c,
        Err(e) => return problem(e),
    };
    match run(&cmd, &["status", "--json"], timeout).await {
        Ok(out) => parse_status(&out).unwrap_or_else(problem),
        Err(e) => problem(e),
    }
}

/// Who does Tailscale say is connecting from `ip`? None if it is not a tailnet address, or
/// Tailscale does not know (or does not answer).
pub async fn whois(configured: &str, ip: IpAddr) -> Option<String> {
    whois_with(configured, ip, WHOIS_TIMEOUT).await
}

pub async fn whois_with(configured: &str, ip: IpAddr, timeout: Duration) -> Option<String> {
    let ip = ip.to_canonical();
    if !in_tailnet_range(ip) {
        return None;
    }
    let cmd = find_command(configured).ok()?;
    let out = run(&cmd, &["whois", "--json", &ip.to_string()], timeout).await.ok()?;
    parse_whois(&out)
}

/// Mark the machines of the list that one of our paired devices is stored at: `devices` is
/// (what we call the device, the host or address stored for it).
pub fn mark_paired(info: &mut TailnetInfo, devices: &[(String, String)]) {
    let norm = |s: &str| s.trim().trim_end_matches('.').to_lowercase();
    for p in info.peers.iter_mut() {
        let mut mine: Vec<String> = p.ips.iter().map(|i| norm(i)).collect();
        if !p.dns.is_empty() {
            mine.push(norm(&p.dns));
        }
        if !p.name.is_empty() {
            mine.push(norm(&p.name));
        }
        if let Some((name, _)) = devices.iter().find(|(_, host)| !host.trim().is_empty() && mine.contains(&norm(host))) {
            p.paired = name.clone();
        }
    }
}

/// For the online machines (at most 128, the first address of each): does something accept a
/// connection on `port`? Only connects and hangs up; nothing is sent.
pub async fn probe(info: &mut TailnetInfo, port: u16) {
    probe_with(info, port, PROBE_TIMEOUT).await
}

pub async fn probe_with(info: &mut TailnetInfo, port: u16, timeout: Duration) {
    let mut set = tokio::task::JoinSet::new();
    let mut started = 0usize;
    for (i, p) in info.peers.iter().enumerate() {
        if !p.online || started >= MAX_PROBES {
            continue;
        }
        let Some(ip) = p.ips.iter().find_map(|s| s.parse::<IpAddr>().ok()) else { continue };
        started += 1;
        set.spawn(async move {
            let ok = matches!(tokio::time::timeout(timeout, TcpStream::connect((ip, port))).await, Ok(Ok(_)));
            (i, ok)
        });
    }
    while let Some(done) = set.join_next().await {
        if let Ok((i, ok)) = done {
            info.peers[i].reachable = Some(ok);
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    const TAILSCALE: &str = include_str!("../tests/fixtures/tailscale_status.json");
    const HEADSCALE: &str = include_str!("../tests/fixtures/headscale_status.json");
    const ALONE: &str = include_str!("../tests/fixtures/alone.json");
    const STOPPED: &str = include_str!("../tests/fixtures/stopped.json");
    const WHOIS: &str = include_str!("../tests/fixtures/whois.json");
    const WHOIS_TAGGED: &str = include_str!("../tests/fixtures/whois_tagged.json");

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name)
    }

    fn run_async<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    /// A little program that stands in for `tailscale`.
    fn fake(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustsync-fakets-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tailscale");
        std::fs::write(&path, format!("#!/bin/sh\n{}\n", body)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn a_tailscale_status_is_understood() {
        let info = parse_status(TAILSCALE).unwrap();
        assert!(info.enabled && info.problem.is_empty());
        assert_eq!(info.tailnet, "me@example.com");
        assert_eq!(info.self_name, "desk");
        assert_eq!(info.self_dns, "desk.tail-scales.ts.net");
        assert_eq!(info.self_ips, ["100.101.102.103", "fd7a:115c:a1e0::1"]);
        // online machines first, by name; odd characters and bad addresses are dropped
        let names: Vec<&str> = info.peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["laptop", "nas", "phone"]);
        let laptop = &info.peers[0];
        assert!(laptop.online);
        assert_eq!(laptop.dns, "laptop.tail-scales.ts.net");
        assert_eq!(laptop.os, "macOS");
        assert_eq!(laptop.last_seen, 1794650400);
        assert_eq!(info.peers[1].ips, ["100.101.102.106"]);
        let phone = &info.peers[2];
        assert!(!phone.online);
        assert_eq!(phone.last_seen, 1794601815); // an offset is taken into account
    }

    #[test]
    fn a_headscale_status_with_missing_fields_is_understood() {
        let info = parse_status(HEADSCALE).unwrap();
        assert!(info.problem.is_empty());
        assert_eq!(info.self_dns, "server.hs.example.net");
        assert_eq!(info.tailnet, "");
        let names: Vec<&str> = info.peers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["pi", "ghost"]); // the null peer is skipped; the offline one is last
        assert!(info.peers[1].ips.is_empty() && !info.peers[1].online && info.peers[1].last_seen == 0);
    }

    #[test]
    fn a_tailnet_without_other_machines_or_without_a_connection() {
        let alone = parse_status(ALONE).unwrap();
        assert!(alone.problem.is_empty() && alone.peers.is_empty());
        assert_eq!(alone.self_ips, ["100.100.0.1"]);
        let stopped = parse_status(STOPPED).unwrap();
        assert!(stopped.problem.contains("switched off"));
        assert!(stopped.peers.is_empty() && stopped.self_ips.is_empty());
        assert!(parse_status(r#"{"BackendState":"NeedsLogin"}"#).unwrap().problem.contains("log in"));
        assert!(parse_status("{}").unwrap().problem.contains("did not say"));
        assert!(parse_status("not json").is_err());
        assert!(parse_status("[1,2]").is_err());
    }

    #[test]
    fn times_are_read_in_all_the_usual_forms() {
        assert_eq!(parse_rfc3339("2026-11-14T10:00:00Z"), 1794650400);
        assert_eq!(parse_rfc3339("2026-11-14T10:00:00.5Z"), 1794650400);
        assert_eq!(parse_rfc3339("2026-11-14T11:00:00+01:00"), 1794650400);
        assert_eq!(parse_rfc3339("2026-11-14T05:00:00-05:00"), 1794650400);
        assert_eq!(parse_rfc3339("1970-01-02T00:00:00Z"), 86400);
        // never seen, or nonsense
        assert_eq!(parse_rfc3339("0001-01-01T00:00:00Z"), 0);
        for bad in ["", "yesterday", "2026-13-14T10:00:00Z", "2026-11-14T25:00:00Z", "2026-11-14T10:00:00", "2026-11-14T10:00:00+0100"] {
            assert_eq!(parse_rfc3339(bad), 0, "{}", bad);
        }
    }

    #[test]
    fn tailnet_addresses_are_recognised() {
        for yes in ["100.64.0.1", "100.101.102.103", "100.127.255.254", "fd7a:115c:a1e0::1", "::ffff:100.100.0.1"] {
            assert!(in_tailnet_range(yes.parse().unwrap()), "{}", yes);
        }
        for no in ["100.63.255.255", "100.128.0.1", "192.168.1.5", "10.0.0.1", "fd7a:115c:a1e1::1", "2001:db8::1"] {
            assert!(!in_tailnet_range(no.parse().unwrap()), "{}", no);
        }
    }

    #[test]
    fn who_is_connecting_is_summed_up() {
        assert_eq!(parse_whois(WHOIS).unwrap(), "laptop (me@example.com)");
        assert_eq!(parse_whois(WHOIS_TAGGED).unwrap(), "build-1 (tag:ci, tag:linux)");
        assert_eq!(parse_whois(r#"{"Node":{"Name":"x.ts.net."}}"#).unwrap(), "x");
        assert!(parse_whois("{}").is_none());
        assert!(parse_whois("garbage").is_none());
        // direction tricks and control characters do not get through
        let tricky = r#"{"Node":{"Hostinfo":{"Hostname":"a\u202eb\u0007"}},"UserProfile":{"LoginName":"x\ny"}}"#;
        assert_eq!(parse_whois(tricky).unwrap(), "ab (xy)");
    }

    /// Which programs are refused. (Called from the test below, not a test of its own: two tests
    /// writing and starting little programs at the same time can make the system answer
    /// "text file busy".)
    fn check_which_programs_are_refused() {
        assert!(find_command("tailscale").unwrap_err().contains("full path"));
        assert!(find_command("/nonexistent/tailscale").unwrap_err().contains("does not exist"));
        let ok = fake("safe", "exit 0");
        assert_eq!(find_command(ok.to_str().unwrap()).unwrap(), ok);
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(find_command(ok.to_str().unwrap()).unwrap_err().contains("other users"));
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(find_command(ok.to_str().unwrap()).unwrap_err().contains("cannot be run"));
        assert!(find_command(ok.parent().unwrap().to_str().unwrap()).unwrap_err().contains("not a program"));
    }

    #[test]
    fn the_program_is_asked_and_misbehaving_programs_are_handled() {
        check_which_programs_are_refused();
        run_async(async {
            // a stand-in that answers like the real one
            let body = format!(
                "case \"$1\" in status) cat '{}';; whois) cat '{}';; *) exit 2;; esac",
                fixture("tailscale_status.json"),
                fixture("whois.json")
            );
            let good = fake("good", &body);
            let cfg = good.to_str().unwrap();
            let info = fetch(cfg).await;
            assert!(info.problem.is_empty(), "{}", info.problem);
            assert_eq!(info.peers.len(), 3);
            let who = whois(cfg, "100.101.102.104".parse().unwrap()).await;
            assert_eq!(who.as_deref(), Some("laptop (me@example.com)"));
            // addresses outside the tailnet are not even asked about
            assert!(whois(cfg, "192.168.1.5".parse().unwrap()).await.is_none());

            // fails
            let failing = fake("failing", "exit 1");
            let p = fetch(failing.to_str().unwrap()).await.problem;
            assert!(p.contains("background service"), "{}", p);
            // prints rubbish
            let rubbish = fake("rubbish", "echo '<html>'");
            assert!(fetch(rubbish.to_str().unwrap()).await.problem.contains("cannot read"));
            // never answers
            let hangs = fake("hangs", "exec sleep 30");
            let p = fetch_with(hangs.to_str().unwrap(), Duration::from_millis(300)).await.problem;
            assert!(p.contains("did not answer"), "{}", p);
            // prints without end
            let endless = fake("endless", "exec head -c 6000000 /dev/zero");
            let p = fetch(endless.to_str().unwrap()).await.problem;
            assert!(p.contains("far too much"), "{}", p);
            // not there at all
            assert!(!fetch("/nonexistent/tailscale").await.problem.is_empty());
        });
    }

    #[test]
    fn paired_devices_are_recognised_by_their_stored_address() {
        let mut info = parse_status(TAILSCALE).unwrap();
        mark_paired(
            &mut info,
            &[
                ("My laptop".to_string(), "Laptop.tail-scales.ts.net".to_string()),
                ("NAS".to_string(), "100.101.102.106".to_string()),
                ("Elsewhere".to_string(), "192.168.1.9".to_string()),
                ("Empty".to_string(), String::new()),
            ],
        );
        assert_eq!(info.peers[0].paired, "My laptop");
        assert_eq!(info.peers[1].paired, "NAS");
        assert_eq!(info.peers[2].paired, "");
    }

    #[test]
    fn machines_that_answer_on_the_port_are_found() {
        run_async(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let open = listener.local_addr().unwrap().port();
            let closed = {
                let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                l.local_addr().unwrap().port()
            };
            let mut info = TailnetInfo {
                enabled: true,
                peers: vec![
                    TailPeer { name: "here".into(), ips: vec!["127.0.0.1".into()], online: true, ..TailPeer::default() },
                    TailPeer { name: "off".into(), ips: vec!["127.0.0.1".into()], online: false, ..TailPeer::default() },
                    TailPeer { name: "noaddr".into(), online: true, ..TailPeer::default() },
                ],
                ..TailnetInfo::default()
            };
            probe_with(&mut info, open, Duration::from_millis(500)).await;
            assert_eq!(info.peers[0].reachable, Some(true));
            assert_eq!(info.peers[1].reachable, None); // offline: not tried
            assert_eq!(info.peers[2].reachable, None);
            probe_with(&mut info, closed, Duration::from_millis(500)).await;
            assert_eq!(info.peers[0].reachable, Some(false));
        });
    }
}
