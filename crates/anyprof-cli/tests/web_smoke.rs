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
            std::env::var("ANYPROF_REQUIRE_CHROME").ok().as_deref() != Some("1"),
            "ANYPROF_REQUIRE_CHROME=1 is set but no Chrome was found (set ANYPROF_CHROME)"
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
    let synthetic = dir.join("synthetic-workers.speedscope.json");
    std::fs::write(&synthetic, synthetic_workers()).unwrap();
    cases.push((synthetic, "studio"));
    let deep_src = dir.join("deep_app.py");
    std::fs::write(
        &deep_src,
        "def root_entry():\n    return dispatch()\ndef narrow_branch():\n    return hidden_leaf()\ndef hidden_leaf():\n    return 42\n",
    )
    .unwrap();
    let deep = dir.join("deep-zoom.speedscope.json");
    std::fs::write(&deep, deep_zoom_fixture(&deep_src)).unwrap();
    cases.push((deep, "deep_zoom"));
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
    check_diff(&mut cdp, &dir, artifacts.as_deref().unwrap_or(&dir));
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
        "studio" => {
            assert!(
                lower.contains("sample activity"),
                "{name}: synthetic series had no sample activity:\n{text}"
            );
            assert!(!lower.contains("no timeline"), "{name}:\n{text}");
            assert!(
                text.contains("Main thread") && text.contains("Worker"),
                "{name}: both profiles were not listed:\n{text}"
            );
            assert!(
                !lower.contains("cpu usage"),
                "{name}: sample activity was labeled as CPU usage:\n{text}"
            );
        }
        "deep_zoom" => {
            assert!(
                text.contains("deep_app.py:1") && text.contains("deep_app.py:2"),
                "{name}: source file and line were not shown in call tree:\n{text}"
            );
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
    if kind == "studio" {
        assert_studio_chart(cdp, session, &name);
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
    if kind == "studio" {
        assert!(
            wait_for(
                cdp,
                session,
                "document.querySelector('#detail').textContent.includes('main') && document.querySelectorAll('[data-testid=tree-row]').length > 8"
            ),
            "{name}: the call tree did not select a function or stayed too small to read"
        );
        drag_timeline(cdp, session);
        assert!(
            wait_for(
                cdp,
                session,
                "!document.body.innerText.includes('tokenize') && document.body.innerText.includes('commit_frame') && !!document.querySelector('[data-testid=time-filter-badge]') && !document.querySelector('[data-testid=time-filter-badge]').hidden"
            ),
            "{name}: the late interval did not keep commit_frame, drop tokenize, and show time-filter-badge"
        );
        slide_timeline_brush(cdp, session);
        assert!(
            wait_for(
                cdp,
                session,
                "document.body.innerText.includes('tokenize') && !document.body.innerText.includes('commit_frame')"
            ),
            "{name}: sliding the timeline brush window left did not switch to the early tokenize interval"
        );
        cdp.call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "document.querySelector('[data-testid=clear-time-filter]').click()",
                "returnByValue": true,
            }),
        );
        assert!(
            wait_for(
                cdp,
                session,
                "document.body.innerText.includes('tokenize') && document.body.innerText.includes('commit_frame') && document.querySelector('[data-testid=time-filter-badge]').hidden"
            ),
            "{name}: clear-time-filter did not restore the full timeline"
        );
        drag_timeline(cdp, session);
        assert!(
            wait_for(
                cdp,
                session,
                "!document.body.innerText.includes('tokenize') && document.body.innerText.includes('commit_frame')"
            ),
            "{name}: re-dragging the late interval failed"
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
    }
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

    let colored = canvas_colored_ratio(cdp, session);
    assert!(
        colored >= 0.08,
        "{name}: flame graph canvas was almost empty ({:.1}% colored pixels)",
        colored * 100.0
    );

    if kind == "deep_zoom" {
        assert_deep_zoom_and_source(cdp, session, &name);
    }

    if (kind == "aggregate" && name.ends_with(".collapsed")) || kind == "studio" {
        cdp.call(
            session,
            "Runtime.evaluate",
            json!({
                "expression": "document.querySelector('[data-testid=theme-dark]').click()",
                "returnByValue": true,
            }),
        );
        if kind == "studio" {
            cdp.call(
                session,
                "Runtime.evaluate",
                json!({
                    "expression": "document.querySelector('[data-testid=tab-tree]').click()",
                    "returnByValue": true,
                }),
            );
            assert!(
                wait_for(
                    cdp,
                    session,
                    "!!document.querySelector('[data-testid=tree-row]')"
                ),
                "{name}: dark call tree did not render"
            );
        }
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
    if kind == "studio" {
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
                "document.documentElement.dataset.theme === 'dark' && !!document.querySelector('[data-testid=flame]')"
            ),
            "{name}: dark flame graph did not render"
        );
        std::thread::sleep(Duration::from_millis(200));
        let dark_flame = canvas_colored_ratio(cdp, session);
        assert!(
            dark_flame >= 0.08,
            "{name}: dark flame graph was almost empty ({:.1}% colored pixels)",
            dark_flame * 100.0
        );
        let shot = cdp.call(
            session,
            "Page.captureScreenshot",
            json!({ "format": "png" }),
        );
        std::fs::write(
            keep.join(format!("{name}.dark-flame.png")),
            base64_decode(shot["data"].as_str().unwrap()),
        )
        .unwrap();
        let (before, after) = drag_inspector(cdp, session);
        assert!(
            after >= before + 40.0,
            "{name}: dragging the detail splitter left the inspector at {after}px from {before}px"
        );
        std::thread::sleep(Duration::from_millis(150));
        let shot = cdp.call(
            session,
            "Page.captureScreenshot",
            json!({ "format": "png" }),
        );
        std::fs::write(
            keep.join(format!("{name}.splitter.png")),
            base64_decode(shot["data"].as_str().unwrap()),
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

fn assert_studio_chart(cdp: &mut Cdp, session: Option<&str>, name: &str) {
    let expression = r#"(function () {
  const button = document.querySelector('[data-testid=timeline-help]');
  button.click();
  const panel = document.querySelector('[data-testid=timeline-help-panel]');
  const text = panel.innerText || '';
  const box = panel.getBoundingClientRect();
  const help = {
    open: !panel.hidden && box.height > 36 && panel.scrollWidth <= panel.clientWidth + 2,
    weight: text.indexOf('sample weight') >= 0,
    cpu: text.indexOf('not a CPU percent') >= 0,
    tracks: text.indexOf('not thread ids recovered') >= 0
  };
  button.click();
  help.closed = !!panel.hidden;
  const head = document.querySelector('.timeline-head');
  const title = head.querySelector('strong');
  const hint = head.querySelector('.hint');
  const range = document.getElementById('range-label');
  function oneLine(node) {
    return !!node && node.scrollWidth <= node.clientWidth + 1 && node.scrollHeight <= 22;
  }
  const ticks = Array.from(document.querySelectorAll('.ruler span')).map(function (node) {
    return node.textContent.trim();
  });
  const units = {};
  ticks.forEach(function (tick) {
    const parts = tick.split(' ');
    units[parts[parts.length - 1]] = true;
  });
  const unitNames = Object.keys(units);
  const numbers = ticks.map(function (tick) { return tick.split(' ')[0]; }).join(',');
  const row = document.querySelector('.row[aria-selected="true"]');
  const nums = row ? Array.from(row.querySelectorAll('.num')).map(function (node) { return node.textContent; }) : [];
  const canvas = document.querySelector('#timeline');
  let gap = { ok: false, gap: 0, marks: 0 };
  if (canvas && canvas.width) {
    const ratio = window.devicePixelRatio || 1;
    const gutter = Math.floor(128 * ratio);
    const trackH = Math.floor(26 * ratio);
    const data = canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data;
    const width = canvas.width;
    function isMark(x, y) {
      const i = (y * width + x) * 4;
      const r = data[i], g = data[i + 1], b = data[i + 2], a = data[i + 3];
      return a > 40 && b > r + 15 && b > g;
    }
    let best = null;
    for (let y = 1; y < trackH - 1; y++) {
      let marks = 0, gaps = 0;
      for (let x = gutter; x < width; x++) {
        if (isMark(x, y)) marks++;
        else gaps++;
      }
      if (!best || marks > best.marks) best = { marks: marks, gaps: gaps };
    }
    if (best && best.marks >= 8) {
      const share = best.gaps / (best.marks + best.gaps);
      gap = { ok: share >= 0.35, gap: share, marks: best.marks };
    }
  }
  const colW = document.querySelector('[data-testid=col-weight]');
  const colS = document.querySelector('[data-testid=col-self]');
  const colSym = document.querySelector('[data-testid=col-symbol]');
  const instrumentsCols = !!(colW && colS && colSym
    && colW.getBoundingClientRect().left < colS.getBoundingClientRect().left
    && colS.getBoundingClientRect().left < colSym.getBoundingClientRect().left
    && row && row.querySelector('.weight-cell .w-val')
    && row.querySelector('.weight-cell .w-pct')
    && row.querySelector('.self-cell .self-val')
    && row.querySelector('.self-cell .cat-icon svg'));
  return {
    help: help,
    title: oneLine(title),
    hint: oneLine(hint) && hint.textContent.indexOf('Not CPU utilization') === 0,
    range: oneLine(range),
    ruler: unitNames.length === 1 && unitNames[0] === 'ms' && numbers === '0,500,1000,1500,2000',
    ticks: ticks,
    zero: nums.indexOf('0') >= 0 && document.body.innerText.indexOf('0 ns') < 0,
    instrumentsCols: instrumentsCols,
    scope: (document.querySelector('[data-testid=heaviest-scope]') || {}).textContent || '',
    first: (document.querySelector('#heaviest li .heavy-fn') || {}).textContent || '',
    heavyWeight: (document.querySelector('#heaviest li .heavy-weight') || {}).textContent || '',
    gap: gap
  };
})()"#;
    let report = cdp.call(
        session,
        "Runtime.evaluate",
        json!({ "expression": expression, "returnByValue": true }),
    );
    let value = report
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null);
    assert!(
        value["help"]["open"].as_bool() == Some(true)
            && value["help"]["closed"].as_bool() == Some(true)
            && value["help"]["weight"].as_bool() == Some(true)
            && value["help"]["cpu"].as_bool() == Some(true)
            && value["help"]["tracks"].as_bool() == Some(true),
        "{name}: timeline details were clipped or incomplete: {value}"
    );
    assert!(
        value["title"].as_bool() == Some(true)
            && value["hint"].as_bool() == Some(true)
            && value["range"].as_bool() == Some(true),
        "{name}: timeline header clipped: {value}"
    );
    assert!(
        value["ruler"].as_bool() == Some(true),
        "{name}: ruler was not one millisecond scale: {value}"
    );
    assert!(
        value["zero"].as_bool() == Some(true),
        "{name}: a zero weight was not plain 0: {value}"
    );
    assert!(
        value["instrumentsCols"].as_bool() == Some(true),
        "{name}: Instruments left-gutter columns (Weight | Self Weight | Symbol Names) were not rendered: {value}"
    );
    assert!(
        value["scope"].as_str() == Some("Through the selected function")
            && value["first"].as_str() == Some("main")
            && !value["heavyWeight"].as_str().unwrap_or_default().is_empty(),
        "{name}: heaviest path was not the selected function: {value}"
    );
    assert!(
        value["gap"]["ok"].as_bool() == Some(true),
        "{name}: sample marks formed a solid band: {value}"
    );
}

/// Drag the detail splitter toward the call tree and return the inspector
/// width before and after.
fn drag_inspector(cdp: &mut Cdp, session: Option<&str>) -> (f64, f64) {
    let point = cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": "(function(){ const splitter = document.querySelector('#split-inspector'); const inspector = document.querySelector('.inspector'); const rect = splitter.getBoundingClientRect(); return {x: rect.left + rect.width / 2, y: rect.top + 80, before: inspector.getBoundingClientRect().width}; })()",
            "returnByValue": true,
        }),
    );
    let value = point
        .pointer("/result/value")
        .cloned()
        .unwrap_or(Value::Null);
    let x = value["x"]
        .as_f64()
        .unwrap_or_else(|| panic!("splitter rect: {point}"));
    let y = value["y"].as_f64().unwrap();
    let before = value["before"].as_f64().unwrap();
    let next = x - 90.0;
    for (kind, px) in [
        ("mousePressed", x),
        ("mouseMoved", next),
        ("mouseReleased", next),
    ] {
        cdp.call(
            session,
            "Input.dispatchMouseEvent",
            json!({
                "type": kind,
                "x": px,
                "y": y,
                "button": "left",
                "buttons": if kind == "mouseReleased" { 0 } else { 1 },
                "clickCount": 1,
            }),
        );
    }
    let after = cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": "document.querySelector('.inspector').getBoundingClientRect().width",
            "returnByValue": true,
        }),
    );
    let width = after
        .pointer("/result/value")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    (before, width)
}

/// Drag across the right side of the sample chart, where the fixture's last
/// sample is `hot` and the earlier samples are `leaf`.
fn drag_timeline(cdp: &mut Cdp, session: Option<&str>) {
    let point = cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": "(function(){ const canvas = document.querySelector('#timeline'); const rect = canvas.getBoundingClientRect(); const gutter = 128; const plot = Math.max(1, rect.width - gutter); return {y: rect.top + Math.min(8, rect.height / 2), x0: rect.left + gutter + plot * 0.80, x1: rect.left + gutter + plot * 0.97}; })()",
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

/// Grab the interior of the active `[0.80, 0.97]` brush selection (`0.885`) and
/// slide it left into the early `[0.06, 0.23]` interval.
fn slide_timeline_brush(cdp: &mut Cdp, session: Option<&str>) {
    let point = cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": "(function(){ const canvas = document.querySelector('#timeline'); const rect = canvas.getBoundingClientRect(); const gutter = 128; const plot = Math.max(1, rect.width - gutter); return {y: rect.top + Math.min(8, rect.height / 2), x0: rect.left + gutter + plot * 0.885, x1: rect.left + gutter + plot * 0.145}; })()",
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

/// Share of chromatic pixels on the flame canvas itself.
///
/// Compact rows no longer stretch to fill the page, so the page-band check
/// would call a real graph blank. The canvas is only as tall as its rows.
fn canvas_colored_ratio(cdp: &mut Cdp, session: Option<&str>) -> f64 {
    let value = cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": "(function(){ const canvas = document.querySelector('[data-testid=flame]'); if (!canvas || !canvas.width || !canvas.height) return 0; const data = canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data; let colored = 0, total = 0; for (let i = 0; i < data.length; i += 16) { const max = Math.max(data[i], data[i+1], data[i+2]); const min = Math.min(data[i], data[i+1], data[i+2]); total++; if (data[i+3] > 0 && max - min > 24) colored++; } return total ? colored / total : 0; })()",
            "returnByValue": true,
        }),
    );
    value
        .pointer("/result/value")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
}

fn synthetic_workers() -> String {
    let frames = [
        "main",
        "event_loop",
        "accept",
        "read_headers",
        "parse_request",
        "tokenize",
        "lex_ident",
        "lex_string",
        "decode_query",
        "render_tree",
        "layout_boxes",
        "measure_text",
        "place_flex",
        "paint_layer",
        "fill_rect",
        "commit_frame",
        "swap_buffers",
        "worker_loop",
        "pop_job",
        "decode_block",
        "inflate_chunk",
        "hash_block",
        "write_buffer",
        "checksum",
        "flush_buffer",
        "fsync_batch",
    ];
    let frame_json = frames
        .iter()
        .map(|name| format!(r#"{{"name":"{name}"}}"#))
        .collect::<Vec<_>>()
        .join(",");
    let mut main_samples = Vec::new();
    let mut worker_samples = Vec::new();
    for i in 0..80 {
        let main: &[u64] = if i < 32 {
            match i % 4 {
                0 => &[0, 1, 2, 3, 4, 5, 6],
                1 => &[0, 1, 2, 3, 4, 5, 7],
                2 => &[0, 1, 2, 3, 4, 8],
                _ => &[0, 1, 2, 3, 4, 5],
            }
        } else if i < 64 {
            match i % 4 {
                0 => &[0, 1, 2, 9, 10, 11],
                1 => &[0, 1, 2, 9, 10, 12],
                2 => &[0, 1, 2, 9, 13, 14],
                _ => &[0, 1, 2, 9, 10],
            }
        } else if i % 2 == 0 {
            &[0, 1, 2, 9, 10, 15, 16]
        } else {
            &[0, 1, 2, 9, 13, 15]
        };
        let worker: &[u64] = if i < 32 {
            if i % 2 == 0 {
                &[17, 18, 19, 20]
            } else {
                &[17, 18, 21]
            }
        } else if i < 64 {
            &[17, 18, 22, 23]
        } else {
            &[17, 18, 24, 25]
        };
        main_samples.push(format!(
            "[{}]",
            main.iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ));
        worker_samples.push(format!(
            "[{}]",
            worker
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    let weights = vec!["25"; 80].join(",");
    format!(
        r#"{{"shared":{{"frames":[{frames}]}},"profiles":[{{"type":"sampled","name":"Main thread","unit":"milliseconds","startValue":0,"endValue":2000,"samples":[{main}],"weights":[{weights}]}},{{"type":"sampled","name":"Worker","unit":"milliseconds","startValue":0,"endValue":2000,"samples":[{worker}],"weights":[{weights}]}}]}}"#,
        frames = frame_json,
        main = main_samples.join(","),
        worker = worker_samples.join(","),
        weights = weights,
    )
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

fn deep_zoom_fixture(source_file: &Path) -> String {
    let file_str = source_file.to_string_lossy().replace('\\', "/");
    let mut frames = vec![
        serde_json::json!({ "name": "[python] root_entry", "file": file_str, "line": 1 }),
        serde_json::json!({ "name": "[python] narrow_branch", "file": file_str, "line": 4 }),
        serde_json::json!({ "name": "[python] hidden_leaf", "file": file_str, "line": 6 }),
    ];
    let mut deep_stack = vec![0_usize];
    for level in 1..60 {
        let idx = frames.len();
        frames.push(serde_json::json!({
            "name": format!("[python] deep_step_{level}"),
            "file": file_str,
            "line": 2,
        }));
        deep_stack.push(idx);
    }
    let narrow_stack = vec![0_usize, 1_usize, 2_usize];
    serde_json::json!({
        "$schema": "https://www.speedscope.app/file-format-schema.json",
        "shared": { "frames": frames },
        "profiles": [{
            "type": "sampled",
            "name": "main",
            "unit": "none",
            "startValue": 0,
            "endValue": 100,
            "samples": [deep_stack, narrow_stack],
            "weights": [98, 2]
        }]
    })
    .to_string()
}

fn assert_deep_zoom_and_source(cdp: &mut Cdp, session: Option<&str>, name: &str) {
    // 1. All 60 levels must render on #flame (height >= 60 * 18 = 1080 CSS px) with no 40-level note.
    let metrics = cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": r#"(function () {
                const canvas = document.querySelector('[data-testid=flame]');
                const rect = canvas.getBoundingClientRect();
                const note = document.querySelector('.flame-note');
                // Level 1 has deep_step_1 (98%) on the left and narrow_branch (2%) at x ~ 99% on the right.
                return {
                    cssHeight: rect.height,
                    hasTruncationNote: !!note,
                    clickX: rect.left + rect.width * 0.99,
                    clickY: rect.top + 18 * 1.5
                };
            })()"#,
            "returnByValue": true,
        }),
    );
    let m = &metrics["result"]["value"];
    let css_height = m["cssHeight"].as_f64().unwrap_or(0.0);
    assert!(
        css_height >= 60.0 * 18.0 - 1.0,
        "{name}: expected 60 levels (>= 1080px), got {css_height}px"
    );
    assert_eq!(
        m["hasTruncationNote"].as_bool(),
        Some(false),
        "{name}: 60-level flame graph showed a truncation warning"
    );

    // 2. Click the 2% narrow_branch bar at level 1 -> zooms subtree to 100% width & syncs call tree.
    let click_x = m["clickX"].as_f64().unwrap();
    let click_y = m["clickY"].as_f64().unwrap();
    for kind in ["mousePressed", "mouseReleased"] {
        cdp.call(
            session,
            "Input.dispatchMouseEvent",
            json!({
                "type": kind,
                "x": click_x,
                "y": click_y,
                "button": "left",
                "buttons": if kind == "mouseReleased" { 0 } else { 1 },
                "clickCount": 1,
            }),
        );
    }
    assert!(
        wait_for(
            cdp,
            session,
            "!!document.querySelector('[data-testid=flame-reset]') && document.querySelector('[data-testid=flame-zoom-bar]').textContent.includes('narrow_branch (2.0%)')"
        ),
        "{name}: clicking the 2% branch did not zoom into narrow_branch"
    );
    // Verify hidden_leaf below narrow_branch now spans the full width (hovering the middle at level 2 hits hidden_leaf).
    let mid_check = cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": r#"(function () {
                const canvas = document.querySelector('[data-testid=flame]');
                const rect = canvas.getBoundingClientRect();
                canvas.dispatchEvent(new MouseEvent('mousemove', {
                    clientX: rect.left + rect.width * 0.5,
                    clientY: rect.top + 18 * 2.5,
                    bubbles: true
                }));
                const tip = document.querySelector('.flame-host .tip');
                const selectedRow = document.querySelector('.flame-tree [data-testid=tree-row][aria-selected="true"]');
                return {
                    tip: tip ? tip.textContent : '',
                    treeSelected: selectedRow ? selectedRow.textContent : ''
                };
            })()"#,
            "returnByValue": true,
        }),
    );
    let tip_text = mid_check["result"]["value"]["tip"].as_str().unwrap_or("");
    let tree_sel = mid_check["result"]["value"]["treeSelected"]
        .as_str()
        .unwrap_or("");
    assert!(
        tip_text.contains("hidden_leaf"),
        "{name}: zoomed 2% branch did not expand hidden_leaf across canvas center (tip={tip_text:?})"
    );
    assert!(
        tree_sel.contains("narrow_branch"),
        "{name}: clicking flame bar did not sync .flame-tree selection ({tree_sel:?})"
    );

    // 3. Press Escape -> unzooms back to full 60-level view.
    cdp.call(
        session,
        "Input.dispatchKeyEvent",
        json!({
            "type": "keyDown",
            "key": "Escape",
            "code": "Escape",
            "windowsVirtualKeyCode": 27,
            "nativeVirtualKeyCode": 27,
        }),
    );
    assert!(
        wait_for(
            cdp,
            session,
            "!document.querySelector('[data-testid=flame-reset]') && document.querySelector('[data-testid=flame]').getBoundingClientRect().height >= 1079"
        ),
        "{name}: Escape did not reset flame zoom"
    );

    // 4. Click the Source button in Selected function -> loads real local source lines with target highlight.
    cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": "document.querySelector('[data-testid=open-source-btn]').click()",
            "returnByValue": true,
        }),
    );
    assert!(
        wait_for(
            cdp,
            session,
            "!!document.querySelector('.source-row.source-target[data-line=\"4\"]') && document.querySelector('[data-testid=source-rows]').textContent.includes('def narrow_branch():')"
        ),
        "{name}: Source view did not highlight line 4 of deep_app.py"
    );
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
        "{name}: switching back from Source to Flame Graph failed"
    );
}

fn check_diff(cdp: &mut Cdp, dir: &Path, keep: &Path) {
    let src_path = dir.join("diff_service.py");
    std::fs::write(
        &src_path,
        "def handle_request(req):\n    user = validate_auth(req)\n    data = legacy_regex(req)\n    body = serialize_json(data)\n    return fast_router(user, body)\n",
    )
    .unwrap();
    let src_escaped = src_path.to_string_lossy().replace('\\', "/");

    let before_json = format!(
        r#"{{
  "$schema": "https://www.speedscope.app/file-format-schema.json",
  "shared": {{
    "frames": [
      {{"name": "[python] handle_request", "file": "{src_escaped}", "line": 1}},
      {{"name": "[python] validate_auth", "file": "{src_escaped}", "line": 2}},
      {{"name": "[python] legacy_regex", "file": "{src_escaped}", "line": 3}},
      {{"name": "[python] serialize_json", "file": "{src_escaped}", "line": 4}},
      {{"name": "[python] fast_router", "file": "{src_escaped}", "line": 5}}
    ]
  }},
  "profiles": [{{
    "type": "sampled",
    "name": "before",
    "unit": "milliseconds",
    "startValue": 0,
    "endValue": 100,
    "samples": [[0, 2], [0, 3], [0, 1], [0, 4]],
    "weights": [50, 20, 15, 15]
  }}]
}}"#
    );
    let after_json = format!(
        r#"{{
  "$schema": "https://www.speedscope.app/file-format-schema.json",
  "shared": {{
    "frames": [
      {{"name": "[python] handle_request", "file": "{src_escaped}", "line": 1}},
      {{"name": "[python] validate_auth", "file": "{src_escaped}", "line": 2}},
      {{"name": "[python] serialize_json", "file": "{src_escaped}", "line": 4}},
      {{"name": "[python] fast_router", "file": "{src_escaped}", "line": 5}},
      {{"name": "[rust] simd_escape", "file": "{src_escaped}", "line": 4}}
    ]
  }},
  "profiles": [{{
    "type": "sampled",
    "name": "after",
    "unit": "milliseconds",
    "startValue": 0,
    "endValue": 135,
    "samples": [[0, 2, 4], [0, 2], [0, 1], [0, 3]],
    "weights": [55, 35, 30, 15]
  }}]
}}"#
    );

    let before = dir.join("baseline.speedscope.json");
    let after = dir.join("current.speedscope.json");
    std::fs::write(&before, before_json).unwrap();
    std::fs::write(&after, after_json).unwrap();

    let viewer = ViewerProcess::start_diff(&before, &after);
    eprintln!("diff: {}", viewer.url);

    let target = cdp.call(None, "Target.createTarget", json!({ "url": "about:blank" }));
    let target_id = target["targetId"].as_str().unwrap().to_string();
    let attached = cdp.call(
        None,
        "Target.attachToTarget",
        json!({ "targetId": target_id, "flatten": true }),
    );
    let session_id = attached["sessionId"].as_str().unwrap().to_string();
    let session = Some(session_id.as_str());
    for method in ["Runtime.enable", "Log.enable", "Page.enable"] {
        cdp.call(session, method, json!({}));
    }
    cdp.call(
        session,
        "Emulation.setDeviceMetricsOverride",
        json!({ "width": 1280, "height": 820, "deviceScaleFactor": 2, "mobile": false }),
    );
    cdp.events.clear();
    cdp.call(session, "Page.navigate", json!({ "url": viewer.url }));

    assert!(
        wait_for(
            cdp,
            session,
            "document.documentElement.dataset.ready === '1' && !!document.querySelector('[data-testid=diff-overview]') && !!document.querySelector('[data-testid=flame]')"
        ),
        "diff viewer did not finish loading"
    );

    let inspect = cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": r#"(function () {
                const overview = document.querySelector('[data-testid=diff-overview]');
                const cols = [...document.querySelectorAll('.cols.diff-cols button')].map(b => b.textContent.trim());
                const rows = [...document.querySelectorAll('[data-testid=tree-row]')].map(r => r.textContent);
                const posCount = document.querySelectorAll('.delta-pos').length;
                const negCount = document.querySelectorAll('.delta-neg').length;
                const canvas = document.querySelector('[data-testid=flame]');
                const ctx = canvas.getContext('2d');
                const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
                let redPixels = 0;
                let bluePixels = 0;
                for (let i = 0; i < data.length; i += 16) {
                    const r = data[i], g = data[i + 1], b = data[i + 2], a = data[i + 3];
                    if (a > 200) {
                        if (r > 170 && r > g + 55 && r > b + 55) redPixels += 1;
                        if (b > 170 && b > r + 55 && b > g + 20) bluePixels += 1;
                    }
                }
                return {
                    overview: overview ? overview.textContent : '',
                    cols,
                    rowText: rows.join('\n'),
                    posCount,
                    negCount,
                    redPixels,
                    bluePixels
                };
            })()"#,
            "returnByValue": true,
        }),
    )["result"]["value"]
        .clone();

    let overview_text = inspect["overview"].as_str().unwrap_or("");
    let row_text = inspect["rowText"].as_str().unwrap_or("");
    assert!(
        overview_text.contains("Baseline")
            && overview_text.contains("After")
            && overview_text.contains("Net Δ"),
        "diff overview missing pills: {overview_text:?}"
    );
    assert!(
        row_text.contains("serialize_json")
            && row_text.contains("legacy_regex")
            && row_text.contains("simd_escape"),
        "diff tree must include regressions, new frames, and 100%-eliminated stacks:\n{row_text}"
    );
    assert!(
        inspect["posCount"].as_u64().unwrap_or(0) > 0
            && inspect["negCount"].as_u64().unwrap_or(0) > 0,
        "diff view missing signed +delta / -delta badges: {inspect}"
    );
    assert!(
        inspect["redPixels"].as_u64().unwrap_or(0) > 50
            && inspect["bluePixels"].as_u64().unwrap_or(0) > 50,
        "differential flame graph must paint both red (+delta) and blue (-delta) bars: {inspect}"
    );

    let shot_light = cdp.call(
        session,
        "Page.captureScreenshot",
        json!({ "format": "png" }),
    );
    std::fs::write(
        keep.join("diff-flamegraph-light.png"),
        base64_decode(shot_light["data"].as_str().unwrap()),
    )
    .unwrap();

    // Switch to dark theme and capture dark differential flame graph screenshot too.
    cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": "document.querySelector('.theme button[data-theme=\"dark\"]').click()",
            "returnByValue": true,
        }),
    );
    std::thread::sleep(Duration::from_millis(80));
    let shot_dark = cdp.call(
        session,
        "Page.captureScreenshot",
        json!({ "format": "png" }),
    );
    std::fs::write(
        keep.join("diff-flamegraph-dark.png"),
        base64_decode(shot_dark["data"].as_str().unwrap()),
    )
    .unwrap();

    // Click inline file:line link on serialize_json to verify diff source gutter.
    cdp.call(
        session,
        "Runtime.evaluate",
        json!({
            "expression": "[...document.querySelectorAll('.loc-link')].find(el => el.textContent.includes('diff_service.py:4')).click()",
            "returnByValue": true,
        }),
    );
    assert!(
        wait_for(
            cdp,
            session,
            "!!document.querySelector('.source-cols.diff-source-cols') && !!document.querySelector('.source-row.source-reg') && !!document.querySelector('.source-row.source-imp')"
        ),
        "diff source view did not render Baseline/After/Delta Self columns and red/blue line highlights"
    );

    let problems = cdp.problems();
    assert!(
        problems.is_empty(),
        "diff viewer logged errors: {problems:#?}"
    );
    cdp.call(None, "Target.closeTarget", json!({ "targetId": target_id }));
}

struct ViewerProcess {
    child: Child,
    url: String,
    lines: mpsc::Receiver<String>,
    seen: std::cell::RefCell<Vec<String>>,
}

impl ViewerProcess {
    fn start(path: &Path) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_anyprof"));
        cmd.arg("-v")
            .arg("view")
            .arg(path)
            .args(["--no-open", "--web-timeout", "5m"]);
        Self::spawn_cmd(cmd)
    }

    fn start_diff(before: &Path, after: &Path) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_anyprof"));
        cmd.arg("-v").arg("diff").arg(before).arg(after).args([
            "-w",
            "--no-open",
            "--web-timeout",
            "5m",
        ]);
        Self::spawn_cmd(cmd)
    }

    fn spawn_cmd(mut cmd: Command) -> Self {
        let mut child = cmd
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
                .unwrap_or_else(|_| panic!("anyprof viewer printed no URL: {seen:#?}"));
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
