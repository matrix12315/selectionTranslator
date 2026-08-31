# Selection Translate

[English](README.md)

Selection Translate 是一款轻量级 Windows x64 划词与悬停文本助手。目标程序能够提供上下文时，它会自动获取目标所在的完整句子，将所选提示词发送到 OpenAI 兼容 API，并在紧凑弹窗中流式渲染 Markdown。常驻程序采用原生 Rust 和 Win32，不使用 Electron 或 WebView；管理器仅在需要时启动。

当前仓库提供未签名的预览版本。划词、手动和可选的悬停功能共用一条带优先级的提取管线：先尝试 UI Automation，仅在适用时回退到剪贴板或 Windows OCR。没有检测到有效文本时，程序不会发送任何 API 请求。

## 轻量是核心特性

低资源占用不是事后优化，而是产品的基础设计：

- 常驻程序使用原生 Rust 和 Win32，不包含 Electron、WebView、浏览器内核、异步运行时或内置 OCR 模型。
- 设置、提示词和历史记录由独立管理器负责；管理器只在打开时运行，最后一个窗口关闭后立即退出。
- OCR 只在需要时截取有限的内存区域，不把截图保存到磁盘。
- SQLite 只在后台短暂写入历史记录时打开，并且仅保留最近 1,000 条完成记录。
- 悬停功能默认关闭；任一提取路径获得有效文本后，其他路径立即停止。

关闭管理器并完成预热后，常驻程序的目标是**专用工作集低于 20 MiB**。五分钟内存测试仍是待完成的发布门槛，因此当前预览版不会宣称已经验证 `<20 MiB`；详见[验证状态](docs/VERIFICATION.md)。

## 安装预览版

1. 从 GitHub Release 下载 Windows x64 ZIP，并解压到可写目录。
2. 运行 `selection-translate-manager.exe`，配置 API 服务和提示词。
3. 运行 `selection-translate-resident.exe`，通知区域中会出现程序图标。

由于程序尚未进行代码签名，Windows SmartScreen 可能显示警告。该软件为便携版，不安装服务，也不会在程序目录旁写入运行数据。用户配置和历史记录位于 `%LOCALAPPDATA%\SelectionTranslate`。

## 配置 API 服务

Selection Translate 使用与 OpenAI Chat Completions 兼容的 API。

1. 打开管理器的**设置**页。
2. 输入 API 基础地址。可以使用纯主机地址，也可以使用以 `/v1` 结尾的版本化地址。不要添加 `/chat/completions`，程序只会自动追加一次。
3. 输入服务商提供的准确模型标识。
4. 除非有意使用其他 Windows 通用凭据名称，否则保持凭据目标为 `SelectionTranslate/OpenAI`。
5. 在 **API key** 中粘贴密钥，点击**保存密钥**，然后点击**保存设置**。
6. 在下拉框中分别选择划词和悬停的默认提示词配置。
7. 更换凭据来源后，启动或重启常驻程序。

通过管理器保存的密钥会进入 Windows 凭据管理器的“通用凭据”。密钥不会写入 `config.toml`、历史记录、日志或发布包。

### 使用环境变量

常驻程序按以下顺序读取密钥：

1. `OPENAI_API_KEY`
2. `SELECTION_TRANSLATE_OPENAI_API_KEY`
3. 配置的 Windows 凭据管理器目标

下面的 PowerShell 命令只把密钥提供给本次启动的常驻程序，不把密钥写进命令文本或永久保存：

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

不要把 API 密钥放进 `config.toml`、快捷方式参数、Git 提交或问题报告。

### 不使用管理器配置

把发布包中的 `config.example.toml` 复制到：

```text
%LOCALAPPDATA%\SelectionTranslate\config.toml
```

可以编辑 API 地址、模型、默认选项和提示词配置，但密钥必须保存在该文件之外。提示词模板可以使用 `{target}`、`{context}` 和 `{source}`；每个模板都必须包含 `{target}`。

## 提示词配置

在管理器中打开**提示词**页，即可选择、新建或编辑配置。每个配置包含稳定 ID、简短的选择器名称和提示词模板。三个主要选择器选项可分别用于翻译、代码解释和通用解释。设置页可以为划词和悬停分别指定默认配置。

选中文本后，指针附近会显示紧凑的提示词选择器。只有点击某个提示词后才会发起 API 请求。只选择一个单词时，如果目标程序能够提供周围文本，程序会同时发送本地提取出的完整句子作为上下文。

## 操作方式

| 功能 | 默认操作 |
| --- | --- |
| 手动查询 | `Ctrl+Alt+T` |
| 开启或关闭本次会话的悬停功能 | `Ctrl+Alt+H` |
| 切换当前提示词配置 | `Ctrl+Alt+P` |
| 打开管理器／休息模式／退出 | 通知区域菜单 |

常驻程序运行时，划词和手动模式始终可用。悬停模式启动时默认关闭，需要手动开启。休息模式会暂停自动工作，直到再次关闭休息模式。结果弹窗提供复制、重试、提示词、固定和关闭操作。

## 常见问题

### 选中文本后没有弹窗

- 在普通文本控件中尝试拖动选择或双击单词。
- 按 `Ctrl+Alt+T` 测试手动提取。
- 确认通知区域中存在常驻程序图标，并且休息模式已关闭。
- 部分受保护或自绘应用既不公开可选文本，也无法提供可用的 OCR 图像；此时程序会保持静默且不会发送请求。

### `API key is not configured`

设置两个受支持的环境变量之一，或通过管理器保存通用凭据。如果常驻程序在创建环境变量之前已经运行，需要从包含该环境变量的环境中重新启动它。

### `Provider connection failed`

这表示 WinHTTP 连接失败，而不是 API 密钥被拒绝。把下方 `HOST` 替换为配置地址中的纯主机名：

```powershell
Test-NetConnection HOST -Port 443
netsh winhttp show proxy
```

如果 `TcpTestSucceeded` 为 false，请检查网络、防火墙、VPN、DNS 或服务商状态。如果 TCP 成功，请将 WinHTTP 代理设置与正常工作的电脑进行比较。无效密钥会单独显示为 `Provider authentication failed`。

## 从源码构建

需要：

- Windows 10/11 x64
- Rust stable MSVC 工具链
- Visual Studio 2022 或更新版本的 C++ Build Tools，以及 Windows SDK
- PowerShell

该工作区把 Cargo 输出放在 `windows/target`。项目打包脚本还使用 [docs/SETUP.md](docs/SETUP.md) 中记录的项目专用 D 盘工具路径。在仓库根目录选择一个新的输出目录：

```powershell
.\windows\scripts\package-release.ps1 -OutputDirectory windows/dist/selection-translate-x64-local
```

脚本会依次检查格式、运行整个工作区测试、执行禁止警告的 Clippy，并进行锁定依赖的 Release 构建，然后才复制程序和文档。它会拒绝非空目标目录，也不会安装依赖、删除旧包、发布或部署。

## 文档

- [详细安装](docs/SETUP.md)
- [快捷键与操作](docs/HOTKEYS.md)
- [文本提取与回退路径](docs/FALLBACKS.md)
- [隐私与数据处理](docs/PRIVACY.md)
- [故障排查](docs/TROUBLESHOOTING.md)
- [验证状态](docs/VERIFICATION.md)

当前版本仍是预览版。自动化测试已经通过，但最终手动兼容性矩阵和五分钟常驻内存测试仍记录在[验证状态](docs/VERIFICATION.md)中。
