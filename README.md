# anyprof

Capture CPU profiles from any running process, no code changes or restarts needed.

anyprof attaches to a live process by pid and writes a CPU profile. The process does not need a profiler baked in, and it does not need to be restarted. Runtimes sit behind one `Backend` trait, and every backend emits the same profile.

**Status:** Node.js sampling works. Give anyprof the pid of a running `node` process; it turns the V8 inspector on, records a CPU profile, and writes collapsed stacks, speedscope, pprof, or `.cpuprofile`. `anyprof view` opens any of those in a built-in, offline speedscope viewer. Browser tabs work the same way when Chrome, Chromium, or Edge is already listening with `--remote-debugging-port`. Native C/C++ sampling works on macOS (Mach `task_for_pid`, frame pointers, Mach-O symbols, root), on Windows x64 (suspend only threads that are running, `StackWalk64`, same user), and on Linux x86_64 (`perf_event_open` task-clock events, frame pointers, ELF symbols). Rust uses that same sampler: a Rust pid is detected, Rust names are demangled, and Rust frames are labeled `[rust]`. Go uses the same on-CPU events and unwinds with the Go pclntab, so a leaf without a frame pointer still has its caller, and Go frames are labeled `[go]`. Other CPU architectures compile and `start` returns not implemented.

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
| C, C++ | `anyprof-native` | macOS, Windows x64, and Linux x86_64 | Fallback for any live pid. CI samples a busy C++ program and an idle program on Apple Silicon, Intel macOS, `windows-latest`, and `ubuntu-latest`. See the [support matrix](#support-matrix). |
| Rust | `anyprof-rust` | macOS, Windows x64, and Linux x86_64 | Reuses the native sampler. Auto-detection reads the executable (no attach). Rust frames are demangled and prefixed `[rust]`. See the [support matrix](#support-matrix). |
| Go | `anyprof-go` | macOS, Windows x64, and Linux x86_64 | Reuses the native on-CPU sampler. Stacks come from Go’s pclntab (pcsp), not from frame pointers. Go frames are prefixed `[go]`. See the [support matrix](#support-matrix). |
| Python | — | Planned | After Go. |
| Java | — | Planned | |
| Ruby | — | Planned | |
| PHP | — | Planned | |
| .NET | — | Planned | |

Auto-detection tries `node`, then `rust`, then `go`, then `native`. The browser backend is only used with `--browser` (or `--backend browser`). Pass `--backend` to skip detection. `--backend native` on a Rust pid still selects the native backend (the auto-named file is `anyprof-native-...`), but Rust symbols in that profile are still labeled `[rust]`.

## Support matrix

"Works" means a live process is sampled in CI and the profile's stacks, sample count, and timing are checked. "Compilation only" means CI builds the crate and asserts that sampling is refused; it is not a working profiler on that OS.

| Runtime | macOS | Windows | Linux |
| --- | --- | --- | --- |
| Node.js | Works (CI) | Works (CI) | Works (CI) |
| Browser page | Works (CI, headless Chrome) | Works (CI, headless Chrome) | Works (CI, headless Chrome) |
| C, C++ | Works (CI) on Apple Silicon and Intel. Root, Mach `task_for_pid`, frame pointers, Mach-O symbols. Busy and idle programs. User frames only. | Works (CI) on x64. Same user, no administrator token. Suspends only threads whose state is Running, walks with `StackWalk64`, resolves dbghelp symbols. Busy and idle programs. Refuses this process before `OpenProcess`. 32-bit processes, ARM64 processes, and non-x64 builds are not sampled. A kernel address is not stored as a user frame. | Works (CI) on x86_64. `perf_event_open` task clock (on-CPU; the thread is not suspended). Frame-pointer walk of the user stack copy, ELF symbols. Busy, idle, and two running threads. Kernel frames with `-k` when `perf_event_paranoid` is 1 or lower. aarch64 and other architectures are not sampled and do not advertise `-k`. |
| Rust | Works (CI) on Apple Silicon and Intel. Same Mach sampler and root requirement as C/C++. rustc 1.99 does not emit frame pointers unless `-C force-frame-pointers=yes`. The library tests sample a busy program, an idle park, and two running threads. | Works (CI) on x64. Same user, no administrator token. `StackWalk64` uses the PE unwind data rustc emits. Names come from dbghelp, or from a PDB whose GUID and age match the image when dbghelp returns none. The fixture passes `-C force-frame-pointers=yes` and `-C debuginfo=2`. Busy, idle, and two running threads. Refuses this process before `OpenProcess`. Non-x64 builds are not sampled. | Works (CI) on x86_64. Same `perf_event_open` sampler as C/C++. rustc 1.99 does not emit frame pointers at `-O0` or `-O2` unless `-C force-frame-pointers=yes`. Busy, idle, and two running threads. `-k` is the native Linux behavior. musl x86_64 is the same code and is only `cargo check`ed. aarch64 and other architectures are not sampled and do not advertise `-k`. |
| Go | Works (CI) on Apple Silicon and Intel for Go 1.22 and Go 1.26. Same Mach sampler and root requirement as C/C++. The user stack is the pclntab pcsp walk of the running goroutine. Busy, parked, and two running goroutines, `CGO_ENABLED=0 -ldflags=-s -w`. Those live tests passed on both hosts and both Go versions in run 44227429060. | Works (CI) on x64 for Go 1.22 and Go 1.26. Same user, no administrator token. pcsp replaces `StackWalk64` when the PC is in the Go pclntab; otherwise `StackWalk64` is unchanged. Busy, parked, and two running goroutines. Refuses this process before `OpenProcess`. Non-x64 builds are not sampled. Run 44227429060 passed those live tests for both Go versions. | Works (CI) on x86_64 for Go 1.22 and Go 1.26. Same `perf_event_open` sampler as C/C++. The copied user stack is 8 KiB and is unwound with pcsp. Busy, parked, and two running goroutines. Also live-sampled here on Go 1.22.2 and Go 1.26.11. `-k` is the native Linux behavior. musl x86_64 is the same code and is only `cargo check`ed. aarch64 and other architectures are not sampled and do not advertise `-k`. |
| Python, Java, Ruby, PHP, .NET | Not implemented | Not implemented | Not implemented |

`macos-latest` is arm64. `macos-15-intel` is x86_64. Each of those jobs samples a busy C++ program and an idle program under sudo. The arm64 job also runs `xpaci` and `xpacd` on an unsigned pointer. The Intel job does not, and a green arm64 job is not x86_64 coverage. arm64e signed userspace is not its own CI image. Signed pointers follow Darwin's `__darwin_arm_thread_state64_ptrauth_strip`: `NO_PTRAUTH` leaves the register pc, lr, sp, and fp alone; `IB_SIGNED_LR` leaves the register lr alone; otherwise `xpaci` strips code pointers and `xpacd` strips data pointers. Saved frame records are stripped with those instructions even when the registers are already raw, because an arm64 process can be inside an arm64e system function. An unsigned pointer is left unchanged. `KERNEL_SIGNED_PC` and `KERNEL_SIGNED_LR` do not switch that to authentication, because a bad signature can trap. Where those instructions are not compiled, a signed pointer is left unchanged instead of masked to 48 bits. System libraries that exist only in the dyld shared cache (`dyld_v1`) keep an address range and do not get function names. The symbol reader still refuses a table above 2,000,000 symbols or a string table above 64 MiB.

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
        +-- anyprof-rust      (Rust; native sampler, Rust detection and names)
        +-- anyprof-go        (Go; native on-CPU events, pclntab stacks)
        +-- anyprof-native    (fallback for any live pid)
        |
        +-- anyprof-cdp       (inspector HTTP discovery + Profiler.*)
        |
        v
anyprof-export       collapsed | speedscope | pprof | cpuprofile
        |
        v
anyprof-web          anyprof view / record --web: embedded speedscope on 127.0.0.1
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

Prebuilt binaries are on [GitHub Releases](https://github.com/anyprofiler/anyprof/releases). Release tags are `v` plus the workspace version, so version `0.1.6` is tag `v0.1.6`. Each archive ships the `anyprof` executable (`anyprof.exe` on Windows) from the `anyprof-cli` crate, plus a checksum.

macOS and Linux:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/anyprofiler/anyprof/releases/download/v0.1.6/anyprof-cli-installer.sh | sh
```

Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/anyprofiler/anyprof/releases/download/v0.1.6/anyprof-cli-installer.ps1 | iex"
```

The installer puts `anyprof` in Cargo's bin directory (`~/.cargo/bin`) and adds that directory to `PATH` when it is missing.

Upgrade an installed binary in place. `upgrade` is the same command, and the typo `uptate` works too:

```bash
anyprof update
```

That downloads the latest GitHub Release and replaces the current binary. If this copy is already current, or newer than the latest release, it says so and exits 0. `anyprof update` only works for a binary the release installer put in place. A `cargo install` build has no install receipt; the command tells you to re-run the installer above.

Updates only come from `github.com/anyprofiler/anyprof`. The command refuses a receipt that names another repository and ignores the installer's host overrides (`ANYPROF_CLI_INSTALLER_GITHUB_BASE_URL`, `ANYPROF_CLI_INSTALLER_GHE_BASE_URL`, `ANYPROF_CLI_DOWNLOAD_URL`, `INSTALLER_DOWNLOAD_URL`). It also ignores `ANYPROF_CLI_INSTALL_DIR`, so the new binary goes where the release installer put this one. The installer checks the archive's SHA-256 before it unpacks. Once the installer starts, Ctrl+C waits for it to exit. On Windows the running `anyprof.exe` is moved to `anyprof.exe.previous.exe` during the install and moved back if the install fails. If a leftover `anyprof.exe.previous.exe` can't be deleted afterwards, the update still exits 0 and prints its path so you can delete it.

Without a release, build from this repo. That needs Rust 1.88 or newer (the workspace is edition 2024, and the `anyprof update` dependencies need 1.88):

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

# C or C++. Omit -o for an auto-named .pb.gz.
# macOS: task_for_pid needs root.
sudo anyprof rec -p 1234 -d 10s
# Linux x86_64 and Windows x64: a same-user process does not need root.
# See Permissions for perf_event_paranoid, ptrace_scope, and SeDebugPrivilege.
anyprof rec -p 1234 -d 10s

# Rust. Same sampler and permissions as C/C++. The binary needs frame
# pointers on macOS and Linux (`-C force-frame-pointers=yes`). Omit -o for
# an auto-named anyprof-rust-<pid>-YYYYMMDD-HHMMSS.pb.gz.
anyprof rec -p 1234 -d 10s

# Import an existing V8 profile.
anyprof conv -i profile.cpuprofile -o cpu.collapsed -t collapsed
anyprof conv -i cpu.collapsed -o cpu.speedscope.json -t speedscope
anyprof conv -i cpu.collapsed -o cpu.pb.gz -t pprof

# Look at a profile in the built-in speedscope viewer (offline).
anyprof view cpu.cpuprofile
anyprof rec -p 1234 -d 10s -w
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
| `-F, --frequency <HZ>` | Samples per second, from 1 to 1000000 (one sample per microsecond). The default depends on the backend (see below). An explicit value wins. Cannot be combined with `--interval`. |
| `--interval <DURATION>` | Time between samples, for example `100us`, `1ms`, or `10ms`. The same setting as `-F`, written as a period. Node and browser round it to whole microseconds. |
| `-k, --kernel` | Ask for kernel frames when the backend supports them. Linux x86_64 records them. Other Linux architectures do not advertise kernel frames, because sampling there is not implemented. macOS and Windows x64 ignore the flag and record user frames. Node and browser profiles do not include kernel frames. |
| `-b, --backend <node\|browser\|rust\|go\|native>` | Skip auto-detection. |
| `-w, --web` | After writing the file, open it in the built-in viewer, the same as `anyprof view <file>`. `--no-open`, `--port`, and `--web-timeout` (see [Viewing profiles](#viewing-profiles)) require `--web`. |
| `-v`, `-vv`, `-vvv` | Log more (`info`, `debug`, `trace`). `RUST_LOG` is used when `-v` is not passed. `-h` and `-V` are help and version. |

Default sampling rate when neither `-F` nor `--interval` is passed:

| Backend | Default interval | Rate |
| --- | --- | --- |
| `node`, `browser` | 100us | 10000 Hz, the same as a Chrome DevTools Performance recording |
| `rust`, `go`, `native` | 10101us | 99 Hz, the `perf record -F 99` convention |

At 100us a mostly idle server still records the sub-millisecond work of each request. A 10s recording holds tens of thousands of samples, and a busy process writes about 2.5 MB of `.cpuprofile` per minute. For long recordings, `--interval 1ms` keeps the file 10 times smaller. The start line names the rate in use, for example `using backend 'node' for pid 1234 (on-cpu, mixed-stacks), sampling every 100us (10000 Hz)`.

After recording, anyprof warns on stderr when the profile has 0 samples. It also warns when more than 95% of samples are V8's `(idle)` or `(program)` nodes and fewer than 100 samples caught JavaScript, native code, or the garbage collector. That means the process was waiting and the profile shows almost none of its work. Record again while the process is under load, for example by sending it requests during the recording. A mostly idle server that still handled some requests, such as 300 JavaScript samples out of 90,000, does not warn. The profile is always written.

While stderr is a terminal, `record` redraws one status line (`Recording node pid 1234... 00:12 elapsed (Ctrl+C to stop)`, or with time left when `-d` is set). A pipe or file gets a single start line.

Default format when `-f` is omitted and `-o` is not a recognized file name:

| Backend | Default | Extension | Open with |
| --- | --- | --- | --- |
| `node` | cpuprofile | `.cpuprofile` | Chrome DevTools Performance panel, VS Code |
| `browser` | cpuprofile | `.cpuprofile` | Chrome DevTools Performance panel |
| `rust` | pprof | `.pb.gz` | `go tool pprof`, pprof web UI |
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

## Viewing profiles

`anyprof view <file>` (alias `v`) opens a profile in [speedscope](https://github.com/jlfwong/speedscope) 1.25.0, which is built into the binary. Nothing is downloaded and the profile never leaves the machine. `anyprof rec ... -w` does the same with the file it just wrote: it prints the path as usual, then starts the viewer.

```bash
anyprof view cpu.cpuprofile
anyprof v cpu.pb.gz --no-open --port 8123
anyprof rec -p 1234 -d 10s -w --web-timeout 10m
```

`view` reads `.cpuprofile`, pprof (`.pb.gz`, gzip-compressed or raw), speedscope JSON, and collapsed stacks. The file is served as-is and speedscope detects the format from its contents, not the extension. pprof is decoded by speedscope's own protobuf importer, so there is no server-side conversion. Formats speedscope reads that anyprof does not write (Linux `perf script`, Instruments, and so on) work too. Java Flight Recorder files do not, because the JFR importer's wasm is not shipped.

| Flag | Meaning |
| --- | --- |
| `--no-open` | Print the URL instead of opening a browser. |
| `--port <PORT>` | Listen on `127.0.0.1:<PORT>`. Default: a free port chosen by the OS. |
| `--web-timeout <DURATION>` | Stop after this long (`humantime`, for example `10m`). Default: serve until Ctrl+C. |

The viewer serves until Ctrl+C (or SIGTERM, or Ctrl+Break on Windows), then exits 0. A second Ctrl+C exits immediately with status 130, even if a response is stuck on a client that stopped reading. The browser tab keeps the profile it already loaded.

anyprof opens the default browser with the [`webbrowser`](https://crates.io/crates/webbrowser) crate (macOS `open`, Windows `ShellExecute`, `xdg-open` and friends elsewhere). It prints the URL instead, together with an `ssh -L <port>:127.0.0.1:<port>` command for viewing from another machine, when:

- `--no-open` is passed,
- `SSH_CONNECTION` is set,
- on Linux and the BSDs, neither `DISPLAY` nor `WAYLAND_DISPLAY` is set (WSL is exempt), or
- opening the browser fails.

The URL looks like `http://127.0.0.1:53211/t/<token>/`. It has no `#fragment`, because macOS `open` and Windows `start` drop fragments. That page is a meta-refresh redirect to `index.html#profileURL=profile&title=<file name>&view=left-heavy`, the hash speedscope reads on startup.

### Security

- The server binds `127.0.0.1` only, never `0.0.0.0` or `::`.
- Every URL is under `/t/<token>/`, where the token is 128 bits from the OS random number generator (`getrandom`), compared in constant time. A wrong token, or any path outside it, is a 404 that does not reveal whether the token was close.
- Only the embedded speedscope files and the one profile (`/t/<token>/profile`) are served. Asset names are looked up in a table compiled into the binary, so there is no filesystem access and no path traversal; `..`, `/`, `\`, and percent-encoded names all miss the table and get a 404.
- Only `GET` and `HEAD` are accepted (others are 405). There are no CORS headers, so other origins cannot read responses.
- Every response carries `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Cache-Control: no-store`, and this Content Security Policy:

  ```
  default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'
  ```

  Everything is same-origin. `'wasm-unsafe-eval'` is there because speedscope's C++/Rust symbol demangler is a WebAssembly module inside its JS bundle; it allows compiling wasm, not JS `eval`. `style-src 'unsafe-inline'` is there because speedscope styles its UI with aphrodite (CSS-in-JS), which writes rules into a `<style>` element; without it the UI is unstyled and the flame graph fails to lay out. No `blob:`, `data:`, or remote sources are allowed.
- Anyone who can read the URL (for example from your shell scrollback, or another user on a shared machine who can see the process's output) can load the profile until the viewer stops. Use `--web-timeout` on shared machines.

### Binary size

The viewer adds about 0.8 MiB to the binary. On x86_64 Linux (glibc), the `dist` profile used for releases went from 10,964,520 to 11,781,840 bytes (+798 KiB, +7.5%); stripped, from 8,758,528 to 9,417,128 bytes (+643 KiB); xz-compressed, as in the release `.tar.xz`, from 2,773,660 to 3,119,540 bytes (+338 KiB). About 210 KiB of that is the gzip-compressed speedscope build. Most of the rest is `tiny_http` (about 120 KiB of code), `webbrowser` (about 50 KiB), and `anyprof-web` itself.

### Vendored speedscope

`crates/anyprof-web/vendor/speedscope/` holds speedscope 1.25.0's static build, one `<name>.gz` per file. `crates/anyprof-web/scripts/vendor-speedscope.sh` recreates it: it downloads the npm tarball, checks its sha256, drops the example profile (`perf-vertx-*.txt`) and the Java Flight Recorder wasm (`jfrview_bg*.wasm`), and gzips the rest with `gzip -n -9`. The files are embedded with [`include_dir`](https://crates.io/crates/include_dir) and sent with `Content-Encoding: gzip` (decompressed on the fly for a client that does not accept gzip). To upgrade, change `VERSION` and `SHA256` in the script and rerun it.

## Permissions

Node attach does not need root. On macOS and Linux, anyprof sends `SIGUSR1` only when the process looks like `node` or `nodejs` (including `node.exe`). That signal's default action is to terminate a process, so a pid that does not look like Node is refused before anything is signaled. Profile a process that is already running: during the first moments of startup, before Node installs its handler, `SIGUSR1` still terminates the process. If you pass `--inspect` with a `ws://` URL, anyprof does not signal. After a profile, Node may keep the inspector listening on `127.0.0.1` until the process exits.

On Windows, anyprof runs `node -e "process._debugProcess(<pid>)"`, which is Node's documented way to activate the inspector. `node` must be on `PATH`.

The browser backend only connects. Start Chrome, Chromium, or Edge with `--remote-debugging-port` yourself. anyprof does not launch a browser.

Native sampling on macOS calls `task_for_pid`. For an ordinary process that needs root, `sudo anyprof rec -p <pid>` is how to run it. sudo does not grant access to a process protected by System Integrity Protection or a hardened runtime, and anyprof does not ask for those protections to be changed. Sampling anyprof's own pid is refused before the attach: suspending this process can leave it stuck. Each sample suspends only threads that are actually running, long enough to read registers and walk frame pointers, then resumes them. A tick where nothing is running adds no samples. `thread_resume` is retried, and if it still fails the profile is discarded instead of returned. The process is not restarted and does not need a profiler linked in. `-k` does not add kernel frames on macOS.

Windows x64 native sampling opens the process with `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ` and suspends only threads whose `NtQuerySystemInformation` state is Running. Ready and Waiting threads are not samples. `GetThreadContext` and `StackWalk64` read the suspended thread. `ResumeThread` runs from `Drop`, including on panic, and is retried; if it still fails, anyprof aborts the profile and says a thread may still be suspended. The sampler does not call `SetThreadContext` or `WriteProcessMemory`. Sampling anyprof's own pid is refused before `OpenProcess`. A same-user, same-integrity process does not need an administrator token. Another user's process, or a higher-integrity process, needs an elevated token with `SeDebugPrivilege`. anyprof tries to enable that privilege once after access is denied and does not otherwise change the token. A protected process (`GetProcessInformation` protection level other than `PROTECTION_LEVEL_NONE`) stays inaccessible: `SeDebugPrivilege` does not open it, and anyprof will not change that protection. `IsWow64Process2` refuses a 32-bit process and an ARM64 process. `IsWow64Process` alone cannot see ARM64, so it is not the check. An x64 process emulated on an ARM64 host reports AMD64 and is not refused. A program counter in kernel space is dropped; later user addresses in that walk are kept. `-k` is ignored. Symbols come from dbghelp (`SymFromAddr`) reading debug info next to the image. dbghelp is not pointed at a symbol server, and `_NT_SYMBOL_PATH` is ignored.

Linux x86_64 native sampling calls `perf_event_open` with `PERF_COUNT_SW_TASK_CLOCK`. That counter ticks only while the thread is scheduled, so the samples are on-CPU and the target is not suspended. Sampling anyprof's own pid is allowed. Each thread gets its own ring buffer; a thread that cannot map one is skipped and named in a warning when at least one thread was opened. User stacks are the frame-pointer walk of `PERF_SAMPLE_STACK_USER`, with the kernel's user call chain used when that walk has fewer than two frames. A non-leaf address loses one byte before symbol lookup. The binary has to keep frame pointers. `-fno-omit-frame-pointer` keeps non-leaf frames. At `-O2`, GCC and Clang both omit the leaf frame pointer unless `-mno-omit-leaf-frame-pointer` is also set, so `spin` then has no `push %rbp`. The CI fixtures are compiled at `-O0`, where GCC still emits that leaf frame. ELF symbols are `SHT_SYMTAB`, then `.dynsym`, with Itanium C++ names demangled. The module name is the file name from `/proc/<pid>/maps`.

What Linux allows depends on sysctls anyprof only reads:

- `kernel.perf_event_paranoid` of 2 or lower: user-space samples of a same-user process, including this process and a process that is not a child, without root. This is enough for the busy and idle fixtures.
- `kernel.perf_event_paranoid` of 3 or 4: `perf_event_open` fails for an unprivileged process, including its own children. Ubuntu runners often ship 4. `sudo sysctl kernel.perf_event_paranoid=1`, or root / `CAP_PERFMON` / `CAP_SYS_ADMIN`, allows per-process sampling. anyprof does not change the sysctl.
- `-k` also asks for kernel samples. That needs paranoid of 1 or lower, or root (`CAP_PERFMON` or `CAP_SYS_ADMIN`). With `kernel.kptr_restrict` above 0, `/proc/kallsyms` hides kernel addresses and those frames stay unnamed. CI sets paranoid to 1 and `kptr_restrict` to 0, then samples a thread in a tight `read` of `/dev/zero`. That read stays on CPU. It is not a blocking read. Without `-k`, the event excludes kernel samples, so a thread on-CPU inside the kernel contributes no samples.
- `kernel.yama.ptrace_scope` limits `ptrace` attach. Scope 1 (Ubuntu's default, including the `ubuntu-latest` image) allows attach to descendants. Scope 2 needs `CAP_SYS_PTRACE`. Scope 3 disables attach. `perf_event_open` asks for `PTRACE_MODE_READ_REALCREDS` and does not set the attach bit, so that sysctl does not decide whether a same-user process can be sampled. CI at scope 1 samples a double-forked same-user process. A process owned by another uid still needs root or `CAP_SYS_PTRACE`. pid 1 fails with `EACCES` for an unprivileged user. The error names the errno, the paranoid value, and the ptrace scope that were read.

Linux builds that are not x86_64 compile and `start` returns not implemented. Those builds do not set `kernel_frames`, so `-k` is not advertised. The CLI only profiles the pid you pass it.

Rust sampling is that native sampler. `anyprof-rust` does not open a second event source. It detects a Rust executable, asks the native sampler to record, and the symbol reader demangles Rust names. A stack mixes `[rust]` frames with bare C frames (`main`, libc) and, on Linux x86_64 with `-k`, `[kernel]` frames. The default is 99 Hz pprof, and an omitted `-o` writes `anyprof-rust-<pid>-YYYYMMDD-HHMMSS.pb.gz`.

Detection does not attach. It reads the executable and stops at the first marker. The last read is shortened so the total does not pass 32 MiB. On Linux the path is `/proc/<pid>/exe`. On macOS it is `proc_pidpath` (same user, no root). On Windows it is `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)` plus `QueryFullProcessImageNameW`, then the handle is closed. That is not the sampler's `OpenProcess`, and it does not enable `SeDebugPrivilege`. A missing path or a read error is "not Rust", and auto-detection falls through to native. `--backend rust` still samples a process that was not detected.

The markers are the ELF `.comment` string `rustc version `, the standard-library panic text for `Option::unwrap` and `Result::unwrap`, the prefix `fatal runtime error:`, and a remapped panic path `/rustc/` plus 40 hex digits plus `/library/`. Executables do not carry the `.rustc` section; that section is for dylibs and proc-macros. On ELF, `rustc version ` is in `.comment` and survives `strip --strip-all`, including a `no_std` binary. Cross-compiled Mach-O and PE objects do not contain that string. A normal Rust binary on those platforms is recognized from the standard-library strings, which live in the binary and survive strip. A fully stripped `no_std` Mach-O or PE that contains none of the markers is not auto-detected. A non-Rust file that happens to contain one of those byte strings in the first 32 MiB is detected as Rust; the same class of false positive exists for the Go build-info scan.

Names: legacy Rust mangling (`_ZN…17h<16 hex>E`) and v0 (`_R…`) are demangled with `rustc-demangle` before Itanium C++ demangling, and the legacy crate hash is omitted. Mach-O adds one extra leading underscore (`__R…`, `__ZN…`); ELF and PE do not. A C++ Itanium name demangles to `ns::fn()` and stays native even when the file is a Rust image. An MSVC `?` name stays native. On Windows, an already-undecorated `crate::function` with no parameter list is labeled Rust when the file is a Rust image, including a rustc segment such as `{closure#0}` or `{impl#0}`. An already-undecorated generic (`foo::<T>`) or trait path (`<T as Trait>`) stays native in that fallback, so a C++ template is not relabeled; a mangled closure or generic is still demangled and labeled Rust. A C symbol in that image (`main`) has no `::` and stays native. dbghelp is given the executable's directory and is not pointed at a symbol server. When `SymFromAddr` returns no name, anyprof reads the PDB named by the PE CodeView record, then the `.pdb` beside the image. The debug directory is read from its file offset, including past the first 1 MiB. A relative CodeView path is used only when it stays inside the executable's directory (`..` is ignored). An absolute path is still opened. Either file is applied only when its GUID matches that record, its PDB-info age is at least the image age, and its DBI age equals the image age when the PDB has one. A stale PDB is skipped and the address stays unnamed. With no CodeView record, the sibling `.pdb` is still tried and cannot be checked. A PDB larger than 1 GiB is not parsed. Put the matching PDB next to the exe (`-C debuginfo=2`); without it the frames keep the address and the module name.

Frame pointers are required on macOS and Linux. `rustc` 1.99 does not emit them at `-O0` or `-O2` unless the binary was built with `-C force-frame-pointers=yes`. Without that flag the leaf is recorded and the callers are not. Windows does not need the flag: `StackWalk64` uses the unwind data rustc emits. The CI fixtures pass the flag on every OS so the program is the same, and they pass `-C debuginfo=2` for the PDB. Permissions, self-pid refusal, `-k`, and the not-implemented architectures are the native ones above. Linux musl x86_64 is the same sampler; CI `cargo check`s it and does not live-sample it.

Go sampling is that same on-CPU source, with a different unwind. Go 1.21+ can emit frame pointers, but a leaf such as `for {}` often does not, and the return PC is not stored in a saved frame-pointer slot. anyprof reads the Go 1.18 / 1.20 pclntab (`0xFFFFFFF0` / `0xFFFFFFF1`) from `.gopclntab`, `__gopclntab`, or, on PE, from `.rdata` after the build-info magic is present. Go 1.18 through 1.25 store `runtime.text` in the header. Go 1.26 keeps magic `0xFFFFFFF1` and stores that word as zero; the base is `moduledata.text`, in `.go.module` / `__go_module` or, on PE, in `.data`. A cgo binary’s `runtime.text` is inside the text section rather than at its first byte, and the section address is not used in its place. An externally linked Mach-O leaves the Go 1.22 header word as an addend from the `__TEXT` vmaddr; the link address is that sum when it falls inside `__text`. A value already inside the text section is kept, which is the internal Mach-O link, ELF, PE, and Go 1.26’s `moduledata.text`. pcsp gives the stack delta; on amd64 the return address is at `SP + pcsp`, and on arm64 it is in LR until the frame saves it at SP. That is the running goroutine. A goroutine parked in `time.Sleep` or on a channel is not on that stack and is not invented as an on-CPU sample. A thread executing the scheduler or a syscall shows those runtime frames, which are still `[go]` when pclntab names them. Inlined callees are not separate frames. cgo stops at the Go function that entered C, plus the return PC; the C frames are not in pclntab, and a leaf that is inside C uses the native walk. One pclntab, from the main executable. A plugin’s text is not a second walk. Names survive `-ldflags=-s -w`. CI builds the fixtures that way with Go 1.22 and Go 1.26, `CGO_ENABLED=0`. Detection does not attach: it scans for `\xff Go buildinf:` on Linux, macOS, and Windows, using the same path queries as Rust. A Go older than 1.18 is still detected and then sampled with the native walk, because this parser does not read the 1.2 or 1.16 pclntab. A 32-bit pclntab is the same fallback. The Linux user-stack copy is the same one the native sampler requests (8 KiB, smaller if the kernel refuses). A return slot past that copy ends the pcsp walk. Permissions, self-pid refusal, `-k`, and the not-implemented architectures are the native ones above.

## Roadmap

1. **Node.js** via the V8 inspector, on macOS, then Windows, then Linux. This is implemented.
2. **C++ (native)** on-CPU sampling, and **browser pages** over the DevTools Protocol. Browser pages are implemented. Native sampling is implemented on macOS (Mach), Windows x64 (`StackWalk64`), and Linux x86_64 (perf events). Other architectures are not implemented.
3. **Rust** and **Go** on-CPU sampling are implemented on the same platforms as native: macOS (Apple Silicon and Intel), Windows x64, and Linux x86_64. Rust reuses the native frame-pointer and unwind walk and demangles symbols. Go reuses the on-CPU events and unwinds with the Go 1.18+ pclntab, including Go 1.26’s zero `textStart`. Other architectures compile and `start` returns not implemented. **Python** is next, then Java, Ruby, PHP, and .NET. Those scripting backends should default to speedscope (see the format table above). Go already defaults to pprof.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace
```

CI runs those checks on macOS, Windows, and Ubuntu, in that order, with Node.js 22 installed so the live Node profile test runs. CI also sets `ANYPROF_REQUIRE_CHROME=1`, so `crates/anyprof-cli/tests/web_smoke.rs` must find Chrome. That test drives headless Chrome over the DevTools Protocol, opens a `.cpuprofile`, a pprof `.pb.gz`, collapsed stacks, and speedscope JSON in `anyprof view`, and checks that the viewer served the profile, that speedscope used the matching importer, that the page logged no exceptions or CSP violations, and that the screenshot contains a flame graph. Locally it skips when Chrome is not found. `ANYPROF_CHROME=<path>` picks the browser, and `ANYPROF_SMOKE_ARTIFACTS=<dir>` keeps the screenshots; CI uploads them as the `web-smoke-<os>` artifact.

CI sets `ANYPROF_REQUIRE_NATIVE=1` on every OS in the test matrix, so a skipped native, Rust, or Go live test is a failure there. The Rust fixtures are compiled with `rustc --edition 2024 -C opt-level=0 -C force-frame-pointers=yes -C debuginfo=2` and sampled by the same jobs. The Go fixtures are compiled with Go 1.22 and again with Go 1.26, `CGO_ENABLED=0`, and `-ldflags=-s -w`. The build is `go build -o <binary> .` in the canonical module directory. On Linux and Windows they take the same `anyprof-native-live.lock` as the C++ tests, so perf and dbghelp are not used by two crates at once.

On `macos-latest` (arm64) and `macos-15-intel` (x86_64), `crates/anyprof-native/tests/live.rs` compiles a busy C++ program, re-executes the test under `sudo`, and checks that the Mach sampler records `anyprof_native::spin`, a caller, a sample count and duration in range for 100 Hz over 1s, and that collapsed stacks, pprof, and speedscope all still contain that symbol. A second live test compiles a process that only calls `pause()` and checks that a 500ms recording does not invent on-CPU samples. The CLI test writes the auto-named `.pb.gz` and converts it. `crates/anyprof-rust/tests/live.rs` does the same for a Rust fixture (`anyprof_rust_spin`, an idle park, and two running threads) and checks the frames are `Runtime::Rust` and demangled. `crates/anyprof-go/tests/live.rs` does the same for a stripped Go fixture (`main.spin` and `main.caller`, a goroutine blocked in `time.Sleep`, and two goroutines on separate OS threads) and checks the frames are `Runtime::Go` with a `.go` file and line. Those tests live in `mod supported`, so the sudo re-exec passes `supported::<test> --exact` and requires `running 1 test`. The Rust CLI test checks auto-detection (`using backend 'rust'`) and `anyprof-rust-<pid>-*.pb.gz`. The Go CLI test checks `using backend 'go'` and `anyprof-go-<pid>-*.pb.gz`. Without sudo locally the live tests skip unless `ANYPROF_REQUIRE_NATIVE=1`, in which case a missing sudo is a failure. A green arm64 job is not Intel coverage.

On `ubuntu-latest` the workflow sets `kernel.perf_event_paranoid=1` and `kernel.kptr_restrict=0` before the tests. It does not change `kernel.yama.ptrace_scope` (the image leaves it at 1). `crates/anyprof-native/tests/linux_live.rs` then samples a busy C++ program, an idle `pause()` loop, two running threads, and an unrelated same-user process, checks that this process can be sampled, checks that pid 1 is denied with a message that names `perf_event_open`, and checks that `-k` on a tight `/dev/zero` `read` records named kernel frames. That loop stays on CPU. The CLI test writes the auto-named `.pb.gz`. `crates/anyprof-rust/tests/live.rs` samples a busy Rust program, an idle `park_timeout`, and two running threads, and checks demangled `[rust]` frames. The Rust CLI test checks auto-detection. The Ubuntu job also `cargo check`s `x86_64-unknown-linux-musl`; that check does not live-sample. A machine where `perf_event_open` is refused skips those tests unless `ANYPROF_REQUIRE_NATIVE=1`. The MSRV job does not set that variable and does not change sysctls, so on a runner with paranoid 4 the live tests skip. That job is not the Linux sampling evidence. aarch64 Linux is not a CI target.

On `windows-latest`, `crates/anyprof-native/tests/windows_live.rs` compiles a busy C++ program and an idle `Sleep` program with `clang++` or `cl` (`vcvars64.bat`), samples them as the same user, checks the busy program kept running, and checks that sampling this process is refused before `OpenProcess`. The CLI test writes the auto-named `.pb.gz`. `crates/anyprof-rust/tests/live.rs` and `crates/anyprof-cli/tests/rust_live.rs` compile the Rust fixtures with `rustc` (`-C debuginfo=2`, which writes a PDB next to the exe) and require demangled `[rust]` frames, including `anyprof_rust_fixture::anyprof_rust_spin`, not only raw addresses. No administrator token is required for those fixtures. A machine with no C++ compiler skips the C++ tests unless `ANYPROF_REQUIRE_NATIVE=1`. The Rust tests need `rustc`, which the toolchain step installs.

`rust-toolchain.toml` pins Rust 1.99.1, which is the compiler CI uses. The MSRV is Rust 1.88 (`rust-version` in `Cargo.toml`); the `msrv` CI job builds and tests the whole workspace on it. The workspace uses Rust edition 2024.

### Cutting a release

1. Bump `version` under `[workspace.package]` in `Cargo.toml`. Every crate inherits it.
2. Commit that change on `main` (the release workflow has to be on the default branch).
3. Tag that commit `vX.Y.Z`. The `v` prefix and the version must match the workspace version. Version `0.1.6` is tag `v0.1.6`.
4. Push the tag: `git push origin v0.1.6`.

Pushing the tag runs `.github/workflows/release.yml`. [cargo-dist](https://axodotdev.github.io/cargo-dist/) builds `anyprof` for Apple Silicon macOS, Intel macOS, 64-bit Windows, and 64-bit Linux (glibc and musl, x86_64 and ARM), then uploads the archives, checksums, shell installer, and PowerShell installer to a GitHub Release.

The live test spawns `node --inspect-port=127.0.0.1:<port>` without telling the backend that port, so port discovery and inspector activation both run. It skips when `node` is not on `PATH`.

| Path | Role |
| --- | --- |
| `crates/anyprof-core` | `Backend`, `Session`, `Profile`, process inspection |
| `crates/anyprof-cdp` | DevTools Protocol client shared by Node and browser pages |
| `crates/anyprof-node` | Node.js backend |
| `crates/anyprof-browser` | Browser-tab backend |
| `crates/anyprof-native` | Native backend. macOS Mach, Windows x64 `StackWalk64`, Linux x86_64 perf events. Also demangles Rust symbols |
| `crates/anyprof-rust` | Rust backend. Detection and the `rust` name; sampling is the native sampler |
| `crates/anyprof-go` | Go backend (pclntab unwind on the native sampler) |
| `crates/anyprof-export` | collapsed, speedscope, pprof, and `.cpuprofile` |
| `crates/anyprof-web` | `anyprof view`: localhost server, embedded speedscope, browser opener |
| `crates/anyprof-cli` | the `anyprof` binary |
| `dist-workspace.toml` | cargo-dist release config |

Linux process inspection reads `/proc`. On other Unix systems, detection uses `ps` for the process name and command line. On Windows it uses `tasklist`. Listening-port discovery uses `/proc/<pid>/net/tcp` (filtered by socket inode), `lsof` on macOS, and `netstat` on Windows.

## License

Apache-2.0. See [LICENSE](LICENSE).

### Third-party licenses

The `anyprof` binary embeds third-party files for `anyprof view`:

- [speedscope](https://github.com/jlfwong/speedscope) 1.25.0, MIT License, copyright (c) 2018 Jamie Wong: [`LICENSE-speedscope.txt`](crates/anyprof-web/vendor/licenses/LICENSE-speedscope.txt).
- Source Code Pro (the font speedscope uses), SIL Open Font License 1.1, copyright 2010-2019 Adobe: [`LICENSE-source-code-pro.md`](crates/anyprof-web/vendor/licenses/LICENSE-source-code-pro.md).

Both license files are included in every release archive next to `LICENSE`.
