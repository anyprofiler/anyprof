use std::io::{Cursor, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use crate::Error;
use crate::assets;
use crate::token::Token;

/// Applied to every response, including 404s.
///
/// Everything is same-origin. Two exceptions to plain `default-src 'self'`
/// are needed by speedscope 1.25.0:
/// - `'wasm-unsafe-eval'`: the C++/Rust symbol demangler is a WebAssembly
///   module embedded in the JS bundle. This allows compiling wasm only; JS
///   `eval` stays blocked (protobuf.js probes `eval("require")` in a `try`
///   and carries on).
/// - `style-src 'unsafe-inline'`: aphrodite (CSS-in-JS) adds a `<style>`
///   element and fills it with `insertRule`. Blocking it leaves the page
///   unstyled and the flame graph's resize loop overflows the stack.
pub(crate) const CONTENT_SECURITY_POLICY: &str =
    "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'";

const POLL: Duration = Duration::from_millis(100);

/// Why [`Viewer::serve`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// `should_stop` returned true, for example after Ctrl+C.
    Stopped,
    /// The timeout passed to [`Viewer::serve`] elapsed.
    TimedOut,
}

/// A localhost HTTP server for one profile and the embedded speedscope build.
///
/// Everything lives under `/t/<token>/`, where the token is 128 random bits.
/// Any other path, including a wrong token, is a 404.
pub struct Viewer {
    server: Server,
    port: u16,
    routes: Routes,
}

impl Viewer {
    /// Listen on `127.0.0.1:port`. Port 0 lets the OS pick a free port.
    ///
    /// `name` is the file name speedscope shows as the title. `profile` is
    /// served unchanged; speedscope detects the format from its contents.
    pub fn bind(name: &str, profile: Vec<u8>, port: u16) -> Result<Self, Error> {
        let token = Token::generate().map_err(|err| Error::Random(err.to_string()))?;
        let server = Server::http(("127.0.0.1", port)).map_err(|source| Error::Bind {
            port,
            message: source.to_string(),
        })?;
        let port = server
            .server_addr()
            .to_ip()
            .map(|addr| addr.port())
            .ok_or_else(|| Error::Bind {
                port,
                message: "listener has no IP address".to_string(),
            })?;
        Ok(Self {
            server,
            port,
            routes: Routes::new(token, name, profile),
        })
    }

    /// The URL to open: `http://127.0.0.1:<port>/t/<token>/`.
    ///
    /// It has no `#fragment`, because macOS `open` and Windows `start` drop
    /// it. The page it serves redirects to speedscope with the hash speedscope
    /// reads.
    #[must_use]
    pub fn url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/t/{}/",
            self.port,
            self.routes.token.as_str()
        )
    }

    /// The bound port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// How many times the profile itself has been requested.
    #[must_use]
    pub fn profile_requests(&self) -> usize {
        self.routes.profile_requests.load(Ordering::Relaxed)
    }

    /// Answer requests until `should_stop` returns true or `timeout` elapses.
    ///
    /// `should_stop` is polled about every 100 ms.
    pub fn serve(
        &self,
        mut should_stop: impl FnMut() -> bool,
        timeout: Option<Duration>,
    ) -> Result<StopReason, Error> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            if should_stop() {
                return Ok(StopReason::Stopped);
            }
            let wait = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(StopReason::TimedOut);
                    }
                    left.min(POLL)
                }
                None => POLL,
            };
            match self.server.recv_timeout(wait) {
                Ok(Some(request)) => self.handle(request),
                Ok(None) => {}
                Err(err) => return Err(Error::Serve(err)),
            }
        }
    }

    fn handle(&self, request: Request) {
        let accepts_gzip = request
            .headers()
            .iter()
            .filter(|header| header.field.equiv("Accept-Encoding"))
            .any(|header| accepts_gzip(header.value.as_str()));
        let reply = self
            .routes
            .route(request.method(), request.url(), accepts_gzip);
        tracing::debug!(
            method = %request.method(),
            status = reply.status,
            "viewer request"
        );
        if let Err(err) = request.respond(reply.into_response()) {
            tracing::debug!("viewer response was not delivered: {err}");
        }
    }
}

struct Routes {
    token: Token,
    landing: String,
    profile: Arc<[u8]>,
    profile_requests: AtomicUsize,
}

impl Routes {
    fn new(token: Token, name: &str, profile: Vec<u8>) -> Self {
        Self {
            landing: landing_page(name),
            token,
            profile: profile.into(),
            profile_requests: AtomicUsize::new(0),
        }
    }

    fn route(&self, method: &Method, url: &str, accepts_gzip: bool) -> Reply {
        let path = url.split(['?', '#']).next().unwrap_or_default();
        let Some(rest) = path.strip_prefix("/t/") else {
            return Reply::not_found();
        };
        let (token, tail) = match rest.split_once('/') {
            Some((token, tail)) => (token, Some(tail)),
            None => (rest, None),
        };
        if !self.token.matches(token) {
            return Reply::not_found();
        }
        if !matches!(method, Method::Get | Method::Head) {
            return Reply::method_not_allowed();
        }
        match tail {
            None => Reply::redirect(format!("/t/{}/", self.token.as_str())),
            Some("") => Reply::ok(
                "text/html; charset=utf-8",
                Body::Owned(self.landing.clone().into_bytes()),
            ),
            Some("profile") => {
                if self.profile_requests.fetch_add(1, Ordering::Relaxed) == 0 {
                    tracing::info!(bytes = self.profile.len(), "browser fetched the profile");
                }
                Reply::ok(
                    "application/octet-stream",
                    Body::Shared(Arc::clone(&self.profile)),
                )
            }
            Some(name) => match assets::lookup(name) {
                Some(asset) if accepts_gzip => {
                    let mut reply = Reply::ok(asset.content_type, Body::Static(asset.gzip));
                    reply.gzip = true;
                    reply
                }
                Some(asset) => match gunzip(asset.gzip) {
                    Some(bytes) => Reply::ok(asset.content_type, Body::Owned(bytes)),
                    None => Reply::not_found(),
                },
                None => Reply::not_found(),
            },
        }
    }
}

/// The page at `/t/<token>/`. A meta refresh, not a script, so it runs under
/// `default-src 'self'`.
fn landing_page(name: &str) -> String {
    let target = format!(
        "index.html#profileURL=profile&title={}&view=left-heavy",
        percent_encode(name)
    );
    let target = html_escape(&target);
    let name = html_escape(name);
    format!(
        "<!DOCTYPE html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n\
         <meta http-equiv=\"refresh\" content=\"0; url={target}\">\n\
         <title>{name} - anyprof</title>\n</head>\n<body>\n\
         <p><a href=\"{target}\">Open {name} in speedscope</a></p>\n</body>\n</html>\n"
    )
}

/// `encodeURIComponent`, minus the `!'()*` it leaves alone.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn accepts_gzip(header: &str) -> bool {
    header.split(',').any(|coding| {
        let mut parts = coding.split(';');
        let name = parts.next().unwrap_or_default().trim();
        let refused = parts.any(|param| {
            param
                .trim()
                .strip_prefix("q=")
                .and_then(|q| q.trim().parse::<f32>().ok())
                .is_some_and(|q| q <= 0.0)
        });
        (name.eq_ignore_ascii_case("gzip") || name == "*") && !refused
    })
}

fn gunzip(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

enum Body {
    Static(&'static [u8]),
    Shared(Arc<[u8]>),
    Owned(Vec<u8>),
}

impl AsRef<[u8]> for Body {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Static(bytes) => bytes,
            Self::Shared(bytes) => bytes,
            Self::Owned(bytes) => bytes,
        }
    }
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: Body,
    gzip: bool,
    location: Option<String>,
}

impl Reply {
    fn ok(content_type: &'static str, body: Body) -> Self {
        Self {
            status: 200,
            content_type,
            body,
            gzip: false,
            location: None,
        }
    }

    fn text(status: u16, message: &'static str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: Body::Static(message.as_bytes()),
            gzip: false,
            location: None,
        }
    }

    fn not_found() -> Self {
        Self::text(404, "not found\n")
    }

    fn method_not_allowed() -> Self {
        Self::text(405, "method not allowed\n")
    }

    fn redirect(location: String) -> Self {
        let mut reply = Self::text(301, "moved\n");
        reply.location = Some(location);
        reply
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("Content-Type", self.content_type.to_string()),
            (
                "Content-Security-Policy",
                CONTENT_SECURITY_POLICY.to_string(),
            ),
            ("X-Content-Type-Options", "nosniff".to_string()),
            ("Referrer-Policy", "no-referrer".to_string()),
            ("Cache-Control", "no-store".to_string()),
        ];
        if self.gzip {
            headers.push(("Content-Encoding", "gzip".to_string()));
            headers.push(("Vary", "Accept-Encoding".to_string()));
        }
        if self.status == 405 {
            headers.push(("Allow", "GET, HEAD".to_string()));
        }
        if let Some(location) = &self.location {
            headers.push(("Location", location.clone()));
        }
        headers
    }

    fn into_response(self) -> Response<Cursor<Body>> {
        let headers = self
            .headers()
            .into_iter()
            .filter_map(|(field, value)| Header::from_bytes(field, value).ok())
            .collect();
        let length = self.body.as_ref().len();
        Response::new(
            StatusCode(self.status),
            headers,
            Cursor::new(self.body),
            Some(length),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use tiny_http::Method;

    use super::{
        CONTENT_SECURITY_POLICY, Reply, Routes, StopReason, Viewer, accepts_gzip, gunzip,
        landing_page, percent_encode,
    };
    use crate::token::Token;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn routes() -> Routes {
        Routes::new(
            Token::from_hex(TOKEN),
            "cpu.cpuprofile",
            b"{\"nodes\":[]}".to_vec(),
        )
    }

    fn get(routes: &Routes, url: &str) -> Reply {
        routes.route(&Method::Get, url, true)
    }

    fn header(reply: &Reply, name: &str) -> Option<String> {
        reply
            .headers()
            .into_iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    #[test]
    fn wrong_or_missing_token_is_404() {
        let routes = routes();
        for url in [
            "/",
            "/index.html",
            "/profile",
            "/t/",
            "/t//",
            "/t//profile",
            "/t/0123456789abcdef0123456789abcdee/",
            "/t/0123456789abcdef0123456789abcdee/profile",
            "/t/0123456789ABCDEF0123456789ABCDEF/profile",
            "/t/0123456789abcdef0123456789abcde/profile",
            "/t/0123456789abcdef0123456789abcdef0/profile",
            "/T/0123456789abcdef0123456789abcdef/profile",
            "/x/t/0123456789abcdef0123456789abcdef/profile",
        ] {
            let reply = get(&routes, url);
            assert_eq!(reply.status, 404, "{url}");
            assert_eq!(header(&reply, "Content-Encoding"), None, "{url}");
        }
        assert_eq!(routes.profile_requests.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn wrong_token_is_404_for_every_method() {
        let routes = routes();
        let reply = routes.route(&Method::Post, "/t/nope/profile", true);
        assert_eq!(reply.status, 404);
        let reply = routes.route(&Method::Post, &format!("/t/{TOKEN}/profile"), true);
        assert_eq!(reply.status, 405);
        assert_eq!(header(&reply, "Allow").as_deref(), Some("GET, HEAD"));
        let head = routes.route(&Method::Head, &format!("/t/{TOKEN}/profile"), true);
        assert_eq!(head.status, 200);
    }

    #[test]
    fn traversal_and_encoded_names_are_404() {
        let routes = routes();
        for tail in [
            "../index.html",
            "..%2findex.html",
            "%2e%2e/index.html",
            "%69ndex.html",
            "./index.html",
            "sub/index.html",
            "..\\index.html",
            "index.html/",
            "index.html.gz",
            "profile/",
            "profile/..",
            "..",
            ".",
            "../../../../etc/passwd",
            "..%5c..%5cwindows%5cwin.ini",
            "C:\\Windows\\win.ini",
            "/etc/passwd",
            "Cargo.toml",
            "src/lib.rs",
        ] {
            let url = format!("/t/{TOKEN}/{tail}");
            assert_eq!(get(&routes, &url).status, 404, "{url}");
        }
        assert_eq!(get(&routes, "/t/../t/x/index.html").status, 404);
    }

    #[test]
    fn root_without_slash_redirects_and_landing_page_points_at_speedscope() {
        let routes = routes();
        let reply = get(&routes, &format!("/t/{TOKEN}"));
        assert_eq!(reply.status, 301);
        assert_eq!(
            header(&reply, "Location").as_deref(),
            Some(format!("/t/{TOKEN}/").as_str())
        );

        let reply = get(&routes, &format!("/t/{TOKEN}/"));
        assert_eq!(reply.status, 200);
        assert_eq!(
            header(&reply, "Content-Type").as_deref(),
            Some("text/html; charset=utf-8")
        );
        let body = std::str::from_utf8(reply.body.as_ref()).unwrap();
        assert!(
            body.contains(
                "url=index.html#profileURL=profile&amp;title=cpu.cpuprofile&amp;view=left-heavy"
            ),
            "{body}"
        );
        assert!(!body.contains("<script"), "{body}");
    }

    #[test]
    fn landing_page_escapes_the_file_name() {
        let page = landing_page("a b&c<\"x'>#=.txt");
        assert!(page.contains("title=a%20b%26c%3C%22x%27%3E%23%3D.txt&amp;"));
        assert!(page.contains("Open a b&amp;c&lt;&quot;x&#39;&gt;#=.txt in speedscope"));
        assert_eq!(percent_encode("héllo"), "h%C3%A9llo");
    }

    #[test]
    fn profile_is_served_raw_and_counted() {
        let routes = routes();
        let reply = get(&routes, &format!("/t/{TOKEN}/profile?x=1"));
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body.as_ref(), b"{\"nodes\":[]}");
        assert_eq!(
            header(&reply, "Content-Type").as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(header(&reply, "Content-Encoding"), None);
        assert_eq!(routes.profile_requests.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn assets_are_gzip_with_their_content_type() {
        let routes = routes();
        let reply = get(&routes, &format!("/t/{TOKEN}/index.html"));
        assert_eq!(reply.status, 200);
        assert_eq!(header(&reply, "Content-Encoding").as_deref(), Some("gzip"));
        assert_eq!(header(&reply, "Vary").as_deref(), Some("Accept-Encoding"));
        assert_eq!(
            header(&reply, "Content-Type").as_deref(),
            Some("text/html; charset=utf-8")
        );
        let html = String::from_utf8(gunzip(reply.body.as_ref()).unwrap()).unwrap();
        assert!(html.contains("<title>speedscope</title>"), "{html}");

        let plain = routes.route(&Method::Get, &format!("/t/{TOKEN}/index.html"), false);
        assert_eq!(header(&plain, "Content-Encoding"), None);
        assert_eq!(std::str::from_utf8(plain.body.as_ref()).unwrap(), html);
    }

    #[test]
    fn every_response_carries_the_security_headers_and_no_cors() {
        let routes = routes();
        for url in [
            format!("/t/{TOKEN}/"),
            format!("/t/{TOKEN}/index.html"),
            format!("/t/{TOKEN}/profile"),
            format!("/t/{TOKEN}/missing.js"),
            "/nope".to_string(),
        ] {
            let reply = get(&routes, &url);
            assert_eq!(
                header(&reply, "Content-Security-Policy").as_deref(),
                Some(CONTENT_SECURITY_POLICY),
                "{url}"
            );
            assert_eq!(
                header(&reply, "X-Content-Type-Options").as_deref(),
                Some("nosniff")
            );
            assert_eq!(header(&reply, "Access-Control-Allow-Origin"), None);
        }
        assert!(CONTENT_SECURITY_POLICY.starts_with("default-src 'self';"));
        assert!(!CONTENT_SECURITY_POLICY.contains('*'));
        assert!(!CONTENT_SECURITY_POLICY.contains("'unsafe-eval'"));
        assert!(!CONTENT_SECURITY_POLICY.contains("blob:"));
        assert!(!CONTENT_SECURITY_POLICY.contains("data:"));
    }

    #[test]
    fn accept_encoding_parsing() {
        assert!(accepts_gzip("gzip, deflate, br, zstd"));
        assert!(accepts_gzip("br;q=1.0, GZIP;q=0.5"));
        assert!(accepts_gzip("*"));
        assert!(!accepts_gzip("gzip;q=0"));
        assert!(!accepts_gzip("br, deflate"));
        assert!(!accepts_gzip("identity"));
        assert!(!accepts_gzip(""));
    }

    /// One raw HTTP/1.0 request against a live viewer.
    fn fetch(port: u16, request: &str) -> (String, Vec<u8>) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reader = BufReader::new(stream);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            head.push_str(&line);
        }
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        (head, body)
    }

    #[test]
    fn live_server_on_loopback_serves_and_stops() {
        let viewer = Arc::new(Viewer::bind("cpu.pb.gz", vec![0x1f, 0x8b, 1, 2], 0).unwrap());
        assert_ne!(viewer.port(), 0);
        let url = viewer.url();
        assert!(url.starts_with(&format!("http://127.0.0.1:{}/t/", viewer.port())));
        assert!(url.ends_with('/'));
        assert!(!url.contains('#'));
        let token = url.rsplit('/').nth(1).unwrap().to_string();
        assert_eq!(token.len(), 32);

        let stop = Arc::new(AtomicBool::new(false));
        let serving = {
            let viewer = Arc::clone(&viewer);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || viewer.serve(|| stop.load(Ordering::SeqCst), None))
        };

        let port = viewer.port();
        let (head, body) = fetch(
            port,
            &format!("GET /t/{token}/profile HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n"),
        );
        assert!(
            head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200"),
            "{head}"
        );
        assert_eq!(body, [0x1f, 0x8b, 1, 2]);
        assert!(
            !head.to_ascii_lowercase().contains("content-encoding"),
            "{head}"
        );

        let (head, _) = fetch(
            port,
            &format!(
                "GET /t/{token}/index.html HTTP/1.0\r\nHost: 127.0.0.1\r\nAccept-Encoding: gzip\r\n\r\n"
            ),
        );
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains("content-encoding: gzip"), "{head}");
        assert!(
            lower.contains("content-security-policy: default-src 'self'"),
            "{head}"
        );
        assert!(!lower.contains("access-control-allow-origin"), "{head}");

        let (head, _) = fetch(
            port,
            "GET /t/00000000000000000000000000000000/profile HTTP/1.0\r\n\r\n",
        );
        assert!(head.contains(" 404"), "{head}");
        let (head, _) = fetch(
            port,
            &format!("GET /t/{token}/../../Cargo.toml HTTP/1.0\r\n\r\n"),
        );
        assert!(head.contains(" 404"), "{head}");

        assert_eq!(viewer.profile_requests(), 1);
        stop.store(true, Ordering::SeqCst);
        assert_eq!(serving.join().unwrap().unwrap(), StopReason::Stopped);
    }

    #[test]
    fn serve_returns_after_the_timeout() {
        let viewer = Viewer::bind("x.collapsed", b"a;b 1\n".to_vec(), 0).unwrap();
        let reason = viewer
            .serve(|| false, Some(Duration::from_millis(150)))
            .unwrap();
        assert_eq!(reason, StopReason::TimedOut);
    }

    #[test]
    fn binding_a_busy_port_fails_clearly() {
        let first = Viewer::bind("a", Vec::new(), 0).unwrap();
        let Err(err) = Viewer::bind("b", Vec::new(), first.port()) else {
            panic!("second bind on the same port should fail");
        };
        assert!(err.to_string().contains(&first.port().to_string()), "{err}");
    }
}
