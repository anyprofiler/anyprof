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
    fn default_format(&self) -> Format;
    fn start(&self, target: &Target, opts: &RecordOptions) -> Result<Box<dyn Session>, Error>;
}

trait Session {
    fn stop(self: Box<Self>) -> Result<Profile, Error>;
}
```

Stacks are leaf-first: `frames[0]` is the leaf (the frame that was on-CPU) and the last frame is the root. That matches pprof. Collapsed stacks and speedscope are root-first, and the exporters reverse the order.

A frame from a non-native runtime is written as `[runtime] name`, for example `[node] work`, in collapsed stacks, speedscope, and pprof, so mixed stacks keep their origin. A `.cpuprofile` recorded from Node or a browser tab is the original V8 document from `Profiler.stop`, not a rebuilt tree. `convert` rebuilds a `.cpuprofile` Chrome DevTools can open: a `(root)` node, `scriptId` on every call frame, and `timeDeltas` the same length as `samples`.

## Install

Prebuilt binaries are on [GitHub Releases](https://github.com/anyprofiler/anyprof/releases). Release tags are `v` plus the workspace version, so version `0.1.2` is tag `v0.1.2`. Each archive ships the `anyprof` executable (`anyprof.exe` on Windows) from the `anyprof-cli` crate, plus a checksum.

macOS and Linux:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/anyprofiler/anyprof/releases/download/v0.1.2/anyprof-cli-installer.sh | sh
```

Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/anyprofiler/anyprof/releases/download/v0.1.2/anyprof-cli-installer.ps1 | iex"
```

The installer puts `anyprof` in Cargo's bin directory (`~/.cargo/bin`) and adds that directory to `PATH` when it is missing.

Upgrade an installed binary in place. `upgrade` is the same command, and the typo `uptate` works too:

```bash
anyprof update
```

That downloads the latest GitHub Release and replaces the current binary. If this copy is already current, or newer than the latest release, it says so and exits 0. `anyprof update` only works for a binary the release installer put in place. A `cargo install` build has no install receipt; the command tells you to re-run the installer above.

Updates only come from `github.com/anyprofiler/anyprof`. The command refuses a receipt that names another repository and ignores the installer's host overrides (`ANYPROF_CLI_INSTALLER_GITHUB_BASE_URL`, `ANYPROF_CLI_INSTALLER_GHE_BASE_URL`, `ANYPROF_CLI_DOWNLOAD_URL`, `INSTALLER_DOWNLOAD_URL`). It also ignores `ANYPROF_CLI_INSTALL_DIR`, so the new binary goes where the release installer put this one. The installer checks the archive's SHA-256 before it unpacks. Once the installer starts, Ctrl+C waits for it to exit. On Windows the running `anyprof.exe` is moved to `anyprof.exe.previous.exe` during the install and moved back if the install fails. If a leftover `anyprof.exe.previous.exe` can't be deleted afterwards, the update still exits 0 and prints its path so you can delete it.

Without a release, build from this repo. That needs Rust 1.85 or newer (the workspace is edition 2024):

```bash
cargo install --git https://github.com/anyprofiler/anyprof anyprof-cli
```

Homebrew and npm packages are not published. Those installers stay off until a tap and an npm scope exist.

## Quick start

CI and release builds use Rust 1.99.1 (`rust-toolchain.toml`). Profiling Node also needs `node` on `PATH` when the target is a Windows process (anyprof shells out to `node -e`).

```bash
cargo build --release

# Node.js. The process does not need to have been started with --inspect.
# Omit -d to record until Ctrl+C. Omit -o to write an auto-named .cpuprofile.
anyprof rec -p 1234 -d 10s -o cpu.cpuprofile
anyprof rec -p 1234

# Pin the inspector port, or pass a websocket URL you already have.
anyprof rec -p 1234 -i 9229 -d 10s -f speedscope -o cpu.json
anyprof rec -p 1234 -i ws://127.0.0.1:9229/<id> -d 10s -o cpu.collapsed

# Browser tab. Start the browser yourself:
#   chrome --remote-debugging-port=9222
anyprof rec -B -t example.com -d 10s -o page.cpuprofile

# Import an existing V8 profile.
anyprof conv -i profile.cpuprofile -o cpu.collapsed -t collapsed
anyprof conv -i cpu.collapsed -o cpu.speedscope.json -t speedscope
anyprof conv -i cpu.collapsed -o cpu.pb.gz -t pprof
```

`record` (`rec`) options:

| Flag | Meaning |
| --- | --- |
| `-p, --pid <PID>` | Process to profile. Required unless `-B` / `--browser` is set. |
| `-B, --browser` | Profile a browser tab instead of a pid. |
| `-t, --tab <URL_TITLE_OR_ID>` | Which tab. Target id, or a substring of the URL or title. Requires `--browser`. |
| `-i, --inspect <PORT_OR_URL>` | Inspector or remote-debugging endpoint: a port, `host:port`, `http://` URL, or `ws://` URL. Node uses a port that process is listening on, then 9229. Before sampling, anyprof reads `process.pid` and refuses the connection when it is a different process. Browser defaults to `127.0.0.1:9222`. |
| `-d, --duration <DURATION>` | How long to sample. Omit it to record until Ctrl+C (SIGINT). SIGTERM, and on Windows Ctrl+Break, end the recording the same way and still write the file. A second Ctrl+C while the profile is being saved exits immediately. Accepts `humantime` values such as `90s` or `1m30s`. There is no default duration. |
| `-o, --output <PATH>` | Output file or directory. Optional. Omit it, pass an existing directory, `.`, `..`, or a trailing `/` or `\`, and anyprof writes `anyprof-<backend>-<pid-or-tab>-YYYYMMDD-HHMMSS.<ext>` there (local time, no colons). If that name exists, `-2`, `-3`, ... is inserted before the full extension. A leading `~` expands to the home directory. Relative paths use the current directory. The path printed at the end is absolute. An explicit file is overwritten. |
| `-f, --format <collapsed\|pprof\|speedscope\|cpuprofile>` | Output format. An explicit value wins. Otherwise an output *file* extension is used. A directory or omitted `-o` uses the backend default below. |
| `-F, --frequency <HZ>` | Samples per second. The default depends on the backend (see below). An explicit value wins. Cannot be combined with `--interval`. |
| `--interval <DURATION>` | Time between samples, for example `100us`, `1ms`, or `10ms`. The same setting as `-F`, written as a period. Node and browser round it to whole microseconds. |
| `-k, --kernel` | Ask for kernel frames when the backend supports them. Node and browser profiles do not include kernel frames. |
| `-b, --backend <node\|browser\|go\|native>` | Skip auto-detection. |
| `-v`, `-vv`, `-vvv` | Log more (`info`, `debug`, `trace`). `RUST_LOG` is used when `-v` is not passed. `-h` and `-V` are help and version. |

Default sampling rate when neither `-F` nor `--interval` is passed:

| Backend | Default interval | Rate |
| --- | --- | --- |
| `node`, `browser` | 100us | 10000 Hz, the same as a Chrome DevTools Performance recording |
| `go`, `native` | 10101us | 99 Hz, the `perf record -F 99` convention |

At 100us a mostly idle server still records the sub-millisecond work of each request. A 10s recording holds tens of thousands of samples, and a busy process writes about 2.5 MB of `.cpuprofile` per minute. For long recordings, `--interval 1ms` keeps the file 10 times smaller. The start line names the rate in use, for example `using backend 'node' for pid 1234 (on-cpu, mixed-stacks), sampling every 100us (10000 Hz)`.

After recording, anyprof warns on stderr when the profile has 0 samples. It also warns when more than 95% of samples are V8's `(idle)` or `(program)` nodes, which means the process was waiting rather than running JavaScript. Record again while the process is under load, for example by sending it requests during the recording. The profile is still written.

While stderr is a terminal, `record` redraws one status line (`Recording node pid 1234... 00:12 elapsed (Ctrl+C to stop)`, or with time left when `-d` is set). A pipe or file gets a single start line.

Default format when `-f` is omitted and `-o` is not a recognized file name:

| Backend | Default | Extension | Open with |
| --- | --- | --- | --- |
| `node` | cpuprofile | `.cpuprofile` | Chrome DevTools Performance panel, VS Code |
| `browser` | cpuprofile | `.cpuprofile` | Chrome DevTools Performance panel |
| `go` | pprof | `.pb.gz` | `go tool pprof`, pprof web UI |
| `native` | pprof | `.pb.gz` | `go tool pprof`, pprof web UI |
| Python, Ruby, PHP, and other scripting runtimes (planned) | speedscope | `.speedscope.json` | [speedscope.app](https://www.speedscope.app) |

`-o cpu.collapsed` still writes collapsed stacks for Node. `--format pprof -o cpu.cpuprofile` still writes pprof. A directory named `profiles.cpuprofile` does not select the format; the backend default does.

`convert` (`conv`) reads collapsed stacks, speedscope JSON, pprof (gzip-compressed or raw protobuf), and V8 `.cpuprofile`, and writes any of the four. `-f` / `--from` is inferred from the file name (`.collapsed`, `.folded`, `.txt`, `.json`, `.speedscope.json`, `.pb.gz`, `.pprof`, `.cpuprofile`) when you omit it. `-t` / `--to` is the output format. `-i` / `--input` and `-o` / `--output` are the files. A `.cpuprofile` that carries no `[runtime]` prefix is imported as Node. A `.cpuprofile` written by `convert` is rebuilt from the unified stacks and is still a tree Chrome DevTools accepts.

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
3. **Rust**, then **Go** (detection already exists on Linux), then Python, Java, Ruby, PHP, and .NET. Those scripting backends should default to speedscope (see the format table above). Go already defaults to pprof.
4. Linux perf events are last among the native platform backends, after macOS and Windows.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

CI runs those checks on macOS, Windows, and Ubuntu, in that order, with Node.js 22 installed so the live Node profile test runs. `rust-toolchain.toml` pins Rust 1.99.1, which is the compiler CI uses. The crate MSRV is 1.85, and the workspace uses Rust edition 2024.

### Cutting a release

1. Bump `version` under `[workspace.package]` in `Cargo.toml`. Every crate inherits it.
2. Commit that change on `main` (the release workflow has to be on the default branch).
3. Tag that commit `vX.Y.Z`. The `v` prefix and the version must match the workspace version. Version `0.1.2` is tag `v0.1.2`.
4. Push the tag: `git push origin v0.1.2`.

Pushing the tag runs `.github/workflows/release.yml`. [cargo-dist](https://axodotdev.github.io/cargo-dist/) builds `anyprof` for Apple Silicon macOS, Intel macOS, 64-bit Windows, and 64-bit Linux (glibc and musl, x86_64 and ARM), then uploads the archives, checksums, shell installer, and PowerShell installer to a GitHub Release.

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
| `dist-workspace.toml` | cargo-dist release config |

Linux process inspection reads `/proc`. On other Unix systems, detection uses `ps` for the process name and command line. On Windows it uses `tasklist`. Listening-port discovery uses `/proc/<pid>/net/tcp` (filtered by socket inode), `lsof` on macOS, and `netstat` on Windows.

## License

Apache-2.0. See [LICENSE](LICENSE).
