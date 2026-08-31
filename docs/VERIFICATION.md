# Verification status

This file records automated checks and the remaining manual release gates. It does not claim that a GUI or memory test happened merely because the code exists.

## Automated checks

Run from the repository root in an x64 MSVC environment, or run the package script with a fresh output directory:

```powershell
cmd /c 'call "D:\Program Files\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat" -arch=x64 && "D:\DevTools\cargo\bin\cargo.exe" fmt --all -- --check'
cmd /c 'call "D:\Program Files\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat" -arch=x64 && "D:\DevTools\cargo\bin\cargo.exe" test --workspace --locked'
cmd /c 'call "D:\Program Files\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat" -arch=x64 && "D:\DevTools\cargo\bin\cargo.exe" clippy --workspace --all-targets --locked -- -D warnings'
cmd /c 'call "D:\Program Files\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat" -arch=x64 && "D:\DevTools\cargo\bin\cargo.exe" build --locked --release -p selection-translate-resident -p selection-translate-manager'
.\windows\scripts\package-release.ps1 -OutputDirectory windows/dist/selection-translate-x64-YYYYMMDD
```

The package script performs the same checks before staging, copies both executables and user documentation, and writes `BUILD-INFO.txt` containing the UTC time, Cargo.lock SHA-256, and Git commit when one exists. Keep generated `windows\target`, `windows\tmp`, and `windows\dist` files out of source control.

## Implemented automated coverage

Core request admission, prompt/config validation, trigger priority, cache identity, provider parsing/error limits, UI Automation sentence offsets, clipboard restoration guards, OCR offsets, SQLite schema/search/pruning, manager prompt/history helpers, and resident history admission have unit tests. The current workspace test and Clippy results should be recorded from the final merged worktree, not inferred from an earlier agent run.

## Manual release gates still required

- Exercise Selection, Manual, and enabled Hover in Chromium, Firefox, Office, VS Code, a native control, a terminal, and a PDF reader.
- Confirm fallback order and stop-on-first-valid-text behavior, whole-sentence context for a hovered word, clipboard restoration, OCR at 100/150/200% DPI and across two monitors, protected/blank surfaces, cancellation, stale completion suppression, offline/typed provider errors, and no request/loading/history row for no-text events.
- Open and close the native manager repeatedly; verify Settings credential actions, prompt editing/reload, History lazy loading/search/filter/order/copy/delete confirmation, and database error handling.
- Warm the resident with one local mock request, close the popup and manager, ensure SQLite is closed, and run `windows\scripts\measure-memory.ps1 -DurationSeconds 300`. Acceptance is private working set below 20 MiB and average CPU at most 0.1%.

Until those checks are performed on the final release binary, this remains an unsigned local test build rather than a release claim.
