//! Records a real Node process and checks the `.cpuprofile` Chrome can load.

use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

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

fn busy_script() -> String {
    r#"
        function anyprofSampleWork(n) {
            let x = n;
            for (let i = 0; i < 200000; i++) {
                x = Math.imul(x ^ i, 1664525) + 1013904223;
            }
            return x;
        }
        function loop() {
            const end = Date.now() + 5;
            while (Date.now() < end) anyprofSampleWork(end);
            setTimeout(loop, 0);
        }
        loop();
    "#
    .to_string()
}

fn assert_devtools_profile(bytes: &[u8]) {
    let value: serde_json::Value = serde_json::from_slice(bytes).expect("cpuprofile JSON");
    let nodes = value["nodes"].as_array().expect("nodes");
    let samples = value["samples"].as_array().expect("samples");
    let deltas = value["timeDeltas"].as_array().expect("timeDeltas");
    assert!(
        samples.len() >= 40,
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
    assert!(
        text.contains("anyprofSampleWork"),
        "busy function missing from profile"
    );
}

#[test]
fn cpuprofile_of_a_busy_node_is_a_real_v8_profile() {
    if !node_available() {
        eprintln!("skipping live cpuprofile record: node is not on PATH");
        return;
    }
    if TcpListener::bind("127.0.0.1:9229").is_err() {
        panic!("127.0.0.1:9229 is already in use; the live profile test needs it free");
    }

    let mut child = spawn_node(&["-e", &busy_script()]);
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
    let _ = child.kill();
    let _ = child.wait();
    assert!(output.status.success(), "record failed: {stderr}");
    assert!(
        !stderr.contains("warning:"),
        "a busy profile should not warn: {stderr}"
    );
    let bytes = std::fs::read(&output_path).expect("read cpuprofile");
    assert_devtools_profile(&bytes);
    let _ = std::fs::remove_file(&output_path);

    // Extension inference, and a stale inspector on 9229 must not be profiled quietly.
    let mut stale = spawn_node(&[
        "--inspect=127.0.0.1:9229",
        "-e",
        "setInterval(() => {}, 1000)",
    ]);
    let mut busy = spawn_node(&["-e", &busy_script()]);
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
            && idle_err.contains("(idle)")
            && idle_err.contains("(program)"),
        "idle profile should warn: {idle_err}"
    );
    let _ = std::fs::remove_file(&idle_path);
}
