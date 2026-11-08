//! HTTPS through the system `curl`.
//!
//! wisplet has no TLS stack (it would add well over a megabyte to a 0.9 MiB binary), so the
//! few things that need HTTPS - `https://` trackers and the search engines - run `curl`, which
//! ships with macOS and virtually every Linux. It is only spawned when such a request is made, so
//! the download path and the idle daemon are unaffected.
//!
//! The URL usually comes from untrusted data (a `.torrent` file, a search result), so:
//! the scheme is checked here *and* restricted again with `--proto`/`--proto-redir`
//! (no `file://`, `ftp://`, ... even through a redirect); the URL is passed as the value of
//! `--url` so it can never be read as an option; `-q` skips the user's `.curlrc`; and the
//! response size and the total time are bounded.
//!
//! The binary can be replaced with the `WISPLET_CURL` environment variable (used by the
//! tests to run a fake `curl`).

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::error::{Error, Result};

const USER_AGENT: &str = "wisplet/0.1";

/// Which `curl` to run.
pub fn bin() -> String {
    std::env::var("WISPLET_CURL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "curl".into())
}

pub struct Request<'a> {
    pub url: &'a str,
    /// Send this as a POST body.
    pub body: Option<&'a [u8]>,
    pub content_type: &'a str,
    /// Responses longer than this are an error (the transfer is stopped).
    pub max_bytes: usize,
    pub timeout: Duration,
    /// Also allow plain `http://` (for a local Jackett-style endpoint). `https://` always works.
    pub allow_http: bool,
}

impl<'a> Request<'a> {
    pub fn get(url: &'a str, max_bytes: usize, timeout: Duration) -> Request<'a> {
        Request { url, body: None, content_type: "", max_bytes, timeout, allow_http: false }
    }
}

/// The scheme must be `https://` (or `http://` when allowed) and contain no control characters.
fn check_url(url: &str, allow_http: bool) -> Result<()> {
    let ok_scheme = url.starts_with("https://") || (allow_http && url.starts_with("http://"));
    if !ok_scheme || url.chars().any(|c| c.is_control() || c == ' ') {
        return Err(Error::Failed(format!("refusing to fetch this URL: {}", url.chars().take(80).collect::<String>())));
    }
    Ok(())
}

fn args(req: &Request<'_>) -> Vec<String> {
    let proto = if req.allow_http { "=https,http" } else { "=https" };
    let mut a: Vec<String> = vec![
        "-q".into(), // must be first: ignore ~/.curlrc
        "--silent".into(),
        "--show-error".into(),
        "--fail".into(),
        "--location".into(),
        "--max-redirs".into(),
        "3".into(),
        "--proto".into(),
        proto.into(),
        "--proto-redir".into(),
        proto.into(),
        "--connect-timeout".into(),
        "8".into(),
        "--max-time".into(),
        req.timeout.as_secs().max(1).to_string(),
        "--compressed".into(),
        "--user-agent".into(),
        USER_AGENT.into(),
    ];
    if req.body.is_some() {
        a.extend(["-H".into(), format!("Content-Type: {}", req.content_type), "--data-binary".into(), "@-".into()]);
    }
    a.extend(["--url".into(), req.url.into()]);
    a
}

fn fetch_blocking(bin: &str, req: &Request<'_>) -> Result<Vec<u8>> {
    check_url(req.url, req.allow_http)?;
    let mut child = None;
    for attempt in 0..5 {
        let spawned = Command::new(bin)
            .args(args(req))
            .stdin(if req.body.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        match spawned {
            Ok(c) => {
                child = Some(c);
                break;
            }
            // "text file busy": the binary was only just written (happens with scripts in tests)
            Err(e) if e.raw_os_error() == Some(26) && attempt < 4 => std::thread::sleep(Duration::from_millis(20)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::Failed(format!(
                    "`{bin}` was not found; it is needed for https:// requests (install curl or set WISPLET_CURL)"
                )))
            }
            Err(e) => return Err(e.into()),
        }
    }
    let mut child = child.ok_or_else(|| Error::Failed("could not start curl".into()))?;
    if let (Some(body), Some(mut stdin)) = (req.body, child.stdin.take()) {
        let _ = stdin.write_all(body); // curl may exit early on a bad URL; its status tells us
    }
    let mut out = Vec::new();
    let mut stdout = child.stdout.take().expect("piped");
    stdout.by_ref().take(req.max_bytes as u64 + 1).read_to_end(&mut out)?;
    if out.len() > req.max_bytes {
        let _ = child.kill();
        let _ = child.wait();
        return Err(Error::Failed(format!("response larger than {} bytes", req.max_bytes)));
    }
    let mut err_text = String::new();
    if let Some(stderr) = child.stderr.take() {
        let _ = stderr.take(2048).read_to_string(&mut err_text);
    }
    let status = child.wait()?;
    if !status.success() {
        let msg = err_text.lines().next().unwrap_or("").trim();
        return Err(Error::Failed(format!("curl failed ({}){}", status.code().map_or("signal".into(), |c| c.to_string()), if msg.is_empty() { String::new() } else { format!(": {msg}") })));
    }
    Ok(out)
}

/// Fetch a URL with `bin` (normally [`bin`]). Runs on the blocking pool so the daemon's single
/// thread keeps serving; the caller should wrap it in its own timeout as well.
pub async fn fetch(bin: &str, req: Request<'_>) -> Result<Vec<u8>> {
    // `Request` borrows; move owned copies into the blocking task.
    let owned_url = req.url.to_string();
    let owned_body = req.body.map(<[u8]>::to_vec);
    let owned_ct = req.content_type.to_string();
    let (bin, max_bytes, timeout, allow_http) = (bin.to_string(), req.max_bytes, req.timeout, req.allow_http);
    tokio::task::spawn_blocking(move || {
        let r = Request {
            url: &owned_url,
            body: owned_body.as_deref(),
            content_type: &owned_ct,
            max_bytes,
            timeout,
            allow_http,
        };
        fetch_blocking(&bin, &r)
    })
    .await
    .map_err(|e| Error::Failed(format!("curl task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// A fake `curl`: a shell script that records its arguments and stdin, prints `stdout` and
    /// exits with `code`.
    fn fake(dir: &Path, stdout: &str, code: i32, stderr: &str) -> PathBuf {
        let path = dir.join("fake-curl");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{d}/args.txt'\ncat > '{d}/stdin.txt' 2>/dev/null </dev/stdin || true\nprintf '%s' '{stdout}'\nprintf '%s' '{stderr}' >&2\nexit {code}\n",
            d = dir.display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn get(url: &str) -> Request<'_> {
        Request::get(url, 1024, Duration::from_secs(5))
    }

    #[tokio::test]
    async fn returns_the_body_and_passes_safe_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake(dir.path(), "hello", 0, "");
        let out = fetch(bin.to_str().unwrap(), get("https://example.org/a?x=1&y=%20")).await.unwrap();
        assert_eq!(out, b"hello");
        let args = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
        let lines: Vec<&str> = args.lines().collect();
        assert_eq!(lines[0], "-q", "the user's .curlrc must be ignored");
        for needle in ["--fail", "--silent", "--max-time", "--compressed", "--proto", "=https", "--proto-redir"] {
            assert!(lines.contains(&needle), "{needle} in {lines:?}");
        }
        assert!(!lines.iter().any(|l| l.contains("http,") || l.contains("=https,http")), "plain http not allowed: {lines:?}");
        let n = lines.iter().position(|l| *l == "--url").unwrap();
        assert_eq!(lines[n + 1], "https://example.org/a?x=1&y=%20", "URL is the value of --url");
        assert_eq!(n + 2, lines.len(), "nothing after the URL");
    }

    #[tokio::test]
    async fn plain_http_only_when_explicitly_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake(dir.path(), "ok", 0, "");
        let b = bin.to_str().unwrap();
        assert!(fetch(b, get("http://127.0.0.1:9117/x")).await.is_err());
        let allowed = Request { allow_http: true, ..get("http://127.0.0.1:9117/x") };
        assert_eq!(fetch(b, allowed).await.unwrap(), b"ok");
        let args = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
        assert!(args.contains("=https,http"));
    }

    #[tokio::test]
    async fn refuses_dangerous_or_malformed_urls_without_running_curl() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake(dir.path(), "x", 0, "");
        let b = bin.to_str().unwrap();
        for bad in [
            "file:///etc/passwd",
            "ftp://x/y",
            "gopher://x",
            "--output=/tmp/pwn",
            "-K/etc/passwd",
            "https://a b/",
            "https://a/\nHost: evil",
            "",
            "HTTPS-ish://x",
        ] {
            let e = fetch(b, get(bad)).await.unwrap_err().to_string();
            assert!(e.contains("refusing"), "{bad:?}: {e}");
        }
        assert!(!dir.path().join("args.txt").exists(), "curl was never started");
    }

    #[tokio::test]
    async fn failures_and_missing_curl_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake(dir.path(), "", 22, "curl: (22) The requested URL returned error: 404");
        let e = fetch(bin.to_str().unwrap(), get("https://x/")).await.unwrap_err().to_string();
        assert!(e.contains("curl failed (22)") && e.contains("404"), "{e}");

        let e = fetch("/definitely/not/curl", get("https://x/")).await.unwrap_err().to_string();
        assert!(e.contains("was not found") && e.contains("WISPLET_CURL"), "{e}");
    }

    #[tokio::test]
    async fn oversized_responses_are_cut_off() {
        let dir = tempfile::tempdir().unwrap();
        let big = "a".repeat(5000);
        let bin = fake(dir.path(), &big, 0, "");
        let e = fetch(bin.to_str().unwrap(), get("https://x/")).await.unwrap_err().to_string();
        assert!(e.contains("larger than 1024"), "{e}");
        // exactly at the limit is fine
        let ok = fake(dir.path(), &"b".repeat(1024), 0, "");
        assert_eq!(fetch(ok.to_str().unwrap(), get("https://x/")).await.unwrap().len(), 1024);
    }

    #[tokio::test]
    async fn post_body_goes_through_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake(dir.path(), "{}", 0, "");
        let req = Request {
            body: Some(br#"{"query":"x"}"#),
            content_type: "application/json",
            ..get("https://api.example/v1")
        };
        assert_eq!(fetch(bin.to_str().unwrap(), req).await.unwrap(), b"{}");
        assert_eq!(std::fs::read_to_string(dir.path().join("stdin.txt")).unwrap(), r#"{"query":"x"}"#);
        let args = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
        assert!(args.contains("Content-Type: application/json") && args.contains("--data-binary") && args.contains("@-"));
    }

    #[test]
    fn bin_honours_the_environment_override() {
        // single test touching this variable, so parallel tests cannot race on it
        std::env::set_var("WISPLET_CURL", "/opt/fake/curl");
        assert_eq!(bin(), "/opt/fake/curl");
        std::env::set_var("WISPLET_CURL", "");
        assert_eq!(bin(), "curl");
        std::env::remove_var("WISPLET_CURL");
        assert_eq!(bin(), "curl");
    }
}
