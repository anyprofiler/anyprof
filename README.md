# anyprof

Capture CPU profiles from any running process, no code changes or restarts needed.

anyprof attaches to a live process by pid and writes a CPU profile. The process does not need a profiler baked in, and it does not need to be restarted. Runtimes sit behind one `Backend` trait, and every backend emits the same profile.

**Status:** Node.js sampling works. Give anyprof the pid of a running `node` process; it turns the V8 inspector on, records a CPU profile, and writes collapsed stacks, speedscope, pprof, or `.cpuprofile`. Browser tabs work the same way when Chrome, Chromium, or Edge is already listening with `--remote-debugging-port`. Native C/C++ sampling and Go sampling are still stubs.

## Goals

- Profile a running process by pid, without code changes or a restart.
- One data model for every runtime. A frame records which runtime it came from; file, line, module, and address are optional and may be empty.
- Backends declare what they can do (on-CPU, off-CPU, mixed stacks, kernel frames, whether they need root) instead of pretending they are identical.
- Write profiles in formats people already use: collapsed stacks, speedscope, pprof, and V8 `.cpuprofile`.

## Runtime support

Platform order for new samplers is **macOS, then Windows, then Linux**. Runtime order is **Node.js, then C++ (native) and browser pages, then Rust, Go, Python, and the rest**.

| Runtime | Crate | Status | Notes |
| --- | --- | --- | --- |
| Node.js | `anyprof-node` | Works | Attaches by pid. macOS and Linux send `SIGUSR1`; Windows runs `node -e "process._debugProcess(pid)"`. Samples over the V8 inspector (Chrome DevTools Protocol). |
| Browser page | `anyprof-browser` | Works | Chrome, Chromium, or Edge started with `--remote-debugging-port`. Select a tab by URL, title, or target id. Does not match pids. |
| C, C++ | `anyprof-native` | Stub (next) | Fallback for any live pid. Planned order is macOS, then Windows, then Linux perf events. |
| Rust | `anyprof-native` | Later | Rust processes currently use the native fallback. A Rust-specific sampler comes after C++. |
| Go | `anyprof-go` | Stub (later) | Linux detection scans the executable for the Go build-info magic so a Go process is not labeled native. Sampling is lower priority than Node, browser pages, and C++. |
| Python | — | Planned | After Go. |
| Java | — | Planned | |
| Ruby | — | Planned | |
| PHP | — | Planned | |
| .NET | — | Planned | |

Auto-detection tries `node`, then `go`, then `native`. The browser backend is only used with `--browser` (or `--backend browser`). Pass `--backend` to skip detection.

## Architecture

```
anyprof record --pid <PID>
anyprof record --browser --tab <url>
        |
        v
anyprof-cli -------- auto-detect / --backend / --browser
        |
        v
anyprof-core        Backend::detect / start -> Session::stop -> Profile
        ^
        |
        +-- anyprof-node      (Node.js, checked first)
        +-- anyprof-browser   (explicit; CDP, same client as Node)
        +-- anyprof-go        (Go, more specific than native)
        +-- anyprof-native    (fallback for any live pid)
        |
        +-- anyprof-cdp       (inspector HTTP discovery + Profiler.*)
        |
        v
anyprof-export       collapsed | speedscope | pprof | cpuprofile
```

`Backend` is the extension point:

```rust
trait Backend {
    fn name(&self) -> &'static str;
    fn detect(&self, pid: u32) -> bool;
    fn capabilities(&self) -> Capabilities;
    fn start(&self, target: &Target, opts: &RecordOptions) -> Result<Box<dyn Session>, Error>;
}

trait Session {
    fn stop(self: Box<Self>) -> Result<Profile, Error>;
}
```

Stacks are leaf-first: `frames[0]` is the leaf (the frame that was on-CPU) and the last frame is the root. That matches pprof. Collapsed stacks and speedscope are root-first, and the exporters reverse the order.

A frame from a non-native runtime is written as `[runtime] name`, for example `[node] work`, so mixed stacks keep their origin in every format. `.cpuprofile` export keeps that prefix so a browser profile stays `[browser]` when it is imported again.

## Quick start

Requires Rust 1.85 or newer. CI uses Rust 1.99. Profiling Node also needs `node` on `PATH` when the target is a Windows process (anyprof shells out to `node -e`).

```bash
cargo build --release

# Node.js. The process does not need to have been started with --inspect.
anyprof record --pid 1234 --duration 10s -o cpu.collapsed

# Pin the inspector port, or pass a websocket URL you already have.
anyprof record --pid 1234 --inspect 9229 --duration 10s --format speedscope -o cpu.json
anyprof record --pid 1234 --inspect ws://127.0.0.1:9229/<id> --duration 10s -o cpu.collapsed

# Browser tab. Start the browser yourself:
#   chrome --remote-debugging-port=9222
anyprof record --browser --tab example.com --duration 10s -o page.collapsed

# Import an existing V8 profile.
anyprof convert -i profile.cpuprofile -o cpu.collapsed
anyprof convert -i cpu.collapsed -o cpu.speedscope.json --to speedscope
anyprof convert -i cpu.collapsed -o cpu.pb.gz --to pprof
```

`record` options:

| Flag | Meaning |
| --- | --- |
| `--pid <PID>` | Process to profile. Required unless `--browser` is set. |
| `--browser` | Profile a browser tab instead of a pid. |
| `--tab <URL_TITLE_OR_ID>` | Which tab. Target id, or a substring of the URL or title. Requires `--browser`. |
| `--inspect <PORT_OR_URL>` | Inspector or remote-debugging endpoint: a port, `host:port`, `http://` URL, or `ws://` URL. Node defaults to a port that process is listening on, then 9229. Browser defaults to `127.0.0.1:9222`. |
| `--duration <DURATION>` | How long to sample. Default `30s`. Accepts `humantime` values such as `90s` or `1m30s`. |
| `-o, --output <PATH>` | Output file. Required. |
| `--format <collapsed\|pprof\|speedscope\|cpuprofile>` | Output format. Default `collapsed`. |
| `--frequency <HZ>` | Requested sampling rate. Default `99`. Node and Chrome receive this as a V8 sampling interval. |
| `--kernel` | Ask for kernel frames when the backend supports them. Node and browser profiles do not include kernel frames. |
| `--backend <node\|browser\|go\|native>` | Skip auto-detection. |
| `-v`, `-vv`, `-vvv` | Log more (`info`, `debug`, `trace`). `RUST_LOG` is used when `-v` is not passed. |

`convert` reads collapsed stacks, speedscope JSON, pprof (gzip-compressed or raw protobuf), and V8 `.cpuprofile`, and writes any of the four. `--from` is inferred from the file name (`.collapsed`, `.folded`, `.json`, `.speedscope.json`, `.pb.gz`, `.pprof`, `.cpuprofile`) when you omit it. A `.cpuprofile` that carries no `[runtime]` prefix is imported as Node.

Collapsed stacks keep the runtime label and function name. Speedscope also keeps file, line, and column. pprof also keeps the instruction address, duration, and sampling period. A frame's module is not written yet.

Collapsed output is the folded format FlameGraph expects:

```
main;[node] work 3
[kernel] schedule;[go] runtime.main 4
```

## Permissions

Node attach does not need root. On macOS and Linux, anyprof sends `SIGUSR1` only when the process looks like `node` or `nodejs` (including `node.exe`). That signal's default action is to terminate a process, so a pid that does not look like Node is refused before anything is signaled. Profile a process that is already running: during the first moments of startup, before Node installs its handler, `SIGUSR1` still terminates the process. If you pass `--inspect` with a `ws://` URL, anyprof does not signal. After a profile, Node may keep the inspector listening on `127.0.0.1` until the process exits.

On Windows, anyprof runs `node -e "process._debugProcess(<pid>)"`, which is Node's documented way to activate the inspector. `node` must be on `PATH`.

The browser backend only connects. Start Chrome, Chromium, or Edge with `--remote-debugging-port` yourself. anyprof does not launch a browser.

Native sampling is not built yet. When the Linux perf sampler lands it will need the same access as `perf`: often root or `CAP_PERFMON` / `CAP_SYS_PTRACE`, depending on `kernel.perf_event_paranoid`. The CLI only profiles the pid you pass it.

## Roadmap

1. **Node.js** via the V8 inspector, on macOS, then Windows, then Linux. This is implemented.
2. **C++ (native)** on-CPU sampling, in the same platform order, and **browser pages** over the DevTools Protocol. Browser pages are implemented. Native sampling is the next sampler.
3. **Rust**, then **Go** (detection already exists on Linux), then Python, Java, Ruby, PHP, and .NET.
4. Linux perf events are last among the native platform backends, after macOS and Windows.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

CI runs those checks on macOS, Windows, and Ubuntu, in that order, with Node.js 22 installed so the live Node profile test runs. `rust-toolchain.toml` pins Rust 1.99.1, which is the compiler CI uses. The crate MSRV is 1.85.

The live test spawns `node --inspect-port=127.0.0.1:<port>` without telling the backend that port, so port discovery and inspector activation both run. It skips when `node` is not on `PATH`.

| Path | Role |
| --- | --- |
| `crates/anyprof-core` | `Backend`, `Session`, `Profile`, process inspection |
| `crates/anyprof-cdp` | DevTools Protocol client shared by Node and browser pages |
| `crates/anyprof-node` | Node.js backend |
| `crates/anyprof-browser` | Browser-tab backend |
| `crates/anyprof-native` | Native backend (stub; pid fallback) |
| `crates/anyprof-go` | Go backend (stub; Linux build-info detection works) |
| `crates/anyprof-export` | collapsed, speedscope, pprof, and `.cpuprofile` |
| `crates/anyprof-cli` | the `anyprof` binary |

Linux process inspection reads `/proc`. On other Unix systems, detection uses `ps` for the process name and command line. On Windows it uses `tasklist`. Listening-port discovery uses `/proc/<pid>/net/tcp` (filtered by socket inode), `lsof` on macOS, and `netstat` on Windows.

## License

Apache-2.0. See [LICENSE](LICENSE).
