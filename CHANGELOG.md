# Changelog

## 0.1.3 - 2026-10-22

- `anyprof view <file>` (alias `v`) opens a `.cpuprofile`, pprof, speedscope JSON, or collapsed-stacks file in speedscope 1.25.0, which is built into the binary and served on `127.0.0.1` under a random 128-bit token path. Nothing is downloaded. `record -w` / `--web` writes the profile as usual, then opens it the same way. `--no-open` prints the URL instead, `--port` picks the port, and `--web-timeout` stops the viewer after a while; otherwise it serves until Ctrl+C. Over SSH or without a display, anyprof prints the URL and an `ssh -L` port-forward command instead of opening a browser. The binary is about 0.8 MiB larger. Release archives include the speedscope (MIT) and Source Code Pro (OFL-1.1) licenses.
- `-F` above 1000000 Hz is rejected with a clear message. It used to print a `0us` sampling banner while V8 sampled every 1us.
- The idle warning shows its percentage with one decimal and never rounds a share above 95% down to `95%`.
- Node's `-v` log reports the V8 sample count in `samples`, not the total of microsecond weights.
- The minimum supported Rust version is 1.88, up from 1.85. The `anyprof update` dependencies (axoupdater and the crates it pulls in) need 1.88, so 0.1.2 did not build on 1.85. A new `msrv` CI job builds and tests the whole workspace on Rust 1.88.
- `record --help` says `-F` accepts 1..=1000000 Hz.
- A second Ctrl+C while the viewer is running exits immediately, even when a response write is stuck on a client that stopped reading. The first Ctrl+C still stops the viewer cleanly, the same as in `record`.
- The headless Chrome smoke test saves a screenshot and a text report (page state, console, DevTools log, DOM, viewer output) before it fails on a load timeout.

## 0.1.2

- Node and browser recordings sample every 100us (10 kHz) by default, the same rate as a Chrome DevTools Performance recording. The old 99 Hz default (a 10101us V8 interval) took about 80 times fewer samples and missed short request handling on a mostly idle server. Go and native keep 99 Hz. An explicit `-F` still wins, and `record` prints the rate it uses.
- `record --interval <DURATION>` sets the time between samples (`100us`, `1ms`, ...) as an alternative to `-F`. The two cannot be combined.
- `record` warns on stderr when more than 95% of samples are `(idle)` or `(program)` and fewer than 100 samples caught JavaScript, native code, or GC, and suggests recording while the process is under load. The zero-sample warning is unchanged. The final `wrote N samples` line counts V8 samples; for `.cpuprofile` recordings it used to print microseconds.
- Negative V8 `timeDeltas` no longer shift later samples onto the wrong weights when importing a `.cpuprofile`.
- `anyprof update` only installs releases from `github.com/anyprofiler/anyprof`. It refuses a receipt that names another repository and ignores the installer's host override variables and `ANYPROF_CLI_INSTALL_DIR`.
- `anyprof update` says when this binary is newer than the latest release, instead of calling it the latest.
- On Windows, leaving the old `anyprof.exe.previous.exe` behind is a warning, not a failure, since the new binary is already in place. The warning names the path from before the rename. Once the installer starts, Ctrl+C waits for it to exit so a failed install can restore the old `anyprof.exe`.
- Clearer `anyprof update` errors: install-location lookups are no longer reported as failed replacements, and the reinstall hint labels the macOS/Linux and Windows commands.
- `uptate` still works but no longer appears in `--help`.

## 0.1.1

- Node and browser recordings write the original V8 `.cpuprofile` from `Profiler.stop`, so Chrome DevTools shows a real profile. `convert` rebuilds a DevTools-valid tree (a `(root)` node, `scriptId`, linked children, and `timeDeltas` the same length as `samples`, with `hitCount` omitted). Before sampling, anyprof reads `process.pid` and refuses an inspector that belongs to a different process.
- `-o` is optional. Omitting it, or passing a directory, `.`, `..`, or a trailing separator, writes an auto-named file (`anyprof-<backend>-<pid-or-tab>-YYYYMMDD-HHMMSS.<ext>`). A leading `~` expands to the home directory. Each backend defaults to its ecosystem format: cpuprofile for Node and browser, pprof for Go and native. An explicit `--format` or output-file extension still wins.
- Short flags (`rec -p -d -f -F -b -i -t -B -k`, `conv -i -o -f -t`) and the `rec` / `conv` aliases. `-d` has no default. Ctrl+C, SIGTERM, and Windows Ctrl+Break end sampling and still save the profile. A second interrupt while the profile is being saved exits immediately.
- `anyprof update` upgrades a release-installer install to the latest GitHub Release. `upgrade` and `uptate` are aliases. If this copy is already current, the command says so and exits 0.

## 0.1.0

- First release. Profile a running Node.js process or a browser tab and write collapsed stacks, speedscope, pprof, or `.cpuprofile`.
