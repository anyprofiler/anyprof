//! Records a real Node process and checks the `.cpuprofile` Chrome can load.

use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::Instant;

/// The CLI live tests both need 127.0.0.1:9229.
static PORT_9229: Mutex<()> = Mutex::new(());

fn anyprof(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(args)
        .output()
        .expect("run anyprof")
}

fn node_available() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

fn spawn_node(args: &[&str]) -> std::process::Child {
    Command::new("node")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn node")
}

/// Same sustained loop as the node crate's live test. The V8 flags and
/// `%NeverOptimizeFunction` keep `anyprofSampleWork` from being inlined away.
const BUSY_JS: &str = r#"
function anyprofSampleWork(n) {
    let x = n | 0;
    for (let i = 0; i < 10000; i++) {
        x = Math.imul(x ^ i, 1664525) + 1013904223 | 0;
    }
    return x;
}
%NeverOptimizeFunction(anyprofSampleWork);
const deadline = Date.now() + 120000;
while (Date.now() < deadline) {
    anyprofSampleWork(deadline);
}
"#;

fn write_busy_script() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("anyprof-sample-work-{}.js", std::process::id()));
    std::fs::write(&path, BUSY_JS).expect("write the sample fixture");
    path
}

fn node_args(script: &std::path::Path) -> Vec<String> {
    vec![
        "--allow-natives-syntax".to_string(),
        "--no-turbo-inlining".to_string(),
        "--no-maglev-inlining".to_string(),
        "--max-inlined-bytecode-size=0".to_string(),
        "--max-inlined-bytecode-size-small=0".to_string(),
        script.to_str().expect("fixture path is utf-8").to_string(),
    ]
}

/// Median gap between samples, in microseconds.
fn median_time_delta_us(bytes: &[u8]) -> i64 {
    let value: serde_json::Value = serde_json::from_slice(bytes).expect("cpuprofile JSON");
    let mut deltas: Vec<i64> = value["timeDeltas"]
        .as_array()
        .expect("timeDeltas")
        .iter()
        .map(|delta| delta.as_i64().expect("integer timeDelta"))
        .collect();
    assert!(!deltas.is_empty(), "profile has no timeDeltas");
    deltas.sort_unstable();
    deltas[deltas.len() / 2]
}

fn assert_devtools_profile(bytes: &[u8], script_name: &str) {
    let value: serde_json::Value = serde_json::from_slice(bytes).expect("cpuprofile JSON");
    let nodes = value["nodes"].as_array().expect("nodes");
    let samples = value["samples"].as_array().expect("samples");
    let deltas = value["timeDeltas"].as_array().expect("timeDeltas");
    // The old exporter bug wrote one or two aggregated samples. Sampling
    // density is checked separately with `median_time_delta_us`.
    assert!(
        samples.len() >= 15,
        "expected many samples, got {}",
        samples.len()
    );
    assert_eq!(samples.len(), deltas.len());
    let start = value["startTime"].as_f64().expect("startTime");
    let end = value["endTime"].as_f64().expect("endTime");
    assert!(end > start, "startTime {start} endTime {end}");
    assert_eq!(nodes[0]["callFrame"]["functionName"], "(root)");
    assert!(nodes[0]["callFrame"]["scriptId"].is_string());
    assert!(nodes[0]["children"].as_array().is_some());

    let mut ids = std::collections::HashSet::new();
    let mut children = std::collections::HashSet::new();
    for node in nodes {
        let id = node["id"].as_u64().expect("id");
        assert!(ids.insert(id), "duplicate node id {id}");
        let frame = &node["callFrame"];
        for field in [
            "functionName",
            "scriptId",
            "url",
            "lineNumber",
            "columnNumber",
        ] {
            assert!(frame.get(field).is_some(), "callFrame missing {field}");
        }
        if let Some(kids) = node["children"].as_array() {
            for child in kids {
                children.insert(child.as_u64().expect("child id"));
            }
        }
    }
    for sample in samples {
        let id = sample.as_u64().expect("sample id");
        assert!(ids.contains(&id), "sample {id} is not a node");
    }
    for node in nodes.iter().skip(1) {
        let id = node["id"].as_u64().unwrap();
        assert!(
            children.contains(&id),
            "node {id} is not linked from a parent"
        );
    }
    let text = String::from_utf8_lossy(bytes);
    if text.contains("anyprofSampleWork") || text.contains(script_name) {
        return;
    }
    let mut counts: Vec<(String, u64)> = nodes
        .iter()
        .filter_map(|node| {
            let name = node["callFrame"]["functionName"].as_str()?.to_string();
            let url = node["callFrame"]["url"].as_str().unwrap_or("");
            Some((format!("{name} {url}"), 1))
        })
        .collect();
    counts.sort();
    panic!(
        "expected anyprofSampleWork or {script_name} in the profile.\nFrames:\n{}",
        counts
            .into_iter()
            .take(12)
            .map(|(name, _)| format!("  {name}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn cpuprofile_of_a_busy_node_is_a_real_v8_profile() {
    let _port = PORT_9229.lock().unwrap_or_else(|err| err.into_inner());
    if !node_available() {
        eprintln!("skipping live cpuprofile record: node is not on PATH");
        return;
    }
    if TcpListener::bind("127.0.0.1:9229").is_err() {
        panic!("127.0.0.1:9229 is already in use; the live profile test needs it free");
    }

    let script = write_busy_script();
    let script_name = script.file_name().unwrap().to_string_lossy().into_owned();
    let node_arguments = node_args(&script);
    let node_arguments: Vec<&str> = node_arguments.iter().map(String::as_str).collect();
    let mut child = spawn_node(&node_arguments);
    let pid = child.id().to_string();
    // Node installs its SIGUSR1 handler during startup.
    thread::sleep(Duration::from_millis(500));

    let output_path =
        std::env::temp_dir().join(format!("anyprof-live-{}.cpuprofile", std::process::id()));
    let _ = std::fs::remove_file(&output_path);
    let output = anyprof(&[
        "record",
        "--pid",
        pid.as_str(),
        "--duration",
        "2s",
        "--format",
        "cpuprofile",
        "-o",
        &output_path.to_string_lossy(),
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let explicit_path =
        std::env::temp_dir().join(format!("anyprof-live-f-{}.cpuprofile", std::process::id()));
    let _ = std::fs::remove_file(&explicit_path);
    let explicit = output.status.success().then(|| {
        anyprof(&[
            "record",
            "--pid",
            pid.as_str(),
            "--duration",
            "1s",
            "-F",
            "200",
            "-o",
            &explicit_path.to_string_lossy(),
        ])
    });
    let _ = child.kill();
    let _ = child.wait();
    assert!(output.status.success(), "record failed: {stderr}");
    assert!(
        stderr.contains("sampling every 100us (10000 Hz)"),
        "node should default to the DevTools interval: {stderr}"
    );
    assert!(
        !stderr.contains("warning:"),
        "a busy profile should not warn: {stderr}"
    );
    let bytes = std::fs::read(&output_path).expect("read cpuprofile");
    assert_devtools_profile(&bytes, &script_name);
    // DevTools records a median of about 125us. The old 99 Hz default was 10101us.
    // A loaded Windows runner has delivered a 2ms median for a 100us request, so
    // the bound only has to rule out the old default.
    let median = median_time_delta_us(&bytes);
    assert!(
        median < 5_000,
        "default sampling should be far denser than 10ms, median timeDelta {median}us: {stderr}"
    );
    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&script);

    let explicit = explicit.expect("explicit -F record ran");
    let explicit_err = String::from_utf8_lossy(&explicit.stderr);
    assert!(
        explicit.status.success(),
        "-F 200 record failed: {explicit_err}"
    );
    assert!(
        explicit_err.contains("sampling every 5000us (200 Hz)"),
        "{explicit_err}"
    );
    let explicit_bytes = std::fs::read(&explicit_path).expect("read -F cpuprofile");
    let median = median_time_delta_us(&explicit_bytes);
    assert!(
        median >= 4_000,
        "-F 200 should win over the 100us default, median timeDelta {median}us"
    );
    let _ = std::fs::remove_file(&explicit_path);

    // Extension inference, and a stale inspector on 9229 must not be profiled quietly.
    let mut stale = spawn_node(&[
        "--inspect=127.0.0.1:9229",
        "-e",
        "setInterval(() => {}, 1000)",
    ]);
    let mut busy = spawn_node(&["-e", "setInterval(() => {}, 1000)"]);
    thread::sleep(Duration::from_millis(500));
    let stale_pid = stale.id().to_string();
    let busy_pid = busy.id().to_string();
    let wrong_path =
        std::env::temp_dir().join(format!("anyprof-stale-{}.cpuprofile", std::process::id()));
    let _ = std::fs::remove_file(&wrong_path);
    let wrong = anyprof(&[
        "record",
        "--pid",
        busy_pid.as_str(),
        "--inspect",
        "9229",
        "--duration",
        "1s",
        "-o",
        &wrong_path.to_string_lossy(),
    ]);
    let wrong_err = String::from_utf8_lossy(&wrong.stderr);
    let _ = busy.kill();
    let _ = stale.kill();
    let _ = busy.wait();
    let _ = stale.wait();
    assert!(
        !wrong.status.success(),
        "profiling a different inspector must fail: {wrong_err}"
    );
    assert!(
        wrong_err.contains(&busy_pid) && wrong_err.contains(&stale_pid),
        "error should name both pids: {wrong_err}"
    );
    assert!(wrong_err.contains("not the requested pid"), "{wrong_err}");

    let mut idle = spawn_node(&[
        "-e",
        "function idleWait(){setTimeout(idleWait,1000)} idleWait()",
    ]);
    thread::sleep(Duration::from_millis(500));
    let idle_pid = idle.id().to_string();
    let idle_path =
        std::env::temp_dir().join(format!("anyprof-idle-{}.cpuprofile", std::process::id()));
    let _ = std::fs::remove_file(&idle_path);
    let idle_out = anyprof(&[
        "record",
        "--pid",
        idle_pid.as_str(),
        "--duration",
        "1s",
        "--format",
        "cpuprofile",
        "-o",
        &idle_path.to_string_lossy(),
    ]);
    let idle_err = String::from_utf8_lossy(&idle_out.stderr);
    let _ = idle.kill();
    let _ = idle.wait();
    assert!(idle_out.status.success(), "idle record failed: {idle_err}");
    assert!(
        idle_err.contains("warning:")
            && idle_err.contains("(idle) or (program)")
            && idle_err.contains("caught work")
            && idle_err.contains("under load"),
        "idle profile should warn: {idle_err}"
    );
    let _ = std::fs::remove_file(&idle_path);
}

/// Ctrl+C is the end of an open-ended recording, not an abort.
#[cfg(unix)]
#[test]
fn sigint_ends_an_open_recording_and_writes_the_profile() {
    let _port = PORT_9229.lock().unwrap_or_else(|err| err.into_inner());
    if !node_available() {
        eprintln!("skipping sigint record: node is not on PATH");
        return;
    }
    if TcpListener::bind("127.0.0.1:9229").is_err() {
        panic!("127.0.0.1:9229 is already in use; the live profile test needs it free");
    }

    let script = write_busy_script();
    let script_name = script.file_name().unwrap().to_string_lossy().into_owned();
    let node_arguments = node_args(&script);
    let node_arguments: Vec<&str> = node_arguments.iter().map(String::as_str).collect();
    let mut node = spawn_node(&node_arguments);
    let pid = node.id().to_string();
    thread::sleep(Duration::from_millis(500));

    let output_path =
        std::env::temp_dir().join(format!("anyprof-sigint-{}.cpuprofile", std::process::id()));
    let _ = std::fs::remove_file(&output_path);
    let mut recorder = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(["rec", "-p", pid.as_str(), "-f", "cpuprofile", "-o"])
        .arg(&output_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn anyprof");
    let stderr_pipe = recorder.stderr.take().expect("stderr");
    let captured = Arc::new(Mutex::new(String::new()));
    let captured_thread = Arc::clone(&captured);
    let reader = thread::spawn(move || {
        let mut pipe = stderr_pipe;
        let mut buf = [0_u8; 256];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]);
                    captured_thread.lock().unwrap().push_str(&chunk);
                }
            }
        }
    });

    let started = Instant::now();
    let mut saw_recording = false;
    while started.elapsed() < Duration::from_secs(20) {
        if recorder.try_wait().ok().flatten().is_some() {
            break;
        }
        if captured.lock().unwrap().contains("Recording") {
            saw_recording = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let early = captured.lock().unwrap().clone();
    assert!(
        saw_recording,
        "recording did not start before the interrupt: {early}"
    );
    thread::sleep(Duration::from_secs(2));
    let killed = Command::new("kill")
        .args(["-INT", &recorder.id().to_string()])
        .status()
        .expect("send SIGINT");
    assert!(killed.success(), "kill -INT failed");

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = recorder.try_wait().expect("wait for anyprof") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = recorder.kill();
            let _ = recorder.wait();
            panic!(
                "anyprof did not exit after SIGINT.\nstderr:\n{}",
                captured.lock().unwrap()
            );
        }
        thread::sleep(Duration::from_millis(50));
    };
    let _ = reader.join();
    let stderr = captured.lock().unwrap().clone();
    let _ = node.kill();
    let _ = node.wait();
    let _ = std::fs::remove_file(&script);
    assert!(status.success(), "SIGINT should save and exit 0: {stderr}");
    let bytes = std::fs::read(&output_path).expect("read cpuprofile");
    assert!(!bytes.is_empty(), "profile should not be empty: {stderr}");
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("cpuprofile JSON");
    let samples = value["samples"].as_array().expect("samples");
    assert!(!samples.is_empty(), "expected samples after ~2s: {stderr}");
    assert_eq!(
        samples.len(),
        value["timeDeltas"].as_array().map(Vec::len).unwrap_or(0)
    );
    assert_eq!(value["nodes"][0]["callFrame"]["functionName"], "(root)");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("anyprofSampleWork") || text.contains(&script_name),
        "fixture missing from sigint profile: {stderr}"
    );
    let _ = std::fs::remove_file(&output_path);
}
