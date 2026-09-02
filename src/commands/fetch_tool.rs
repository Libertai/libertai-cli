//! Pi `Tool` impl for fetching public URLs locally with `reqwest::blocking`.
//!
//! Replaces the previous LibertAI `/fetch` wrapper. Returns the page's
//! `<title>` (best-effort regex), the final URL after redirects, and up
//! to 16k chars of body text. Strips HTML to plain text via a tiny
//! tag-stripping pass — full readability extraction is out of scope.
//!
//! The result envelope keeps the same `{ text, cite }` shape the FE
//! renderer expects so `parseCitations` keeps working unchanged.

use std::net::{IpAddr, Ipv6Addr, ToSocketAddrs};
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use url::Url;

use pi::model::{ContentBlock, TextContent};
use pi::sdk::{Result as PiResult, Tool, ToolExecution, ToolOutput, ToolUpdate};
use pi::tools::ToolEffects;

const NAME: &str = "fetch";
const LABEL: &str = "Fetch URL contents";
const DESCRIPTION: &str = "Fetch the contents of a public http(s) URL. Returns the page \
title, final URL after redirects, and up to 16,000 characters of body text \
(HTML is stripped to plain text). Use this to read a page the agent has just \
discovered via `search` or that the user has linked to.";

/// Body-size cap for the returned text. Mirrors the previous LibertAI
/// fetch tool so context-window pressure stays predictable.
const MAX_CHARS: usize = 16_000;

#[derive(Debug, Clone, Deserialize)]
struct FetchInput {
    url: String,
}

pub struct FetchTool;

impl FetchTool {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for FetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for FetchTool {
    fn name(&self) -> &str {
        NAME
    }
    fn label(&self) -> &str {
        LABEL
    }
    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Absolute http(s) URL to fetch." }
            },
            "required": ["url"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> PiResult<ToolExecution> {
        let parsed: FetchInput = match serde_json::from_value(input) {
            Ok(v) => v,
            Err(e) => return Ok(err_output(&format!("invalid `fetch` payload: {e}"))),
        };

        let page = match local_fetch(&parsed.url, MAX_CHARS) {
            Ok(p) => p,
            Err(e) => return Ok(err_output(&format!("fetch failed: {e}"))),
        };

        let envelope = json!({
            "text": format!("{}\n{}\n\n{}", page.title, page.final_url, page.text)
                .trim()
                .to_string(),
            "cite": [ { "title": page.title, "url": page.final_url } ],
        });
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(envelope.to_string()))],
            details: None,
            is_error: false,
        }
        .into())
    }

    fn effects(&self) -> ToolEffects {
        ToolEffects::read()
    }
}

fn err_output(msg: &str) -> ToolExecution {
    ToolOutput {
        content: vec![ContentBlock::Text(TextContent::new(msg))],
        details: None,
        is_error: true,
    }
    .into()
}

/// Result of a one-shot HTTP GET + body text extraction.
#[derive(Debug)]
pub struct FetchedPage {
    pub final_url: String,
    pub title: String,
    pub text: String,
}

/// Set `LIBERTAI_FETCH_ALLOW_METADATA=1` to allow fetching the cloud
/// metadata endpoints (`169.254.169.254` and `fd00:ec2::254`). Off by
/// default — those endpoints almost never a legitimate fetch target and
/// are the standard SSRF→credential-exfiltration vector.
const ENV_ALLOW_METADATA: &str = "LIBERTAI_FETCH_ALLOW_METADATA";
const MAX_REDIRECTS: usize = 8;

/// Coarse classification of a URL's resolved host, used to gate redirects.
/// Order matters: `Metadata` is checked before `Private` because the
/// metadata IP (`169.254.169.254`) is itself inside the link-local range.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HostClass {
    Public,
    Private,
    Metadata,
}

/// Classify an already-resolved IP. `Metadata` is checked first because
/// `169.254.169.254` is technically inside the link-local range.
fn classify_ip(ip: IpAddr) -> HostClass {
    match ip {
        IpAddr::V4(v4) => {
            if v4.octets() == [169, 254, 169, 254] {
                return HostClass::Metadata;
            }
            // CGNAT 100.64.0.0/10 — `Ipv4Addr::is_shared` is unstable
            // (feature `ip`), so check the range manually.
            let o = v4.octets();
            let is_cgnat = o[0] == 100 && (64..=127).contains(&o[1]);
            if v4.is_loopback()
                || v4.is_unspecified()
                || v4.is_private()
                || v4.is_link_local()
                || is_cgnat
                || v4.is_broadcast()
            {
                return HostClass::Private;
            }
            HostClass::Public
        }
        IpAddr::V6(v6) => {
            // AWS IMDSv6: `fd00:ec2::254` → `fd00:0ec2::0254`
            if v6.octets()
                == [
                    0xfd, 0x00, 0x0e, 0xc2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02, 0x54,
                ]
            {
                return HostClass::Metadata;
            }
            if v6.is_loopback()
                || v6.is_unspecified()
                || is_ipv6_unique_local(&v6)
                || is_ipv6_link_local(&v6)
            {
                return HostClass::Private;
            }
            HostClass::Public
        }
    }
}

fn is_ipv6_unique_local(v6: &Ipv6Addr) -> bool {
    (v6.octets()[0] & 0xfe) == 0xfc
}

fn is_ipv6_link_local(v6: &Ipv6Addr) -> bool {
    let o = v6.octets();
    o[0] == 0xfe && (o[1] & 0xc0) == 0x80
}

/// Resolve the URL's host and classify it. IP literals skip DNS;
/// hostnames are resolved (best-effort — on DNS failure we return
/// `Public` and let reqwest attempt the fetch and surface its own
/// error, so a transient resolver glitch doesn't change our policy
/// answer for an actually-public host).
fn classify_url(url: &Url) -> HostClass {
    match url.host() {
        Some(url::Host::Ipv4(v4)) => classify_ip(IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => classify_ip(IpAddr::V6(v6)),
        Some(url::Host::Domain(host)) => {
            let port = url.port_or_known_default().unwrap_or(80);
            match (host, port).to_socket_addrs() {
                Ok(iter) => {
                    let mut all_private = true;
                    for sa in iter {
                        match classify_ip(sa.ip()) {
                            HostClass::Metadata => return HostClass::Metadata,
                            HostClass::Private => {}
                            HostClass::Public => all_private = false,
                        }
                    }
                    if all_private {
                        HostClass::Private
                    } else {
                        HostClass::Public
                    }
                }
                Err(_) => HostClass::Public,
            }
        }
        None => HostClass::Public,
    }
}

/// Returns `Ok(())` if fetching `url` is permitted under the policy,
/// `Err(reason)` otherwise. `reason` is shown verbatim to the user.
///
/// Rules:
/// - Non-http(s) schemes are rejected (covers `file://` redirect targets).
/// - The cloud metadata endpoints are always rejected unless
///   `allow_metadata` is set (escape hatch: `LIBERTAI_FETCH_ALLOW_METADATA`).
/// - Redirects from a *public* initial URL to a *private*/link-local IP
///   are rejected (the classic blind-SSRF shape). Direct private→private
///   hops — e.g. `localhost:3000` → `127.0.0.1:3001` while developing a
///   local service — are allowed.
fn check_url_policy(
    url: &Url,
    initial_class: HostClass,
    allow_metadata: bool,
) -> Result<(), &'static str> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("non-http(s) URL");
    }
    match classify_url(url) {
        HostClass::Metadata if !allow_metadata => Err("refusing to fetch cloud metadata endpoint \
                 (set LIBERTAI_FETCH_ALLOW_METADATA=1 to override)"),
        HostClass::Private if initial_class == HostClass::Public => {
            Err("refusing redirect from public URL to private/link-local IP")
        }
        _ => Ok(()),
    }
}

/// One-shot HTTP GET with redirect following, body-size cap, and a
/// best-effort HTML→text pass. Shared between the agent `fetch` tool
/// and the standalone `libertai fetch` CLI command so both behave
/// identically.
///
/// Redirects are followed manually (the underlying client is built with
/// `redirect::Policy::none()`) so each hop can be vetted against
/// `check_url_policy` — that's what closes the prompt-injection SSRF
/// shape (public page → 302 → `http://169.254.169.254/…`).
pub fn local_fetch(url: &str, max_chars: usize) -> anyhow::Result<FetchedPage> {
    use anyhow::{anyhow, Context};

    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(anyhow!("only http(s) URLs are allowed"));
    }

    let parsed = Url::parse(url).with_context(|| format!("parsing {url}"))?;
    let allow_metadata = std::env::var(ENV_ALLOW_METADATA).is_ok();
    let initial_class = classify_url(&parsed);
    check_url_policy(&parsed, initial_class, allow_metadata)
        .map_err(|reason| anyhow!("{reason}"))?;

    let client = http_client()?;
    let mut current_url = parsed;
    let mut resp = client
        .get(current_url.as_str())
        .send()
        .with_context(|| format!("GET {}", current_url))?;
    let mut hops = 0;
    while resp.status().is_redirection() {
        if hops >= MAX_REDIRECTS {
            return Err(anyhow!("too many redirects (max {MAX_REDIRECTS})"));
        }
        let loc = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .ok_or_else(|| anyhow!("redirect {} without Location header", resp.status()))?
            .to_str()
            .with_context(|| "decoding Location header")?;
        let next = current_url
            .join(loc)
            .with_context(|| format!("resolving redirect to {loc}"))?;
        check_url_policy(&next, initial_class, allow_metadata)
            .map_err(|reason| anyhow!("{reason}"))?;
        resp = client
            .get(next.as_str())
            .send()
            .with_context(|| format!("GET {}", next))?;
        current_url = next;
        hops += 1;
    }
    let status = resp.status();
    let final_url = current_url.to_string();
    if !status.is_success() {
        return Err(anyhow!("HTTP {status} from {final_url}"));
    }
    let body = resp
        .text()
        .with_context(|| format!("decoding body from {final_url}"))?;

    let title = extract_title(&body).unwrap_or_else(|| final_url.clone());
    let text = strip_to_text(&body, max_chars);

    Ok(FetchedPage {
        final_url,
        title,
        text,
    })
}

fn http_client() -> anyhow::Result<&'static reqwest::blocking::Client> {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    if let Some(c) = CLIENT.get() {
        return Ok(c);
    }
    let built = reqwest::blocking::Client::builder()
        .user_agent(concat!("libertai-cli/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        // Manual redirect following in `local_fetch` so each hop can be
        // vetted against the SSRF policy. See `check_url_policy`.
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    Ok(CLIENT.get_or_init(|| built))
}

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let after = open + "<title".len();
    let gt = lower[after..].find('>')? + after + 1;
    let close = lower[gt..].find("</title>")? + gt;
    let raw = html.get(gt..close)?.trim();
    if raw.is_empty() {
        None
    } else {
        Some(decode_entities(raw))
    }
}

/// Strip HTML tags + collapse whitespace, then truncate to `max_chars`.
/// Not a readability pass — just enough that an LLM can read the page.
fn strip_to_text(html: &str, max_chars: usize) -> String {
    // Drop <script>, <style>, <noscript>, and HTML comments wholesale —
    // they're noise for an LLM and often dwarf the visible body.
    let mut buf = String::with_capacity(html.len());
    // `to_ascii_lowercase` leaves non-ASCII bytes untouched, so byte offsets
    // into `lower` and `html` stay interchangeable.
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < html.len() {
        if let Some(skip) = skip_block(&lower, i, "<script", "</script>")
            .or_else(|| skip_block(&lower, i, "<style", "</style>"))
            .or_else(|| skip_block(&lower, i, "<noscript", "</noscript>"))
            .or_else(|| skip_comment(&lower, i))
        {
            i = skip;
            continue;
        }
        // `i` is always a char boundary: the skips land just past an ASCII
        // delimiter and every other step advances by one whole char.
        let Some(ch) = html[i..].chars().next() else {
            break;
        };
        buf.push(ch);
        i += ch.len_utf8();
    }
    // Tag-strip + entity-decode, then collapse whitespace.
    let stripped = strip_tags(&buf);
    let decoded = decode_entities(&stripped);
    let collapsed = collapse_whitespace(&decoded);
    if collapsed.chars().count() > max_chars {
        let head: String = collapsed.chars().take(max_chars).collect();
        format!("{head}\n\n…[truncated; first {max_chars} chars]")
    } else {
        collapsed
    }
}

fn skip_block(lower: &str, i: usize, open: &str, close: &str) -> Option<usize> {
    let from = lower.get(i..)?;
    if !from.starts_with(open) {
        return None;
    }
    let close_at = from.find(close)?;
    Some(i + close_at + close.len())
}

fn skip_comment(lower: &str, i: usize) -> Option<usize> {
    let from = lower.get(i..)?;
    if !from.starts_with("<!--") {
        return None;
    }
    let close_at = from.find("-->")?;
    Some(i + close_at + "-->".len())
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                out.push(' ');
            }
        } else if ch == '<' {
            in_tag = true;
        } else {
            out.push(ch);
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    let mut consecutive_newlines = 0u8;
    for ch in s.chars() {
        if ch == '\n' {
            consecutive_newlines = consecutive_newlines.saturating_add(1);
            if consecutive_newlines <= 2 {
                out.push('\n');
            }
            last_was_space = true;
        } else if ch.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            consecutive_newlines = 0;
            last_was_space = false;
            out.push(ch);
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_title_basic() {
        assert_eq!(
            extract_title("<html><head><title>Hello</title></head>").as_deref(),
            Some("Hello"),
        );
    }

    #[test]
    fn extract_title_with_attrs() {
        assert_eq!(
            extract_title("<title lang=\"en\">  Spaced  </title>").as_deref(),
            Some("Spaced"),
        );
    }

    #[test]
    fn strip_drops_scripts_and_styles() {
        let html = "<script>alert(1)</script>before<style>p{}</style>middle<!-- c -->after";
        let out = strip_to_text(html, 1000);
        assert_eq!(out, "beforemiddleafter");
    }

    #[test]
    fn strip_decodes_entities() {
        assert_eq!(strip_to_text("a &amp; b &lt;c&gt;", 100), "a & b <c>");
    }

    #[test]
    fn strip_preserves_non_ascii_text() {
        assert_eq!(
            strip_to_text("<p>Café — naïve 日本語</p>", 100),
            "Café — naïve 日本語"
        );
        // Multi-byte text either side of a dropped block, and inside a tag the
        // stripper has to walk past.
        assert_eq!(
            strip_to_text(
                "一<script>var s = '二';</script><a href=\"/三\">四</a>",
                100
            ),
            "一 四"
        );
    }

    #[test]
    fn strip_truncates_by_chars_not_bytes() {
        let body = "日".repeat(200);
        let out = strip_to_text(&body, 50);
        assert!(out.starts_with(&"日".repeat(50)), "{out}");
        assert!(out.contains("…[truncated; first 50 chars]"), "{out}");
    }

    #[test]
    fn strip_truncates() {
        let body = "x".repeat(200);
        let out = strip_to_text(&body, 50);
        assert!(out.starts_with(&"x".repeat(50)));
        assert!(out.contains("…[truncated; first 50 chars]"));
    }

    // ---------- SSRF policy: unit tests on the classifiers ----------

    #[test]
    fn classify_ipv4_private_ranges() {
        for s in [
            "http://127.0.0.1/",
            "http://127.255.255.255/",
            "http://10.0.0.1/",
            "http://10.255.255.255/",
            "http://172.16.0.1/",
            "http://172.31.255.255/",
            "http://192.168.1.1/",
            "http://0.0.0.0/",
            "http://169.254.0.1/",
            "http://100.64.0.1/",
            "http://100.127.255.255/",
        ] {
            let url = Url::parse(s).unwrap();
            let class = classify_url(&url);
            assert!(matches!(class, HostClass::Private), "{s} -> {class:?}");
        }
    }

    #[test]
    fn classify_ipv4_public_ranges() {
        for s in [
            "http://1.1.1.1/",
            "http://8.8.8.8/",
            "http://172.32.0.1/",
            "http://11.0.0.1/",
        ] {
            let url = Url::parse(s).unwrap();
            let class = classify_url(&url);
            assert!(matches!(class, HostClass::Public), "{s} -> {class:?}");
        }
    }

    #[test]
    fn classify_metadata_ipv4() {
        let url = Url::parse("http://169.254.169.254/").unwrap();
        assert!(matches!(classify_url(&url), HostClass::Metadata));
    }

    #[test]
    fn classify_ipv6_loopback_and_ula() {
        assert!(matches!(
            classify_url(&Url::parse("http://[::1]/").unwrap()),
            HostClass::Private
        ));
        assert!(matches!(
            classify_url(&Url::parse("http://[fc00::1]/").unwrap()),
            HostClass::Private
        ));
        assert!(matches!(
            classify_url(&Url::parse("http://[fd01::1]/").unwrap()),
            HostClass::Private
        ));
        assert!(matches!(
            classify_url(&Url::parse("http://[fe80::1]/").unwrap()),
            HostClass::Private
        ));
    }

    #[test]
    fn classify_metadata_ipv6() {
        // AWS IMDSv6: `fd00:ec2::254`
        assert!(matches!(
            classify_url(&Url::parse("http://[fd00:ec2::254]/").unwrap()),
            HostClass::Metadata
        ));
    }

    #[test]
    fn policy_allows_public_to_public() {
        let target = Url::parse("http://1.1.1.1/").unwrap();
        check_url_policy(&target, HostClass::Public, false).expect("public → public allowed");
    }

    #[test]
    fn policy_allows_private_when_initial_private() {
        // Direct localhost dev-server workflow: agent fetches its own
        // local service, possibly via a self-redirect.
        let target = Url::parse("http://127.0.0.1:3000/").unwrap();
        check_url_policy(&target, HostClass::Private, false).expect("private → private allowed");
    }

    #[test]
    fn policy_blocks_redirect_from_public_to_loopback() {
        let target = Url::parse("http://127.0.0.1:8080/").unwrap();
        let err = check_url_policy(&target, HostClass::Public, false)
            .expect_err("public → loopback must be blocked");
        assert!(err.contains("private"), "got: {err}");
    }

    #[test]
    fn policy_blocks_redirect_from_public_to_private() {
        for s in [
            "http://10.0.0.1/",
            "http://172.16.5.5/",
            "http://192.168.1.1/",
            "http://169.254.0.1/",
            "http://100.64.0.1/",
        ] {
            let target = Url::parse(s).unwrap();
            let err = check_url_policy(&target, HostClass::Public, false)
                .expect_err("public → private must be blocked");
            assert!(err.contains("private"), "{s}: got {err}");
        }
    }

    #[test]
    fn policy_blocks_metadata_regardless_of_initial() {
        let target = Url::parse("http://169.254.169.254/").unwrap();
        // From a private origin (e.g. local dev server redirecting to IMDS):
        let err =
            check_url_policy(&target, HostClass::Private, false).expect_err("metadata blocked");
        assert!(err.contains("metadata"), "got: {err}");
        // From a public origin:
        let err =
            check_url_policy(&target, HostClass::Public, false).expect_err("metadata blocked");
        assert!(err.contains("metadata"), "got: {err}");
    }

    #[test]
    fn policy_allows_metadata_with_env_override() {
        let target = Url::parse("http://169.254.169.254/").unwrap();
        check_url_policy(&target, HostClass::Private, true)
            .expect("metadata allowed with override");
        check_url_policy(&target, HostClass::Public, true).expect("metadata allowed with override");
    }

    #[test]
    fn policy_rejects_non_http_redirect() {
        let target = Url::parse("file:///etc/passwd").unwrap();
        let err = check_url_policy(&target, HostClass::Public, false).expect_err("file:// blocked");
        assert!(err.contains("non-http(s)"), "got: {err}");
    }

    // ---------- SSRF policy: end-to-end via `local_fetch` ----------

    /// One-shot local server that handles a single request then exits.
    /// Returns the bound `http://127.0.0.1:<port>` URL.
    fn one_shot_server<F>(responder: F) -> String
    where
        F: FnOnce(tiny_http::Request) + Send + 'static,
    {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind local server");
        let port = match server.server_addr() {
            tiny_http::ListenAddr::IP(addr) => addr.port(),
            tiny_http::ListenAddr::Unix(_) => panic!("expected IP listener"),
        };
        std::thread::spawn(move || {
            if let Some(req) = server.incoming_requests().next() {
                responder(req);
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    #[test]
    fn local_fetch_allows_direct_localhost() {
        let url = one_shot_server(|req| {
            let _ = req.respond(tiny_http::Response::from_string(
                "<html><head><title>Local Dev</title></head><body>hi</body></html>",
            ));
        });
        let page = local_fetch(&url, 1000).expect("direct localhost fetch should succeed");
        assert_eq!(page.title, "Local Dev");
        assert!(page.text.contains("hi"));
        assert!(
            page.final_url.starts_with(&url),
            "final_url {} should start with {}",
            page.final_url,
            url
        );
    }

    #[test]
    fn local_fetch_blocks_metadata_ip_directly() {
        // No server needed — the policy check fires before the request
        // is sent (well, before we ever consult a redirect).
        let err = local_fetch("http://169.254.169.254/latest/meta-data/", 1000)
            .expect_err("metadata IP must be blocked");
        assert!(err.to_string().contains("metadata"), "got: {err}");
    }

    #[test]
    fn local_fetch_blocks_redirect_to_metadata() {
        let url = one_shot_server(|req| {
            let loc = "Location: http://169.254.169.254/latest/meta-data/iam/security-credentials/"
                .parse::<tiny_http::Header>()
                .unwrap();
            let _ = req.respond(
                tiny_http::Response::from_string("")
                    .with_status_code(302)
                    .with_header(loc),
            );
        });
        let err = local_fetch(&url, 1000).expect_err("redirect to metadata must be blocked");
        assert!(err.to_string().contains("metadata"), "got: {err}");
    }

    #[test]
    fn local_fetch_blocks_redirect_from_public_to_private_helper() {
        // We can't bind a public IP from a unit test, so this proves the
        // policy is reachable from `local_fetch` by serving a redirect
        // from a private origin (which is allowed) to another private
        // origin (also allowed) — i.e. the legitimate dev-server case.
        let target_url = one_shot_server(|req| {
            let _ = req.respond(tiny_http::Response::from_string(
                "<html><head><title>Backend</title></head><body>ok</body></html>",
            ));
        });
        let target_url_for_redirect = target_url.clone();
        let origin_url = one_shot_server(move |req| {
            let h = format!("Location: {target_url_for_redirect}")
                .parse::<tiny_http::Header>()
                .unwrap();
            let _ = req.respond(
                tiny_http::Response::from_string("")
                    .with_status_code(302)
                    .with_header(h),
            );
        });
        let page = local_fetch(&origin_url, 1000).expect("private → private redirect allowed");
        assert_eq!(page.title, "Backend");
        assert!(
            page.final_url.starts_with(&target_url),
            "final_url {} should start with {}",
            page.final_url,
            target_url
        );
    }

    #[test]
    fn local_fetch_rejects_non_http_initial_url() {
        let err = local_fetch("ftp://example.com/", 1000).expect_err("ftp:// blocked");
        assert!(err.to_string().contains("only http(s)"), "got: {err}");
    }
}
