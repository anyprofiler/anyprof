//! Apple Instruments (`xcrun xctrace`) `Time Profiler` sampler and XML parser.
//!
//! On macOS, `xcrun xctrace record --template 'Time Profiler'` samples on-CPU
//! call stacks through Apple's system-signed Instruments daemon (`ktrace` PMI)
//! without requiring `sudo` or custom entitlements on `anyprof`. When
//! `task_for_pid` is refused for an unprivileged user (for example before
//! `anyprof setup` has signed the binary, or against a target whose runtime
//! blocks third-party `task_for_pid` while permitting Instruments), `macos.rs`
//! falls back to this module automatically for native, Rust, and Go targets.
//! Users can also select `-b xctrace` explicitly.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::HashMap;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::time::Instant;

#[cfg(target_os = "macos")]
use anyprof_core::Profile;
use anyprof_core::{
    Backend, Capabilities, Error, Format, Frame, LanguageUnwind, MAX_TIMED_SAMPLES, RecordOptions,
    Runtime, Session, Stack, Target, TimedSample, process_exists,
};

use crate::macho::{ImageSymbols, MachCpu};
use crate::policy;
use crate::symbol::{frame_runtime, present_macho_symbol};

const MAX_XML_BYTES: usize = 256 * 1024 * 1024;
const MAX_XML_DEPTH: usize = 64;
const MAX_XCTRACE_FRAMES: usize = 128;

/// Explicit Apple Instruments (`xcrun xctrace`) `Time Profiler` backend.
#[derive(Debug, Default)]
pub struct XctraceBackend;

impl Backend for XctraceBackend {
    fn name(&self) -> &'static str {
        "xctrace"
    }

    fn detect(&self, _pid: u32) -> bool {
        // Explicit backend (`-b xctrace`); automatic non-root fallback happens
        // inside `macos::record` when `task_for_pid` is denied.
        false
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            on_cpu: true,
            off_cpu: false,
            mixed_stacks: true,
            kernel_frames: false,
            needs_root: false,
        }
    }

    fn default_format(&self) -> Format {
        Format::Pprof
    }

    fn default_sampling_interval(&self) -> Duration {
        Duration::from_millis(1)
    }

    fn start(&self, target: &Target, opts: &RecordOptions) -> Result<Box<dyn Session>, Error> {
        if opts.duration.is_some_and(|duration| duration.is_zero()) {
            return Err(Error::backend(
                self.name(),
                "profile duration must be greater than zero",
            ));
        }
        if !process_exists(target.pid) {
            return Err(Error::backend(
                self.name(),
                format!("pid {} is not running", target.pid),
            ));
        }
        #[cfg(target_os = "macos")]
        {
            let mut xc_opts = opts.clone();
            if xc_opts.language_unwind == LanguageUnwind::None {
                xc_opts.language_unwind = LanguageUnwind::Auto;
            }
            crate::spawn_xctrace_session(*target, xc_opts)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = opts;
            Err(Error::not_implemented(
                self.name(),
                "Apple Instruments xctrace sampling is only available on macOS",
            ))
        }
    }
}

/// True when Xcode's `xctrace` utility is installed and discoverable via `xcrun`.
#[cfg(target_os = "macos")]
pub(crate) fn is_available() -> bool {
    if std::env::var_os("ANYPROF_DISABLE_XCTRACE").is_some() {
        return false;
    }
    resolve_xctrace_bin().is_some()
}

#[cfg(target_os = "macos")]
fn resolve_xctrace_bin() -> Option<PathBuf> {
    let output = std::process::Command::new("xcrun")
        .args(["--find", "xctrace"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    path.is_file().then_some(path)
}

/// Fallback entry point from `macos::record` when `task_for_pid` failed as non-root.
///
/// Preserves the original `task_for_pid` error diagnostic if `xctrace` also
/// fails (for example against `launchd` pid 1 or in an unauthorized SSH session).
#[cfg(target_os = "macos")]
pub(crate) fn record_fallback(
    target: &Target,
    opts: &RecordOptions,
    session_stop: &anyprof_core::StopGate,
    ready_tx: &std::sync::mpsc::SyncSender<Result<(), Error>>,
    attach_err: Error,
) -> Result<Profile, Error> {
    let attach_msg = attach_err.to_string();
    let res = record_impl(target, opts, session_stop, "native", |ready| match ready {
        Ok(()) => {
            let _ = ready_tx.send(Ok(()));
        }
        Err(xc_err) => {
            let _ = ready_tx.send(Err(Error::backend(
                "native",
                format!("{attach_msg} (Instruments xctrace fallback also failed: {xc_err})"),
            )));
        }
    });
    match res {
        Ok(profile) => Ok(profile),
        Err(xc_err) => Err(Error::backend(
            "native",
            format!("{attach_msg} (Instruments xctrace fallback also failed: {xc_err})"),
        )),
    }
}

/// Direct `xctrace` session entry point for [`XctraceBackend`].
#[cfg(target_os = "macos")]
pub(crate) fn record_direct(
    target: &Target,
    opts: &RecordOptions,
    session_stop: &anyprof_core::StopGate,
    ready_tx: &std::sync::mpsc::SyncSender<Result<(), Error>>,
) -> Result<Profile, Error> {
    record_impl(target, opts, session_stop, "xctrace", |ready| {
        let _ = ready_tx.send(ready);
    })
}

#[cfg(target_os = "macos")]
fn record_impl(
    target: &Target,
    opts: &RecordOptions,
    session_stop: &anyprof_core::StopGate,
    backend_name: &'static str,
    mut signal_ready: impl FnMut(Result<(), Error>),
) -> Result<Profile, Error> {
    use std::os::unix::process::CommandExt as _;

    if let Err(message) = policy::reject_self_sample(target.pid, std::process::id()) {
        let err = Error::backend(backend_name, message);
        signal_ready(Err(Error::backend(backend_name, message)));
        return Err(err);
    }
    let Some(xctrace_bin) = resolve_xctrace_bin() else {
        let msg =
            "xcrun xctrace was not found (install Xcode / Instruments or run 'anyprof setup')";
        signal_ready(Err(Error::backend(backend_name, msg)));
        return Err(Error::backend(backend_name, msg));
    };

    let temp_dir = match TempTraceDir::new(target.pid) {
        Ok(dir) => dir,
        Err(io_err) => {
            let msg = format!("creating temp trace dir: {io_err}");
            signal_ready(Err(Error::backend(backend_name, msg.clone())));
            return Err(Error::backend(backend_name, msg));
        }
    };
    let trace_path = temp_dir.path.join("capture.trace");

    let mut cmd = std::process::Command::new(&xctrace_bin);
    cmd.args([
        "record",
        "--quiet",
        "--no-prompt",
        "--template",
        "Time Profiler",
        "--attach",
        &target.pid.to_string(),
        "--output",
    ])
    .arg(&trace_path)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .process_group(0);

    if let Some(limit) = opts.duration {
        let ms = limit.as_millis().max(100);
        cmd.args(["--time-limit", &format!("{ms}ms")]);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(io_err) => {
            let msg = format!("spawning xctrace record: {io_err}");
            signal_ready(Err(Error::backend(backend_name, msg.clone())));
            return Err(Error::backend(backend_name, msg));
        }
    };

    // Watch briefly for immediate attach/authorization failures before signaling ready.
    let probe_start = Instant::now();
    while probe_start.elapsed() < Duration::from_millis(350) {
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() || !trace_path.exists() {
                let stderr = read_pipe(&mut child.stderr);
                let stdout = read_pipe(&mut child.stdout);
                let detail = first_nonempty(&stderr, &stdout)
                    .unwrap_or_else(|| format!("xctrace record exited with {status}"));
                signal_ready(Err(Error::backend(backend_name, detail.clone())));
                return Err(Error::backend(backend_name, detail));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    signal_ready(Ok(()));
    let started = Instant::now();
    let mut sent_sigint = false;

    loop {
        if let Ok(Some(status)) = child.try_wait() {
            if !status.success() && !trace_path.exists() {
                let stderr = read_pipe(&mut child.stderr);
                let stdout = read_pipe(&mut child.stdout);
                let detail = first_nonempty(&stderr, &stdout)
                    .unwrap_or_else(|| format!("xctrace record failed with {status}"));
                return Err(Error::backend(backend_name, detail));
            }
            break;
        }

        let stop_now = opts.stop.stop_requested() || session_stop.stop_requested();
        let early_interrupt = match opts.duration {
            // When `--time-limit` was given to `xctrace`, let `xctrace` finish its
            // own recording window (including first-boot Instruments XPC daemon
            // cold start, which can take ~15-25s on fresh VMs) unless `opts.stop`
            // fired early (Ctrl+C / child exit) or `xctrace` exceeded a 45s grace window.
            Some(limit) => {
                (opts.stop.stop_requested()
                    && started.elapsed().saturating_add(Duration::from_millis(80)) < limit)
                    || started.elapsed() > limit.saturating_add(Duration::from_secs(45))
            }
            None => stop_now,
        };

        if early_interrupt && !sent_sigint {
            const SIGINT: i32 = 2;
            unsafe extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            if let Ok(pid_i32) = i32::try_from(child.id()) {
                unsafe {
                    let _ = kill(pid_i32, SIGINT);
                }
            }
            sent_sigint = true;
        }
        if sent_sigint && started.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::backend(
                backend_name,
                "xctrace record did not finish writing trace after SIGINT",
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    let elapsed = opts
        .duration
        .map(|d| d.min(started.elapsed()))
        .unwrap_or_else(|| started.elapsed());

    if !trace_path.exists() {
        let stderr = read_pipe(&mut child.stderr);
        let stdout = read_pipe(&mut child.stdout);
        let detail = first_nonempty(&stderr, &stdout)
            .unwrap_or_else(|| "xctrace record produced no .trace bundle".to_string());
        return Err(Error::backend(backend_name, detail));
    }

    let xml = export_time_profile_xml(&xctrace_bin, &trace_path)
        .map_err(|err| Error::backend(backend_name, err))?;
    let (stacks, samples) =
        parse_time_profile_xml(&xml, target.pid, opts.language_unwind, MachCpu::host())
            .map_err(|err| Error::backend(backend_name, err))?;

    let comm = anyprof_core::inspect(target.pid).and_then(|s| s.comm);
    Ok(Profile {
        pid: target.pid,
        comm,
        duration: elapsed,
        period: Some(opts.sampling_interval.unwrap_or(Duration::from_millis(1))),
        stacks,
        samples,
        raw_cpuprofile: None,
    })
}

#[cfg(target_os = "macos")]
fn export_time_profile_xml(xctrace_bin: &Path, trace_path: &Path) -> Result<String, String> {
    const XPATH: &str = "/trace-toc/run[@number=\"1\"]/data/table[@schema=\"time-profile\"]";
    let mut last_err = String::new();
    for attempt in 0..3 {
        let output = std::process::Command::new(xctrace_bin)
            .arg("export")
            .arg("--input")
            .arg(trace_path)
            .args(["--xpath", XPATH])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|err| format!("invoking xctrace export: {err}"))?;
        if output.status.success() {
            if output.stdout.len() > MAX_XML_BYTES {
                return Err(format!(
                    "xctrace export output exceeds {} MiB cap",
                    MAX_XML_BYTES / (1024 * 1024)
                ));
            }
            return String::from_utf8(output.stdout)
                .map_err(|err| format!("xctrace export emitted invalid UTF-8: {err}"));
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        // If the target was completely idle for the entire window, Time Profiler
        // can omit the `time-profile` table or emit an empty run.
        if stderr.contains("matches no exportable entity")
            || stderr.contains("has no content to export")
        {
            return Ok("<trace-query-result/>".to_string());
        }
        last_err = if stderr.is_empty() {
            format!("xctrace export exited with {}", output.status)
        } else {
            stderr
        };
        // `xctrace export` is known to occasionally exit 139 (SIGSEGV) transiently; retry twice.
        if attempt < 2 {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    Err(format!("xctrace export failed: {last_err}"))
}

#[cfg(target_os = "macos")]
fn read_pipe<R: std::io::Read>(pipe: &mut Option<R>) -> String {
    let Some(mut reader) = pipe.take() else {
        return String::new();
    };
    let mut buf = String::new();
    let _ = reader.read_to_string(&mut buf);
    buf.trim().to_string()
}

#[cfg(target_os = "macos")]
fn first_nonempty(a: &str, b: &str) -> Option<String> {
    if !a.is_empty() {
        Some(a.to_string())
    } else if !b.is_empty() {
        Some(b.to_string())
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
struct TempTraceDir {
    path: PathBuf,
}

#[cfg(target_os = "macos")]
impl TempTraceDir {
    fn new(pid: u32) -> std::io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "anyprof-xctrace-{}-{pid}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

#[cfg(target_os = "macos")]
impl Drop for TempTraceDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct RawXcFrame {
    name: String,
    addr: Option<u64>,
    binary_name: Option<String>,
    binary_path: Option<String>,
    load_addr: Option<u64>,
}

struct RawSample {
    time_ns: u64,
    tid: u64,
    thread_name: Option<String>,
    stack_idx: u32,
}

/// Parse `xctrace export` XML for `schema="time-profile"` into [`Stack`]s and [`TimedSample`]s.
pub(crate) fn parse_time_profile_xml(
    xml: &str,
    target_pid: u32,
    language_unwind: LanguageUnwind,
    cpu: MachCpu,
) -> Result<(Vec<Stack>, Vec<TimedSample>), String> {
    if xml.len() > MAX_XML_BYTES {
        return Err("xctrace XML exceeds 256 MiB limit".to_string());
    }
    let arena = parse_xml_arena(xml)?;
    let mut rows = Vec::new();
    for (idx, node) in arena.nodes.iter().enumerate() {
        if node.tag == "row" {
            rows.push(idx);
        }
    }
    if rows.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut unique_stacks: Vec<(Vec<RawXcFrame>, u64)> = Vec::new();
    let mut stack_index_of: HashMap<Vec<RawXcFrame>, u32> = HashMap::new();
    let mut raw_samples: Vec<RawSample> = Vec::new();
    let mut drop_timed = false;

    for row_idx in rows {
        let row = &arena.nodes[row_idx];
        if target_pid != 0
            && let Some(proc_idx) = arena.child_resolved(row, "process")
            && let Some(pid_idx) = arena.child_resolved(&arena.nodes[proc_idx], "pid")
            && let Ok(row_pid) = arena.nodes[pid_idx].text.trim().parse::<u32>()
            && row_pid != 0
            && row_pid != target_pid
        {
            continue;
        }

        // Skip explicitly parked/waiting rows if a custom template recorded them.
        if let Some(state_idx) = arena.child_resolved(row, "thread-state") {
            let fmt = arena.nodes[state_idx].attr("fmt").unwrap_or("");
            if !fmt.is_empty() && fmt != "Running" && !fmt.starts_with("Running ") {
                continue;
            }
        }

        let Some(bt_idx) = arena
            .child_resolved(row, "backtrace")
            .or_else(|| arena.child_resolved(row, "stack"))
        else {
            continue;
        };
        let frames = extract_backtrace_frames(&arena, bt_idx);
        if frames.is_empty() {
            continue;
        }

        let time_ns = arena
            .child_resolved(row, "sample-time")
            .and_then(|i| arena.nodes[i].text.trim().parse::<u64>().ok())
            .unwrap_or(0);

        let (tid, thread_name) = extract_thread(&arena, row, target_pid);

        let stack_idx = if let Some(&idx) = stack_index_of.get(&frames) {
            unique_stacks[idx as usize].1 = unique_stacks[idx as usize].1.saturating_add(1);
            idx
        } else {
            let idx = u32::try_from(unique_stacks.len())
                .map_err(|_| "too many distinct stacks in xctrace output".to_string())?;
            stack_index_of.insert(frames.clone(), idx);
            unique_stacks.push((frames, 1));
            idx
        };

        if !drop_timed {
            if raw_samples.len() >= MAX_TIMED_SAMPLES {
                raw_samples.clear();
                drop_timed = true;
            } else {
                raw_samples.push(RawSample {
                    time_ns,
                    tid,
                    thread_name,
                    stack_idx,
                });
            }
        }
    }

    let mut image_cache: HashMap<(String, u64), Option<ImageSymbols>> = HashMap::new();
    let stacks: Vec<Stack> = unique_stacks
        .into_iter()
        .map(|(raw_frames, weight)| {
            let frames = raw_frames
                .into_iter()
                .enumerate()
                .map(|(idx, raw)| {
                    resolve_xc_frame(raw, idx == 0, language_unwind, cpu, &mut image_cache)
                })
                .collect();
            Stack { frames, weight }
        })
        .collect();

    raw_samples.sort_by_key(|s| s.time_ns);
    let base_ns = raw_samples.first().map(|s| s.time_ns).unwrap_or(0);
    let samples = raw_samples
        .into_iter()
        .map(|s| TimedSample {
            stack_index: s.stack_idx as usize,
            weight: 1,
            tid: s.tid,
            thread_name: s.thread_name,
            offset_ns: s.time_ns.saturating_sub(base_ns),
        })
        .collect();

    Ok((stacks, samples))
}

fn resolve_xc_frame(
    raw: RawXcFrame,
    leaf: bool,
    language_unwind: LanguageUnwind,
    cpu: MachCpu,
    image_cache: &mut HashMap<(String, u64), Option<ImageSymbols>>,
) -> Frame {
    let module_fallback = raw.binary_name.clone().or_else(|| {
        raw.binary_path
            .as_deref()
            .and_then(|p| Path::new(p).file_name())
            .map(|n| n.to_string_lossy().into_owned())
    });

    if let (Some(addr), Some(path), Some(load_addr)) =
        (raw.addr, raw.binary_path.as_deref(), raw.load_addr)
    {
        let key = (path.to_string(), load_addr);
        let entry = image_cache.entry(key).or_insert_with(|| {
            let mod_name = module_fallback.clone().unwrap_or_else(|| path.to_string());
            ImageSymbols::parse_file(Path::new(path), load_addr, &mod_name, cpu)
        });

        if let Some(image) = entry.as_ref() {
            let lookup = policy::symbol_lookup_address(addr, leaf);
            if language_unwind.allows_go()
                && let Some(go) = &image.go
                && let Some(sym) = go.symbol(lookup).or_else(|| go.symbol(addr))
            {
                let mut frame = Frame::new(Runtime::Go, sym.name);
                frame.file = sym.file;
                frame.line = sym.line;
                frame.address = Some(addr);
                frame.module = Some(image.module.clone());
                return frame;
            }

            let file_loc = image
                .location_at(lookup)
                .or_else(|| image.location_at(addr))
                .map(|(f, l)| (f.to_string(), l));

            if let Some((sym_name, sym_rt)) = image.name_at(lookup).or_else(|| image.name_at(addr))
            {
                let rt = frame_runtime(sym_rt, sym_name, false, image.rust_image);
                let mut frame = Frame::new(rt, sym_name);
                frame.address = Some(addr);
                frame.module = Some(image.module.clone());
                if let Some((file, line)) = file_loc {
                    frame.file = Some(file);
                    frame.line = Some(line);
                }
                return frame;
            }

            let presented = present_macho_symbol(&raw.name);
            let rt = frame_runtime(
                presented.runtime,
                &presented.name,
                presented.from_cpp,
                image.rust_image,
            );
            let mut frame = Frame::new(rt, presented.name);
            frame.address = Some(addr);
            frame.module = Some(image.module.clone());
            if let Some((file, line)) = file_loc {
                frame.file = Some(file);
                frame.line = Some(line);
            }
            return frame;
        }
    }

    let presented = present_macho_symbol(&raw.name);
    let mut frame = Frame::new(presented.runtime, presented.name);
    frame.address = raw.addr;
    frame.module = module_fallback;
    frame
}

fn extract_thread(arena: &XmlArena, row: &XmlNode, target_pid: u32) -> (u64, Option<String>) {
    let Some(thread_idx) = arena.child_resolved(row, "thread") else {
        return (u64::from(target_pid), None);
    };
    let thread_node = &arena.nodes[thread_idx];
    let tid = arena
        .child_resolved(thread_node, "tid")
        .and_then(|i| parse_u64_num(&arena.nodes[i].text))
        .unwrap_or_else(|| u64::from(target_pid));
    let name = thread_node.attr("fmt").and_then(clean_thread_fmt);
    (tid, name)
}

fn clean_thread_fmt(fmt: &str) -> Option<String> {
    let trimmed = fmt.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Instruments formats thread labels as `"main 0x627b (app, pid: 123)"` or `"0x627b"`.
    let before_hex = match trimmed.find(" 0x") {
        Some(pos) => trimmed[..pos].trim(),
        None if trimmed.starts_with("0x") => "",
        None => trimmed,
    };
    if before_hex.is_empty() {
        None
    } else {
        Some(before_hex.to_string())
    }
}

fn extract_backtrace_frames(arena: &XmlArena, bt_idx: usize) -> Vec<RawXcFrame> {
    let bt = &arena.nodes[bt_idx];
    let mut out = Vec::new();
    for &child_idx in &bt.children {
        if out.len() >= MAX_XCTRACE_FRAMES {
            break;
        }
        let child = &arena.nodes[child_idx];
        if child.tag == "frame" {
            let resolved_idx = arena.resolve(child_idx);
            let frame_node = &arena.nodes[resolved_idx];
            let addr = frame_node.attr("addr").and_then(parse_u64_num);
            // Exclude XNU kernel addresses (`0xffff8000...`+) when Time Profiler
            // reports user-space traces so kernel addresses are not mislabeled Native.
            if addr.is_some_and(|a| a >= 0xffff_8000_0000_0000) {
                continue;
            }
            let raw_name = frame_node.attr("name").unwrap_or("").trim();
            let name = if raw_name == "<unknown>" {
                String::new()
            } else {
                raw_name.to_string()
            };
            let (binary_name, binary_path, load_addr) =
                if let Some(bin_idx) = arena.child_resolved(frame_node, "binary") {
                    let bin = &arena.nodes[bin_idx];
                    (
                        bin.attr("name").map(str::to_string),
                        bin.attr("path").map(str::to_string),
                        bin.attr("load-addr").and_then(parse_u64_num),
                    )
                } else {
                    (None, None, None)
                };
            if !name.is_empty() || addr.is_some() {
                out.push(RawXcFrame {
                    name,
                    addr,
                    binary_name,
                    binary_path,
                    load_addr,
                });
            }
        } else if child.tag == "text-addresses" {
            let resolved_idx = arena.resolve(child_idx);
            for tok in arena.nodes[resolved_idx].text.split_whitespace() {
                if out.len() >= MAX_XCTRACE_FRAMES {
                    break;
                }
                if let Some(addr) = parse_u64_num(tok)
                    && addr < 0xffff_8000_0000_0000
                {
                    out.push(RawXcFrame {
                        name: String::new(),
                        addr: Some(addr),
                        binary_name: None,
                        binary_path: None,
                        load_addr: None,
                    });
                }
            }
        }
    }
    out
}

fn parse_u64_num(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u64::from_str_radix(hex, 16).ok()
    } else {
        trimmed.parse::<u64>().ok()
    }
}

struct XmlNode {
    tag: String,
    attrs: Vec<(String, String)>,
    text: String,
    children: Vec<usize>,
}

impl XmlNode {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

struct XmlArena {
    nodes: Vec<XmlNode>,
    by_id: HashMap<(String, String), usize>,
}

impl XmlArena {
    fn resolve(&self, idx: usize) -> usize {
        let node = &self.nodes[idx];
        if node.attr("id").is_some() {
            return idx;
        }
        if let Some(r) = node.attr("ref")
            && let Some(&orig) = self.by_id.get(&(node.tag.clone(), r.to_string()))
        {
            return orig;
        }
        idx
    }

    fn child_resolved(&self, parent: &XmlNode, tag: &str) -> Option<usize> {
        parent
            .children
            .iter()
            .copied()
            .find(|&idx| self.nodes[idx].tag == tag)
            .map(|idx| self.resolve(idx))
    }
}

fn parse_xml_arena(xml: &str) -> Result<XmlArena, String> {
    let bytes = xml.as_bytes();
    let mut pos = 0usize;
    let mut nodes = vec![XmlNode {
        tag: "__root__".to_string(),
        attrs: Vec::new(),
        text: String::new(),
        children: Vec::new(),
    }];
    let mut by_id: HashMap<(String, String), usize> = HashMap::new();
    let mut stack: Vec<usize> = vec![0];

    while pos < bytes.len() {
        if let Some(rel) = xml[pos..].find('<') {
            if rel > 0 {
                let text = unescape_xml(&xml[pos..pos + rel]);
                if !text.trim().is_empty()
                    && let Some(&top) = stack.last()
                {
                    nodes[top].text.push_str(&text);
                }
                pos += rel;
            }
        } else {
            break;
        }

        let rest = &xml[pos..];
        if rest.starts_with("<!--") {
            let Some(end) = rest.find("-->") else {
                return Err("unterminated XML comment in xctrace output".to_string());
            };
            pos += end + 3;
            continue;
        }
        if rest.starts_with("<?") {
            let Some(end) = rest.find("?>") else {
                return Err("unterminated XML processing instruction".to_string());
            };
            pos += end + 2;
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            let Some(end) = rest.find("]]>") else {
                return Err("unterminated CDATA section".to_string());
            };
            if let Some(&top) = stack.last() {
                nodes[top].text.push_str(&rest[9..end]);
            }
            pos += end + 3;
            continue;
        }
        if rest.starts_with("<!") {
            let Some(end) = rest.find('>') else {
                return Err("unterminated XML declaration".to_string());
            };
            pos += end + 1;
            continue;
        }
        if rest.starts_with("</") {
            let Some(end) = rest.find('>') else {
                return Err("unterminated closing XML tag".to_string());
            };
            let close_tag = rest[2..end].trim();
            if stack.len() <= 1 {
                return Err(format!("unexpected closing tag </{close_tag}>"));
            }
            let popped = stack.pop().unwrap();
            if nodes[popped].tag != close_tag {
                return Err(format!(
                    "mismatched XML closing tag: expected </{}>, got </{close_tag}>",
                    nodes[popped].tag
                ));
            }
            pos += end + 1;
            continue;
        }

        let parsed = parse_open_tag(rest)?;
        pos += parsed.consumed;
        if stack.len() >= MAX_XML_DEPTH {
            return Err("xctrace XML exceeded maximum nesting depth".to_string());
        }
        let idx = nodes.len();
        if let Some((_, id_val)) = parsed.attrs.iter().find(|(k, _)| k == "id") {
            by_id
                .entry((parsed.tag.clone(), id_val.clone()))
                .or_insert(idx);
        }
        nodes.push(XmlNode {
            tag: parsed.tag,
            attrs: parsed.attrs,
            text: String::new(),
            children: Vec::new(),
        });
        if let Some(&parent) = stack.last() {
            nodes[parent].children.push(idx);
        }
        if !parsed.self_closing {
            stack.push(idx);
        }
    }

    Ok(XmlArena { nodes, by_id })
}

struct OpenTag {
    tag: String,
    attrs: Vec<(String, String)>,
    self_closing: bool,
    consumed: usize,
}

fn parse_open_tag(s: &str) -> Result<OpenTag, String> {
    let bytes = s.as_bytes();
    let mut i = 1usize; // skip '<'
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    let name_start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' && bytes[i] != b'/'
    {
        i += 1;
    }
    if i == name_start {
        return Err("empty XML element name".to_string());
    }
    let tag = s[name_start..i].to_string();
    let mut attrs = Vec::new();

    loop {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return Err(format!("unterminated opening tag <{tag}"));
        }
        if bytes[i] == b'>' {
            return Ok(OpenTag {
                tag,
                attrs,
                self_closing: false,
                consumed: i + 1,
            });
        }
        if bytes[i] == b'/' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'>' {
                return Ok(OpenTag {
                    tag,
                    attrs,
                    self_closing: true,
                    consumed: i + 2,
                });
            }
            return Err(format!("malformed self-closing tag <{tag}"));
        }

        let key_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'>'
            && bytes[i] != b'/'
        {
            i += 1;
        }
        let key = s[key_start..i].to_string();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            return Err(format!("expected '=' after attribute '{key}' in <{tag}>"));
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || (bytes[i] != b'"' && bytes[i] != b'\'') {
            return Err(format!("expected quoted attribute value for '{key}'"));
        }
        let quote = bytes[i];
        i += 1;
        let val_start = i;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        if i >= bytes.len() {
            return Err(format!("unterminated attribute value for '{key}'"));
        }
        let val = unescape_xml(&s[val_start..i]);
        i += 1;
        attrs.push((key, val));
    }
}

fn unescape_xml(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        if let Some(semi) = after.find(';') {
            let entity = &after[..semi];
            match entity {
                "lt" => out.push('<'),
                "gt" => out.push('>'),
                "amp" => out.push('&'),
                "quot" => out.push('"'),
                "apos" => out.push('\''),
                _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                    if let Ok(code) = u32::from_str_radix(&entity[2..], 16)
                        && let Some(ch) = char::from_u32(code)
                    {
                        out.push(ch);
                    }
                }
                _ if entity.starts_with('#') => {
                    if let Ok(code) = entity[1..].parse::<u32>()
                        && let Some(ch) = char::from_u32(code)
                    {
                        out.push(ch);
                    }
                }
                _ => {
                    out.push('&');
                    out.push_str(entity);
                    out.push(';');
                }
            }
            rest = &after[semi + 1..];
        } else {
            out.push('&');
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::parse_time_profile_xml;
    use crate::macho::MachCpu;
    use anyprof_core::{LanguageUnwind, Runtime};

    #[test]
    fn parses_xctrace_rows_resolves_ref_nodes_demangles_rust_and_filters_pid() {
        let xml = r#"<?xml version="1.0"?>
<trace-query-result>
<node xpath='//trace-toc[1]/run[1]/data[1]/table[@schema="time-profile"]'>
<schema name="time-profile"/>
<row>
    <sample-time id="1" fmt="00:00.100.000">100000000</sample-time>
    <thread id="2" fmt="worker-1 0x627b (demo, pid: 4242)">
        <tid id="3" fmt="0x627b">25211</tid>
        <process id="4" fmt="demo (4242)">
            <pid id="5" fmt="4242">4242</pid>
        </process>
    </thread>
    <process ref="4"/>
    <thread-state id="6" fmt="Running"/>
    <weight id="7" fmt="1.00 ms">1000000</weight>
    <backtrace id="10">
        <frame id="11" name="_ZN4demo11hot_generic17h0123456789abcdefE" addr="0x100004120">
            <binary id="12" name="demo" UUID="AAAA" arch="arm64" load-addr="0x100000000" path="/nonexistent/demo"/>
        </frame>
        <frame id="13" name="outer_call&lt;int&gt;(int)" addr="0x100004010">
            <binary ref="12"/>
        </frame>
        <frame id="14" name="kernel_trap" addr="0xfffffe0012345678"/>
    </backtrace>
</row>
<row>
    <sample-time id="20" fmt="00:00.050.000">50000000</sample-time>
    <thread ref="2"/>
    <process ref="4"/>
    <thread-state ref="6"/>
    <backtrace ref="10"/>
</row>
<row>
    <sample-time id="30" fmt="00:00.150.000">150000000</sample-time>
    <thread id="31" fmt="other 0x9999">
        <tid id="32">9999</tid>
    </thread>
    <process id="33" fmt="other (9999)">
        <pid id="34">9999</pid>
    </process>
    <backtrace ref="10"/>
</row>
<row>
    <sample-time id="40" fmt="00:00.200.000">200000000</sample-time>
    <thread ref="2"/>
    <process ref="4"/>
    <thread-state id="41" fmt="Blocked"/>
    <backtrace ref="10"/>
</row>
</node>
</trace-query-result>
"#;

        let (stacks, samples) =
            parse_time_profile_xml(xml, 4242, LanguageUnwind::Auto, MachCpu::Arm64)
                .expect("parse xctrace xml");

        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].weight, 2, "other pid and Blocked state excluded");
        assert_eq!(stacks[0].frames.len(), 2, "kernel address filtered out");
        assert_eq!(stacks[0].frames[0].runtime, Runtime::Rust);
        assert!(
            stacks[0].frames[0].name.contains("demo::hot_generic"),
            "got {}",
            stacks[0].frames[0].name
        );
        assert_eq!(stacks[0].frames[0].module.as_deref(), Some("demo"));
        assert_eq!(stacks[0].frames[1].name, "outer_call<int>(int)");
        assert_eq!(stacks[0].frames[1].runtime, Runtime::Native);

        assert_eq!(samples.len(), 2);
        // Sorted chronologically: 50ms -> offset 0ns, 100ms -> offset 50_000_000ns.
        assert_eq!(samples[0].offset_ns, 0);
        assert_eq!(samples[0].tid, 25211);
        assert_eq!(samples[0].thread_name.as_deref(), Some("worker-1"));
        assert_eq!(samples[1].offset_ns, 50_000_000);
    }

    #[test]
    fn empty_export_returns_empty_profile_without_error() {
        let (stacks, samples) = parse_time_profile_xml(
            "<trace-query-result/>",
            123,
            LanguageUnwind::None,
            MachCpu::Arm64,
        )
        .expect("empty xml");
        assert!(stacks.is_empty());
        assert!(samples.is_empty());
    }
}
