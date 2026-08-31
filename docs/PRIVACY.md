# Privacy and data handling

- A remote request is sent only after a non-empty normalized target passes the request gate. No-text events do not create a loading popup, provider request, or history row.
- The target, locally derived context, source label, and rendered prompt are sent to the configured OpenAI-compatible endpoint. Do not use the application with sensitive text unless that provider's policy is acceptable to you.
- API keys are read from process environment variables or Windows Credential Manager. They are not serialized into TOML, prompts, logs, package files, or SQLite history.
- Completed target/context/output metadata is stored in the local SQLite history database. Failed, cancelled, stale, partial, and empty jobs are not stored; rows are pruned to the newest 1,000. The database is opened briefly for resident writes and while the manager History view is in use.
- Manual and drag/double-click Selection clipboard fallback temporarily use the selected text. Plain clicks and Hover never synthesize Copy. Before copying, the app retains only a bounded independent snapshot of supported HGLOBAL-backed clipboard formats; it restores that snapshot through a short-lived owner, including when extraction fails. It does not retain or restore the live OLE clipboard object.
- OCR captures are memory-only and are released after recognition. No screenshots are saved by the resident or package script.
- The package is unsigned and intended for local testing. Inspect the source and endpoint configuration before use.
