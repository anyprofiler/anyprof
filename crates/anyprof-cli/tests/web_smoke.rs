//! `anyprof view` in headless Chrome, driven over the DevTools Protocol.
//!
//! For a `.cpuprofile`, a pprof `.pb.gz`, collapsed stacks, and speedscope
//! JSON, this checks that the viewer served the profile, that speedscope
//! picked the matching importer, that the page logged no exceptions or CSP
//! violations, and that the screenshot shows a flame graph.
//!
//! Skips when no Chrome is found unless `ANYPROF_REQUIRE_CHROME` is set (CI
//! sets it). `ANYPROF_CHROME` overrides the browser path.
//! `ANYPROF_SMOKE_ARTIFACTS` names a directory to keep the screenshots in.

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

const DEADLINE: Duration = Duration::from_secs(60);

#[test]
fn speedscope_renders_profiles_in_headless_chrome() {
    let Some(chrome) = find_chrome() else {
        assert!(
            std::env::var_os("ANYPROF_REQUIRE_CHROME").is_none(),
            "ANYPROF_REQUIRE_CHROME is set but no Chrome was found (set ANYPROF_CHROME)"
        );
        eprintln!("skipping: no Chrome found (set ANYPROF_CHROME to run this test)");
        return;
    };
    eprintln!("using {}", chrome.display());

    let dir = scratch_dir("anyprof-web-smoke");
    let collapsed = dir.join("smoke.collapsed");
    std::fs::write(&collapsed, collapsed_fixture()).unwrap();
    let mut cases = vec![(collapsed.clone(), "Importing as collapsed stack format")];
    for (name, to, importer) in [
        (
            "smoke.cpuprofile",
            "cpuprofile",
            "Importing as Chrome CPU Profile",
        ),
        (
            "smoke.pb.gz",
            "pprof",
            "Importing as protobuf encoded pprof file",
        ),
        (
            "smoke.speedscope.json",
            "speedscope",
            "Importing as speedscope json file",
        ),
    ] {
        let path = dir.join(name);
        let status = Command::new(env!("CARGO_BIN_EXE_anyprof"))
            .arg("convert")
            .arg("-i")
            .arg(&collapsed)
            .arg("-o")
            .arg(&path)
            .args(["-t", to])
            .status()
            .unwrap();
        assert!(status.success(), "convert to {to}");
        cases.push((path, importer));
    }

    let browser = Browser::launch(&chrome, &dir.join("chrome-profile"));
    let artifacts = std::env::var_os("ANYPROF_SMOKE_ARTIFACTS").map(PathBuf::from);
    if let Some(dir) = &artifacts {
        std::fs::create_dir_all(dir).unwrap();
    }
    let mut cdp = browser.connect();
    for (path, importer) in &cases {
        let screenshot = check_one(&mut cdp, path, importer);
        if let Some(dir) = &artifacts {
            let name = path.file_name().unwrap().to_string_lossy();
            std::fs::write(dir.join(format!("{name}.png")), &screenshot).unwrap();
        }
    }
    drop(cdp);
    drop(browser);
    let _ = std::fs::remove_dir_all(&dir);
}

fn check_one(cdp: &mut Cdp, path: &Path, importer: &str) -> Vec<u8> {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let viewer = ViewerProcess::start(path);
    eprintln!("{name}: {}", viewer.url);

    let target = cdp.call(None, "Target.createTarget", json!({ "url": "about:blank" }));
    let target_id = target["targetId"].as_str().unwrap().to_string();
    let attached = cdp.call(
        None,
        "Target.attachToTarget",
        json!({ "targetId": target_id, "flatten": true }),
    );
    let session = attached["sessionId"].as_str().unwrap().to_string();
    let session = Some(session.as_str());
    for method in ["Runtime.enable", "Log.enable", "Page.enable"] {
        cdp.call(session, method, json!({}));
    }
    cdp.call(
        session,
        "Emulation.setDeviceMetricsOverride",
        json!({ "width": 1200, "height": 800, "deviceScaleFactor": 1, "mobile": false }),
    );
    cdp.events.clear();
    cdp.call(session, "Page.navigate", json!({ "url": viewer.url }));

    let started = Instant::now();
    let state = loop {
        let state = cdp.call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "JSON.stringify({hash: location.hash, text: document.body ? document.body.innerText : '', canvases: document.querySelectorAll('canvas').length})",
                "returnByValue": true,
            }),
        );
        let state: Value = state
            .pointer("/result/value")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null);
        let text = state["text"].as_str().unwrap_or_default();
        let imported = cdp.console().iter().any(|line| line.contains(importer));
        if imported && text.contains(&name) && text.contains("Left Heavy") {
            break state;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "{name}: speedscope did not finish loading.\nstate: {state}\nconsole: {:#?}\nlog: {:#?}\nviewer: {}",
            cdp.console(),
            cdp.problems(),
            viewer.stderr()
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    // Let the flame graph draw a frame after the import.
    std::thread::sleep(Duration::from_millis(750));

    let text = state["text"].as_str().unwrap_or_default();
    assert!(
        !text.contains("Something went wrong"),
        "{name}: speedscope error page: {text}"
    );
    assert!(
        !text.contains("Drag and drop"),
        "{name}: still on the landing page"
    );
    assert!(
        state["hash"]
            .as_str()
            .unwrap_or_default()
            .contains("profileURL=profile"),
        "{name}: {state}"
    );
    assert!(
        state["canvases"].as_u64().unwrap_or(0) > 0,
        "{name}: {state}"
    );
    let problems = cdp.problems();
    assert!(
        problems.is_empty(),
        "{name}: exceptions or CSP violations: {problems:#?}"
    );

    let shot = cdp.call(
        session,
        "Page.captureScreenshot",
        json!({ "format": "png" }),
    );
    let png = base64_decode(shot["data"].as_str().unwrap());
    let colored = flame_graph_coverage(&png);
    assert!(
        colored >= 0.02,
        "{name}: no flame graph in the screenshot ({:.1}% colored pixels)",
        colored * 100.0
    );

    let stderr = viewer.stderr();
    assert!(
        stderr.contains("browser fetched the profile"),
        "{name}: the viewer never served the profile:\n{stderr}"
    );
    cdp.call(None, "Target.closeTarget", json!({ "targetId": target_id }));
    eprintln!(
        "{name}: rendered ({:.1}% of the flame graph area colored)",
        colored * 100.0
    );
    png
}

/// Share of saturated pixels between 20% and 60% of the page height.
///
/// That band is below the toolbar, so it is white with gray grid lines and
/// black text unless the WebGL flame graph drew its colored frames.
fn flame_graph_coverage(png: &[u8]) -> f64 {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().unwrap();
    let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut pixels).unwrap();
    let channels = info.color_type.samples();
    assert!(
        channels >= 3,
        "unexpected PNG color type {:?}",
        info.color_type
    );
    let (width, height) = (info.width as usize, info.height as usize);
    let (mut colored, mut total) = (0_usize, 0_usize);
    for y in height / 5..height * 3 / 5 {
        let row = &pixels[y * info.line_size..y * info.line_size + width * channels];
        for pixel in row.chunks_exact(channels) {
            let max = pixel[..3].iter().max().unwrap();
            let min = pixel[..3].iter().min().unwrap();
            total += 1;
            if max - min > 24 {
                colored += 1;
            }
        }
    }
    colored as f64 / total.max(1) as f64
}

fn collapsed_fixture() -> String {
    let names = [
        "main", "parse", "tokenize", "render", "layout", "paint", "compute", "hash",
    ];
    let mut out = String::new();
    let mut seed = 7_u32;
    for _ in 0..120 {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let depth = 2 + (seed >> 16) as usize % 6;
        let frames: Vec<String> = (0..depth)
            .map(|level| {
                let pick = (seed as usize >> (level * 3)) % names.len();
                format!("[node] {}{level}", names[pick])
            })
            .collect();
        out.push_str(&frames.join(";"));
        out.push_str(&format!(" {}\n", 1 + (seed >> 8) % 40));
    }
    out
}

struct ViewerProcess {
    child: Child,
    url: String,
    lines: mpsc::Receiver<String>,
    seen: std::cell::RefCell<Vec<String>>,
}

impl ViewerProcess {
    fn start(path: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_anyprof"))
            .arg("-v")
            .arg("view")
            .arg(path)
            .args(["--no-open", "--web-timeout", "5m"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        let mut seen = Vec::new();
        let url = loop {
            let line = lines
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("anyprof view printed no URL: {seen:#?}"));
            let url = line
                .split_whitespace()
                .find(|word| word.starts_with("http://127.0.0.1:"))
                .map(str::to_string);
            seen.push(line);
            if let Some(url) = url {
                break url;
            }
        };
        assert!(url.contains("/t/") && url.ends_with('/'), "{url}");
        Self {
            child,
            url,
            lines,
            seen: std::cell::RefCell::new(seen),
        }
    }

    fn stderr(&self) -> String {
        let mut seen = self.seen.borrow_mut();
        while let Ok(line) = self.lines.try_recv() {
            seen.push(line);
        }
        seen.join("\n")
    }
}

impl Drop for ViewerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Browser {
    child: Child,
    profile: PathBuf,
}

impl Browser {
    fn launch(chrome: &Path, profile: &Path) -> Self {
        let _ = std::fs::remove_dir_all(profile);
        std::fs::create_dir_all(profile).unwrap();
        let mut command = Command::new(chrome);
        command
            .arg("--headless=new")
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", profile.display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-sync",
                "--use-mock-keychain",
                "--password-store=basic",
                "--enable-unsafe-swiftshader",
                "--window-size=1200,800",
                "about:blank",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if cfg!(target_os = "linux") {
            // GitHub's Ubuntu images restrict unprivileged user namespaces.
            command.arg("--no-sandbox");
        }
        let child = command.spawn().unwrap();
        Self {
            child,
            profile: profile.to_path_buf(),
        }
    }

    fn connect(&self) -> Cdp {
        let file = self.profile.join("DevToolsActivePort");
        let started = Instant::now();
        let (port, path) = loop {
            if let Ok(text) = std::fs::read_to_string(&file) {
                let mut lines = text.lines();
                if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                    break (port.trim().to_string(), path.trim().to_string());
                }
            }
            assert!(
                started.elapsed() < DEADLINE,
                "Chrome did not write {}",
                file.display()
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        let url = format!("ws://127.0.0.1:{port}{path}");
        let (mut socket, _) = tungstenite::connect(url.as_str()).unwrap();
        if let MaybeTlsStream::Plain(tcp) = socket.get_mut() {
            tcp.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        }
        Cdp {
            socket,
            next_id: 1,
            events: Vec::new(),
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Cdp {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    events: Vec<Value>,
}

impl Cdp {
    fn call(&mut self, session: Option<&str>, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        self.socket
            .send(Message::text(message.to_string()))
            .unwrap();
        loop {
            let message = self
                .socket
                .read()
                .unwrap_or_else(|err| panic!("{method}: {err}"));
            let Message::Text(text) = message else {
                continue;
            };
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if value.get("id").and_then(Value::as_u64) == Some(id) {
                assert!(value.get("error").is_none(), "{method}: {}", value["error"]);
                return value["result"].clone();
            }
            if value.get("method").is_some() {
                self.events.push(value);
            }
        }
    }

    fn console(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|event| event["method"] == "Runtime.consoleAPICalled")
            .map(|event| {
                event["params"]["args"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|arg| match &arg["value"] {
                        Value::String(text) => text.clone(),
                        Value::Null => arg["description"].as_str().unwrap_or("").to_string(),
                        other => other.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    /// Uncaught exceptions, CSP violations, and failed requests.
    fn problems(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|event| match event["method"].as_str()? {
                "Runtime.exceptionThrown" => Some(format!(
                    "exception: {}",
                    event["params"]["exceptionDetails"]
                )),
                "Log.entryAdded" => {
                    let entry = &event["params"]["entry"];
                    let source = entry["source"].as_str().unwrap_or_default();
                    let level = entry["level"].as_str().unwrap_or_default();
                    let text = entry["text"].as_str().unwrap_or_default();
                    // A missing favicon or source map is not a rendering failure.
                    let benign = text.contains("favicon") || text.contains(".map");
                    (level == "error"
                        && !benign
                        && matches!(source, "security" | "network" | "javascript"))
                    .then(|| format!("{source}: {text}"))
                }
                _ => None,
            })
            .collect()
    }
}

fn find_chrome() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ANYPROF_CHROME") {
        return Some(PathBuf::from(path));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        candidates.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
        candidates.push("/Applications/Chromium.app/Contents/MacOS/Chromium".into());
    } else if cfg!(windows) {
        for base in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(base) {
                candidates.push(
                    PathBuf::from(base)
                        .join("Google")
                        .join("Chrome")
                        .join("Application")
                        .join("chrome.exe"),
                );
            }
        }
    } else {
        let path = std::env::var_os("PATH").unwrap_or_default();
        for name in [
            "google-chrome-stable",
            "google-chrome",
            "chromium",
            "chromium-browser",
        ] {
            for dir in std::env::split_paths(&path) {
                candidates.push(dir.join(name));
            }
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn scratch_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn base64_decode(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0_u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    out
}
