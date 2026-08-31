# Troubleshooting

## The resident exits immediately

Check that the build is 64-bit Windows and launch it from a terminal once to see a local error. Common causes are a second resident instance, an unavailable UI Automation/OCR capability, or an invalid configuration. The resident does not download a model or silently install a dependency.

## The popup never appears

Confirm that the target control exposes text, that the selection is a drag/double-click rather than a normal click, and that the text is not whitespace or zero-width formatting only. Automatic extraction failures are intentionally silent. Press `Ctrl+Alt+T` to try Manual extraction.

## “API key is not configured”

Set `OPENAI_API_KEY` or `SELECTION_TRANSLATE_OPENAI_API_KEY` for the resident process, or create a Windows Credential Manager Generic Credential using the exact target `SelectionTranslate/OpenAI`. Keep the key out of `config.toml` and command-line arguments.

## “Provider configuration is invalid” or an offline error

Validate the TOML, endpoint, and model. The default endpoint is HTTPS. Plain HTTP is allowed only for loopback addresses used by local mocks. DNS, TLS, timeout, HTTP, rate-limit, malformed response, oversized/incomplete response, and cancellation failures are classified as local popup errors without logging request bodies or credentials.

## “Provider connection failed”

This is a WinHTTP transport failure, not an API-key rejection. A rejected key is reported separately as “Provider authentication failed.” On the affected machine, replace `HOST` below with only the hostname from the configured endpoint and run:

```powershell
Test-NetConnection HOST -Port 443
netsh winhttp show proxy
```

If `TcpTestSucceeded` is false, the machine, firewall, VPN, or network cannot reach the provider. If TCP succeeds, compare the WinHTTP proxy output with a machine where the provider works. Do not paste the API key into these commands or into a bug report.

The endpoint setting is an OpenAI-compatible base URL. Both a host-only URL and a versioned base URL ending in `/v1` are accepted; the resident appends the chat-completions route exactly once.

## OCR does not find the text

Install/enable the relevant Windows OCR language and ensure the target is visible at the time of capture. Selection OCR uses a rectangle around the selection; Hover/Manual OCR use an in-memory region around the pointer. High-DPI monitors are handled using the process's per-monitor DPI awareness, but unusual scaling or protected surfaces may still prevent capture.

## The package script fails

Run it from the repository root and verify these files exist:

```text
D:\DevTools\cargo\bin\cargo.exe
D:\Program Files\Microsoft Visual Studio\18\Community\Common7\Tools\VsDevCmd.bat
```

The script intentionally writes only under `windows\target`, `windows\tmp`, and the selected `windows\dist` package directory; it does not install or publish anything. It runs all verification checks and the locked release build every time. A non-empty output directory is rejected to prevent stale files from being mixed into a package; select a new directory rather than deleting the old one automatically.

## History is empty

Only completed, non-empty results are saved. Open the manager's History view to load the database. Failed, cancelled, stale, partial, and cache-invalidated jobs are intentionally absent. A corrupt or inaccessible database is shown as a local manager error and does not block the resident's extraction pipeline.

## The manager does not appear

The resident enforces one instance and forwards an Open Manager request to the existing instance. Check the notification area and close an old resident before retrying. The manager is on-demand and should consume no manager memory while closed.
