# Changelog

## 0.1.1

- Node and browser recordings write the original V8 `.cpuprofile` from `Profiler.stop`, so Chrome DevTools shows a real profile. `convert` rebuilds a DevTools-valid tree (a `(root)` node, `scriptId`, linked children, and `timeDeltas` the same length as `samples`, with `hitCount` omitted). Before sampling, anyprof reads `process.pid` and refuses an inspector that belongs to a different process.
- `-o` is optional. Omitting it, or passing a directory, `.`, `..`, or a trailing separator, writes an auto-named file (`anyprof-<backend>-<pid-or-tab>-YYYYMMDD-HHMMSS.<ext>`). A leading `~` expands to the home directory. Each backend defaults to its ecosystem format: cpuprofile for Node and browser, pprof for Go and native. An explicit `--format` or output-file extension still wins.
- Short flags (`rec -p -d -f -F -b -i -t -B -k`, `conv -i -o -f -t`) and the `rec` / `conv` aliases. `-d` has no default. Ctrl+C, SIGTERM, and Windows Ctrl+Break end sampling and still save the profile. A second interrupt while the profile is being saved exits immediately.
- `anyprof update` upgrades a release-installer install to the latest GitHub Release. `upgrade` and `uptate` are aliases. If this copy is already current, the command says so and exits 0.

## 0.1.0

- First release. Profile a running Node.js process or a browser tab and write collapsed stacks, speedscope, pprof, or `.cpuprofile`.
