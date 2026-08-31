# Selection Translate

Selection Translate is a lightweight native Windows x64 assistant for selected or hovered text. It obtains sentence context when the target application exposes it, sends the chosen prompt to an OpenAI-compatible API, and streams Markdown into a compact popup. The resident uses native Windows UI instead of Electron or WebView, and the manager runs only when opened.

This repository currently provides an unsigned preview build. Selection, Manual, and opt-in Hover share one priority-aware extraction pipeline. UI Automation is tried first and clipboard or Windows OCR is used only when applicable. No provider request is made when no valid text is detected.

## Install the preview

1. Download the Windows x64 ZIP from the GitHub release and extract it to a writable directory.
2. Run `selection-translate-manager.exe` to configure the provider and prompts.
3. Run `selection-translate-resident.exe`. Its icon appears in the notification area.

Windows SmartScreen may warn because the binaries are not code-signed. The package is portable: it does not install a service or write files beside the executables. User configuration and history are stored under `%LOCALAPPDATA%\SelectionTranslate`.

## Configure an API provider

Selection Translate expects an OpenAI-compatible Chat Completions API.

1. Open the manager's **Settings** tab.
2. Enter the provider base URL. A host-only URL or a versioned URL ending in `/v1` is accepted. Do not append `/chat/completions`; the application adds that route exactly once.
3. Enter the provider's exact model identifier.
4. Leave the credential target as `SelectionTranslate/OpenAI` unless you intentionally use a different Generic Credential name.
5. Paste the API key into **API key**, choose **Save key**, and then choose **Save settings**.
6. Select the default Selection and Hover prompt profiles from their dropdowns.
7. Start or restart the resident after changing the credential source.

The manager stores a saved key as a Windows Credential Manager Generic Credential. The key is never written to `config.toml`, history, logs, or release files.

### Environment-variable alternative

The resident checks credentials in this order:

1. `OPENAI_API_KEY`
2. `SELECTION_TRANSLATE_OPENAI_API_KEY`
3. the configured Windows Credential Manager target

To provide a key only to one PowerShell-launched resident process without putting it in the command itself or saving it permanently:

```powershell
$secureKey = Read-Host "API key" -AsSecureString
$keyPointer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secureKey)
try {
    $env:OPENAI_API_KEY = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($keyPointer)
    .\selection-translate-resident.exe
} finally {
    [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($keyPointer)
    Remove-Item Env:OPENAI_API_KEY -ErrorAction SilentlyContinue
}
```

Do not put an API key in `config.toml`, a shortcut argument, a Git commit, or a bug report.

### Configure without the manager

Copy `config.example.toml` from the extracted package to:

```text
%LOCALAPPDATA%\SelectionTranslate\config.toml
```

Edit the endpoint, model, defaults, and prompt profiles, but keep the API key outside this file. Prompt templates may use `{target}`, `{context}`, and `{source}`; every template must contain `{target}`.

## Prompt profiles

Open **Prompts** in the manager to select, create, or edit profiles. A profile has a stable ID, a short chooser name, and a template. The three primary chooser entries can be configured for translation, code explanation, and general explanation. Settings dropdowns choose independent defaults for Selection and Hover.

When text is selected, the compact chooser displays profile names near the pointer. The provider request starts only after a profile is chosen. A word target is sent with locally derived sentence context when the application exposes the surrounding text.

## Controls

| Action | Default control |
| --- | --- |
| Manual lookup | `Ctrl+Alt+T` |
| Enable or disable Hover for this session | `Ctrl+Alt+H` |
| Cycle the active profile | `Ctrl+Alt+P` |
| Open manager / Rest mode / Exit | Notification-area menu |

Selection and Manual are enabled while the resident is active. Hover is off at startup and must be enabled manually. Rest mode suppresses automatic work until disabled. Popup controls provide Copy, Retry, Prompt, Pin, and Close.

## Troubleshooting

### No popup after selecting text

- Try a drag selection or double-click in a normal text control.
- Press `Ctrl+Alt+T` to test Manual extraction.
- Confirm the resident icon is present and Rest mode is disabled.
- Some protected or custom-rendered applications expose neither selectable text nor usable OCR pixels; extraction then remains silent and sends no request.

### `API key is not configured`

Use one of the two supported environment variables or save a Generic Credential through the manager. If the resident was already running when an environment variable was created, restart it from the environment that contains the variable.

### `Provider connection failed`

This indicates a WinHTTP connection problem, not an API-key rejection. Replace `HOST` with only the hostname from the configured endpoint:

```powershell
Test-NetConnection HOST -Port 443
netsh winhttp show proxy
```

If `TcpTestSucceeded` is false, check the network, firewall, VPN, DNS, or provider availability. If TCP succeeds, compare the WinHTTP proxy configuration with a working machine. An invalid key is reported separately as `Provider authentication failed`.

## Build from source

Requirements:

- Windows 10/11 x64
- Rust stable MSVC toolchain
- Visual Studio 2022 or newer C++ Build Tools with the Windows SDK
- PowerShell

This workspace keeps Cargo output under `windows/target`. The project packaging script also expects the project-specific D: tool locations documented in [docs/SETUP.md](docs/SETUP.md). From the repository root, choose a new output directory:

```powershell
.\windows\scripts\package-release.ps1 -OutputDirectory windows/dist/selection-translate-x64-local
```

The script runs formatting checks, all workspace tests, Clippy with warnings denied, and a locked release build before copying the executables and documentation. It refuses a non-empty destination and does not install dependencies, delete old packages, publish, or deploy.

## Documentation

- [Detailed setup](docs/SETUP.md)
- [Hotkeys and controls](docs/HOTKEYS.md)
- [Extraction fallbacks](docs/FALLBACKS.md)
- [Privacy and data handling](docs/PRIVACY.md)
- [Troubleshooting](docs/TROUBLESHOOTING.md)
- [Verification status](docs/VERIFICATION.md)

The current release is a preview. Automated tests pass, but the final manual compatibility matrix and five-minute resident memory gate are still tracked in [docs/VERIFICATION.md](docs/VERIFICATION.md).
