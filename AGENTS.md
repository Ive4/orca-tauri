# orca-tauri implementation rules

This is a Rust/Tauri v2 tray shell around an independently versioned official
Orca headless core. Develop and qualify Apple Silicon macOS first, then Windows
11 x64. See the platform order in README.md when changing packaging or running
release validation.

## Preserve these invariants

- Cold startup creates no WebView. Closing/toggling a window destroys it; never
  replace this with a hidden always-alive window.
- Launch the pinned standalone runtime directly with `orcad-server.js`.
  Package Bun by default on macOS, with a Node-only build option. Windows uses Node.
  Do not switch to the official Electron launcher or enable its fallback.
- Windows process ownership starts **inside CreateProcess** using
  `PROC_THREAD_ATTRIBUTE_JOB_LIST`. Keep `KILL_ON_JOB_CLOSE`, no breakaway,
  and a non-inherited Job handle. Never kill global processes by name.
- Startup/stop failures retain ownership until cleanup completes. Waiting for
  actual tree termination is required before reusing the port for rollback.
- Readiness comes from owned stdout: PID, loopback endpoint, runtime slot
  version, PTY self-test. A desktop Release tag is not the runtime slot version.
- Never log pairing fragments or put them in status, notifications or IPC.
  The remote Orca page receives no privileged Tauri capabilities.
- Candidate downloads and extraction do not change the active core. Commit the
  pointer only after a candidate starts and validates; never overwrite a running
  version directory. Binary rollback does not undo user database migrations.
- Preserve localhost proxy bypass. Do not silently change the user's global
  proxy or terminate an unknown listener using the selected port.
- Browser pairing compatibility is based on upstream v1.4.264. Check the actual
  storage/identity contract when qualifying later upstream versions.

## Validation

```text
cargo test --locked --no-default-features
cargo clippy --locked --no-default-features --all-targets -- -D warnings
node scripts/test-pairing-init.mjs
cargo fmt --all --check
cargo check --locked --all-targets
```

The last command needs native Tauri development prerequisites. For local Mac
packaging, run `scripts/build-macos.sh` using the official app pinned in
`core-version.macos-arm64.json`. Windows packaging uses
`scripts/extract-headless.ps1` and requires its runtime qualification.

Report the platform and scope of each check. Core tests, compile checks,
installer audits and native runtime/UI/memory tests provide different evidence.
Do not call the shell a measured 10 MB application without actual measurements.
