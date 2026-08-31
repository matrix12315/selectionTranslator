<#
Packaged forced-OCR Hover gate.

`-StaticCheck` is inert: it parses/compiles the native probes but never creates
a window, moves the pointer, opens a socket, or starts the resident. A real run
is explicit through `-ResidentPath`; the harness itself shows no confirmation dialog.
#>
[CmdletBinding()]
param(
    [string] $ResidentPath,
    [switch] $StaticCheck,
    [switch] $KeepArtifacts
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$scriptRoot = Split-Path -Parent $PSCommandPath
$windowsRoot = [IO.Path]::GetFullPath((Join-Path $scriptRoot '..'))

Add-Type -AssemblyName System.Windows.Forms
if (-not ('HoverOcrNative' -as [type])) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class HoverOcrNative {
  [StructLayout(LayoutKind.Sequential)] public struct INPUT { public uint type; public INPUTUNION data; }
  [StructLayout(LayoutKind.Explicit)] public struct INPUTUNION {
    [FieldOffset(0)] public KEYBDINPUT keyboard;
    [FieldOffset(0)] public MOUSEINPUT mouse;
  }
  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT {
    public ushort vk; public ushort scan; public uint flags; public uint time; public UIntPtr extra;
  }
  [StructLayout(LayoutKind.Sequential)] public struct MOUSEINPUT {
    public int dx; public int dy; public uint mouseData; public uint flags; public uint time; public UIntPtr extra;
  }
  [DllImport("user32.dll", SetLastError=true)] static extern uint SendInput(uint count, INPUT[] input, int size);
  [DllImport("user32.dll")] static extern int GetSystemMetrics(int index);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hwnd, int command);
  [DllImport("user32.dll", SetLastError=true)] public static extern bool SetWindowPos(IntPtr hwnd, IntPtr after, int x, int y, int width, int height, uint flags);
  [DllImport("user32.dll")] public static extern short GetAsyncKeyState(int vk);
  [DllImport("user32.dll")] public static extern uint GetClipboardSequenceNumber();
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr hwnd, uint flags);
  [DllImport("user32.dll", CharSet=CharSet.Unicode, ExactSpelling=true, SetLastError=true)] public static extern IntPtr FindWindowW(string cls, IntPtr title);
  [DllImport("user32.dll")] public static extern IntPtr GetDlgItem(IntPtr parent, int id);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int SendMessageW(IntPtr hwnd, uint msg, IntPtr wp, StringBuilder lp);

  public static int InputSize() { return Marshal.SizeOf(typeof(INPUT)); }
  public static uint WindowPid(IntPtr hwnd) { uint pid=0; if(hwnd != IntPtr.Zero){GetWindowThreadProcessId(hwnd, out pid);} return pid; }
  static INPUT Key(ushort vk, bool up) { return new INPUT { type=1, data=new INPUTUNION { keyboard=new KEYBDINPUT { vk=vk, flags=up ? 2U : 0U } } }; }
  static INPUT Mouse(uint flags) { return new INPUT { type=0, data=new INPUTUNION { mouse=new MOUSEINPUT { flags=flags } } }; }
  public static uint MoveTo(int x, int y) { int left=GetSystemMetrics(76), top=GetSystemMetrics(77), width=GetSystemMetrics(78), height=GetSystemMetrics(79); if(width <= 1 || height <= 1){return 0;} int dx=(int)(((long)(x-left)*65535)/(width-1)); int dy=(int)(((long)(y-top)*65535)/(height-1)); var a=new INPUT[]{new INPUT{type=0,data=new INPUTUNION{mouse=new MOUSEINPUT{dx=dx,dy=dy,flags=0xC001U}}}}; return SendInput(1,a,InputSize()); }
  static bool IsDown(int vk) { return (GetAsyncKeyState(vk) & 0x8000) != 0; }
  static bool WaitModifiersUp() { for(int i=0;i<20;i++){ if(ModifiersUp()){return true;} System.Threading.Thread.Sleep(5); } return false; }
  public static bool ModifiersUp() { return !IsDown(0x11) && !IsDown(0x12) && !IsDown(0x10) && !IsDown(0x5B) && !IsDown(0x5C); }
  public static uint ReleasePhysicalModifiers() { var a=new INPUT[]{Key(0x11,true),Key(0x12,true),Key(0x10,true),Key(0x5B,true),Key(0x5C,true)}; return SendInput((uint)a.Length,a,InputSize()); }
  public static uint TapAlt() { var a=new INPUT[]{Key(0x12,false),Key(0x12,true)}; return SendInput((uint)a.Length,a,InputSize()); }
  public static uint Click() { var a=new INPUT[]{Mouse(0x0002U),Mouse(0x0004U)}; return SendInput((uint)a.Length,a,InputSize()); }
  public static uint ToggleHover() {
    if(ReleasePhysicalModifiers() != 5 || !WaitModifiersUp()){return 0;}
    var a=new INPUT[]{Key(0x11,false),Key(0x12,false),Key(0x48,false),Key(0x48,true),Key(0x12,true),Key(0x11,true)};
    return SendInput((uint)a.Length,a,InputSize());
  }
  public static string Text(IntPtr hwnd, int capacity) { var b=new StringBuilder(capacity); SendMessageW(hwnd,0x000D,new IntPtr(capacity),b); return b.ToString(); }
}
'@
}

if ($StaticCheck) {
    if ([HoverOcrNative]::InputSize() -ne 40) {
        throw "unexpected Win32 INPUT size: $([HoverOcrNative]::InputSize())"
    }
    $fixturePath = Join-Path $scriptRoot 'hover-ocr-fixture.ps1'
    if (-not (Test-Path -LiteralPath $fixturePath -PathType Leaf)) { throw 'forced-OCR fixture is missing' }
    $fixture = Get-Content -LiteralPath $fixturePath -Raw
    $source = Get-Content -LiteralPath $PSCommandPath -Raw
    foreach ($required in @('Add_Paint', 'DrawString', 'uia_text_pattern', '$false', 'target_x', 'blank_x', 'far_x')) {
        if ($fixture -notmatch [regex]::Escape($required)) { throw "fixture contract missing: $required" }
    }
    foreach ($required in @(
        'Wait-Request', 'Read-RequestBody', 'Content-Length', 'Wait-Popup',
        'GetClipboardSequenceNumber', 'Clipboard-State', 'History-RowCount',
        'hover_uia_failure', 'hover_ocr_success', 'MoveTo',
        'message-window-finder.exe', 'summary.json', 'package_sha256',
        '[hotkeys]', 'cycle_profiles = "Ctrl+Alt+P"'
    )) {
        if ($source -notmatch [regex]::Escape($required)) { throw "runtime assertion missing: $required" }
    }
    $deleteCommand = 'Remove' + '-Item'
    $clipboardSetter = '\[Clipboard\]::' + 'SetText'
    if ($source -match $deleteCommand -or $source -match $clipboardSetter) {
        throw 'the verifier must neither delete artifacts nor overwrite clipboard text'
    }
    [pscustomobject]@{
        native_input_size = [HoverOcrNative]::InputSize()
        owner_drawn_fixture = $true
        uia_text_pattern_absent = $true
        content_length_safe_request_reader = $true
        popup_probe = $true
        clipboard_and_foreground_probes = $true
        history_probe = $true
        trace_probe = $true
        no_gui_pointer_socket_or_resident_started = $true
    } | ConvertTo-Json -Compress
    exit 0
}

if ([string]::IsNullOrWhiteSpace($ResidentPath)) {
    throw 'ResidentPath is required for a real Hover OCR run; refusing stale package selection.'
}
if (-not (Test-Path -LiteralPath $ResidentPath -PathType Leaf)) {
    throw "ResidentPath does not exist: $ResidentPath"
}

function Write-Text([string] $Path, [string] $Text) {
    [IO.File]::WriteAllText($Path, $Text, [Text.Encoding]::UTF8)
}

function Read-Json([string] $Path, [datetime] $Deadline) {
    while ((Get-Date) -lt $Deadline) {
        if (Test-Path -LiteralPath $Path) {
            try { return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json } catch {}
        }
        Start-Sleep -Milliseconds 25
    }
    throw "timed out reading $Path"
}

function Wait-Request([Net.Sockets.TcpListener] $Listener, [datetime] $Deadline) {
    while ((Get-Date) -lt $Deadline) {
        if ($Listener.Pending()) { return $Listener.AcceptTcpClient() }
        Start-Sleep -Milliseconds 25
    }
    return $null
}

function Wait-TraceStage([string] $Path, [string] $Stage, [datetime] $Deadline) {
    while ((Get-Date) -lt $Deadline) {
        if (Test-Path -LiteralPath $Path) {
            $trace = Get-Content -LiteralPath $Path -Raw
            if ($trace.IndexOf("stage=$Stage", [StringComparison]::Ordinal) -ge 0) { return }
        }
        Start-Sleep -Milliseconds 25
    }
    throw "timed out waiting for trace stage=$Stage path=$Path"
}

function Assert-NoRequest([Net.Sockets.TcpListener] $Listener, [datetime] $Deadline, [string] $Reason) {
    while ((Get-Date) -lt $Deadline) {
        if ($Listener.Pending()) { throw $Reason }
        Start-Sleep -Milliseconds 25
    }
}

function Read-RequestBody([Net.Sockets.TcpClient] $Client) {
    $stream = $Client.GetStream()
    $stream.ReadTimeout = 5000
    $buffer = New-Object byte[] 4096
    $bytes = New-Object System.Collections.Generic.List[byte]
    $headerEnd = -1
    while ($headerEnd -lt 0) {
        $count = $stream.Read($buffer, 0, $buffer.Length)
        if ($count -le 0) { throw 'provider connection closed before request headers' }
        for ($index = 0; $index -lt $count; $index++) { [void] $bytes.Add($buffer[$index]) }
        for ($index = 0; $index -le $bytes.Count - 4; $index++) {
            if ($bytes[$index] -eq 13 -and $bytes[$index + 1] -eq 10 -and $bytes[$index + 2] -eq 13 -and $bytes[$index + 3] -eq 10) {
                $headerEnd = $index + 4
                break
            }
        }
    }
    $headers = [Text.Encoding]::ASCII.GetString($bytes.ToArray(), 0, $headerEnd)
    $contentLength = $null
    foreach ($line in ($headers -split "`r`n")) {
        if ($line -match '^Content-Length:\s*(\d+)\s*$') { $contentLength = [int] $Matches[1] }
    }
    if ($null -eq $contentLength -or $contentLength -lt 1 -or $contentLength -gt 1048576) {
        throw 'provider request has missing or invalid Content-Length'
    }
    while ($bytes.Count -lt $headerEnd + $contentLength) {
        $remaining = $headerEnd + $contentLength - $bytes.Count
        $count = $stream.Read($buffer, 0, [Math]::Min($buffer.Length, $remaining))
        if ($count -le 0) { throw 'provider connection closed before request body' }
        for ($index = 0; $index -lt $count; $index++) { [void] $bytes.Add($buffer[$index]) }
    }
    [Text.Encoding]::UTF8.GetString($bytes.ToArray(), $headerEnd, $contentLength)
}

function Send-Response([Net.Sockets.TcpClient] $Client, [string] $Text) {
    $json = @{ choices = @(@{ message = @{ content = $Text } }) } | ConvertTo-Json -Compress -Depth 5
    $body = [Text.Encoding]::UTF8.GetBytes($json)
    $head = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 200 OK`r`nContent-Type: application/json`r`nContent-Length: $($body.Length)`r`nConnection: close`r`n`r`n")
    $stream = $Client.GetStream()
    $stream.Write($head, 0, $head.Length)
    $stream.Write($body, 0, $body.Length)
    $stream.Flush()
}

function Assert-Request([string] $Body, [string] $ExpectedSentence) {
    try { $payload = $Body | ConvertFrom-Json } catch { throw "provider request was not valid JSON: $($_.Exception.Message)" }
    $users = @($payload.messages | Where-Object { $_.role -eq 'user' })
    if ($users.Count -ne 1 -or $users[0].content -isnot [string]) {
        throw 'provider request must contain exactly one string user message'
    }
    $expectedPrefix = "Target: hovered`nContext: $ExpectedSentence`nSource: "
    $content = [string] $users[0].content
    if (-not $content.StartsWith($expectedPrefix, [StringComparison]::Ordinal)) {
        throw 'OCR request did not preserve exact target and wrapped sentence context'
    }
    if ($content.IndexOf('Unrelated far-column sentence.', [StringComparison]::Ordinal) -ge 0) {
        throw 'OCR request included the unrelated far-column sentence'
    }
    if ([string]::IsNullOrWhiteSpace($content.Substring($expectedPrefix.Length))) {
        throw 'OCR request source field was empty'
    }
}

function Get-Popup([int] $ResidentPid) {
    $window = [HoverOcrNative]::FindWindowW('SelectionTranslatePopup', [IntPtr]::Zero)
    [uint32] $ownerProcessId = 0
    if ($window -ne [IntPtr]::Zero) { [HoverOcrNative]::GetWindowThreadProcessId($window, [ref] $ownerProcessId) | Out-Null }
    $text = ''
    if ($window -ne [IntPtr]::Zero) {
        $output = [HoverOcrNative]::GetDlgItem($window, 1)
        if ($output -ne [IntPtr]::Zero) { $text = [HoverOcrNative]::Text($output, 8192) }
    }
    [pscustomobject]@{
        hwnd = $window
        pid = [int] $ownerProcessId
        visible = ($window -ne [IntPtr]::Zero -and [HoverOcrNative]::IsWindowVisible($window))
        text = $text
        expected_pid = $ResidentPid
    }
}

function Wait-Popup([int] $ResidentPid, [string] $Needle, [datetime] $Deadline) {
    $last = $null
    while ((Get-Date) -lt $Deadline) {
        $last = Get-Popup $ResidentPid
        if ($last.pid -eq $ResidentPid -and $last.visible -and $last.text.IndexOf($Needle, [StringComparison]::Ordinal) -ge 0) { return $last }
        Start-Sleep -Milliseconds 40
    }
    throw "popup probe failed: expected=$Needle pid=$($last.pid) visible=$($last.visible) text_length=$($last.text.Length)"
}

function Clipboard-State {
    $text = ''
    $containsText = $false
    for ($attempt = 0; $attempt -lt 20; $attempt++) {
        try {
            $containsText = [Windows.Forms.Clipboard]::ContainsText()
            if ($containsText) { $text = [Windows.Forms.Clipboard]::GetText() }
            $sha = [Security.Cryptography.SHA256]::Create()
            try { $hash = ([BitConverter]::ToString($sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($text)))).Replace('-', '') } finally { $sha.Dispose() }
            return [pscustomobject]@{
                fingerprint = "$containsText|$($text.Length)|$hash"
                sequence = [HoverOcrNative]::GetClipboardSequenceNumber()
            }
        } catch { Start-Sleep -Milliseconds 25 }
    }
    throw 'clipboard was unavailable for a bounded fingerprint read'
}

function History-RowCount([string] $DatabasePath) {
    $counter = Join-Path $windowsRoot 'target\debug\selection-history-count.exe'
    if (-not (Test-Path -LiteralPath $counter -PathType Leaf)) { throw "history counter missing: $counter" }
    $output = & $counter $DatabasePath 2>&1
    if ($LASTEXITCODE -ne 0 -or ($output -join "`n") -notmatch 'row_count=(\d+)') { throw 'history counter failed' }
    [int] $Matches[1]
}

function Wait-HistoryRows([string] $DatabasePath, [int] $Expected, [datetime] $Deadline) {
    $last = -1
    while ((Get-Date) -lt $Deadline) {
        if (Test-Path -LiteralPath $DatabasePath) {
            try { $last = History-RowCount $DatabasePath } catch {}
            if ($last -eq $Expected) { return $last }
        }
        Start-Sleep -Milliseconds 50
    }
    throw "history row count mismatch: expected=$Expected actual=$last"
}

$runRoot = Join-Path (Join-Path $windowsRoot 'tmp') ('hover-ocr-run-' + (Get-Date -Format yyyyMMddHHmmssfff))
New-Item -ItemType Directory -Path $runRoot -Force | Out-Null
$summaryPath = Join-Path $runRoot 'summary.json'
$summary = [ordered]@{
    status = 'failed'
    run_root = $runRoot
    request_count = 0
    history_row_count = 0
    no_image_files = $false
}
$fixture = $null
$resident = $null
$client = $null
$listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$oldEnvironment = @{
    LOCALAPPDATA = $env:LOCALAPPDATA
    OPENAI_API_KEY = $env:OPENAI_API_KEY
    BASE = $env:SELECTION_TRANSLATE_OPENAI_BASE_URL
    MODEL = $env:SELECTION_TRANSLATE_OPENAI_MODEL
    TRACE = $env:SELECTION_TRANSLATE_RUNTIME_TRACE
}

try {
    $resolvedResident = (Resolve-Path -LiteralPath $ResidentPath).Path
    $summary.package_path = $resolvedResident
    $summary.package_sha256 = (Get-FileHash -LiteralPath $resolvedResident -Algorithm SHA256).Hash
    $listener.Start()
    $port = ([Net.IPEndPoint] $listener.LocalEndpoint).Port
    $expectedSentence = 'The hovered word needs context for a precise explanation.'

    $configDirectory = Join-Path $runRoot 'SelectionTranslate'
    New-Item -ItemType Directory -Path $configDirectory -Force | Out-Null
    $config = @"
[[profiles]]
id = "hover-ocr-test"
name = "Hover OCR test"
system_prompt = "Return only the fixed test result."
user_template = "Target: {target}\nContext: {context}\nSource: {source}"

[defaults]
selection = "hover-ocr-test"
hover = "hover-ocr-test"

[provider]
endpoint = "http://127.0.0.1:$port"
model = "hover-ocr-test-model"
credential_target = "SelectionTranslate/HoverOcrE2E"

[hotkeys]
cycle_profiles = "Ctrl+Alt+P"
"@
    Write-Text (Join-Path $configDirectory 'config.toml') $config

    $fixturePath = Join-Path $scriptRoot 'hover-ocr-fixture.ps1'
    $fixture = Start-Process powershell.exe -ArgumentList @(
        '-NoProfile', '-NonInteractive', '-STA', '-ExecutionPolicy', 'Bypass',
        '-File', $fixturePath, '-RunRoot', $runRoot
    ) -PassThru -WindowStyle Hidden
    $ready = Read-Json (Join-Path $runRoot 'hover-ocr-fixture-ready.json') ((Get-Date).AddSeconds(10))
    if ([int] $ready.pid -ne $fixture.Id -or [int64] $ready.hwnd -eq 0 -or [int64] $ready.control_hwnd -eq 0) {
        throw 'forced-OCR fixture PID/HWND contract failed'
    }
    if ([bool] $ready.uia_text_pattern) { throw 'fixture unexpectedly advertises UIA TextPattern' }
    $controlRoot = [HoverOcrNative]::GetAncestor([IntPtr] $ready.control_hwnd, 2)
    if ($controlRoot.ToInt64() -ne [int64] $ready.hwnd) { throw 'fixture owner-drawn control does not belong to fixture root window' }
    [HoverOcrNative]::ShowWindow([IntPtr] $ready.hwnd, 5) | Out-Null
    if (-not [HoverOcrNative]::SetWindowPos([IntPtr] $ready.hwnd, [IntPtr] (-1), 0, 0, 0, 0, 0x53)) {
        throw 'fixture topmost presentation failed'
    }
    [HoverOcrNative]::TapAlt() | Out-Null
    [HoverOcrNative]::SetForegroundWindow([IntPtr] $ready.hwnd) | Out-Null
    if ([HoverOcrNative]::MoveTo([int] $ready.blank_x, [int] $ready.blank_y) -ne 1) { throw 'fixture setup pointer move failed' }
    if ([HoverOcrNative]::Click() -ne 2) { throw 'fixture foreground setup click failed' }
    $foregroundBefore = [HoverOcrNative]::GetForegroundWindow().ToInt64()
    if ($foregroundBefore -ne [int64] $ready.hwnd) { throw 'fixture did not become the foreground baseline' }
    $clipboardBefore = Clipboard-State

    $tracePath = Join-Path $runRoot 'runtime-trace.log'
    $env:LOCALAPPDATA = $runRoot
    $env:OPENAI_API_KEY = [Guid]::NewGuid().ToString('N')
    $env:SELECTION_TRANSLATE_OPENAI_BASE_URL = "http://127.0.0.1:$port"
    $env:SELECTION_TRANSLATE_OPENAI_MODEL = 'hover-ocr-test-model'
    $env:SELECTION_TRANSLATE_RUNTIME_TRACE = $tracePath
    $resident = Start-Process -FilePath $resolvedResident -WorkingDirectory ([IO.Path]::GetDirectoryName($resolvedResident)) -PassThru -WindowStyle Hidden
    Start-Sleep -Milliseconds 2000
    if ($resident.HasExited) { throw 'resident exited during forced-OCR startup' }
    $finder = Join-Path $windowsRoot 'tmp\message-window-finder.exe'
    if (-not (Test-Path -LiteralPath $finder -PathType Leaf)) { throw "message-window-finder.exe missing: $finder" }
    & $finder ([string] $resident.Id) 'SelectionTranslateResident' *> (Join-Path $runRoot 'readiness.txt')
    if ($LASTEXITCODE -ne 0) { throw 'resident readiness message window missing' }
    if ([HoverOcrNative]::GetForegroundWindow().ToInt64() -ne $foregroundBefore) { throw 'resident startup changed foreground window' }

    if ([HoverOcrNative]::ToggleHover() -ne 6) { throw 'Hover enable hotkey failed' }
    Wait-TraceStage $tracePath 'hover_enabled' ((Get-Date).AddSeconds(3))
    if ([HoverOcrNative]::MoveTo([int] $ready.target_x, [int] $ready.target_y) -ne 1) { throw 'target pointer move failed' }
    $client = Wait-Request $listener ((Get-Date).AddSeconds(10))
    if ($null -eq $client) { throw "forced OCR target produced no request; trace_path=$tracePath" }
    $body = Read-RequestBody $client
    $loadingPopup = Wait-Popup $resident.Id 'Translating' ((Get-Date).AddSeconds(5))
    Assert-Request $body $expectedSentence
    $resultText = 'hover-ocr-result-' + [Guid]::NewGuid().ToString('N')
    Send-Response $client $resultText
    $client.Dispose()
    $client = $null
    $completedPopup = Wait-Popup $resident.Id $resultText ((Get-Date).AddSeconds(8))
    $summary.request_count = 1
    $summary.target_preserved = $true
    $summary.joined_sentence_preserved = $true
    $summary.far_column_excluded = $true
    $summary.popup_loading_completed = ($loadingPopup.visible -and $completedPopup.visible)

    if ([HoverOcrNative]::MoveTo([int] $ready.blank_x, [int] $ready.blank_y) -ne 1) { throw 'blank pointer move failed' }
    Assert-NoRequest $listener ((Get-Date).AddSeconds(4)) 'blank coordinate produced a provider request'
    $summary.blank_no_request = $true

    if ([HoverOcrNative]::ToggleHover() -ne 6) { throw 'Hover disable hotkey failed' }
    if ([HoverOcrNative]::MoveTo([int] $ready.target_x, [int] $ready.target_y) -ne 1) { throw 'disabled target pointer move failed' }
    Assert-NoRequest $listener ((Get-Date).AddSeconds(1)) 'disabled Hover produced a provider request'
    $summary.disabled_no_request = $true

    $clipboardAfter = Clipboard-State
    if ($clipboardAfter.fingerprint -ne $clipboardBefore.fingerprint -or $clipboardAfter.sequence -ne $clipboardBefore.sequence) {
        throw 'forced-OCR Hover changed clipboard contents or sequence'
    }
    $summary.clipboard_preserved = $true
    $summary.clipboard_sequence_preserved = $true
    $foregroundAfter = [HoverOcrNative]::GetForegroundWindow().ToInt64()
    if ($foregroundAfter -ne $foregroundBefore) { throw "forced-OCR Hover changed foreground: before=$foregroundBefore after=$foregroundAfter" }
    $summary.foreground_preserved = $true

    $historyPath = Join-Path $configDirectory 'history.sqlite3'
    $summary.history_row_count = Wait-HistoryRows $historyPath 1 ((Get-Date).AddSeconds(5))
    if (-not (Test-Path -LiteralPath $tracePath -PathType Leaf)) { throw 'runtime trace was not created' }
    $trace = Get-Content -LiteralPath $tracePath -Raw
    $uiaIndex = $trace.IndexOf('stage=hover_uia_failure', [StringComparison]::Ordinal)
    $ocrIndex = $trace.IndexOf('stage=hover_ocr_success', [StringComparison]::Ordinal)
    if ($uiaIndex -lt 0 -or $ocrIndex -le $uiaIndex) { throw 'trace did not prove UIA failure followed by OCR success' }
    $summary.trace_uia_failure_then_ocr_success = $true
    $summary.status = 'passed'
}
catch {
    $summary.error = $_.Exception.Message
    throw
}
finally {
    try { $listener.Stop() } catch {}
    if ($null -ne $client) { try { $client.Dispose() } catch {} }
    if ($null -ne $fixture) {
        try { Write-Text (Join-Path $runRoot 'hover-ocr-fixture-stop') 'stop' } catch {}
        try { [void] $fixture.WaitForExit(1500) } catch {}
        try { if (-not $fixture.HasExited) { Stop-Process -Id $fixture.Id -Force -ErrorAction SilentlyContinue } } catch {}
    }
    if ($null -ne $resident) {
        try { if (-not $resident.HasExited) { Stop-Process -Id $resident.Id -Force -ErrorAction SilentlyContinue } } catch {}
    }
    $env:LOCALAPPDATA = $oldEnvironment.LOCALAPPDATA
    $env:OPENAI_API_KEY = $oldEnvironment.OPENAI_API_KEY
    $env:SELECTION_TRANSLATE_OPENAI_BASE_URL = $oldEnvironment.BASE
    $env:SELECTION_TRANSLATE_OPENAI_MODEL = $oldEnvironment.MODEL
    $env:SELECTION_TRANSLATE_RUNTIME_TRACE = $oldEnvironment.TRACE
    $summary.no_image_files = @(
        Get-ChildItem -LiteralPath $runRoot -File -Recurse -ErrorAction SilentlyContinue |
            Where-Object { $_.Extension -match '^\.(png|bmp|jpe?g|gif|tiff?)$' }
    ).Count -eq 0
    try { Write-Text $summaryPath ($summary | ConvertTo-Json -Depth 6) } catch {}
    Write-Output "summary_path=$summaryPath"
}
