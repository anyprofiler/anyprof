//! Profiles a real Node.js process. Skips when `node` is not on `PATH`.

use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyprof_core::{Backend, RecordOptions, Target};
use anyprof_node::NodeBackend;

static NODE_LIVE_LOCK: Mutex<()> = Mutex::new(());

struct Port9229Lock {
    path: std::path::PathBuf,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Port9229Lock {
    fn acquire() -> Self {
        let guard = NODE_LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let path = std::env::temp_dir().join("anyprof-port-9229.lock");
        let start = Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => {
                    return Self {
                        path,
                        _guard: guard,
                    };
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if start.elapsed() > Duration::from_secs(120) {
                        panic!("timed out waiting for {path:?}");
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(err) => panic!("lock {path:?}: {err}"),
            }
        }
    }
}

impl Drop for Port9229Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn node_available() -> bool {
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => true,
        _ => {
            eprintln!("skipping live node profile: node is not on PATH");
            false
        }
    }
}

fn port_open(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok()
}

fn wait_port_closed(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !port_open(port) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn profiles_a_live_node_process() {
    let _guard = NODE_LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !node_available() {
        return;
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
    assert!(
        !port_open(port),
        "inspector port {port} should not be open before activation"
    );

    let started = NodeBackend.start(
        &Target { pid },
        &RecordOptions {
            duration: Some(Duration::from_secs(2)),
            sampling_interval: Some(Duration::from_millis(1)),
            ..RecordOptions::default()
        },
    );

    let closed_after_sample = wait_port_closed(port, Duration::from_secs(2));
    let still_alive = child.try_wait().ok().flatten().is_none();
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
        still_alive,
        "node exited during sampling\nnode stderr:\n{stderr}"
    );
    assert!(
        closed_after_sample,
        "activated inspector port {port} remained open after sampling\nnode stderr:\n{stderr}"
    );
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

#[test]
fn uninspected_node_closes_port_9229_and_keep_inspector_leaves_it_open() {
    let _guard = Port9229Lock::acquire();
    if !node_available() {
        return;
    }

    // Use the default 9229 port when free on this machine, otherwise fall back
    // to an ephemeral `--inspect-port` so an unrelated local process on 9229
    // cannot collide with the test.
    let port = match TcpListener::bind("127.0.0.1:9229") {
        Ok(listener) => {
            drop(listener);
            9229
        }
        Err(_) => {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind free port");
            let p = listener.local_addr().unwrap().port();
            drop(listener);
            p
        }
    };

    let script_path = std::env::temp_dir().join(format!(
        "anyprof-uninspected-{}-{port}.js",
        std::process::id()
    ));
    std::fs::write(&script_path, BUSY_JS).expect("write fixture");
    let mut cmd = Command::new("node");
    cmd.arg("--allow-natives-syntax");
    if port != 9229 {
        cmd.arg(format!("--inspect-port=127.0.0.1:{port}"));
    }
    cmd.arg(script_path.to_str().expect("utf-8"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn uninspected node");
    let pid = child.id();
    thread::sleep(Duration::from_millis(500));
    assert!(
        !port_open(port),
        "uninspected node must not listen on 127.0.0.1:{port} before sampling"
    );

    // Default behavior (`keep_inspector: false`) closes 127.0.0.1:9229 after sampling.
    let session = NodeBackend
        .start(
            &Target { pid },
            &RecordOptions {
                duration: Some(Duration::from_millis(300)),
                sampling_interval: Some(Duration::from_millis(1)),
                ..RecordOptions::default()
            },
        )
        .expect("sample uninspected node");
    let profile = session.stop().expect("stop session");
    assert!(profile.total_weight() > 0);
    assert!(
        wait_port_closed(port, Duration::from_secs(2)),
        "127.0.0.1:{port} should be closed after sampling an uninspected node process"
    );
    assert!(
        child.try_wait().ok().flatten().is_none(),
        "closing the activated inspector must not terminate the node process"
    );

    // `keep_inspector: true` (`--keep-inspector`) leaves the activated port listening.
    let session = NodeBackend
        .start(
            &Target { pid },
            &RecordOptions {
                duration: Some(Duration::from_millis(300)),
                sampling_interval: Some(Duration::from_millis(1)),
                keep_inspector: true,
                ..RecordOptions::default()
            },
        )
        .expect("re-sample node with keep_inspector");
    let _ = session.stop().expect("stop second session");
    assert!(
        port_open(port),
        "127.0.0.1:{port} should stay open when keep_inspector is true"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&script_path);
}

#[test]
fn preexisting_inspect_zero_port_stays_open_after_sampling() {
    let _guard = NODE_LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !node_available() {
        return;
    }

    let script_path =
        std::env::temp_dir().join(format!("anyprof-preopened-{}.js", std::process::id()));
    std::fs::write(&script_path, BUSY_JS).expect("write fixture");
    let mut child = Command::new("node")
        .args([
            "--allow-natives-syntax",
            "--inspect=127.0.0.1:0",
            script_path.to_str().expect("utf-8"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn node --inspect=0");
    let pid = child.id();
    let stderr = child.stderr.take().expect("stderr");
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = thread::spawn(move || {
        let mut buf = BufReader::new(stderr);
        let mut line = String::new();
        while buf.read_line(&mut line).unwrap_or(0) > 0 {
            if let Some(ws_pos) = line.find("ws://127.0.0.1:") {
                let after = &line[ws_pos + "ws://127.0.0.1:".len()..];
                let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(port) = digits.parse::<u16>() {
                    let _ = tx.send(port);
                }
            }
            line.clear();
        }
    });
    let port = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("read Debugger listening port from node --inspect=0 stderr");
    assert!(
        port_open(port),
        "pre-existing --inspect=0 port {port} should be listening"
    );

    let session = NodeBackend
        .start(
            &Target { pid },
            &RecordOptions {
                duration: Some(Duration::from_millis(400)),
                sampling_interval: Some(Duration::from_millis(1)),
                ..RecordOptions::default()
            },
        )
        .expect("sample node started with --inspect=0");
    let profile = session.stop().expect("stop session");
    assert!(profile.total_weight() > 0);
    thread::sleep(Duration::from_millis(150));
    assert!(
        child.try_wait().ok().flatten().is_none(),
        "node --inspect=0 process should remain running"
    );
    assert!(
        port_open(port),
        "pre-existing --inspect=0 port {port} must remain open after sampling"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
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
