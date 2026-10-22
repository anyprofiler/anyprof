//! Profiles a real Node.js process. Skips when `node` is not on `PATH`.

use std::io::Read;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use anyprof_core::{Backend, RecordOptions, Runtime, Target};
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

    let script = r#"
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
    "#;
    let mut child = Command::new("node")
        .args([&format!("--inspect-port=127.0.0.1:{port}"), "-e", script])
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
            duration: Duration::from_secs(2),
            frequency_hz: 1000,
            include_kernel: false,
            inspect: None,
            tab: None,
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
    assert!(
        profile.stacks.iter().any(|stack| {
            stack.frames.iter().any(|frame| {
                frame.runtime == Runtime::Node && frame.name.contains("anyprofSampleWork")
            })
        }),
        "expected anyprofSampleWork in {:?}\nnode stderr:\n{stderr}",
        profile
            .stacks
            .iter()
            .map(|stack| {
                stack
                    .frames
                    .iter()
                    .map(|frame| frame.name.as_str())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>()
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
    assert!(
        raw.windows(b"anyprofSampleWork".len())
            .any(|window| window == b"anyprofSampleWork")
    );
}
