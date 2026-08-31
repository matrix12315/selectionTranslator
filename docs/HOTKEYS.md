# Hotkeys and controls

| Action | Default control | Behavior |
| --- | --- | --- |
| Manual lookup | `Ctrl+Alt+T` | Extract the current target and request the selected profile. |
| Toggle Hover | `Ctrl+Alt+H` | Enable/disable pointer-stop lookup for this resident session. Off at startup. |
| Cycle profiles | `Ctrl+Alt+P` | Cycle the prompt profile for the active trigger. The setting is configurable. |
| Open manager | Tray menu | Open the on-demand Settings, Prompts, and History manager. |
| Exit | Tray menu | Stops the resident and removes its global hotkeys. |

Selection is automatic after a drag or double-click when the foreground control exposes text. A normal click does not OCR a nearby word. Hover waits briefly after the pointer stops and is only active after it is enabled. Popup buttons provide Copy, Retry, Prompt, Pin, and Close; a pinned popup suppresses automatic replacement until Retry or Prompt is used. A hover word is sent with the locally derived sentence context when available.

Selection and Manual jobs have priority over Hover. Each pipeline stops after the first valid extractor result; a missing target creates no loading state, provider request, or history row.

If a global hotkey conflicts with another program, the resident stays running and reports the conflict locally rather than replacing the other registration.
