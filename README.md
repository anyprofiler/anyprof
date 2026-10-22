# anyprof

Capture CPU profiles from any running process, no code changes or restarts needed.

anyprof attaches to a live process by pid and writes a CPU profile. The process does not need a profiler baked in, and it does not need to be restarted. Runtimes sit behind one `Backend` trait, and every backend emits the same profile.

**Status:** Node.js sampling works. Give anyprof the pid of a running `node` process; it turns the V8 inspector on, records a CPU profile, and writes collapsed stacks, speedscope, pprof, or `.cpuprofile`. `anyprof view` opens any of those in a built-in, offline speedscope viewer. Browser tabs work the same way when Chrome, Chromium, or Edge is already listening with `--remote-debugging-port`. Native C/C++ sampling and Go sampling are still stubs.

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
| `-k, --kernel` | Ask for kernel frames when the backend supports them. Node and browser profiles do not include kernel frames. |
| `-b, --backend <node\|browser\|go\|native>` | Skip auto-detection. |
| `-w, --web` | After writing the file, open it in the built-in viewer, the same as `anyprof view <file>`. `--no-open`, `--port`, and `--web-timeout` (see [Viewing profiles](#viewing-profiles)) require `--web`. |
| `-v`, `-vv`, `-vvv` | Log more (`info`, `debug`, `trace`). `RUST_LOG` is used when `-v` is not passed. `-h` and `-V` are help and version. |

Default sampling rate when neither `-F` nor `--interval` is passed:

| Backend | Default interval | Rate |
| --- | --- | --- |
| `node`, `browser` | 100us | 10000 Hz, the same as a Chrome DevTools Performance recording |
| `go`, `native` | 10101us | 99 Hz, the `perf record -F 99` convention |

At 100us a mostly idle server still records the sub-millisecond work of each request. A 10s recording holds tens of thousands of samples, and a busy process writes about 2.5 MB of `.cpuprofile` per minute. For long recordings, `--interval 1ms` keeps the file 10 times smaller. The start line names the rate in use, for example `using backend 'node' for pid 1234 (on-cpu, mixed-stacks), sampling every 100us (10000 Hz)`.

After recording, anyprof warns on stderr when the profile has 0 samples. It also warns when more than 95% of samples are V8's `(idle)` or `(program)` nodes and fewer than 100 samples caught JavaScript, native code, or the garbage collector. That means the process was waiting and the profile shows almost none of its work. Record again while the process is under load, for example by sending it requests during the recording. A mostly idle server that still handled some requests, such as 300 JavaScript samples out of 90,000, does not warn. The profile is always written.

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

The viewer serves until Ctrl+C (or SIGTERM, or Ctrl+Break on Windows), then exits 0. The browser tab keeps the profile it already loaded.

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

CI runs those checks on macOS, Windows, and Ubuntu, in that order, with Node.js 22 installed so the live Node profile test runs. CI also sets `ANYPROF_REQUIRE_CHROME=1`, so `crates/anyprof-cli/tests/web_smoke.rs` must find Chrome. That test drives headless Chrome over the DevTools Protocol, opens a `.cpuprofile`, a pprof `.pb.gz`, collapsed stacks, and speedscope JSON in `anyprof view`, and checks that the viewer served the profile, that speedscope used the matching importer, that the page logged no exceptions or CSP violations, and that the screenshot contains a flame graph. Locally it skips when Chrome is not found. `ANYPROF_CHROME=<path>` picks the browser, and `ANYPROF_SMOKE_ARTIFACTS=<dir>` keeps the screenshots; CI uploads them as the `web-smoke-<os>` artifact. `rust-toolchain.toml` pins Rust 1.99.1, which is the compiler CI uses. The MSRV is Rust 1.88 (`rust-version` in `Cargo.toml`); the `msrv` CI job builds and tests the whole workspace on it. The workspace uses Rust edition 2024.

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
