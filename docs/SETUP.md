# Local setup

This package targets 64-bit Windows 10 22H2/Windows 11 with the MSVC runtime. It is native, unsigned, and intended for local testing; it is not an installer or a public release.

## Configure the provider

1. Copy `config.example.toml` to `%LOCALAPPDATA%\SelectionTranslate\config.toml`.
2. Edit the endpoint, model, defaults, or prompt templates. Templates may use only `{target}`, `{context}`, and `{source}` and must include `{target}`.
3. Keep the API key outside the file. The resident checks `OPENAI_API_KEY`, then `SELECTION_TRANSLATE_OPENAI_API_KEY`, then the Windows Credential Manager target named `SelectionTranslate/OpenAI`. A manager Settings action can save/delete the key through Credential Manager; do not paste it into TOML.
4. Start `selection-translate-resident.exe`. It places an icon in the notification area.

The default endpoint is `https://api.openai.com/v1`; loopback HTTP endpoints are supported for local mock testing. Other plain-HTTP endpoints are rejected.

## Run from the repository

Choose a fresh package directory. The script refuses a non-empty destination and never deletes an old package:

```powershell
.\windows\scripts\package-release.ps1 -OutputDirectory windows/dist/selection-translate-x64-20260819
& .\windows\dist\selection-translate-x64-20260819\selection-translate-resident.exe
```

The script verifies formatting, workspace tests, workspace Clippy (`-D warnings`), and a locked release build before staging the two executables and documentation. Cargo output, compiler temporary files, and script scratch data stay under `windows\target` and `windows\tmp`; the script does not install dependencies, sign binaries, publish, deploy, or remove files.

## Manager and runtime behavior

Use the tray menu to open the manager. Settings saves the TOML atomically and prompts/configuration reload through `ReadDirectoryChangesW` without restarting the resident. Prompts can be selected, edited, created, and cycled with `Ctrl+Alt+P`. History is opened and loaded only when its manager view is selected; completed rows are stored in SQLite and retained to the newest 1,000.

If configuration or credentials are unavailable, automatic jobs remain silent and Manual shows a local configuration error. No request or history row is created without valid target text.
