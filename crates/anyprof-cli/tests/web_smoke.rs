//! `anyprof view` in headless Chrome, driven over the DevTools Protocol.
//!
//! Folded profiles (collapsed stacks and the pprof, speedscope, and
//! `.cpuprofile` files `convert` writes from them) must say there is no
//! timeline. A hand-written V8 time series must show sample activity. A
//! function name that looks like markup must stay text. The page must not log
//! exceptions or CSP violations, and the flame graph must be visible.
//!
//! Skips when no Chrome is found unless `ANYPROF_REQUIRE_CHROME` is set (CI
//! sets it). `ANYPROF_CHROME` overrides the browser path.
//! `ANYPROF_SMOKE_ARTIFACTS` names a directory to keep the screenshots in.
//! When a page does not finish loading, a screenshot and a text report are
//! saved there (or in the scratch directory) before the test fails.

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
fn viewer_renders_profiles_in_headless_chrome() {
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
    let mut cases = vec![(collapsed.clone(), "aggregate")];
    for (name, to) in [
        ("smoke.cpuprofile", "cpuprofile"),
        ("smoke.pb.gz", "pprof"),
        ("smoke.speedscope.json", "speedscope"),
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
        cases.push((path, "aggregate"));
    }
    let series = dir.join("series.cpuprofile");
    std::fs::write(&series, SERIES_CPUPROFILE).unwrap();
    cases.push((series, "timeline"));
    let markup = dir.join("markup.collapsed");
    std::fs::write(
        &markup,
        "<img src=x onerror=alert(1)>;<script>alert(1)</script> 4\n",
    )
    .unwrap();
    cases.push((markup, "markup"));
    let broken = dir.join("broken.cpuprofile");
    std::fs::write(&broken, b"{").unwrap();
    cases.push((broken, "broken"));

    let browser = Browser::launch(&chrome, &dir.join("chrome-profile"));
    let artifacts = std::env::var_os("ANYPROF_SMOKE_ARTIFACTS").map(PathBuf::from);
    if let Some(dir) = &artifacts {
        std::fs::create_dir_all(dir).unwrap();
    }
    let mut cdp = browser.connect();
    for (path, kind) in &cases {
        check_one(&mut cdp, path, kind, artifacts.as_deref().unwrap_or(&dir));
    }
    drop(cdp);
    drop(browser);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Load `path` and check it rendered. Screenshots and failure reports go to
/// `keep`.
const SERIES_CPUPROFILE: &str = r#"{
  "nodes": [
    {"id": 1, "callFrame": {"functionName": "(root)", "scriptId": "1", "url": "", "lineNumber": -1, "columnNumber": -1}, "children": [2]},
    {"id": 2, "callFrame": {"functionName": "hot", "scriptId": "7", "url": "app.js", "lineNumber": 9, "columnNumber": 0}, "children": [3]},
    {"id": 3, "callFrame": {"functionName": "leaf", "scriptId": "7", "url": "app.js", "lineNumber": 12, "columnNumber": 0}, "children": []}
  ],
  "startTime": 1000,
  "endTime": 4000,
  "samples": [3, 3, 3, 2],
  "timeDeltas": [1000, 1000, 1000, 1000]
}"#;

fn check_one(cdp: &mut Cdp, path: &Path, kind: &str, keep: &Path) {
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
        // Fails while the landing page's meta refresh navigates; poll again.
        let state = cdp.try_call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "JSON.stringify({ready: document.documentElement.dataset.ready || '', timeline: document.documentElement.dataset.timeline || '', text: document.body ? document.body.innerText : '', imgs: document.querySelectorAll('img').length, inlineScripts: [...document.scripts].filter(s => !s.src).length})",
                "returnByValue": true,
            }),
        );
        let state: Value = state
            .ok()
            .as_ref()
            .and_then(|state| state.pointer("/result/value"))
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Value::Null);
        let text = state["text"].as_str().unwrap_or_default();
        let ready = state["ready"].as_str() == Some("1");
        let failed = state["ready"].as_str() == Some("error");
        if kind == "broken"
            && failed
            && text
                .to_ascii_lowercase()
                .contains("could not read this profile")
        {
            break state;
        }
        if kind != "broken" && ready && text.contains(&name) {
            break state;
        }
        if started.elapsed() >= DEADLINE {
            let report = format!(
                "{name}: viewer did not finish loading within {DEADLINE:?}.\nurl: {}\nstate: {state}\nconsole: {:#?}\nlog: {:#?}\nviewer: {}",
                viewer.url,
                cdp.console(),
                cdp.problems(),
                viewer.stderr()
            );
            let saved = save_timeout_diagnostics(cdp, session, keep, &name, &report);
            panic!("{report}\nsaved: {saved:#?}");
        }
        std::thread::sleep(Duration::from_millis(250));
    };

    let text = state["text"].as_str().unwrap_or_default();
    let lower = text.to_ascii_lowercase();
    match kind {
        "aggregate" => assert!(
            lower.contains("no timeline"),
            "{name}: folded profile drew a timeline:\n{text}"
        ),
        "timeline" => {
            assert!(
                lower.contains("sample activity"),
                "{name}: time series had no sample activity:\n{text}"
            );
            assert!(
                !lower.contains("no timeline"),
                "{name}: time series hid its timeline:\n{text}"
            );
            assert!(
                !lower.contains("cpu usage"),
                "{name}: sample activity was labeled as CPU usage:\n{text}"
            );
        }
        "markup" => {
            assert_eq!(state["imgs"].as_u64(), Some(0), "{name}: {state}");
            assert_eq!(state["inlineScripts"].as_u64(), Some(0), "{name}: {state}");
            assert!(
                text.contains("<script>alert(1)</script>"),
                "{name}: markup name was not shown as text:\n{text}"
            );
            assert!(lower.contains("no timeline"), "{name}:\n{text}");
        }
        "broken" => assert!(
            lower.contains("could not read this profile"),
            "{name}: a malformed profile rendered a tree:\n{text}"
        ),
        other => panic!("unknown case {other}"),
    }
    if kind == "broken" {
        let stderr = viewer.stderr();
        assert!(
            stderr.contains("browser fetched the profile"),
            "{name}: the viewer never served the session:\n{stderr}"
        );
        cdp.call(None, "Target.closeTarget", json!({ "targetId": target_id }));
        eprintln!("{name}: refused");
        return;
    }
    if kind == "timeline" {
        drag_timeline(cdp, session);
        assert!(
            wait_for(
                cdp,
                session,
                "!document.body.innerText.includes('leaf') && document.querySelector('[data-testid=zoom-range]') && !document.querySelector('[data-testid=zoom-range]').hidden"
            ),
            "{name}: dragging the last samples did not drop the earlier stack"
        );
        let selected = cdp.call(
            session,
            "Page.captureScreenshot",
            json!({ "format": "png" }),
        );
        std::fs::write(
            keep.join(format!("{name}.selection.png")),
            base64_decode(selected["data"].as_str().unwrap()),
        )
        .unwrap();
        cdp.call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "document.querySelector('[data-testid=zoom-range]').click()",
                "returnByValue": true,
            }),
        );
        assert!(
            wait_for(
                cdp,
                session,
                "!document.body.innerText.includes('leaf') && document.querySelector('[data-testid=zoom-range]').hidden"
            ),
            "{name}: zoom did not keep the selected interval"
        );
        cdp.call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "document.querySelector('[data-testid=reset-range]').click()",
                "returnByValue": true,
            }),
        );
        assert!(
            wait_for(cdp, session, "document.body.innerText.includes('leaf')"),
            "{name}: reset did not restore the full series"
        );
    }
    if kind == "aggregate" && name.ends_with(".collapsed") {
        let disabled = cdp.call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "document.querySelector('[data-testid=hide-system]').disabled",
                "returnByValue": true,
            }),
        );
        assert_eq!(
            disabled.pointer("/result/value").and_then(Value::as_bool),
            Some(true),
            "{name}: hide system was enabled without a system frame"
        );
        cdp.call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "document.getElementById('pane').focus()",
                "returnByValue": true,
            }),
        );
        cdp.call(
            session,
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyDown",
                "key": "ArrowDown",
                "code": "ArrowDown",
                "windowsVirtualKeyCode": 40,
                "nativeVirtualKeyCode": 40,
            }),
        );
        assert!(
            wait_for(
                cdp,
                session,
                "!!document.querySelector('[data-testid=tree-row][aria-selected=\"true\"]')"
            ),
            "{name}: ArrowDown did not select a row"
        );
    }
    if kind == "markup" {
        cdp.call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "document.querySelector('[data-testid=invert]').click()",
                "returnByValue": true,
            }),
        );
        let inverted = wait_for(
            cdp,
            session,
            "document.querySelector('[data-testid=tree-row] .label') && document.querySelector('[data-testid=tree-row] .label').textContent.includes('<script>')",
        );
        assert!(inverted, "{name}: invert did not put the leaf at the root");
        cdp.call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "document.querySelector('[data-testid=search]').value = 'no-such-function-zz'; document.querySelector('[data-testid=search]').dispatchEvent(new Event('input', {bubbles: true}));",
                "returnByValue": true,
            }),
        );
        let filtered = wait_for(
            cdp,
            session,
            "document.body.innerText.includes('No functions match.')",
        );
        assert!(filtered, "{name}: search did not filter the tree");
        cdp.call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "document.querySelector('[data-testid=search]').value = ''; document.querySelector('[data-testid=search]').dispatchEvent(new Event('input', {bubbles: true}));",
                "returnByValue": true,
            }),
        );
        assert!(
            wait_for(
                cdp,
                session,
                "document.querySelector('[data-testid=search]').value === '' && !!document.querySelector('[data-testid=tree-row]')"
            ),
            "{name}: clearing search did not restore the tree"
        );
    }
    let shot = cdp.call(
        session,
        "Page.captureScreenshot",
        json!({ "format": "png" }),
    );
    std::fs::write(
        keep.join(format!("{name}.call-tree.png")),
        base64_decode(shot["data"].as_str().unwrap()),
    )
    .unwrap();
    cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": "document.querySelector('[data-testid=tab-flame]').click()",
            "returnByValue": true,
        }),
    );
    assert!(
        wait_for(
            cdp,
            session,
            "!!document.querySelector('[data-testid=flame]')"
        ),
        "{name}: flame graph did not render"
    );
    let problems = cdp.problems();
    assert!(
        problems.is_empty(),
        "{name}: exceptions or CSP violations: {problems:#?}"
    );
    std::thread::sleep(Duration::from_millis(200));
    let shot = cdp.call(
        session,
        "Page.captureScreenshot",
        json!({ "format": "png" }),
    );
    let png = base64_decode(shot["data"].as_str().unwrap());
    std::fs::write(keep.join(format!("{name}.png")), &png).unwrap();

    let colored = flame_graph_coverage(&png);
    assert!(
        colored >= 0.02,
        "{name}: no flame graph in the screenshot ({:.1}% colored pixels)",
        colored * 100.0
    );

    if kind == "aggregate" && name.ends_with(".collapsed") {
        cdp.call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "document.querySelector('[data-testid=theme-dark]').click()",
                "returnByValue": true,
            }),
        );
        std::thread::sleep(Duration::from_millis(150));
        let dark = cdp.call(
            session,
            "Page.captureScreenshot",
            json!({ "format": "png" }),
        );
        std::fs::write(
            keep.join(format!("{name}.dark.png")),
            base64_decode(dark["data"].as_str().unwrap()),
        )
        .unwrap();
    }

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
}

/// Drag across the right side of the sample chart, where the fixture's last
/// sample is `hot` and the earlier samples are `leaf`.
fn drag_timeline(cdp: &mut Cdp, session: Option<&str>) {
    let point = cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": "(function(){ const canvas = document.querySelector('#timeline'); const rect = canvas.getBoundingClientRect(); const gutter = 120; const plot = Math.max(1, rect.width - gutter); return {y: rect.top + Math.min(8, rect.height / 2), x0: rect.left + gutter + plot * 0.80, x1: rect.left + gutter + plot * 0.97}; })()",
            "returnByValue": true,
        }),
    );
    let value = point
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null);
    let x0 = value["x0"]
        .as_f64()
        .unwrap_or_else(|| panic!("timeline rect: {point}"));
    let x1 = value["x1"].as_f64().unwrap();
    let y = value["y"].as_f64().unwrap();
    for (kind, x) in [
        ("mousePressed", x0),
        ("mouseMoved", x1),
        ("mouseReleased", x1),
    ] {
        cdp.call(
            session,
            "Input.dispatchMouseEvent",
            json!({
                "type": kind,
                "x": x,
                "y": y,
                "button": "left",
                "buttons": if kind == "mouseReleased" { 0 } else { 1 },
                "clickCount": 1,
            }),
        );
    }
}

fn wait_for(cdp: &mut Cdp, session: Option<&str>, expression: &str) -> bool {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        let state = cdp.try_call(
            session,
            "Runtime.evaluate",
            json!({ "expression": expression, "returnByValue": true }),
        );
        let ready = state
            .ok()
            .as_ref()
            .and_then(|state| state.pointer("/result/value"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if ready {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Write `<name>.timeout.png` (when the page can still be captured) and
/// `<name>.timeout.txt` (`report` plus the page's DOM) to `dir`.
fn save_timeout_diagnostics(
    cdp: &mut Cdp,
    session: Option<&str>,
    dir: &Path,
    name: &str,
    report: &str,
) -> Vec<PathBuf> {
    let mut saved = Vec::new();
    let shot = cdp.try_call(
        session,
        "Page.captureScreenshot",
        json!({ "format": "png" }),
    );
    let shot_note = match shot.as_ref().map(|shot| shot["data"].as_str()) {
        Ok(Some(data)) => {
            let path = dir.join(format!("{name}.timeout.png"));
            match std::fs::write(&path, base64_decode(data)) {
                Ok(()) => {
                    saved.push(path);
                    "saved".to_string()
                }
                Err(err) => format!("not saved: {err}"),
            }
        }
        Ok(None) => "no data in the reply".to_string(),
        Err(err) => format!("failed: {err}"),
    };
    let dom = cdp
        .try_call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "document.documentElement ? document.documentElement.outerHTML : ''",
                "returnByValue": true,
            }),
        )
        .map(|reply| {
            reply["result"]["value"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_else(|err| format!("(could not read the DOM: {err})"));
    let text = format!("{report}\n\nscreenshot: {shot_note}\n\ndom:\n{dom}\n");
    let path = dir.join(format!("{name}.timeout.txt"));
    if std::fs::write(&path, text).is_ok() {
        saved.push(path);
    }
    saved
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
        if cfg!(windows) {
            // The default software GL on GPU-less Windows runners (D3D11 WARP)
            // leaves speedscope's render-to-texture framebuffer incomplete.
            command.arg("--use-angle=swiftshader");
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
        let value = self
            .try_call(session, method, params)
            .unwrap_or_else(|error| panic!("{method}: {error}"));
        if let Some(details) = value.get("exceptionDetails") {
            panic!("{method} threw in the page: {details}");
        }
        value
    }

    /// The command's result, or the protocol error it returned.
    fn try_call(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, Value> {
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
                return match value.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(value["result"].clone()),
                };
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
                    let url = entry["url"].as_str().unwrap_or_default();
                    // Chrome asks for /favicon.ico at the origin root, outside
                    // the token, and DevTools asks for source maps that are
                    // not shipped. Neither affects rendering.
                    let benign = url.ends_with("/favicon.ico") || url.ends_with(".map");
                    (level == "error"
                        && !benign
                        && matches!(source, "security" | "network" | "javascript"))
                    .then(|| format!("{source}: {text} {url}"))
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
