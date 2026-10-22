//! Profiles a real Node.js process. Skips when `node` is not on `PATH`.

use std::io::Read;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyprof_core::{Backend, RecordOptions, Target};
use anyprof_node::NodeBackend;

#[test]
fn profiles_a_live_node_process() {
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => {}
        _ => {
            eprintln!("skipping live node profile: node is not on PATH");
            return;
        }
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a free port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let script_path = std::env::temp_dir().join(format!(
        "anyprof-sample-work-{}-{port}.js",
        std::process::id()
    ));
    std::fs::write(&script_path, BUSY_JS).expect("write the sample fixture");
    let script_name = script_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut child = Command::new("node")
        .args([
            "--allow-natives-syntax",
            "--no-turbo-inlining",
            "--no-maglev-inlining",
            "--max-inlined-bytecode-size=0",
            "--max-inlined-bytecode-size-small=0",
            &format!("--inspect-port=127.0.0.1:{port}"),
            script_path.to_str().expect("fixture path is utf-8"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn node");
    let stderr_pipe = child.stderr.take();
    let stderr_thread = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut pipe) = stderr_pipe {
            let _ = pipe.read_to_string(&mut buf);
        }
        buf
    });
    let pid = child.id();
    // Node installs its SIGUSR1 handler during startup. Signaling before that
    // terminates the process. A process the user is already running has the
    // handler in place; this test has to wait for it.
    thread::sleep(Duration::from_millis(500));

    let started = NodeBackend.start(
        &Target { pid },
        &RecordOptions {
            duration: Some(Duration::from_secs(2)),
            sampling_interval: Some(Duration::from_millis(1)),
            ..RecordOptions::default()
        },
    );

    let _ = child.kill();
    let _ = child.wait();
    let stderr = stderr_thread.join().unwrap_or_default();
    let session = match started {
        Ok(session) => session,
        Err(err) => panic!("node profile failed: {err}\nnode stderr:\n{stderr}"),
    };
    let profile = session
        .stop()
        .unwrap_or_else(|err| panic!("stop failed: {err}\nnode stderr:\n{stderr}"));
    let hit_fixture = profile.stacks.iter().any(|stack| {
        stack.frames.iter().any(|frame| {
            frame.name.contains("anyprofSampleWork")
                || frame
                    .file
                    .as_deref()
                    .is_some_and(|file| file.contains(&script_name))
        })
    });
    assert!(
        hit_fixture,
        "expected anyprofSampleWork or {script_name} in a frame.\nTop stacks:\n{}\nnode stderr:\n{stderr}",
        top_stacks(&profile)
    );
    assert_eq!(profile.pid, pid);
    assert!(profile.total_weight() > 0);
    let raw = profile
        .raw_cpuprofile
        .as_deref()
        .expect("node capture keeps the original V8 profile");
    let value: serde_json::Value = serde_json::from_slice(raw).expect("raw cpuprofile is JSON");
    let samples = value["samples"].as_array().expect("samples");
    assert!(
        samples.len() > 20,
        "expected many V8 samples, got {}",
        samples.len()
    );
    assert_eq!(
        samples.len(),
        value["timeDeltas"]
            .as_array()
            .map(|deltas| deltas.len())
            .unwrap_or(0)
    );
    assert_eq!(value["nodes"][0]["callFrame"]["functionName"], "(root)");
    let raw_text = String::from_utf8_lossy(raw);
    assert!(
        raw_text.contains("anyprofSampleWork") || raw_text.contains(&script_name),
        "raw profile missing the fixture.\nTop stacks:\n{}\nnode stderr:\n{stderr}",
        top_stacks(&profile)
    );
    let _ = std::fs::remove_file(&script_path);
}

/// Sustained CPU work. V8 flags on the `node` command, plus
/// `%NeverOptimizeFunction`, keep TurboFan and Maglev from inlining
/// `anyprofSampleWork` into its caller (Node 22 on macOS does that for the
/// short timer-tick version, so the name never appears in the profile).
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

fn top_stacks(profile: &anyprof_core::Profile) -> String {
    let mut stacks: Vec<_> = profile.stacks.iter().collect();
    stacks.sort_by_key(|stack| std::cmp::Reverse(stack.weight));
    stacks
        .into_iter()
        .take(8)
        .map(|stack| {
            let frames: Vec<_> = stack
                .frames
                .iter()
                .map(|frame| match frame.file.as_deref() {
                    Some(file) => format!("{} ({file})", frame.name),
                    None => frame.name.clone(),
                })
                .collect();
            format!("  {} x {}", stack.weight, frames.join(" <- "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}
