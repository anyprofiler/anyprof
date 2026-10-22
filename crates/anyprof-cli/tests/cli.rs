use std::process::Command;

fn anyprof(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(args)
        .output()
        .expect("run anyprof")
}

#[test]
fn help_lists_record_and_convert() {
    let output = anyprof(&["--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Capture CPU profiles from any running process"));
    assert!(stdout.contains("record"));
    assert!(
        stdout.contains("aliases: rec") || stdout.contains("[alias: rec]"),
        "record alias missing from help:\n{stdout}"
    );
    assert!(stdout.contains("convert"));
    assert!(
        stdout.contains("aliases: conv") || stdout.contains("[alias: conv]"),
        "convert alias missing from help:\n{stdout}"
    );
}

#[test]
fn record_help_shows_pid_duration_and_format() {
    let output = anyprof(&["record", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("-p, --pid"));
    assert!(stdout.contains("-d, --duration"));
    assert!(stdout.contains("-f, --format"));
    assert!(stdout.contains("-F, --frequency"));
    assert!(stdout.contains("-B, --browser"));
    assert!(stdout.contains("-b, --backend"));
    assert!(stdout.contains("-i, --inspect"));
    assert!(stdout.contains("-t, --tab"));
    assert!(stdout.contains("-k, --kernel"));
    assert!(stdout.contains("Ctrl+C"));
    assert!(!stdout.contains("default: 30s"));
}

#[test]
fn convert_help_shows_from_and_to() {
    let output = anyprof(&["convert", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("-f, --from"));
    assert!(stdout.contains("-t, --to"));
    assert!(stdout.contains("-i, --input"));
    assert!(stdout.contains("collapsed"));
    assert!(stdout.contains("speedscope"));
    assert!(stdout.contains("pprof"));
    assert!(stdout.contains("cpuprofile"));
}

#[test]
fn record_dispatches_through_detection_and_reports_the_stub() {
    let output_path =
        std::env::temp_dir().join(format!("anyprof-record-{}.collapsed", std::process::id()));
    let _ = std::fs::remove_file(&output_path);
    let pid = std::process::id().to_string();
    let output = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .args(["record", "--pid", pid.as_str(), "--duration", "1s", "-o"])
        .arg(&output_path)
        .output()
        .expect("run anyprof record");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("using backend"),
        "auto-detection should name the backend: {stderr}"
    );
    assert!(
        stderr.contains("not implemented yet"),
        "stubs should fail clearly: {stderr}"
    );
    assert!(!output_path.exists(), "a stub must not write a profile");
}

#[test]
fn convert_collapsed_to_speedscope() {
    let dir = std::env::temp_dir();
    let input = dir.join(format!("anyprof-in-{}.collapsed", std::process::id()));
    let output = dir.join(format!("anyprof-out-{}.json", std::process::id()));
    std::fs::write(&input, "main;worker 5\n").unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_anyprof"))
        .arg("convert")
        .arg("-i")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--to", "speedscope"])
        .output()
        .expect("run anyprof convert");
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(result.status.success(), "{stderr}");
    let body = std::fs::read_to_string(&output).unwrap();
    assert!(body.contains("worker"), "{body}");
    assert!(
        body.contains("www.speedscope.app/file-format-schema.json"),
        "{body}"
    );
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
}
