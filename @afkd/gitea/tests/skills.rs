//! The skill's `fetch_attachment.py`, run as an agent runs it — `python3` on the script, the
//! credentials and the issue in the environment — against the stateful fake Gitea through
//! `GITEA_BASE_URL`, directly and behind a tunnelling proxy, as a sandbox with a network
//! list runs it. Each leg reads back the file written and which requests rode a tunnel.

mod common;

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use common::fake::{Asset, FakeGitea, Seen, TOKEN};
use common::TempDir;

/// The token's own login.
const ME: &str = "afkd-bot";

const REPO: &str = "acme/widgets";

/// The issue the run works on.
const ISSUE: u64 = 7;

/// A screenshot pasted onto the issue: wide glyphs and spaces in its name.
const NAME: &str = "截图 2026-11-13 14.02.png";

/// The variables a proxy is named by, in both the cases Python reads.
const PROXY_VARS: [&str; 8] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

/// The screenshot's bytes, which no text decoding or line-ending change leaves alone.
fn screenshot() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    bytes.extend((0..=255u8).cycle().take(4096));
    bytes.extend_from_slice(b"\r\n\n\r\0IEND\xaeB`\x82");
    bytes
}

/// A fake holding the issue, with the screenshot attached to it.
fn issue() -> (FakeGitea, Asset) {
    let fake = FakeGitea::start(ME);
    fake.issue(
        REPO,
        ISSUE,
        "fix(tui): a wide glyph — 漢字 — splits the footer",
        "See the screenshot.",
        &[],
        &[],
    );
    let asset = fake.asset(REPO, ISSUE, NAME, &screenshot());
    (fake, asset)
}

/// `fetch_attachment.py` for asset `id`, with the fake's credentials, base and issue, a
/// scratch dir, and none of the selfdev run's pull request nor any proxy the test's own
/// environment names.
fn fetch_command(id: u64, fake: &FakeGitea, scratch: &TempDir) -> Command {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/gitea/fetch_attachment.py");
    let mut command = Command::new("python3");
    command
        .arg(path)
        .arg(id.to_string())
        .env("GITEA_TOKEN", TOKEN)
        .env("GITEA_BASE_URL", fake.base_url())
        .env("GITEA_REPO", REPO)
        .env("GITEA_ISSUE_NUMBER", ISSUE.to_string())
        .env_remove("GITEA_PR_NUMBER")
        .env("AFKD_SCRATCH_DIR", scratch.path())
        .env("PYTHONDONTWRITEBYTECODE", "1");
    for var in PROXY_VARS {
        command.env_remove(var);
    }
    command
}

/// Name `proxy` in every variable afkd's sandbox hands a child, with `NO_PROXY` emptied so
/// the loopback fake stands in for a remote host.
fn behind_proxy<'a>(command: &'a mut Command, proxy: &str) -> &'a mut Command {
    for var in &PROXY_VARS[..6] {
        command.env(var, proxy);
    }
    command.env("NO_PROXY", "").env("no_proxy", "")
}

/// One connection the proxy tunnelled: the `CONNECT` head it was sent, and the request
/// line it carried.
#[derive(Debug, Clone)]
struct Tunnel {
    head: String,
    request: String,
}

/// A proxy on a loopback port that answers every `CONNECT` by tunnelling to where it asks,
/// and records each tunnel; its URL, with credentials, and the records.
///
/// A tunnel is recorded before its request goes upstream, so every record is in place once
/// the helper has its replies.
fn tunnelling_proxy() -> (String, Arc<Mutex<Vec<Tunnel>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let at = listener.local_addr().expect("its address");
    let tunnels = Arc::new(Mutex::new(Vec::new()));
    let records = Arc::clone(&tunnels);
    thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let records = Arc::clone(&records);
            thread::spawn(move || tunnel(client, &records));
        }
    });
    (format!("http://afkd:t0k3n@{at}"), tunnels)
}

/// Serve one `CONNECT`: answer it, record it with the request line it carries, and copy
/// both ways until each side is done.
fn tunnel(mut client: TcpStream, records: &Mutex<Vec<Tunnel>>) {
    let head = read_until(&mut client, b"\r\n\r\n");
    let target = head.split(' ').nth(1).expect("a CONNECT target").to_owned();
    let mut upstream = TcpStream::connect(&target).expect("the fake answers");
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .expect("answer the CONNECT");
    let request = read_until(&mut client, b"\r\n");
    records
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(Tunnel {
            head,
            request: request.clone(),
        });
    upstream
        .write_all(request.as_bytes())
        .expect("pass the request line on");
    let (mut up_read, mut up_write) = (upstream.try_clone().expect("clone"), upstream);
    let mut client_read = client.try_clone().expect("clone");
    let back = thread::spawn(move || {
        let _ = std::io::copy(&mut up_read, &mut client);
        let _ = client.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut client_read, &mut up_write);
    let _ = up_write.shutdown(Shutdown::Write);
    let _ = back.join();
}

/// Read a stream byte by byte through the first `end`, as text.
fn read_until(stream: &mut TcpStream, end: &[u8]) -> String {
    let mut text = Vec::new();
    let mut byte = [0u8];
    while !text.ends_with(end) {
        stream
            .read_exact(&mut byte)
            .expect("a line through its end");
        text.push(byte[0]);
    }
    String::from_utf8(text).expect("a text line")
}

/// The fake's downloads of `asset`.
fn downloads(fake: &FakeGitea, asset: &Asset) -> Vec<Seen> {
    fake.seen()
        .into_iter()
        .filter(|s| s.path == format!("/attachments/{}", asset.uuid))
        .collect()
}

/// With no proxy named, the asset is downloaded straight from its host into the scratch
/// dir under its own name, byte for byte, and the written path is printed.
#[test]
fn fetch_attachment_downloads_the_asset_into_the_scratch_dir() {
    let (fake, asset) = issue();
    let scratch = TempDir::new("fetch-direct");
    let out = fetch_command(asset.id, &fake, &scratch)
        .output()
        .expect("run python3");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let written = scratch.path().join("attachments").join(NAME);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}\n", written.display())
    );
    assert!(
        std::fs::read(&written).unwrap() == screenshot(),
        "the screenshot came back changed"
    );
    assert_eq!(downloads(&fake, &asset).len(), 1, "{:?}", fake.seen());
}

/// Behind the proxy, the asset's download rides a tunnel too, past the API reads: the file
/// comes through byte for byte, and the token header with it, which the fake refuses a
/// download without.
#[test]
fn fetch_attachment_downloads_through_the_proxy_its_environment_names() {
    let (fake, asset) = issue();
    let scratch = TempDir::new("fetch-through-proxy");
    let (proxy, tunnels) = tunnelling_proxy();
    let out = behind_proxy(&mut fetch_command(asset.id, &fake, &scratch), &proxy)
        .output()
        .expect("run python3");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let written = scratch.path().join("attachments").join(NAME);
    assert!(
        std::fs::read(&written).unwrap() == screenshot(),
        "the screenshot came back changed"
    );
    assert_eq!(downloads(&fake, &asset).len(), 1, "{:?}", fake.seen());

    let tunnels = tunnels.lock().unwrap_or_else(PoisonError::into_inner);
    let download = format!("GET /attachments/{} ", asset.uuid);
    let carried: Vec<&Tunnel> = tunnels
        .iter()
        .filter(|t| t.request.starts_with(&download))
        .collect();
    assert_eq!(carried.len(), 1, "{tunnels:?}");
    let host = fake.base_url().trim_start_matches("http://");
    assert!(
        carried[0].head.starts_with(&format!("CONNECT {host} ")),
        "{:?}",
        carried[0]
    );
    assert!(
        carried[0]
            .head
            .contains("Proxy-Authorization: Basic YWZrZDp0MGszbg==\r\n"),
        "{:?}",
        carried[0]
    );
}
