#![cfg_attr(windows, windows_subsystem = "windows")]

//! The on-demand manager. It is a plain Win32 window: the resident process
//! never loads this executable, and no WebView/runtime is needed for editing
//! the small TOML configuration.

#[cfg(windows)]
mod windows_app {
    use selection_core::{
        default_config_path, save_atomic, AppConfig, ExtractionSource, PromptConfig, UiLanguage,
    };
    use selection_platform_windows::{
        app::{
            ensure_resident_running, notify_config_changed, notify_credentials_changed,
            RefreshOutcome, ResidentStartOutcome,
        },
        credentials,
    };
    use selection_storage::{
        default_history_path, HistoryDatabase, HistoryEntry, HistoryOrder, HistoryQuery,
    };
    use std::ffi::c_void;
    use std::path::PathBuf;
    use windows::core::{w, Error, PCWSTR};
    use windows::Win32::Foundation::{
        COLORREF, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
    };
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawFocusRect,
        DrawTextW, EndPaint, FillRect, FillRgn, FrameRgn, InvalidateRect, SelectObject, SetBkColor,
        SetTextColor, UpdateWindow, DRAW_TEXT_FORMAT, FONT_CHARSET, FONT_CLIP_PRECISION,
        FONT_OUTPUT_PRECISION, FONT_QUALITY, HBRUSH, HFONT, HGDIOBJ,
    };
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_UNICODETEXT;
    use windows::Win32::UI::Controls::{SetWindowTheme, DRAWITEMSTRUCT, ODT_BUTTON};
    use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow};
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
        GetMessageW, GetParent, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
        IsDialogMessageW, MessageBoxW, PostQuitMessage, RegisterClassW, SendMessageW,
        SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage,
        BS_OWNERDRAW, BS_PUSHBUTTON, CREATESTRUCTW, ES_AUTOHSCROLL, ES_AUTOVSCROLL, ES_MULTILINE,
        ES_PASSWORD, GWLP_USERDATA, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_YESNO, MINMAXINFO,
        SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOW, WINDOW_STYLE, WM_CLOSE, WM_COMMAND,
        WM_CREATE, WM_DESTROY, WM_DPICHANGED, WM_DRAWITEM, WM_ERASEBKGND, WM_GETMINMAXINFO,
        WM_NOTIFY, WM_PAINT, WM_SETFONT, WM_SIZE, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD,
        WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_CONTROLPARENT, WS_OVERLAPPED, WS_SYSMENU,
        WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
    };

    const CLASS_NAME: PCWSTR = w!("SelectionTranslateManager");
    const PAGE_CLASS: PCWSTR = w!("SelectionTranslateManagerPage");
    const ID_SETTINGS_TAB: usize = 100;
    const ID_PROMPTS_TAB: usize = 101;
    const ID_HISTORY_TAB: usize = 102;
    const ID_SAVE_SETTINGS: usize = 110;
    const ID_SAVE_KEY: usize = 111;
    const ID_DELETE_KEY: usize = 112;
    const ID_SAVE_PROMPT: usize = 120;
    const ID_NEW_PROMPT: usize = 121;
    const ID_PREVIOUS_PROMPT: usize = 122;
    const ID_NEXT_PROMPT: usize = 123;
    const ID_CLOSE: usize = 130;
    const ID_HISTORY_PROMPT: usize = 141;
    const ID_HISTORY_SOURCE: usize = 142;
    const ID_HISTORY_ORDER: usize = 143;
    const ID_HISTORY_LIST: usize = 144;
    const ID_HISTORY_REFRESH: usize = 145;
    const ID_HISTORY_COPY: usize = 146;
    const ID_HISTORY_DELETE: usize = 147;
    const ID_LANGUAGE: usize = 148;
    // Stable child-control IDs are part of the manager's UI automation
    // surface.  Tests and assistive tools must not infer field identity from
    // mutable label/value text or child-window enumeration order.
    const ID_SETTINGS_PAGE: usize = 150;
    const ID_PROMPTS_PAGE: usize = 151;
    const ID_HISTORY_PAGE: usize = 152;
    const ID_SETTINGS_ENDPOINT: usize = 160;
    const ID_SETTINGS_MODEL: usize = 161;
    const ID_SETTINGS_CREDENTIAL_TARGET: usize = 162;
    const ID_SETTINGS_API_KEY: usize = 163;
    const ID_SETTINGS_SELECTION_DEFAULT: usize = 164;
    const ID_SETTINGS_HOVER_DEFAULT: usize = 165;
    const ID_PROMPT_ID: usize = 166;
    const ID_PROMPT_NAME: usize = 167;
    const ID_PROMPT_SYSTEM: usize = 168;
    const ID_PROMPT_USER_TEMPLATE: usize = 169;
    const ID_PROMPT_MODEL: usize = 170;
    const ID_PROMPT_TEMPERATURE: usize = 171;
    const ID_PROMPT_MAX_TOKENS: usize = 172;
    const ID_PROMPT_SELECTION_DEFAULT: usize = 173;
    const ID_PROMPT_HOVER_DEFAULT: usize = 174;
    const ID_HISTORY_SEARCH: usize = 175;

    const DEFAULT_DPI: u32 = 96;
    const NAV_WIDTH: i32 = 176;
    const PAGE_HEADER_HEIGHT: i32 = 56;
    const MIN_CONTENT_WIDTH: i32 = 780;
    const MIN_CLIENT_HEIGHT: i32 = 610;
    const MANAGER_BG: COLORREF = COLORREF(0x002A_170F);
    const NAV_BG: COLORREF = COLORREF(0x001C_0F0A);
    const SURFACE_BG: COLORREF = COLORREF(0x0036_2218);
    const SURFACE_HOVER: COLORREF = COLORREF(0x0045_2D21);
    const BORDER: COLORREF = COLORREF(0x0055_4133);
    const TEXT: COLORREF = COLORREF(0x00F0_E8E2);
    const MUTED: COLORREF = COLORREF(0x00B8_A394);
    const ACCENT: COLORREF = COLORREF(0x00FA_A560);
    const DANGER: COLORREF = COLORREF(0x0071_71F8);

    const LB_ADDSTRING: u32 = 0x0180;
    const LB_RESETCONTENT: u32 = 0x0184;
    const LB_GETCURSEL: u32 = 0x0188;
    const LBN_SELCHANGE: usize = 1;
    const CBN_SELCHANGE: usize = 1;
    const CB_ADDSTRING: u32 = 0x0143;
    const CB_RESETCONTENT: u32 = 0x014B;
    const CB_SETCURSEL: u32 = 0x014E;
    const CB_GETCURSEL: u32 = 0x0147;
    const LBS_NOTIFY: u32 = 0x0001;
    const CBS_DROPDOWNLIST: u32 = 0x0003;
    const EM_SETREADONLY: u32 = 0x00CF;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalFree(hmem: HGLOBAL) -> HGLOBAL;
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    enum View {
        Settings,
        Prompts,
        History,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum TextKey {
        WindowTitle,
        Brand,
        Manager,
        Settings,
        Prompts,
        History,
        Close,
        SettingsSubtitle,
        PromptsSubtitle,
        HistorySubtitle,
        InterfaceLanguage,
        ProviderEndpoint,
        Model,
        CredentialTarget,
        ApiKey,
        SaveKey,
        DeleteSavedKey,
        SelectionDefaultProfile,
        HoverDefaultProfile,
        SaveSettings,
        CredentialPrivacy,
        Profile,
        Previous,
        Next,
        New,
        Id,
        Name,
        SystemPrompt,
        UserTemplate,
        ModelOverride,
        Temperature,
        MaxOutputTokens,
        DefaultsSelectionHover,
        SavePrompt,
        PromptHint,
        SearchTargetOutput,
        Refresh,
        CopyOutput,
        Prompt,
        Source,
        Order,
        Target,
        Context,
        Output,
        DeleteSelected,
        HistoryPrivacy,
        AllPrompts,
        AllSources,
        Selection,
        Hover,
        Clipboard,
        Ocr,
        Newest,
        Oldest,
        English,
        SimplifiedChinese,
    }

    const ALL_TEXT_KEYS: &[TextKey] = &[
        TextKey::WindowTitle,
        TextKey::Brand,
        TextKey::Manager,
        TextKey::Settings,
        TextKey::Prompts,
        TextKey::History,
        TextKey::Close,
        TextKey::SettingsSubtitle,
        TextKey::PromptsSubtitle,
        TextKey::HistorySubtitle,
        TextKey::InterfaceLanguage,
        TextKey::ProviderEndpoint,
        TextKey::Model,
        TextKey::CredentialTarget,
        TextKey::ApiKey,
        TextKey::SaveKey,
        TextKey::DeleteSavedKey,
        TextKey::SelectionDefaultProfile,
        TextKey::HoverDefaultProfile,
        TextKey::SaveSettings,
        TextKey::CredentialPrivacy,
        TextKey::Profile,
        TextKey::Previous,
        TextKey::Next,
        TextKey::New,
        TextKey::Id,
        TextKey::Name,
        TextKey::SystemPrompt,
        TextKey::UserTemplate,
        TextKey::ModelOverride,
        TextKey::Temperature,
        TextKey::MaxOutputTokens,
        TextKey::DefaultsSelectionHover,
        TextKey::SavePrompt,
        TextKey::PromptHint,
        TextKey::SearchTargetOutput,
        TextKey::Refresh,
        TextKey::CopyOutput,
        TextKey::Prompt,
        TextKey::Source,
        TextKey::Order,
        TextKey::Target,
        TextKey::Context,
        TextKey::Output,
        TextKey::DeleteSelected,
        TextKey::HistoryPrivacy,
        TextKey::AllPrompts,
        TextKey::AllSources,
        TextKey::Selection,
        TextKey::Hover,
        TextKey::Clipboard,
        TextKey::Ocr,
        TextKey::Newest,
        TextKey::Oldest,
        TextKey::English,
        TextKey::SimplifiedChinese,
    ];

    fn ui_text(language: UiLanguage, key: TextKey) -> &'static str {
        use TextKey::*;
        match (language, key) {
            (UiLanguage::English, WindowTitle) => "Selection Translate — Manager",
            (UiLanguage::English, Brand) => "SELECTION TRANSLATE",
            (UiLanguage::English, Manager) => "Manager",
            (UiLanguage::English, Settings) => "Settings",
            (UiLanguage::English, Prompts) => "Prompts",
            (UiLanguage::English, History) => "History",
            (UiLanguage::English, Close) => "Close",
            (UiLanguage::English, SettingsSubtitle) => "Provider, credentials and default profiles",
            (UiLanguage::English, PromptsSubtitle) => "Create and tune reusable LLM instructions",
            (UiLanguage::English, HistorySubtitle) => {
                "Search recent translations without keeping the database open"
            }
            (UiLanguage::English, InterfaceLanguage) => "Interface language",
            (UiLanguage::English, ProviderEndpoint) => "Provider endpoint",
            (UiLanguage::English, Model) => "Model",
            (UiLanguage::English, CredentialTarget) => "Credential target",
            (UiLanguage::English, ApiKey) => "API key",
            (UiLanguage::English, SaveKey) => "Save key",
            (UiLanguage::English, DeleteSavedKey) => "Delete saved key",
            (UiLanguage::English, SelectionDefaultProfile) => "Selection default profile",
            (UiLanguage::English, HoverDefaultProfile) => "Hover default profile",
            (UiLanguage::English, SaveSettings) => "Save settings",
            (UiLanguage::English, CredentialPrivacy) => {
                "Keys are held by Windows Credential Manager; they never enter config.toml."
            }
            (UiLanguage::English, Profile) => "Profile",
            (UiLanguage::English, Previous) => "Previous",
            (UiLanguage::English, Next) => "Next",
            (UiLanguage::English, New) => "New",
            (UiLanguage::English, Id) => "ID",
            (UiLanguage::English, Name) => "Name",
            (UiLanguage::English, SystemPrompt) => "System prompt",
            (UiLanguage::English, UserTemplate) => "User template",
            (UiLanguage::English, ModelOverride) => "Model override",
            (UiLanguage::English, Temperature) => "Temperature",
            (UiLanguage::English, MaxOutputTokens) => "Max output tokens",
            (UiLanguage::English, DefaultsSelectionHover) => "Defaults: selection / hover",
            (UiLanguage::English, SavePrompt) => "Save prompt",
            (UiLanguage::English, PromptHint) => {
                "Use {target}, {context}, and {source}; every user template needs {target}."
            }
            (UiLanguage::English, SearchTargetOutput) => "Search target/output",
            (UiLanguage::English, Refresh) => "Refresh",
            (UiLanguage::English, CopyOutput) => "Copy output",
            (UiLanguage::English, Prompt) => "Prompt",
            (UiLanguage::English, Source) => "Source",
            (UiLanguage::English, Order) => "Order",
            (UiLanguage::English, Target) => "Target",
            (UiLanguage::English, Context) => "Context",
            (UiLanguage::English, Output) => "Output",
            (UiLanguage::English, DeleteSelected) => "Delete selected",
            (UiLanguage::English, HistoryPrivacy) => {
                "History is loaded only while this tab is open; the database is never held open."
            }
            (UiLanguage::English, AllPrompts) => "All prompts",
            (UiLanguage::English, AllSources) => "All sources",
            (UiLanguage::English, Selection) => "Selection",
            (UiLanguage::English, Hover) => "Hover",
            (UiLanguage::English, Clipboard) => "Clipboard",
            (UiLanguage::English, Ocr) => "OCR",
            (UiLanguage::English, Newest) => "Newest",
            (UiLanguage::English, Oldest) => "Oldest",
            (UiLanguage::English, English) => "English",
            (UiLanguage::English, SimplifiedChinese) => "Simplified Chinese",
            (UiLanguage::SimplifiedChinese, WindowTitle) => "划词翻译 — 管理器",
            (UiLanguage::SimplifiedChinese, Brand) => "划词翻译",
            (UiLanguage::SimplifiedChinese, Manager) => "管理器",
            (UiLanguage::SimplifiedChinese, Settings) => "设置",
            (UiLanguage::SimplifiedChinese, Prompts) => "提示词",
            (UiLanguage::SimplifiedChinese, History) => "历史记录",
            (UiLanguage::SimplifiedChinese, Close) => "关闭",
            (UiLanguage::SimplifiedChinese, SettingsSubtitle) => "服务商、凭据和默认配置",
            (UiLanguage::SimplifiedChinese, PromptsSubtitle) => "创建和调整可复用的 LLM 指令",
            (UiLanguage::SimplifiedChinese, HistorySubtitle) => "搜索最近结果；数据库仅按需打开",
            (UiLanguage::SimplifiedChinese, InterfaceLanguage) => "界面语言",
            (UiLanguage::SimplifiedChinese, ProviderEndpoint) => "服务端点",
            (UiLanguage::SimplifiedChinese, Model) => "模型",
            (UiLanguage::SimplifiedChinese, CredentialTarget) => "凭据目标",
            (UiLanguage::SimplifiedChinese, ApiKey) => "API 密钥",
            (UiLanguage::SimplifiedChinese, SaveKey) => "保存密钥",
            (UiLanguage::SimplifiedChinese, DeleteSavedKey) => "删除已存密钥",
            (UiLanguage::SimplifiedChinese, SelectionDefaultProfile) => "划词默认配置",
            (UiLanguage::SimplifiedChinese, HoverDefaultProfile) => "悬停默认配置",
            (UiLanguage::SimplifiedChinese, SaveSettings) => "保存设置",
            (UiLanguage::SimplifiedChinese, CredentialPrivacy) => {
                "密钥保存在 Windows 凭据管理器中，绝不会写入 config.toml。"
            }
            (UiLanguage::SimplifiedChinese, Profile) => "配置",
            (UiLanguage::SimplifiedChinese, Previous) => "上一个",
            (UiLanguage::SimplifiedChinese, Next) => "下一个",
            (UiLanguage::SimplifiedChinese, New) => "新建",
            (UiLanguage::SimplifiedChinese, Id) => "ID",
            (UiLanguage::SimplifiedChinese, Name) => "名称",
            (UiLanguage::SimplifiedChinese, SystemPrompt) => "系统提示词",
            (UiLanguage::SimplifiedChinese, UserTemplate) => "用户模板",
            (UiLanguage::SimplifiedChinese, ModelOverride) => "模型覆盖",
            (UiLanguage::SimplifiedChinese, Temperature) => "温度",
            (UiLanguage::SimplifiedChinese, MaxOutputTokens) => "最大输出令牌数",
            (UiLanguage::SimplifiedChinese, DefaultsSelectionHover) => "默认配置：划词 / 悬停",
            (UiLanguage::SimplifiedChinese, SavePrompt) => "保存提示词",
            (UiLanguage::SimplifiedChinese, PromptHint) => {
                "可使用 {target}、{context} 和 {source}；用户模板必须包含 {target}。"
            }
            (UiLanguage::SimplifiedChinese, SearchTargetOutput) => "搜索目标/输出",
            (UiLanguage::SimplifiedChinese, Refresh) => "刷新",
            (UiLanguage::SimplifiedChinese, CopyOutput) => "复制输出",
            (UiLanguage::SimplifiedChinese, Prompt) => "提示词",
            (UiLanguage::SimplifiedChinese, Source) => "来源",
            (UiLanguage::SimplifiedChinese, Order) => "排序",
            (UiLanguage::SimplifiedChinese, Target) => "目标",
            (UiLanguage::SimplifiedChinese, Context) => "上下文",
            (UiLanguage::SimplifiedChinese, Output) => "输出",
            (UiLanguage::SimplifiedChinese, DeleteSelected) => "删除所选项",
            (UiLanguage::SimplifiedChinese, HistoryPrivacy) => {
                "仅在此页面打开历史数据库，离开后立即关闭。"
            }
            (UiLanguage::SimplifiedChinese, AllPrompts) => "全部提示词",
            (UiLanguage::SimplifiedChinese, AllSources) => "全部来源",
            (UiLanguage::SimplifiedChinese, Selection) => "划词",
            (UiLanguage::SimplifiedChinese, Hover) => "悬停",
            (UiLanguage::SimplifiedChinese, Clipboard) => "剪贴板",
            (UiLanguage::SimplifiedChinese, Ocr) => "OCR",
            (UiLanguage::SimplifiedChinese, Newest) => "最新优先",
            (UiLanguage::SimplifiedChinese, Oldest) => "最早优先",
            (UiLanguage::SimplifiedChinese, English) => "English",
            (UiLanguage::SimplifiedChinese, SimplifiedChinese) => "简体中文",
        }
    }

    /// Every manager-authored status, validation error, and confirmation message is represented
    /// by this catalog before it is rendered.  Provider output, history fields, and OS/Rust error
    /// details are deliberately passed as opaque values and are never translated.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum StatusEvent<'a> {
        ManagerInitializationFailed {
            detail: &'a str,
        },
        ConfigLoadFailed {
            detail: &'a str,
        },
        LocalAppDataUnavailable {
            operation: StatusOperation,
        },
        SaveInterfaceLanguageFailed {
            detail: &'a str,
        },
        InterfaceLanguageSaved,
        HistoryRefreshed,
        HistoryUnavailable {
            detail: &'a str,
        },
        SelectHistoryEntry,
        HistoryEntryUnavailable,
        OutputCopied,
        CopyOutputFailed {
            detail: &'a str,
        },
        DeleteHistoryConfirm {
            target: &'a str,
        },
        DeletionCancelled,
        HistoryEntryDeleted,
        HistoryEntryAlreadyDeleted,
        DeleteHistoryFailed {
            detail: &'a str,
        },
        OutputTooLarge,
        ClipboardMemoryLockFailed,
        ResidentStart(ResidentStartOutcome),
        ConfigRefresh(RefreshOutcome),
        CredentialRefresh {
            outcome: RefreshOutcome,
            deleted: bool,
        },
        CannotSaveSettings {
            detail: &'a str,
        },
        EnterApiKey,
        ApiKeySavedToCredentialManager,
        ApiKeyInactiveTargetSaved,
        SaveApiKeyFailed {
            detail: &'a str,
        },
        NoSavedApiKey,
        ApiKeyInactiveTargetDeleted,
        DeleteApiKeyFailed {
            detail: &'a str,
        },
        NoPromptProfile,
        CannotSavePrompt {
            detail: &'a str,
        },
        NewPromptUnsaved,
        NewDraft,
        UnsavedPromptDiscarded,
        PromptInvalid {
            detail: &'a str,
        },
        InvalidTemperature,
        InvalidMaxOutputTokens,
        ConfigPathUnavailable,
        CredentialStatusPresent,
        CredentialStatusAbsent,
        CredentialStatusUnavailable {
            detail: &'a str,
        },
        HistoryCount {
            count: usize,
        },
        ProfilePosition {
            current: usize,
            total: usize,
        },
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum StatusOperation {
        Save,
        History,
    }

    fn status_text(language: UiLanguage, event: StatusEvent<'_>) -> String {
        use StatusEvent::*;
        match event {
            ManagerInitializationFailed { detail } => match language {
                UiLanguage::English => format!("Manager initialization failed: {detail}"),
                UiLanguage::SimplifiedChinese => format!("管理器初始化失败：{detail}"),
            },
            ConfigLoadFailed { detail } => match language {
                UiLanguage::English => format!("Could not load config: {detail}"),
                UiLanguage::SimplifiedChinese => format!("无法加载配置：{detail}"),
            },
            LocalAppDataUnavailable { operation } => match (language, operation) {
                (UiLanguage::English, StatusOperation::Save) => "LOCALAPPDATA is not available; changes cannot be saved".to_owned(),
                (UiLanguage::SimplifiedChinese, StatusOperation::Save) => "LOCALAPPDATA 不可用；无法保存更改".to_owned(),
                (UiLanguage::English, StatusOperation::History) => "LOCALAPPDATA is not available; history cannot be opened".to_owned(),
                (UiLanguage::SimplifiedChinese, StatusOperation::History) => "LOCALAPPDATA 不可用；无法打开历史记录".to_owned(),
            },
            SaveInterfaceLanguageFailed { detail } => match language {
                UiLanguage::English => format!("Could not save interface language: {detail}"),
                UiLanguage::SimplifiedChinese => format!("无法保存界面语言：{detail}"),
            },
            InterfaceLanguageSaved => match language {
                UiLanguage::English => "Interface language saved.".to_owned(),
                UiLanguage::SimplifiedChinese => "界面语言已保存。".to_owned(),
            },
            HistoryRefreshed => match language {
                UiLanguage::English => "History refreshed.".to_owned(),
                UiLanguage::SimplifiedChinese => "历史记录已刷新。".to_owned(),
            },
            HistoryUnavailable { detail } => match language {
                UiLanguage::English => format!("History unavailable: {detail}"),
                UiLanguage::SimplifiedChinese => format!("历史记录不可用：{detail}"),
            },
            SelectHistoryEntry => match language {
                UiLanguage::English => "Select a history entry first.".to_owned(),
                UiLanguage::SimplifiedChinese => "请先选择一条历史记录。".to_owned(),
            },
            HistoryEntryUnavailable => match language {
                UiLanguage::English => "The selected history entry is no longer available.".to_owned(),
                UiLanguage::SimplifiedChinese => "所选历史记录已不可用。".to_owned(),
            },
            OutputCopied => match language {
                UiLanguage::English => "Output copied to the clipboard.".to_owned(),
                UiLanguage::SimplifiedChinese => "输出已复制到剪贴板。".to_owned(),
            },
            CopyOutputFailed { detail } => match language {
                UiLanguage::English => format!("Could not copy output: {detail}"),
                UiLanguage::SimplifiedChinese => format!("无法复制输出：{detail}"),
            },
            DeleteHistoryConfirm { target } => match language {
                UiLanguage::English => format!("Delete this history entry?\n\n{target}"),
                UiLanguage::SimplifiedChinese => format!("删除这条历史记录？\n\n{target}"),
            },
            DeletionCancelled => match language {
                UiLanguage::English => "Deletion cancelled.".to_owned(),
                UiLanguage::SimplifiedChinese => "已取消删除。".to_owned(),
            },
            HistoryEntryDeleted => match language {
                UiLanguage::English => "History entry deleted.".to_owned(),
                UiLanguage::SimplifiedChinese => "历史记录已删除。".to_owned(),
            },
            HistoryEntryAlreadyDeleted => match language {
                UiLanguage::English => "The history entry was already deleted.".to_owned(),
                UiLanguage::SimplifiedChinese => "该历史记录已被删除。".to_owned(),
            },
            DeleteHistoryFailed { detail } => match language {
                UiLanguage::English => format!("Could not delete history entry: {detail}"),
                UiLanguage::SimplifiedChinese => format!("无法删除历史记录：{detail}"),
            },
            OutputTooLarge => match language {
                UiLanguage::English => "output is too large".to_owned(),
                UiLanguage::SimplifiedChinese => "输出内容过大".to_owned(),
            },
            ClipboardMemoryLockFailed => match language {
                UiLanguage::English => "could not lock clipboard memory".to_owned(),
                UiLanguage::SimplifiedChinese => "无法锁定剪贴板内存".to_owned(),
            },
            ResidentStart(outcome) => match (language, outcome) {
                (UiLanguage::English, ResidentStartOutcome::AlreadyRunning) => "Resident is running.".to_owned(),
                (UiLanguage::English, ResidentStartOutcome::Started) => "Resident started and is ready.".to_owned(),
                (UiLanguage::English, ResidentStartOutcome::Unavailable) => "Resident could not be reached; settings can be saved, but translation is unavailable.".to_owned(),
                (UiLanguage::SimplifiedChinese, ResidentStartOutcome::AlreadyRunning) => "驻留程序正在运行。".to_owned(),
                (UiLanguage::SimplifiedChinese, ResidentStartOutcome::Started) => "驻留程序已启动并准备就绪。".to_owned(),
                (UiLanguage::SimplifiedChinese, ResidentStartOutcome::Unavailable) => "无法连接驻留程序；可以保存设置，但翻译功能当前不可用。".to_owned(),
            },
            ConfigRefresh(outcome) => match (language, outcome) {
                (UiLanguage::English, RefreshOutcome::Acknowledged) => "Saved; the running resident confirmed the refresh.".to_owned(),
                (UiLanguage::English, RefreshOutcome::ResidentAbsent) => "Saved, but no running resident was found; translation is unavailable.".to_owned(),
                (UiLanguage::English, RefreshOutcome::Unacknowledged) => "Saved, but the resident did not confirm the refresh; restart the app before translating.".to_owned(),
                (UiLanguage::English, RefreshOutcome::Rejected) => "Saved, but the resident rejected the refresh; restart the app before translating.".to_owned(),
                (UiLanguage::SimplifiedChinese, RefreshOutcome::Acknowledged) => "已保存；正在运行的驻留程序已确认刷新。".to_owned(),
                (UiLanguage::SimplifiedChinese, RefreshOutcome::ResidentAbsent) => "已保存，但未找到正在运行的驻留程序；翻译功能不可用。".to_owned(),
                (UiLanguage::SimplifiedChinese, RefreshOutcome::Unacknowledged) => "已保存，但驻留程序未确认刷新；请在翻译前重启应用。".to_owned(),
                (UiLanguage::SimplifiedChinese, RefreshOutcome::Rejected) => "已保存，但驻留程序拒绝刷新；请在翻译前重启应用。".to_owned(),
            },
            CredentialRefresh { outcome, deleted } => {
                let (en, zh) = match (outcome, deleted) {
                    (RefreshOutcome::Acknowledged, false) => ("API key saved; the running resident confirmed the credential refresh.", "API 密钥已保存；正在运行的驻留程序已确认凭据刷新。"),
                    (RefreshOutcome::Acknowledged, true) => ("API key deleted; the running resident confirmed the credential refresh.", "API 密钥已删除；正在运行的驻留程序已确认凭据刷新。"),
                    (RefreshOutcome::ResidentAbsent, false) => ("API key saved, but no running resident was found; translation is unavailable.", "API 密钥已保存，但未找到正在运行的驻留程序；翻译功能不可用。"),
                    (RefreshOutcome::ResidentAbsent, true) => ("API key deleted, but no running resident was found; translation is unavailable.", "API 密钥已删除，但未找到正在运行的驻留程序；翻译功能不可用。"),
                    (RefreshOutcome::Unacknowledged | RefreshOutcome::Rejected, false) => ("API key saved, but the resident did not confirm the credential refresh; restart the app.", "API 密钥已保存，但驻留程序未确认凭据刷新；请重启应用。"),
                    (RefreshOutcome::Unacknowledged | RefreshOutcome::Rejected, true) => ("API key deleted, but the resident did not confirm the credential refresh; restart the app.", "API 密钥已删除，但驻留程序未确认凭据刷新；请重启应用。"),
                };
                match language { UiLanguage::English => en.to_owned(), UiLanguage::SimplifiedChinese => zh.to_owned() }
            }
            CannotSaveSettings { detail } => match language {
                UiLanguage::English => format!("Cannot save settings: {detail}"),
                UiLanguage::SimplifiedChinese => format!("无法保存设置：{detail}"),
            },
            EnterApiKey => match language { UiLanguage::English => "Enter a key before saving it.".to_owned(), UiLanguage::SimplifiedChinese => "请输入密钥后再保存。".to_owned() },
            ApiKeySavedToCredentialManager => match language { UiLanguage::English => "Saved in Windows Credential Manager.".to_owned(), UiLanguage::SimplifiedChinese => "已保存到 Windows 凭据管理器。".to_owned() },
            ApiKeyInactiveTargetSaved => match language { UiLanguage::English => "API key saved, but this credential target is not active; save settings to refresh the resident.".to_owned(), UiLanguage::SimplifiedChinese => "API 密钥已保存，但此凭据目标未启用；请保存设置以刷新驻留程序。".to_owned() },
            SaveApiKeyFailed { detail } => match language { UiLanguage::English => format!("Could not save API key: {detail}"), UiLanguage::SimplifiedChinese => format!("无法保存 API 密钥：{detail}") },
            NoSavedApiKey => match language { UiLanguage::English => "No saved key for this target.".to_owned(), UiLanguage::SimplifiedChinese => "此目标没有已保存的密钥。".to_owned() },
            ApiKeyInactiveTargetDeleted => match language { UiLanguage::English => "API key deleted, but this credential target is not active; the resident was not refreshed.".to_owned(), UiLanguage::SimplifiedChinese => "API 密钥已删除，但此凭据目标未启用；未刷新驻留程序。".to_owned() },
            DeleteApiKeyFailed { detail } => match language { UiLanguage::English => format!("Could not delete API key: {detail}"), UiLanguage::SimplifiedChinese => format!("无法删除 API 密钥：{detail}") },
            NoPromptProfile => match language { UiLanguage::English => "There is no prompt profile to save.".to_owned(), UiLanguage::SimplifiedChinese => "没有可保存的提示词配置。".to_owned() },
            CannotSavePrompt { detail } => match language { UiLanguage::English => format!("Cannot save prompt: {detail}"), UiLanguage::SimplifiedChinese => format!("无法保存提示词：{detail}") },
            NewPromptUnsaved => match language { UiLanguage::English => "New prompt is unsaved. Edit it, then choose Save prompt.".to_owned(), UiLanguage::SimplifiedChinese => "新提示词尚未保存。编辑后请选择“保存提示词”。".to_owned() },
            NewDraft => match language { UiLanguage::English => "New draft".to_owned(), UiLanguage::SimplifiedChinese => "新草稿".to_owned() },
            UnsavedPromptDiscarded => match language { UiLanguage::English => "Unsaved new prompt discarded; the saved configuration was unchanged.".to_owned(), UiLanguage::SimplifiedChinese => "已丢弃未保存的新提示词；已保存的配置未更改。".to_owned() },
            PromptInvalid { detail } => match language { UiLanguage::English => format!("Prompt is invalid: {detail}"), UiLanguage::SimplifiedChinese => format!("提示词无效：{detail}") },
            InvalidTemperature => match language { UiLanguage::English => "Temperature must be a number from 0 to 2.".to_owned(), UiLanguage::SimplifiedChinese => "温度必须是 0 到 2 之间的数字。".to_owned() },
            InvalidMaxOutputTokens => match language { UiLanguage::English => "Max output tokens must be a positive integer.".to_owned(), UiLanguage::SimplifiedChinese => "最大输出令牌数必须是正整数。".to_owned() },
            ConfigPathUnavailable => match language { UiLanguage::English => "LOCALAPPDATA is not available".to_owned(), UiLanguage::SimplifiedChinese => "LOCALAPPDATA 不可用".to_owned() },
            CredentialStatusPresent => match language { UiLanguage::English => "A saved key is present (value hidden).".to_owned(), UiLanguage::SimplifiedChinese => "存在已保存的密钥（值已隐藏）。".to_owned() },
            CredentialStatusAbsent => match language { UiLanguage::English => "No saved key for this target.".to_owned(), UiLanguage::SimplifiedChinese => "此目标没有已保存的密钥。".to_owned() },
            CredentialStatusUnavailable { detail } => match language { UiLanguage::English => format!("Credential status unavailable: {detail}"), UiLanguage::SimplifiedChinese => format!("凭据状态不可用：{detail}") },
            HistoryCount { count } => match language { UiLanguage::English if count == 1 => "1 entry".to_owned(), UiLanguage::English => format!("{count} entries"), UiLanguage::SimplifiedChinese => format!("{count} 条记录") },
            ProfilePosition { current, total } => match language { UiLanguage::English => format!("{current} of {total}"), UiLanguage::SimplifiedChinese => format!("第 {current} 个，共 {total} 个") },
        }
    }

    fn text_key_from_english(value: &str) -> Option<TextKey> {
        ALL_TEXT_KEYS
            .iter()
            .copied()
            .find(|key| ui_text(UiLanguage::English, *key) == value)
    }

    #[derive(Clone, Copy)]
    struct ManagerLayout {
        nav: RECT,
        page: RECT,
        status: RECT,
        nav_buttons: [RECT; 4],
    }

    impl ManagerLayout {
        fn for_client(width: i32, height: i32, dpi: u32) -> Self {
            let logical_width = unscale_from_dpi(width.max(1), dpi);
            let logical_height = unscale_from_dpi(height.max(1), dpi);
            let nav_width = NAV_WIDTH.min(logical_width);
            let button_left = 16;
            let button_right = (nav_width - 16).max(button_left + 1);
            let button = |top: i32| RECT {
                left: button_left,
                top,
                right: button_right,
                bottom: top + 42,
            };
            let close_top = (logical_height - 58).max(216);
            Self {
                nav: RECT {
                    left: 0,
                    top: 0,
                    right: nav_width,
                    bottom: logical_height,
                },
                page: RECT {
                    left: nav_width,
                    top: 0,
                    right: logical_width,
                    bottom: logical_height,
                },
                status: RECT {
                    left: 16,
                    top: 238,
                    right: button_right,
                    bottom: close_top - 12,
                },
                nav_buttons: [button(78), button(126), button(174), button(close_top)],
            }
        }
    }

    #[derive(Clone, Copy)]
    struct ControlPlacement {
        hwnd: HWND,
        view: View,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    }

    #[derive(Clone, Copy)]
    struct LocalizedControl {
        hwnd: HWND,
        key: TextKey,
    }

    struct ThemeResources {
        background: HBRUSH,
        nav: HBRUSH,
        surface: HBRUSH,
        body_font: HFONT,
        title_font: HFONT,
        label_font: HFONT,
    }

    impl ThemeResources {
        fn new(dpi: u32) -> Self {
            let face = w!("Segoe UI");
            let font = |size: i32, weight: i32| unsafe {
                CreateFontW(
                    -scale_for_dpi(size, dpi),
                    0,
                    0,
                    0,
                    weight,
                    0,
                    0,
                    0,
                    FONT_CHARSET(1),
                    FONT_OUTPUT_PRECISION(0),
                    FONT_CLIP_PRECISION(0),
                    FONT_QUALITY(5),
                    0,
                    face,
                )
            };
            Self {
                background: unsafe { CreateSolidBrush(MANAGER_BG) },
                nav: unsafe { CreateSolidBrush(NAV_BG) },
                surface: unsafe { CreateSolidBrush(SURFACE_BG) },
                body_font: font(14, 400),
                title_font: font(24, 600),
                label_font: font(12, 600),
            }
        }
    }

    impl Drop for ThemeResources {
        fn drop(&mut self) {
            unsafe {
                for object in [
                    HGDIOBJ(self.background.0),
                    HGDIOBJ(self.nav.0),
                    HGDIOBJ(self.surface.0),
                    HGDIOBJ(self.body_font.0),
                    HGDIOBJ(self.title_font.0),
                    HGDIOBJ(self.label_font.0),
                ] {
                    if !object.0.is_null() {
                        let _ = DeleteObject(object);
                    }
                }
            }
        }
    }

    struct Handles {
        settings_nav: HWND,
        prompts_nav: HWND,
        history_nav: HWND,
        close: HWND,
        language: HWND,
        settings_page: HWND,
        prompts_page: HWND,
        history_page: HWND,
        endpoint: HWND,
        model: HWND,
        credential_target: HWND,
        api_key: HWND,
        credential_status: HWND,
        settings_selection_default: HWND,
        settings_hover_default: HWND,
        prompt_selection_default: HWND,
        prompt_hover_default: HWND,
        profile_number: HWND,
        profile_id: HWND,
        profile_name: HWND,
        system_prompt: HWND,
        user_template: HWND,
        profile_model: HWND,
        temperature: HWND,
        max_tokens: HWND,
        prompt_status: HWND,
        history_search: HWND,
        history_prompt: HWND,
        history_source: HWND,
        history_order: HWND,
        history_list: HWND,
        history_target: HWND,
        history_context: HWND,
        history_output: HWND,
        history_meta: HWND,
        status: HWND,
        placements: Vec<ControlPlacement>,
        localized: Vec<LocalizedControl>,
    }

    impl Default for Handles {
        fn default() -> Self {
            let null = HWND(std::ptr::null_mut());
            Self {
                settings_nav: null,
                prompts_nav: null,
                history_nav: null,
                close: null,
                language: null,
                settings_page: null,
                prompts_page: null,
                history_page: null,
                endpoint: null,
                model: null,
                credential_target: null,
                api_key: null,
                credential_status: null,
                settings_selection_default: null,
                settings_hover_default: null,
                prompt_selection_default: null,
                prompt_hover_default: null,
                profile_number: null,
                profile_id: null,
                profile_name: null,
                system_prompt: null,
                user_template: null,
                profile_model: null,
                temperature: null,
                max_tokens: null,
                prompt_status: null,
                history_search: null,
                history_prompt: null,
                history_source: null,
                history_order: null,
                history_list: null,
                history_target: null,
                history_context: null,
                history_output: null,
                history_meta: null,
                status: null,
                placements: Vec::new(),
                localized: Vec::new(),
            }
        }
    }

    struct ManagerState {
        config: AppConfig,
        config_path: Option<PathBuf>,
        load_error: Option<String>,
        handles: Handles,
        view: View,
        profile_index: usize,
        /// A new profile is kept outside `config` until its explicit Save
        /// action succeeds. This prevents Settings saves or navigation from
        /// accidentally persisting an unfinished draft.
        draft_prompt: Option<PromptConfig>,
        history_entries: Vec<HistoryEntry>,
        history_loaded: bool,
        resident_start: ResidentStartOutcome,
        credential_status: CredentialStatusState,
        theme: ThemeResources,
        dpi: u32,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum CredentialStatusState {
        Present,
        Absent,
        Unavailable(String),
    }

    impl ManagerState {
        fn language(&self) -> UiLanguage {
            self.config.ui.manager_language
        }
    }

    struct Secret(String);
    impl Drop for Secret {
        fn drop(&mut self) {
            unsafe { self.0.as_mut_vec().fill(0) }
        }
    }

    pub fn run() -> windows::core::Result<()> {
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        let resident_start = ensure_resident_running();
        let instance = unsafe { GetModuleHandleW(None)? };
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: HINSTANCE(instance.0),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if unsafe { RegisterClassW(&class) } == 0 {
            return Err(Error::from_win32());
        }
        let page_class = WNDCLASSW {
            lpfnWndProc: Some(page_proc),
            hInstance: HINSTANCE(instance.0),
            lpszClassName: PAGE_CLASS,
            ..Default::default()
        };
        if unsafe { RegisterClassW(&page_class) } == 0 {
            return Err(Error::from_win32());
        }
        let (config, load_error) = load_config();
        let initial_language = config.ui.manager_language;
        let window_title = wide(ui_text(initial_language, TextKey::WindowTitle));
        let dpi = unsafe { GetDpiForSystem() }.max(DEFAULT_DPI);
        let state = Box::new(ManagerState {
            config,
            config_path: default_config_path(),
            load_error,
            handles: Handles::default(),
            view: View::Settings,
            profile_index: 0,
            draft_prompt: None,
            history_entries: Vec::new(),
            history_loaded: false,
            resident_start,
            credential_status: CredentialStatusState::Absent,
            theme: ThemeResources::new(dpi),
            dpi,
        });
        let state_ptr = Box::into_raw(state);
        let hwnd = unsafe {
            match CreateWindowExW(
                WS_EX_CONTROLPARENT,
                CLASS_NAME,
                PCWSTR(window_title.as_ptr()),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
                0,
                0,
                scale_for_dpi(980, dpi),
                scale_for_dpi(680, dpi),
                None,
                None,
                Some(HINSTANCE(instance.0)),
                Some(state_ptr.cast()),
            ) {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    drop(Box::from_raw(state_ptr));
                    return Err(error);
                }
            }
        };
        unsafe {
            let window_dpi = GetDpiForWindow(hwnd).max(DEFAULT_DPI);
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                scale_for_dpi(980, window_dpi),
                scale_for_dpi(680, window_dpi),
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
            let _ = ShowWindow(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::SW_SHOWDEFAULT,
            );
        }
        let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
        let result = loop {
            let result = unsafe { GetMessageW(&mut message, None, 0, 0) };
            if result.0 == -1 {
                break Err(Error::from_win32());
            }
            if result.0 == 0 {
                break Ok(());
            }
            unsafe {
                if IsDialogMessageW(hwnd, &message).as_bool() {
                    continue;
                }
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        };
        unsafe {
            let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ManagerState;
            if !raw.is_null() {
                let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(raw));
            }
        }
        result
    }

    fn load_config() -> (AppConfig, Option<String>) {
        let Some(path) = default_config_path() else {
            return (
                AppConfig::default(),
                Some(status_text(
                    UiLanguage::English,
                    StatusEvent::LocalAppDataUnavailable {
                        operation: StatusOperation::Save,
                    },
                )),
            );
        };
        if !path.exists() {
            return (AppConfig::default(), None);
        }
        match AppConfig::load(&path) {
            Ok(config) => (config, None),
            Err(error) => (
                AppConfig::default(),
                Some(status_text(
                    UiLanguage::English,
                    StatusEvent::ConfigLoadFailed {
                        detail: &error.to_string(),
                    },
                )),
            ),
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_CREATE {
            let create = &*(lparam.0 as *const CREATESTRUCTW);
            let state = create.lpCreateParams as *mut ManagerState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            let window_dpi = GetDpiForWindow(hwnd).max(DEFAULT_DPI);
            if (*state).dpi != window_dpi {
                (*state).dpi = window_dpi;
                (*state).theme = ThemeResources::new(window_dpi);
            }
            if let Err(error) = initialize_controls(hwnd, &mut *state) {
                set_status(
                    &*state,
                    &status_text(
                        (*state).language(),
                        StatusEvent::ManagerInitializationFailed {
                            detail: &error.to_string(),
                        },
                    ),
                );
            }
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                scale_for_dpi(980, window_dpi),
                scale_for_dpi(680, window_dpi),
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
            apply_manager_layout(hwnd, &*state);
            return LRESULT(0);
        }
        let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ManagerState;
        if state.is_null() {
            return DefWindowProcW(hwnd, message, wparam, lparam);
        }
        match message {
            WM_COMMAND => handle_command(hwnd, &mut *state, wparam.0),
            WM_SIZE => {
                apply_manager_layout(hwnd, &*state);
                LRESULT(0)
            }
            WM_DPICHANGED => {
                let dpi = ((wparam.0 >> 16) as u32).max(DEFAULT_DPI);
                let suggested = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    (suggested.right - suggested.left).max(1),
                    (suggested.bottom - suggested.top).max(1),
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
                (*state).dpi = dpi;
                (*state).theme = ThemeResources::new(dpi);
                apply_control_fonts(&*state);
                apply_manager_layout(hwnd, &*state);
                let _ = InvalidateRect(Some(hwnd), None, true);
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let limits = &mut *(lparam.0 as *mut MINMAXINFO);
                limits.ptMinTrackSize.x =
                    scale_for_dpi(NAV_WIDTH + MIN_CONTENT_WIDTH, (*state).dpi);
                limits.ptMinTrackSize.y = scale_for_dpi(MIN_CLIENT_HEIGHT + 40, (*state).dpi);
                LRESULT(0)
            }
            WM_PAINT => {
                paint_manager(hwnd, &*state);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_DRAWITEM => {
                if lparam.0 != 0 {
                    draw_manager_button(&*(lparam.0 as *const DRAWITEMSTRUCT), &*state);
                }
                LRESULT(1)
            }
            0x0133 | 0x0134 | 0x0135 | 0x0138 => {
                themed_control_color(&*state, message, wparam, lparam)
            }
            WM_CLOSE => {
                DestroyWindow(hwnd).ok();
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(state));
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, message, wparam, lparam),
        }
    }

    unsafe extern "system" fn page_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if matches!(
            message,
            WM_COMMAND | WM_NOTIFY | WM_DRAWITEM | 0x0133 | 0x0134 | 0x0135 | 0x0138
        ) {
            if let Ok(parent) = unsafe { windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd) }
            {
                return unsafe { SendMessageW(parent, message, Some(wparam), Some(lparam)) };
            }
        }
        if message == WM_PAINT {
            paint_page(hwnd);
            return LRESULT(0);
        }
        if message == WM_ERASEBKGND {
            return LRESULT(1);
        }
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    fn initialize_controls(hwnd: HWND, state: &mut ManagerState) -> windows::core::Result<()> {
        let mut h = Handles::default();
        h.settings_nav = add_button(
            hwnd,
            &mut h,
            ID_SETTINGS_TAB,
            "Settings",
            18,
            16,
            112,
            32,
            None,
        )?;
        h.prompts_nav = add_button(
            hwnd,
            &mut h,
            ID_PROMPTS_TAB,
            "Prompts",
            136,
            16,
            112,
            32,
            None,
        )?;
        h.history_nav = add_button(
            hwnd,
            &mut h,
            ID_HISTORY_TAB,
            "History",
            254,
            16,
            112,
            32,
            None,
        )?;
        h.close = add_button(hwnd, &mut h, ID_CLOSE, "Close", 652, 16, 100, 32, None)?;

        // Page controls live under one dedicated child container each.  This
        // makes tab switching a single parent visibility operation; hidden
        // controls can never paint over the newly selected page.
        h.settings_page = create_page_container(hwnd, ID_SETTINGS_PAGE)?;
        h.prompts_page = create_page_container(hwnd, ID_PROMPTS_PAGE)?;
        h.history_page = create_page_container(hwnd, ID_HISTORY_PAGE)?;

        add_label(
            hwnd,
            &mut h,
            "Interface language",
            486,
            24,
            122,
            22,
            Some(View::Settings),
        )?;
        h.language = add_combo(
            hwnd,
            &mut h,
            ID_LANGUAGE,
            610,
            18,
            130,
            180,
            Some(View::Settings),
        )?;

        add_label(
            hwnd,
            &mut h,
            "Provider endpoint",
            28,
            76,
            180,
            22,
            Some(View::Settings),
        )?;
        h.endpoint = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            220,
            72,
            520,
            26,
            Some(View::Settings),
            false,
            false,
            ID_SETTINGS_ENDPOINT,
        )?;
        add_label(
            hwnd,
            &mut h,
            "Model",
            28,
            116,
            180,
            22,
            Some(View::Settings),
        )?;
        h.model = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            220,
            112,
            520,
            26,
            Some(View::Settings),
            false,
            false,
            ID_SETTINGS_MODEL,
        )?;
        add_label(
            hwnd,
            &mut h,
            "Credential target",
            28,
            156,
            180,
            22,
            Some(View::Settings),
        )?;
        h.credential_target = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            220,
            152,
            520,
            26,
            Some(View::Settings),
            false,
            false,
            ID_SETTINGS_CREDENTIAL_TARGET,
        )?;
        add_label(
            hwnd,
            &mut h,
            "API key",
            28,
            196,
            180,
            22,
            Some(View::Settings),
        )?;
        h.api_key = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            220,
            192,
            380,
            26,
            Some(View::Settings),
            true,
            false,
            ID_SETTINGS_API_KEY,
        )?;
        add_button(
            hwnd,
            &mut h,
            ID_SAVE_KEY,
            "Save key",
            610,
            191,
            130,
            28,
            Some(View::Settings),
        )?;
        add_button(
            hwnd,
            &mut h,
            ID_DELETE_KEY,
            "Delete saved key",
            220,
            228,
            160,
            28,
            Some(View::Settings),
        )?;
        h.credential_status = add_label(hwnd, &mut h, "", 392, 232, 348, 22, Some(View::Settings))?;
        add_label(
            hwnd,
            &mut h,
            "Selection default profile",
            28,
            286,
            180,
            22,
            Some(View::Settings),
        )?;
        h.settings_selection_default = add_combo(
            hwnd,
            &mut h,
            ID_SETTINGS_SELECTION_DEFAULT,
            220,
            282,
            520,
            220,
            Some(View::Settings),
        )?;
        add_label(
            hwnd,
            &mut h,
            "Hover default profile",
            28,
            326,
            180,
            22,
            Some(View::Settings),
        )?;
        h.settings_hover_default = add_combo(
            hwnd,
            &mut h,
            ID_SETTINGS_HOVER_DEFAULT,
            220,
            322,
            520,
            220,
            Some(View::Settings),
        )?;
        add_button(
            hwnd,
            &mut h,
            ID_SAVE_SETTINGS,
            "Save settings",
            220,
            366,
            160,
            32,
            Some(View::Settings),
        )?;
        add_label(
            hwnd,
            &mut h,
            "Keys are held by Windows Credential Manager; they never enter config.toml.",
            28,
            420,
            712,
            36,
            Some(View::Settings),
        )?;

        add_label(hwnd, &mut h, "Profile", 28, 76, 80, 22, Some(View::Prompts))?;
        h.profile_number = add_label(hwnd, &mut h, "", 112, 76, 120, 22, Some(View::Prompts))?;
        add_button(
            hwnd,
            &mut h,
            ID_PREVIOUS_PROMPT,
            "Previous",
            270,
            70,
            100,
            30,
            Some(View::Prompts),
        )?;
        add_button(
            hwnd,
            &mut h,
            ID_NEXT_PROMPT,
            "Next",
            378,
            70,
            86,
            30,
            Some(View::Prompts),
        )?;
        add_button(
            hwnd,
            &mut h,
            ID_NEW_PROMPT,
            "New",
            472,
            70,
            86,
            30,
            Some(View::Prompts),
        )?;
        add_label(hwnd, &mut h, "ID", 28, 118, 180, 22, Some(View::Prompts))?;
        h.profile_id = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            220,
            114,
            520,
            26,
            Some(View::Prompts),
            false,
            false,
            ID_PROMPT_ID,
        )?;
        add_label(hwnd, &mut h, "Name", 28, 158, 180, 22, Some(View::Prompts))?;
        h.profile_name = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            220,
            154,
            520,
            26,
            Some(View::Prompts),
            false,
            false,
            ID_PROMPT_NAME,
        )?;
        add_label(
            hwnd,
            &mut h,
            "System prompt",
            28,
            198,
            180,
            22,
            Some(View::Prompts),
        )?;
        h.system_prompt = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            220,
            194,
            520,
            64,
            Some(View::Prompts),
            false,
            true,
            ID_PROMPT_SYSTEM,
        )?;
        add_label(
            hwnd,
            &mut h,
            "User template",
            28,
            278,
            180,
            22,
            Some(View::Prompts),
        )?;
        h.user_template = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            220,
            274,
            520,
            74,
            Some(View::Prompts),
            false,
            true,
            ID_PROMPT_USER_TEMPLATE,
        )?;
        add_label(
            hwnd,
            &mut h,
            "Model override",
            28,
            368,
            180,
            22,
            Some(View::Prompts),
        )?;
        h.profile_model = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            220,
            364,
            520,
            26,
            Some(View::Prompts),
            false,
            false,
            ID_PROMPT_MODEL,
        )?;
        add_label(
            hwnd,
            &mut h,
            "Temperature",
            28,
            408,
            180,
            22,
            Some(View::Prompts),
        )?;
        h.temperature = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            220,
            404,
            180,
            26,
            Some(View::Prompts),
            false,
            false,
            ID_PROMPT_TEMPERATURE,
        )?;
        add_label(
            hwnd,
            &mut h,
            "Max output tokens",
            424,
            408,
            150,
            22,
            Some(View::Prompts),
        )?;
        h.max_tokens = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            574,
            404,
            166,
            26,
            Some(View::Prompts),
            false,
            false,
            ID_PROMPT_MAX_TOKENS,
        )?;
        add_label(
            hwnd,
            &mut h,
            "Defaults: selection / hover",
            28,
            448,
            180,
            22,
            Some(View::Prompts),
        )?;
        h.prompt_selection_default = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            220,
            444,
            240,
            26,
            Some(View::Prompts),
            false,
            false,
            ID_PROMPT_SELECTION_DEFAULT,
        )?;
        h.prompt_hover_default = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            500,
            444,
            240,
            26,
            Some(View::Prompts),
            false,
            false,
            ID_PROMPT_HOVER_DEFAULT,
        )?;
        h.prompt_status = add_label(hwnd, &mut h, "", 28, 486, 712, 40, Some(View::Prompts))?;
        add_button(
            hwnd,
            &mut h,
            ID_SAVE_PROMPT,
            "Save prompt",
            220,
            536,
            160,
            32,
            Some(View::Prompts),
        )?;
        add_label(
            hwnd,
            &mut h,
            "Use {target}, {context}, and {source}; every user template needs {target}.",
            392,
            538,
            348,
            34,
            Some(View::Prompts),
        )?;

        add_label(
            hwnd,
            &mut h,
            "Search target/output",
            28,
            76,
            150,
            22,
            Some(View::History),
        )?;
        h.history_search = add_edit_with_id(
            hwnd,
            &mut h,
            "",
            178,
            72,
            330,
            26,
            Some(View::History),
            false,
            false,
            ID_HISTORY_SEARCH,
        )?;
        add_button(
            hwnd,
            &mut h,
            ID_HISTORY_REFRESH,
            "Refresh",
            520,
            70,
            100,
            30,
            Some(View::History),
        )?;
        add_button(
            hwnd,
            &mut h,
            ID_HISTORY_COPY,
            "Copy output",
            628,
            70,
            112,
            30,
            Some(View::History),
        )?;
        add_label(hwnd, &mut h, "Prompt", 28, 114, 70, 22, Some(View::History))?;
        h.history_prompt = add_combo(
            hwnd,
            &mut h,
            ID_HISTORY_PROMPT,
            100,
            110,
            220,
            260,
            Some(View::History),
        )?;
        add_label(
            hwnd,
            &mut h,
            "Source",
            340,
            114,
            70,
            22,
            Some(View::History),
        )?;
        h.history_source = add_combo(
            hwnd,
            &mut h,
            ID_HISTORY_SOURCE,
            410,
            110,
            150,
            260,
            Some(View::History),
        )?;
        add_label(hwnd, &mut h, "Order", 578, 114, 54, 22, Some(View::History))?;
        h.history_order = add_combo(
            hwnd,
            &mut h,
            ID_HISTORY_ORDER,
            632,
            110,
            108,
            260,
            Some(View::History),
        )?;
        h.history_list = add_list(hwnd, &mut h, 28, 148, 712, 156, Some(View::History))?;
        add_label(hwnd, &mut h, "Target", 28, 320, 75, 22, Some(View::History))?;
        h.history_target =
            add_readonly_edit(hwnd, &mut h, 103, 316, 637, 36, Some(View::History), true)?;
        add_label(
            hwnd,
            &mut h,
            "Context",
            28,
            364,
            75,
            22,
            Some(View::History),
        )?;
        h.history_context =
            add_readonly_edit(hwnd, &mut h, 103, 360, 637, 54, Some(View::History), true)?;
        add_label(hwnd, &mut h, "Output", 28, 426, 75, 22, Some(View::History))?;
        h.history_output =
            add_readonly_edit(hwnd, &mut h, 103, 422, 637, 92, Some(View::History), true)?;
        h.history_meta = add_label(hwnd, &mut h, "", 28, 522, 712, 24, Some(View::History))?;
        add_button(
            hwnd,
            &mut h,
            ID_HISTORY_DELETE,
            "Delete selected",
            28,
            552,
            140,
            30,
            Some(View::History),
        )?;
        add_label(
            hwnd,
            &mut h,
            "History is loaded only while this tab is open; the database is never held open.",
            184,
            554,
            556,
            28,
            Some(View::History),
        )?;
        h.status = add_label(hwnd, &mut h, "", 28, 584, 712, 28, None)?;
        state.handles = h;
        apply_control_fonts(state);
        apply_control_themes(state);
        apply_static_localization(hwnd, state);
        set_text(state.handles.endpoint, &state.config.provider.endpoint);
        set_text(state.handles.model, &state.config.provider.model);
        set_text(
            state.handles.credential_target,
            &state.config.provider.credential_target,
        );
        set_text(
            state.handles.prompt_selection_default,
            &state.config.defaults.selection,
        );
        set_text(
            state.handles.prompt_hover_default,
            &state.config.defaults.hover,
        );
        state.profile_index = 0;
        refresh_prompt_form(state);
        state.view = View::Settings;
        show_view(hwnd, state);
        refresh_settings_form(state);
        if let Some(error) = state.load_error.clone() {
            set_status(state, &error);
        } else {
            update_credential_status(state);
            set_status(
                state,
                &status_text(
                    state.language(),
                    StatusEvent::ResidentStart(state.resident_start),
                ),
            );
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_control(
        state: &mut Handles,
        hwnd: HWND,
        view: Option<View>,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    ) -> HWND {
        if let Some(view) = view {
            state.placements.push(ControlPlacement {
                hwnd,
                view,
                x,
                y,
                width,
                height,
            });
        }
        hwnd
    }

    fn page_parent(parent: HWND, h: &Handles, view: Option<View>, y: i32) -> (HWND, i32) {
        match view {
            Some(View::Settings) => (h.settings_page, y - PAGE_TOP),
            Some(View::Prompts) => (h.prompts_page, y - PAGE_TOP),
            Some(View::History) => (h.history_page, y - PAGE_TOP),
            None => (parent, y),
        }
    }

    const PAGE_TOP: i32 = 56;

    fn create_page_container(parent: HWND, id: usize) -> windows::core::Result<HWND> {
        create_control_with_style(
            parent,
            PAGE_CLASS,
            "",
            WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            0,
            PAGE_TOP,
            780,
            528,
            id,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn add_label(
        parent: HWND,
        h: &mut Handles,
        text: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        view: Option<View>,
    ) -> windows::core::Result<HWND> {
        let (parent, y) = page_parent(parent, h, view, y);
        let hwnd = create_control(
            parent,
            w!("STATIC"),
            text,
            WS_CHILD | visibility_style(view),
            x,
            y,
            width,
            height,
            0,
        )?;
        if let Some(key) = text_key_from_english(text) {
            h.localized.push(LocalizedControl { hwnd, key });
        }
        Ok(add_control(h, hwnd, view, x, y, width, height))
    }
    #[allow(clippy::too_many_arguments)]
    fn add_button(
        parent: HWND,
        h: &mut Handles,
        id: usize,
        text: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        view: Option<View>,
    ) -> windows::core::Result<HWND> {
        let (parent, y) = page_parent(parent, h, view, y);
        let hwnd = create_control(
            parent,
            w!("BUTTON"),
            text,
            WS_CHILD
                | visibility_style(view)
                | WS_TABSTOP
                | WINDOW_STYLE(BS_PUSHBUTTON as u32 | BS_OWNERDRAW as u32),
            x,
            y,
            width,
            height,
            id,
        )?;
        if let Some(key) = text_key_from_english(text) {
            h.localized.push(LocalizedControl { hwnd, key });
        }
        Ok(add_control(h, hwnd, view, x, y, width, height))
    }
    #[allow(clippy::too_many_arguments)]
    fn add_edit(
        parent: HWND,
        h: &mut Handles,
        text: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        view: Option<View>,
        password: bool,
        multiline: bool,
    ) -> windows::core::Result<HWND> {
        add_edit_with_id(
            parent, h, text, x, y, width, height, view, password, multiline, 0,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn add_edit_with_id(
        parent: HWND,
        h: &mut Handles,
        text: &str,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        view: Option<View>,
        password: bool,
        multiline: bool,
        id: usize,
    ) -> windows::core::Result<HWND> {
        let (parent, y) = page_parent(parent, h, view, y);
        let mut style = WS_CHILD
            | WS_CLIPSIBLINGS
            | visibility_style(view)
            | WS_TABSTOP
            | WS_BORDER
            | WINDOW_STYLE(ES_AUTOHSCROLL as u32);
        if password {
            style |= WINDOW_STYLE(ES_PASSWORD as u32);
        }
        if multiline {
            style |= WINDOW_STYLE(ES_MULTILINE as u32)
                | WINDOW_STYLE(ES_AUTOVSCROLL as u32)
                | WS_VSCROLL;
        }
        let hwnd = create_control(parent, w!("EDIT"), text, style, x, y, width, height, id)?;
        Ok(add_control(h, hwnd, view, x, y, width, height))
    }

    #[allow(clippy::too_many_arguments)]
    fn add_readonly_edit(
        parent: HWND,
        h: &mut Handles,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        view: Option<View>,
        multiline: bool,
    ) -> windows::core::Result<HWND> {
        let hwnd = add_edit(parent, h, "", x, y, width, height, view, false, multiline)?;
        unsafe {
            let _ = SendMessageW(hwnd, EM_SETREADONLY, Some(WPARAM(1)), Some(LPARAM(0)));
        }
        Ok(hwnd)
    }

    #[allow(clippy::too_many_arguments)]
    fn add_combo(
        parent: HWND,
        h: &mut Handles,
        id: usize,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        view: Option<View>,
    ) -> windows::core::Result<HWND> {
        let (parent, y) = page_parent(parent, h, view, y);
        let style = WS_CHILD
            | WS_CLIPSIBLINGS
            | visibility_style(view)
            | WS_TABSTOP
            | WS_VSCROLL
            | WINDOW_STYLE(CBS_DROPDOWNLIST)
            | WS_BORDER;
        let hwnd =
            create_control_with_style(parent, w!("COMBOBOX"), "", style, x, y, width, height, id)?;
        Ok(add_control(h, hwnd, view, x, y, width, height))
    }

    #[allow(clippy::too_many_arguments)]
    fn add_list(
        parent: HWND,
        h: &mut Handles,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        view: Option<View>,
    ) -> windows::core::Result<HWND> {
        let (parent, y) = page_parent(parent, h, view, y);
        let style = WS_CHILD
            | WS_CLIPSIBLINGS
            | visibility_style(view)
            | WS_TABSTOP
            | WS_BORDER
            | WS_VSCROLL
            | WINDOW_STYLE(LBS_NOTIFY);
        let hwnd = create_control_with_style(
            parent,
            w!("LISTBOX"),
            "",
            style,
            x,
            y,
            width,
            height,
            ID_HISTORY_LIST,
        )?;
        Ok(add_control(h, hwnd, view, x, y, width, height))
    }
    #[allow(clippy::too_many_arguments)]
    fn create_control(
        parent: HWND,
        class: PCWSTR,
        text: &str,
        style: windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        id: usize,
    ) -> windows::core::Result<HWND> {
        let text = wide(text);
        let style = style | WS_CLIPSIBLINGS;
        let dpi = unsafe { GetDpiForWindow(parent) }.max(96);
        let x = scale_for_dpi(x, dpi);
        let y = scale_for_dpi(y, dpi);
        let width = scale_for_dpi(width, dpi);
        let height = scale_for_dpi(height, dpi);
        unsafe {
            let hwnd = CreateWindowExW(
                Default::default(),
                class,
                PCWSTR(text.as_ptr()),
                style,
                x,
                y,
                width,
                height,
                Some(parent),
                if id == 0 {
                    None
                } else {
                    Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                        id as *mut c_void,
                    ))
                },
                None,
                None,
            )?;
            let _ = SetWindowTheme(hwnd, w!("DarkMode_Explorer"), PCWSTR::null());
            Ok(hwnd)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create_control_with_style(
        parent: HWND,
        class: PCWSTR,
        text: &str,
        style: windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        id: usize,
    ) -> windows::core::Result<HWND> {
        let text = wide(text);
        let dpi = unsafe { GetDpiForWindow(parent) }.max(96);
        let x = scale_for_dpi(x, dpi);
        let y = scale_for_dpi(y, dpi);
        let width = scale_for_dpi(width, dpi);
        let height = scale_for_dpi(height, dpi);
        unsafe {
            let extended_style = if class == PAGE_CLASS {
                WS_EX_CONTROLPARENT
            } else {
                Default::default()
            };
            let hwnd = CreateWindowExW(
                extended_style,
                class,
                PCWSTR(text.as_ptr()),
                style,
                x,
                y,
                width,
                height,
                Some(parent),
                if id == 0 {
                    None
                } else {
                    Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                        id as *mut c_void,
                    ))
                },
                None,
                None,
            )?;
            let _ = SetWindowTheme(hwnd, w!("DarkMode_Explorer"), PCWSTR::null());
            Ok(hwnd)
        }
    }

    fn scale_for_dpi(value: i32, dpi: u32) -> i32 {
        ((value as i64 * dpi.max(96) as i64) / 96).clamp(1, i32::MAX as i64) as i32
    }

    fn unscale_from_dpi(value: i32, dpi: u32) -> i32 {
        ((value as i64 * 96) / dpi.max(96) as i64).clamp(1, i32::MAX as i64) as i32
    }

    fn scaled_rect(rect: RECT, dpi: u32) -> RECT {
        RECT {
            left: scale_for_dpi(rect.left, dpi),
            top: scale_for_dpi(rect.top, dpi),
            right: scale_for_dpi(rect.right, dpi),
            bottom: scale_for_dpi(rect.bottom, dpi),
        }
    }

    fn place_control(hwnd: HWND, rect: RECT, dpi: u32) {
        let rect = scaled_rect(rect, dpi);
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                rect.left,
                rect.top,
                (rect.right - rect.left).max(1),
                (rect.bottom - rect.top).max(1),
                SWP_NOACTIVATE | SWP_NOZORDER,
            );
        }
    }

    fn apply_manager_layout(hwnd: HWND, state: &ManagerState) {
        let mut client = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut client) }.is_err() {
            return;
        }
        let layout = ManagerLayout::for_client(client.right, client.bottom, state.dpi);
        for (control, rect) in [
            (state.handles.settings_nav, layout.nav_buttons[0]),
            (state.handles.prompts_nav, layout.nav_buttons[1]),
            (state.handles.history_nav, layout.nav_buttons[2]),
            (state.handles.close, layout.nav_buttons[3]),
            (state.handles.status, layout.status),
        ] {
            place_control(control, rect, state.dpi);
        }
        let page_rect = scaled_rect(layout.page, state.dpi);
        for page in [
            state.handles.settings_page,
            state.handles.prompts_page,
            state.handles.history_page,
        ] {
            unsafe {
                let _ = SetWindowPos(
                    page,
                    None,
                    page_rect.left,
                    page_rect.top,
                    (page_rect.right - page_rect.left).max(1),
                    (page_rect.bottom - page_rect.top).max(1),
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
        }
        for placement in &state.handles.placements {
            let _ = placement.view;
            place_control(
                placement.hwnd,
                RECT {
                    left: placement.x,
                    top: placement.y + PAGE_HEADER_HEIGHT,
                    right: placement.x + placement.width,
                    bottom: placement.y + PAGE_HEADER_HEIGHT + placement.height,
                },
                state.dpi,
            );
        }
        unsafe {
            for button in [
                state.handles.settings_nav,
                state.handles.prompts_nav,
                state.handles.history_nav,
            ] {
                let _ = InvalidateRect(Some(button), None, true);
                let _ = UpdateWindow(button);
            }
            let _ = InvalidateRect(Some(hwnd), None, true);
            for page in [
                state.handles.settings_page,
                state.handles.prompts_page,
                state.handles.history_page,
            ] {
                let _ = InvalidateRect(Some(page), None, true);
            }
        }
    }

    fn apply_control_fonts(state: &ManagerState) {
        let body = state.theme.body_font;
        for hwnd in [
            state.handles.settings_nav,
            state.handles.prompts_nav,
            state.handles.history_nav,
            state.handles.close,
            state.handles.status,
        ]
        .into_iter()
        .chain(state.handles.placements.iter().map(|item| item.hwnd))
        {
            unsafe {
                let _ = SendMessageW(
                    hwnd,
                    WM_SETFONT,
                    Some(WPARAM(body.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }
    }

    fn apply_control_themes(state: &ManagerState) {
        for hwnd in [
            state.handles.settings_nav,
            state.handles.prompts_nav,
            state.handles.history_nav,
            state.handles.close,
            state.handles.status,
        ]
        .into_iter()
        .chain(state.handles.placements.iter().map(|item| item.hwnd))
        {
            unsafe {
                let _ = SetWindowTheme(hwnd, w!("DarkMode_Explorer"), PCWSTR::null());
            }
        }
        // COMBOBOX does not inherit the dark palette from its parent.  Apply the dark Explorer
        // part explicitly to both the closed control and its lazily-created drop-down list; the
        // latter is why merely handling WM_CTLCOLOR* still leaves a light arrow/list on some
        // Windows builds.
        for combo in [
            state.handles.language,
            state.handles.history_prompt,
            state.handles.history_source,
            state.handles.history_order,
        ] {
            unsafe {
                let _ = SetWindowTheme(combo, w!("DarkMode_Explorer"), PCWSTR::null());
                let _ = InvalidateRect(Some(combo), None, true);
            }
        }
    }

    fn paint_manager(hwnd: HWND, state: &ManagerState) {
        let mut paint = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
        let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
        let mut client = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut client) }.is_ok() {
            unsafe {
                let _ = FillRect(hdc, &client, state.theme.background);
            }
            let layout = ManagerLayout::for_client(client.right, client.bottom, state.dpi);
            let nav = scaled_rect(layout.nav, state.dpi);
            unsafe {
                let _ = FillRect(hdc, &nav, state.theme.nav);
            }
            let mut brand = scaled_rect(
                RECT {
                    left: 16,
                    top: 18,
                    right: NAV_WIDTH - 12,
                    bottom: 48,
                },
                state.dpi,
            );
            let mut descriptor = scaled_rect(
                RECT {
                    left: 16,
                    top: 50,
                    right: NAV_WIDTH - 12,
                    bottom: 69,
                },
                state.dpi,
            );
            let old_font = unsafe { SelectObject(hdc, HGDIOBJ(state.theme.label_font.0)) };
            unsafe {
                SetBkColor(hdc, NAV_BG);
                SetTextColor(hdc, TEXT);
                let mut text: Vec<u16> = ui_text(state.language(), TextKey::Brand)
                    .encode_utf16()
                    .collect();
                let _ = DrawTextW(hdc, &mut text, &mut brand, DRAW_TEXT_FORMAT(0x0100));
                SetTextColor(hdc, MUTED);
                let mut text: Vec<u16> = ui_text(state.language(), TextKey::Manager)
                    .encode_utf16()
                    .collect();
                let _ = DrawTextW(hdc, &mut text, &mut descriptor, DRAW_TEXT_FORMAT(0x0100));
                let _ = SelectObject(hdc, old_font);
            }
        }
        unsafe {
            let _ = EndPaint(hwnd, &paint);
        }
    }

    fn page_identity(state: &ManagerState, hwnd: HWND) -> Option<View> {
        if hwnd == state.handles.settings_page {
            Some(View::Settings)
        } else if hwnd == state.handles.prompts_page {
            Some(View::Prompts)
        } else if hwnd == state.handles.history_page {
            Some(View::History)
        } else {
            None
        }
    }

    fn paint_page(hwnd: HWND) {
        let Ok(parent) = (unsafe { GetParent(hwnd) }) else {
            return;
        };
        let state_ptr = unsafe { GetWindowLongPtrW(parent, GWLP_USERDATA) as *const ManagerState };
        if state_ptr.is_null() {
            return;
        }
        let state = unsafe { &*state_ptr };
        let mut paint = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
        let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
        let mut client = RECT::default();
        if unsafe { GetClientRect(hwnd, &mut client) }.is_ok() {
            unsafe {
                let _ = FillRect(hdc, &client, state.theme.background);
            }
            let (title_key, subtitle_key) = match page_identity(state, hwnd) {
                Some(View::Settings) => (TextKey::Settings, TextKey::SettingsSubtitle),
                Some(View::Prompts) => (TextKey::Prompts, TextKey::PromptsSubtitle),
                Some(View::History) => (TextKey::History, TextKey::HistorySubtitle),
                None => {
                    return unsafe {
                        let _ = EndPaint(hwnd, &paint);
                    }
                }
            };
            let title = ui_text(state.language(), title_key);
            let subtitle = ui_text(state.language(), subtitle_key);
            let mut title_rect = scaled_rect(
                RECT {
                    left: 28,
                    top: 14,
                    right: 748,
                    bottom: 43,
                },
                state.dpi,
            );
            let mut subtitle_rect = scaled_rect(
                RECT {
                    left: 30,
                    top: 43,
                    right: 748,
                    bottom: 64,
                },
                state.dpi,
            );
            unsafe {
                let old = SelectObject(hdc, HGDIOBJ(state.theme.title_font.0));
                SetBkColor(hdc, MANAGER_BG);
                SetTextColor(hdc, TEXT);
                let mut text: Vec<u16> = title.encode_utf16().collect();
                let _ = DrawTextW(hdc, &mut text, &mut title_rect, DRAW_TEXT_FORMAT(0x0100));
                let _ = SelectObject(hdc, HGDIOBJ(state.theme.body_font.0));
                SetTextColor(hdc, MUTED);
                let mut text: Vec<u16> = subtitle.encode_utf16().collect();
                let _ = DrawTextW(hdc, &mut text, &mut subtitle_rect, DRAW_TEXT_FORMAT(0x0100));
                let _ = SelectObject(hdc, old);
            }
        }
        unsafe {
            let _ = EndPaint(hwnd, &paint);
        }
    }

    fn themed_control_color(
        state: &ManagerState,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut _);
        let child = HWND(lparam.0 as *mut c_void);
        let in_navigation = child == state.handles.status;
        let is_label = message == 0x0138;
        let background = if in_navigation {
            NAV_BG
        } else if is_label {
            MANAGER_BG
        } else {
            SURFACE_BG
        };
        unsafe {
            SetTextColor(hdc, if in_navigation { MUTED } else { TEXT });
            SetBkColor(hdc, background);
        }
        LRESULT(if in_navigation {
            state.theme.nav.0 as isize
        } else if is_label {
            state.theme.background.0 as isize
        } else {
            state.theme.surface.0 as isize
        })
    }

    fn draw_manager_button(item: &DRAWITEMSTRUCT, state: &ManagerState) {
        if item.CtlType != ODT_BUTTON {
            return;
        }
        let pressed = item.itemState.0 & 0x0001 != 0;
        let disabled = item.itemState.0 & 0x0004 != 0;
        let focused = item.itemState.0 & 0x0010 != 0;
        let selected_nav = matches!(
            (item.CtlID as usize, state.view),
            (ID_SETTINGS_TAB, View::Settings)
                | (ID_PROMPTS_TAB, View::Prompts)
                | (ID_HISTORY_TAB, View::History)
        );
        let destructive = matches!(item.CtlID as usize, ID_DELETE_KEY | ID_HISTORY_DELETE);
        let primary = matches!(
            item.CtlID as usize,
            ID_SAVE_SETTINGS | ID_SAVE_KEY | ID_SAVE_PROMPT | ID_HISTORY_REFRESH
        );
        let fill = if disabled {
            MANAGER_BG
        } else if pressed {
            SURFACE_HOVER
        } else if selected_nav || primary {
            ACCENT
        } else {
            SURFACE_BG
        };
        let text_color = if destructive { DANGER } else { TEXT };
        let border_color = if focused || selected_nav {
            ACCENT
        } else {
            BORDER
        };
        let fill_brush = unsafe { CreateSolidBrush(fill) };
        let border_brush = unsafe { CreateSolidBrush(border_color) };
        let mut rect = item.rcItem;
        let radius = scale_for_dpi(8, state.dpi).max(2);
        let region = unsafe {
            CreateRoundRectRgn(
                rect.left,
                rect.top,
                rect.right + 1,
                rect.bottom + 1,
                radius,
                radius,
            )
        };
        unsafe {
            if !region.0.is_null() {
                let _ = FillRgn(item.hDC, region, fill_brush);
                let _ = FrameRgn(item.hDC, region, border_brush, 1, 1);
                let _ = DeleteObject(region.into());
            } else {
                let _ = FillRect(item.hDC, &rect, fill_brush);
            }
            let _ = DeleteObject(fill_brush.into());
            let _ = DeleteObject(border_brush.into());
            let length = GetWindowTextLengthW(item.hwndItem).max(0) as usize;
            let mut text = vec![0u16; length + 1];
            let written = GetWindowTextW(item.hwndItem, &mut text).max(0) as usize;
            let font = SendMessageW(item.hwndItem, 0x0031, Some(WPARAM(0)), Some(LPARAM(0)));
            let old = if font.0 != 0 {
                Some(SelectObject(item.hDC, HGDIOBJ(font.0 as *mut _)))
            } else {
                None
            };
            SetBkColor(item.hDC, fill);
            SetTextColor(item.hDC, text_color);
            let drawn = written.min(text.len());
            let _ = DrawTextW(
                item.hDC,
                &mut text[..drawn],
                &mut rect,
                DRAW_TEXT_FORMAT(0x0001 | 0x0020 | 0x0100),
            );
            if focused {
                rect.left += scale_for_dpi(4, state.dpi);
                rect.top += scale_for_dpi(4, state.dpi);
                rect.right -= scale_for_dpi(4, state.dpi);
                rect.bottom -= scale_for_dpi(4, state.dpi);
                let _ = DrawFocusRect(item.hDC, &rect);
            }
            if let Some(old) = old {
                let _ = SelectObject(item.hDC, old);
            }
        }
    }

    /// Page children are visible within their initially hidden container.
    /// Exactly one container is shown by `show_view` after initialization.
    fn visibility_style(view: Option<View>) -> WINDOW_STYLE {
        let _ = view;
        WS_VISIBLE
    }

    fn handle_command(hwnd: HWND, state: &mut ManagerState, command: usize) -> LRESULT {
        let id = command & 0xffff;
        let notification = (command >> 16) & 0xffff;
        if id == ID_HISTORY_LIST && notification == LBN_SELCHANGE {
            history_selection_changed(state);
            return LRESULT(0);
        }
        if state.view == View::History
            && notification == CBN_SELCHANGE
            && matches!(id, ID_HISTORY_PROMPT | ID_HISTORY_SOURCE | ID_HISTORY_ORDER)
        {
            refresh_history(state);
            return LRESULT(0);
        }
        if id == ID_LANGUAGE && notification == CBN_SELCHANGE {
            save_manager_language(hwnd, state);
            return LRESULT(0);
        }
        match id {
            ID_SETTINGS_TAB => switch_view(hwnd, state, View::Settings),
            ID_PROMPTS_TAB => switch_view(hwnd, state, View::Prompts),
            ID_HISTORY_TAB => switch_view(hwnd, state, View::History),
            ID_CLOSE => unsafe {
                DestroyWindow(hwnd).ok();
            },
            ID_SAVE_SETTINGS => save_settings(state),
            ID_SAVE_KEY => save_key(state),
            ID_DELETE_KEY => delete_key(state),
            ID_SAVE_PROMPT => save_prompt(state),
            ID_NEW_PROMPT => new_prompt(state),
            ID_PREVIOUS_PROMPT => previous_prompt(state),
            ID_NEXT_PROMPT => next_prompt(state),
            ID_HISTORY_REFRESH => refresh_history(state),
            ID_HISTORY_COPY => copy_history_output(hwnd, state),
            ID_HISTORY_DELETE => delete_history(hwnd, state),
            _ => {}
        }
        LRESULT(0)
    }
    fn switch_view(hwnd: HWND, state: &mut ManagerState, view: View) {
        if state.view == View::Prompts && view != View::Prompts {
            discard_draft(state);
        }
        state.view = view;
        show_view(hwnd, state);
        if view == View::Settings {
            refresh_settings_form(state);
            update_credential_status(state);
        } else if view == View::History {
            refresh_history(state);
        }
    }
    fn show_view(hwnd: HWND, state: &ManagerState) {
        // Switch visibility at the page-container boundary.  The page
        // containers own every page-specific child, so there is no stale
        // per-control visibility state that can leak across tabs.
        for (page, visible) in [
            (View::Settings, state.handles.settings_page),
            (View::Prompts, state.handles.prompts_page),
            (View::History, state.handles.history_page),
        ] {
            set_page_visibility(visible, page_is_visible(page, state.view));
        }
        unsafe {
            for button in [
                state.handles.settings_nav,
                state.handles.prompts_nav,
                state.handles.history_nav,
            ] {
                let _ = InvalidateRect(Some(button), None, true);
                let _ = UpdateWindow(button);
            }
            let _ = InvalidateRect(Some(hwnd), None, true);
            let _ = UpdateWindow(hwnd);
        }
    }

    fn set_page_visibility(hwnd: HWND, visible: bool) {
        unsafe {
            // ShowWindow owns the WS_VISIBLE transition. Mutating that style
            // bit first makes ShowWindow believe the requested state is
            // already active, which can leave a hidden page unpainted or an
            // old page's pixels on screen.
            let _ = ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
        }
    }

    /// Exactly one page container is visible after every tab selection.
    fn page_is_visible(page: View, selected_view: View) -> bool {
        page == selected_view
    }

    fn populate_language_selector(state: &ManagerState) {
        reset_combo(state.handles.language);
        for key in [TextKey::English, TextKey::SimplifiedChinese] {
            add_combo_string(state.handles.language, ui_text(state.language(), key));
        }
        set_combo_selection(
            state.handles.language,
            match state.language() {
                UiLanguage::English => 0,
                UiLanguage::SimplifiedChinese => 1,
            },
        );
    }

    fn apply_static_localization(hwnd: HWND, state: &ManagerState) {
        set_text(hwnd, ui_text(state.language(), TextKey::WindowTitle));
        for control in &state.handles.localized {
            set_text(control.hwnd, ui_text(state.language(), control.key));
            unsafe {
                let _ = InvalidateRect(Some(control.hwnd), None, true);
            }
        }
        populate_language_selector(state);
        populate_history_filters(state);
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, true);
            for page in [
                state.handles.settings_page,
                state.handles.prompts_page,
                state.handles.history_page,
            ] {
                let _ = InvalidateRect(Some(page), None, true);
            }
        }
    }

    fn save_manager_language(hwnd: HWND, state: &mut ManagerState) {
        let next_language = match combo_selection(state.handles.language) {
            Some(0) => UiLanguage::English,
            Some(1) => UiLanguage::SimplifiedChinese,
            _ => {
                populate_language_selector(state);
                return;
            }
        };
        if next_language == state.language() {
            return;
        }

        // Clone the last saved model and change only the presentation field.
        // Reading any Settings/Prompts controls here would accidentally save
        // unrelated edits that the user has not committed yet.
        let next = config_with_manager_language(&state.config, next_language);
        if let Err(error) = save_config(state, &next) {
            populate_language_selector(state);
            set_status(
                state,
                &status_text(
                    state.language(),
                    StatusEvent::SaveInterfaceLanguageFailed {
                        detail: &error.to_string(),
                    },
                ),
            );
            return;
        }

        state.config = next;
        apply_static_localization(hwnd, state);
        refresh_profile_number(state);
        relabel_credential_status(state);
        if selected_history_index(state.handles.history_list, state.history_entries.len()).is_some()
        {
            history_selection_changed(state);
        } else {
            clear_history_detail(state);
        }
        // Keep any prompt validation/save result visible while relabeling controls.  It may be a
        // transient message in the previous language, but clearing it makes a language switch
        // look like the operation was lost.
        set_status(
            state,
            &status_text(state.language(), StatusEvent::InterfaceLanguageSaved),
        );
    }

    fn config_with_manager_language(config: &AppConfig, language: UiLanguage) -> AppConfig {
        let mut next = config.clone();
        next.ui.manager_language = language;
        next
    }

    fn populate_history_filters(state: &ManagerState) {
        let prompt_selection = combo_selection(state.handles.history_prompt).unwrap_or(0);
        let source_selection = combo_selection(state.handles.history_source).unwrap_or(0);
        let order_selection = combo_selection(state.handles.history_order).unwrap_or(0);
        reset_combo(state.handles.history_prompt);
        add_combo_string(
            state.handles.history_prompt,
            ui_text(state.language(), TextKey::AllPrompts),
        );
        for profile in &state.config.profiles {
            add_combo_string(
                state.handles.history_prompt,
                &format!("{} — {}", profile.id, profile.name),
            );
        }
        set_combo_selection(
            state.handles.history_prompt,
            prompt_selection.min(state.config.profiles.len()),
        );

        reset_combo(state.handles.history_source);
        for key in [
            TextKey::AllSources,
            TextKey::Selection,
            TextKey::Hover,
            TextKey::Clipboard,
            TextKey::Ocr,
        ] {
            add_combo_string(state.handles.history_source, ui_text(state.language(), key));
        }
        set_combo_selection(state.handles.history_source, source_selection.min(4));

        reset_combo(state.handles.history_order);
        add_combo_string(
            state.handles.history_order,
            ui_text(state.language(), TextKey::Newest),
        );
        add_combo_string(
            state.handles.history_order,
            ui_text(state.language(), TextKey::Oldest),
        );
        set_combo_selection(state.handles.history_order, order_selection.min(1));
    }

    /// Rebuild the Settings-page default selectors from the saved profile list.
    ///
    /// The combo item text is presentation-only: the selected item's index is
    /// resolved back to the profile ID when settings are saved. This keeps the
    /// TOML format stable while letting users choose profiles by their friendly
    /// names instead of memorizing IDs.
    fn populate_profile_selectors(state: &ManagerState) {
        let selection =
            selected_profile_index(&state.config.profiles, &state.config.defaults.selection);
        let hover = selected_profile_index(&state.config.profiles, &state.config.defaults.hover);
        for (hwnd, selected) in [
            (state.handles.settings_selection_default, selection),
            (state.handles.settings_hover_default, hover),
        ] {
            reset_combo(hwnd);
            for profile in &state.config.profiles {
                add_combo_string(hwnd, &profile_option_label(profile));
            }
            if let Some(index) = selected {
                set_combo_selection(hwnd, index);
            }
        }
    }

    fn selected_profile_index(profiles: &[PromptConfig], id: &str) -> Option<usize> {
        profiles.iter().position(|profile| profile.id == id)
    }

    fn profile_option_label(profile: &PromptConfig) -> String {
        let name = profile.name.trim();
        if name.is_empty() {
            profile.id.clone()
        } else {
            format!("{name} — {}", profile.id)
        }
    }

    fn selected_profile_id(hwnd: HWND, profiles: &[PromptConfig]) -> Option<String> {
        profile_id_for_index(combo_selection(hwnd), profiles)
    }

    fn profile_id_for_index(index: Option<usize>, profiles: &[PromptConfig]) -> Option<String> {
        index
            .and_then(|index| profiles.get(index))
            .map(|profile| profile.id.clone())
    }

    fn refresh_history(state: &mut ManagerState) {
        state.history_loaded = true;
        let query = history_query(state);
        let result = default_history_path()
            .ok_or_else(|| {
                status_text(
                    state.language(),
                    StatusEvent::LocalAppDataUnavailable {
                        operation: StatusOperation::History,
                    },
                )
            })
            .and_then(|path| HistoryDatabase::open(path).map_err(|error| error.to_string()))
            .and_then(|database| database.search(&query).map_err(|error| error.to_string()));
        match result {
            Ok(entries) => {
                state.history_entries = entries;
                populate_history_list(state);
                set_text(
                    state.handles.history_meta,
                    &status_text(
                        state.language(),
                        StatusEvent::HistoryCount {
                            count: state.history_entries.len(),
                        },
                    ),
                );
                set_status(
                    state,
                    &status_text(state.language(), StatusEvent::HistoryRefreshed),
                );
            }
            Err(error) => {
                state.history_entries.clear();
                populate_history_list(state);
                clear_history_detail(state);
                let message = status_text(
                    state.language(),
                    StatusEvent::HistoryUnavailable { detail: &error },
                );
                set_text(state.handles.history_meta, &message);
                set_status(state, &message);
            }
        }
    }

    fn history_query(state: &ManagerState) -> HistoryQuery {
        let search = nonempty(read_text(state.handles.history_search));
        let prompt_id = selected_history_prompt(state);
        let source = selected_history_source(state);
        let order = match combo_selection(state.handles.history_order) {
            Some(1) => HistoryOrder::OldestFirst,
            _ => HistoryOrder::NewestFirst,
        };
        HistoryQuery {
            search,
            prompt_id,
            source,
            order,
            ..HistoryQuery::default()
        }
    }

    fn selected_history_prompt(state: &ManagerState) -> Option<String> {
        let index = combo_selection(state.handles.history_prompt)?;
        index.checked_sub(1).and_then(|profile_index| {
            state
                .config
                .profiles
                .get(profile_index)
                .map(|profile| profile.id.clone())
        })
    }

    fn selected_history_source(state: &ManagerState) -> Option<ExtractionSource> {
        history_source_for_index(combo_selection(state.handles.history_source))
    }

    fn history_source_for_index(index: Option<usize>) -> Option<ExtractionSource> {
        match index {
            Some(1) => Some(ExtractionSource::UiaSelection),
            Some(2) => Some(ExtractionSource::UiaPoint),
            Some(3) => Some(ExtractionSource::Clipboard),
            Some(4) => Some(ExtractionSource::Ocr),
            _ => None,
        }
    }

    fn populate_history_list(state: &ManagerState) {
        unsafe {
            let _ = SendMessageW(
                state.handles.history_list,
                LB_RESETCONTENT,
                Some(WPARAM(0)),
                Some(LPARAM(0)),
            );
        }
        for entry in &state.history_entries {
            add_list_string(state.handles.history_list, &format_history_row(entry));
        }
        clear_history_detail(state);
    }

    fn history_selection_changed(state: &mut ManagerState) {
        let index = selected_history_index(state.handles.history_list, state.history_entries.len());
        if let Some(index) = index {
            if let Some(entry) = state.history_entries.get(index) {
                set_text(state.handles.history_target, &entry.target);
                set_text(
                    state.handles.history_context,
                    entry
                        .context
                        .as_deref()
                        .unwrap_or_else(|| match state.language() {
                            UiLanguage::English => "(no context)",
                            UiLanguage::SimplifiedChinese => "（无上下文）",
                        }),
                );
                set_text(state.handles.history_output, &entry.output);
                set_text(
                    state.handles.history_meta,
                    &format!(
                        "{} · {} · {} · {}{}",
                        entry.created_at_utc,
                        source_label(state.language(), entry.source),
                        entry.prompt_id,
                        entry.model,
                        if entry.served_from_cache {
                            match state.language() {
                                UiLanguage::English => " · cache",
                                UiLanguage::SimplifiedChinese => " · 缓存",
                            }
                        } else {
                            ""
                        },
                    ),
                );
                return;
            }
        }
        clear_history_detail(state);
    }

    fn clear_history_detail(state: &ManagerState) {
        set_text(state.handles.history_target, "");
        set_text(state.handles.history_context, "");
        set_text(state.handles.history_output, "");
        if state.history_loaded {
            set_text(
                state.handles.history_meta,
                &status_text(
                    state.language(),
                    StatusEvent::HistoryCount {
                        count: state.history_entries.len(),
                    },
                ),
            );
        }
    }

    #[cfg(test)]
    fn history_count_text(language: UiLanguage, count: usize) -> String {
        status_text(language, StatusEvent::HistoryCount { count })
    }

    fn copy_history_output(_hwnd: HWND, state: &mut ManagerState) {
        let Some(index) =
            selected_history_index(state.handles.history_list, state.history_entries.len())
        else {
            set_status(
                state,
                &status_text(state.language(), StatusEvent::SelectHistoryEntry),
            );
            return;
        };
        let Some(entry) = state.history_entries.get(index) else {
            set_status(
                state,
                &status_text(state.language(), StatusEvent::HistoryEntryUnavailable),
            );
            return;
        };
        match copy_text_to_clipboard(&entry.output, state.language()) {
            Ok(()) => set_status(
                state,
                &status_text(state.language(), StatusEvent::OutputCopied),
            ),
            Err(error) => set_status(
                state,
                &status_text(
                    state.language(),
                    StatusEvent::CopyOutputFailed { detail: &error },
                ),
            ),
        }
    }

    fn delete_history(hwnd: HWND, state: &mut ManagerState) {
        let Some(index) =
            selected_history_index(state.handles.history_list, state.history_entries.len())
        else {
            set_status(
                state,
                &status_text(state.language(), StatusEvent::SelectHistoryEntry),
            );
            return;
        };
        let Some(entry) = state.history_entries.get(index) else {
            set_status(
                state,
                &status_text(state.language(), StatusEvent::HistoryEntryUnavailable),
            );
            return;
        };
        let confirmation = status_text(
            state.language(),
            StatusEvent::DeleteHistoryConfirm {
                target: &single_line(&entry.target),
            },
        );
        let message = wide(&confirmation);
        let title = wide(ui_text(state.language(), TextKey::WindowTitle));
        let result = unsafe {
            MessageBoxW(
                Some(hwnd),
                PCWSTR(message.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
            )
        };
        if result != IDYES {
            set_status(
                state,
                &status_text(state.language(), StatusEvent::DeletionCancelled),
            );
            return;
        }
        let result = default_history_path()
            .ok_or_else(|| {
                status_text(
                    state.language(),
                    StatusEvent::LocalAppDataUnavailable {
                        operation: StatusOperation::History,
                    },
                )
            })
            .and_then(|path| HistoryDatabase::open(path).map_err(|error| error.to_string()))
            .and_then(|database| {
                database
                    .delete_one(entry.id)
                    .map_err(|error| error.to_string())
            });
        match result {
            Ok(true) => {
                set_status(
                    state,
                    &status_text(state.language(), StatusEvent::HistoryEntryDeleted),
                );
                refresh_history(state);
            }
            Ok(false) => set_status(
                state,
                &status_text(state.language(), StatusEvent::HistoryEntryAlreadyDeleted),
            ),
            Err(error) => set_status(
                state,
                &status_text(
                    state.language(),
                    StatusEvent::DeleteHistoryFailed { detail: &error },
                ),
            ),
        }
    }

    fn format_history_row(entry: &HistoryEntry) -> String {
        format!(
            "{}  |  {}",
            single_line(&entry.target),
            single_line(&entry.output)
        )
    }

    fn single_line(value: &str) -> String {
        let mut result = value.replace(['\r', '\n', '\t'], " ");
        if result.chars().count() > 180 {
            result = result.chars().take(177).collect();
            result.push_str("...");
        }
        result
    }

    fn source_label(language: UiLanguage, source: ExtractionSource) -> &'static str {
        match (language, source) {
            (UiLanguage::English, ExtractionSource::UiaSelection) => "selection",
            (UiLanguage::English, ExtractionSource::UiaPoint) => "hover",
            (UiLanguage::English, ExtractionSource::Clipboard) => "clipboard",
            (_, ExtractionSource::Ocr) => "OCR",
            (UiLanguage::SimplifiedChinese, ExtractionSource::UiaSelection) => "划词",
            (UiLanguage::SimplifiedChinese, ExtractionSource::UiaPoint) => "悬停",
            (UiLanguage::SimplifiedChinese, ExtractionSource::Clipboard) => "剪贴板",
        }
    }

    fn selected_history_index(hwnd: HWND, length: usize) -> Option<usize> {
        let selected =
            unsafe { SendMessageW(hwnd, LB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 };
        valid_history_index(selected, length)
    }

    fn valid_history_index(selected: isize, length: usize) -> Option<usize> {
        usize::try_from(selected)
            .ok()
            .filter(|index| *index < length)
    }

    fn reset_combo(hwnd: HWND) {
        unsafe {
            let _ = SendMessageW(hwnd, CB_RESETCONTENT, Some(WPARAM(0)), Some(LPARAM(0)));
        }
    }

    fn add_combo_string(hwnd: HWND, value: &str) {
        let value = wide(value);
        unsafe {
            let _ = SendMessageW(
                hwnd,
                CB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(value.as_ptr() as isize)),
            );
        }
    }

    fn set_combo_selection(hwnd: HWND, index: usize) {
        unsafe {
            let _ = SendMessageW(hwnd, CB_SETCURSEL, Some(WPARAM(index)), Some(LPARAM(0)));
        }
    }

    fn combo_selection(hwnd: HWND) -> Option<usize> {
        let value = unsafe { SendMessageW(hwnd, CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 };
        usize::try_from(value).ok()
    }

    fn add_list_string(hwnd: HWND, value: &str) {
        let value = wide(value);
        unsafe {
            let _ = SendMessageW(
                hwnd,
                LB_ADDSTRING,
                Some(WPARAM(0)),
                Some(LPARAM(value.as_ptr() as isize)),
            );
        }
    }

    fn copy_text_to_clipboard(value: &str, language: UiLanguage) -> Result<(), String> {
        let mut utf16: Vec<u16> = value.encode_utf16().collect();
        utf16.push(0);
        let bytes = utf16
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .ok_or_else(|| status_text(language, StatusEvent::OutputTooLarge))?;
        let memory =
            unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }.map_err(|error| error.to_string())?;
        let pointer = unsafe { GlobalLock(memory) }.cast::<u16>();
        if pointer.is_null() {
            unsafe {
                GlobalFree(memory);
            }
            return Err(status_text(
                language,
                StatusEvent::ClipboardMemoryLockFailed,
            ));
        }
        unsafe {
            std::ptr::copy_nonoverlapping(utf16.as_ptr(), pointer, utf16.len());
            let _ = GlobalUnlock(memory);
        }
        unsafe { OpenClipboard(None) }.map_err(|error| {
            unsafe {
                GlobalFree(memory);
            }
            error.to_string()
        })?;
        let result = unsafe { EmptyClipboard() }
            .and_then(|_| unsafe {
                SetClipboardData(CF_UNICODETEXT.0.into(), Some(HANDLE(memory.0)))
            })
            .map_err(|error| error.to_string());
        unsafe {
            CloseClipboard().ok();
        }
        if result.is_err() {
            unsafe {
                GlobalFree(memory);
            }
        }
        result.map(|_| ())
    }

    fn refresh_settings_form(state: &ManagerState) {
        set_text(state.handles.endpoint, &state.config.provider.endpoint);
        set_text(state.handles.model, &state.config.provider.model);
        set_text(
            state.handles.credential_target,
            &state.config.provider.credential_target,
        );
        populate_profile_selectors(state);
    }

    #[cfg(test)]
    fn resident_start_status(language: UiLanguage, outcome: ResidentStartOutcome) -> String {
        status_text(language, StatusEvent::ResidentStart(outcome))
    }

    fn config_refresh_status(language: UiLanguage, outcome: RefreshOutcome) -> String {
        status_text(language, StatusEvent::ConfigRefresh(outcome))
    }

    fn credential_refresh_status(
        language: UiLanguage,
        outcome: RefreshOutcome,
        deleted: bool,
    ) -> String {
        status_text(
            language,
            StatusEvent::CredentialRefresh { outcome, deleted },
        )
    }

    fn save_settings(state: &mut ManagerState) {
        discard_draft(state);
        let mut next = state.config.clone();
        next.provider.endpoint = read_text(state.handles.endpoint).trim().to_owned();
        next.provider.model = read_text(state.handles.model).trim().to_owned();
        next.provider.credential_target =
            read_text(state.handles.credential_target).trim().to_owned();
        // The Settings controls display profile names, but the configuration
        // stores stable profile IDs. Keep the last valid ID if a native combo
        // has no selection (for example while it is being rebuilt).
        if let Some(id) = selected_profile_id(
            state.handles.settings_selection_default,
            &state.config.profiles,
        ) {
            next.defaults.selection = id;
        }
        if let Some(id) =
            selected_profile_id(state.handles.settings_hover_default, &state.config.profiles)
        {
            next.defaults.hover = id;
        }
        if let Err(error) = next.validate() {
            set_status(
                state,
                &status_text(
                    state.language(),
                    StatusEvent::CannotSaveSettings {
                        detail: &error.to_string(),
                    },
                ),
            );
            return;
        }
        if let Err(error) = save_config(state, &next) {
            set_status(
                state,
                &status_text(
                    state.language(),
                    StatusEvent::CannotSaveSettings {
                        detail: &error.to_string(),
                    },
                ),
            );
            return;
        }
        state.config = next;
        let refresh = notify_config_changed();
        populate_profile_selectors(state);
        set_status(state, &config_refresh_status(state.language(), refresh));
        update_credential_status(state);
    }
    fn save_key(state: &mut ManagerState) {
        let target = read_text(state.handles.credential_target).trim().to_owned();
        let secret = read_secret(state.handles.api_key);
        if secret.0.trim().is_empty() {
            set_status(
                state,
                &status_text(state.language(), StatusEvent::EnterApiKey),
            );
            return;
        }
        match credentials::write_api_key(&target, &secret.0) {
            Ok(()) => {
                set_text(state.handles.api_key, "");
                state.credential_status = CredentialStatusState::Present;
                set_text(
                    state.handles.credential_status,
                    &status_text(
                        state.language(),
                        StatusEvent::ApiKeySavedToCredentialManager,
                    ),
                );
                if target == state.config.provider.credential_target {
                    let refresh = notify_credentials_changed();
                    set_status(
                        state,
                        &credential_refresh_status(state.language(), refresh, false),
                    );
                } else {
                    set_status(
                        state,
                        &status_text(state.language(), StatusEvent::ApiKeyInactiveTargetSaved),
                    );
                }
            }
            Err(error) => set_status(
                state,
                &status_text(
                    state.language(),
                    StatusEvent::SaveApiKeyFailed {
                        detail: &error.to_string(),
                    },
                ),
            ),
        }
    }
    fn delete_key(state: &mut ManagerState) {
        let target = read_text(state.handles.credential_target).trim().to_owned();
        match credentials::delete_api_key(&target) {
            Ok(()) => {
                state.credential_status = CredentialStatusState::Absent;
                set_text(
                    state.handles.credential_status,
                    &status_text(state.language(), StatusEvent::NoSavedApiKey),
                );
                if target == state.config.provider.credential_target {
                    let refresh = notify_credentials_changed();
                    set_status(
                        state,
                        &credential_refresh_status(state.language(), refresh, true),
                    );
                } else {
                    set_status(
                        state,
                        &status_text(state.language(), StatusEvent::ApiKeyInactiveTargetDeleted),
                    );
                }
            }
            Err(error) => set_status(
                state,
                &status_text(
                    state.language(),
                    StatusEvent::DeleteApiKeyFailed {
                        detail: &error.to_string(),
                    },
                ),
            ),
        }
    }

    fn save_prompt(state: &mut ManagerState) {
        if state.draft_prompt.is_none()
            && (state.config.profiles.is_empty()
                || state.profile_index >= state.config.profiles.len())
        {
            set_status(
                state,
                &status_text(state.language(), StatusEvent::NoPromptProfile),
            );
            return;
        }
        let prompt = match prompt_from_form(state) {
            Ok(prompt) => prompt,
            Err(error) => {
                set_text(state.handles.prompt_status, &error);
                return;
            }
        };
        let was_draft = state.draft_prompt.is_some();
        let old_id = (!was_draft).then(|| state.config.profiles[state.profile_index].id.clone());
        let mut next = apply_prompt(
            &state.config,
            (!was_draft).then_some(state.profile_index),
            old_id.as_deref(),
            prompt,
        );
        next.defaults.selection = read_text(state.handles.prompt_selection_default)
            .trim()
            .to_owned();
        next.defaults.hover = read_text(state.handles.prompt_hover_default)
            .trim()
            .to_owned();
        if let Err(error) = next.validate() {
            set_text(
                state.handles.prompt_status,
                &status_text(
                    state.language(),
                    StatusEvent::CannotSavePrompt {
                        detail: &error.to_string(),
                    },
                ),
            );
            return;
        }
        if let Err(error) = save_config(state, &next) {
            set_text(
                state.handles.prompt_status,
                &status_text(
                    state.language(),
                    StatusEvent::CannotSavePrompt {
                        detail: &error.to_string(),
                    },
                ),
            );
            return;
        }
        state.config = next;
        let refresh = notify_config_changed();
        populate_history_filters(state);
        state.draft_prompt = None;
        if was_draft {
            state.profile_index = state.config.profiles.len() - 1;
        }
        set_text(
            state.handles.prompt_status,
            &config_refresh_status(state.language(), refresh),
        );
        refresh_prompt_form(state);
    }
    fn new_prompt(state: &mut ManagerState) {
        let mut prompt = PromptConfig::new(format!("custom-{}", state.config.profiles.len() + 1));
        prompt.name = "New prompt".to_owned();
        prompt.system_prompt = "Interpret the target using the supplied context.".to_owned();
        prompt.user_template =
            "Target:\n{target}\nContext:\n{context}\nSource:\n{source}".to_owned();
        state.draft_prompt = Some(prompt);
        refresh_prompt_form(state);
        set_text(
            state.handles.prompt_status,
            &status_text(state.language(), StatusEvent::NewPromptUnsaved),
        );
    }
    fn previous_prompt(state: &mut ManagerState) {
        discard_draft(state);
        if !state.config.profiles.is_empty() {
            state.profile_index = state.profile_index.saturating_sub(1);
            refresh_prompt_form(state);
        }
    }
    fn next_prompt(state: &mut ManagerState) {
        discard_draft(state);
        if !state.config.profiles.is_empty() {
            state.profile_index = (state.profile_index + 1) % state.config.profiles.len();
            refresh_prompt_form(state);
        }
    }
    fn refresh_prompt_form(state: &mut ManagerState) {
        refresh_profile_number(state);
        if let Some(prompt) = state
            .draft_prompt
            .as_ref()
            .or_else(|| state.config.profiles.get(state.profile_index))
        {
            set_text(state.handles.profile_id, &prompt.id);
            set_text(state.handles.profile_name, &prompt.name);
            set_text(state.handles.system_prompt, &prompt.system_prompt);
            set_text(state.handles.user_template, &prompt.user_template);
            set_text(
                state.handles.profile_model,
                prompt.model.as_deref().unwrap_or_default(),
            );
            set_text(
                state.handles.temperature,
                &prompt
                    .temperature
                    .map_or_else(String::new, |v| v.to_string()),
            );
            set_text(
                state.handles.max_tokens,
                &prompt
                    .max_output_tokens
                    .map_or_else(String::new, |v| v.to_string()),
            );
            set_text(
                state.handles.prompt_selection_default,
                &state.config.defaults.selection,
            );
            set_text(
                state.handles.prompt_hover_default,
                &state.config.defaults.hover,
            );
        }
    }

    fn refresh_profile_number(state: &ManagerState) {
        let number = if state.draft_prompt.is_some() {
            status_text(state.language(), StatusEvent::NewDraft)
        } else {
            status_text(
                state.language(),
                StatusEvent::ProfilePosition {
                    current: state.profile_index.saturating_add(1),
                    total: state.config.profiles.len(),
                },
            )
        };
        set_text(state.handles.profile_number, &number);
    }

    fn discard_draft(state: &mut ManagerState) {
        if state.draft_prompt.take().is_some() {
            refresh_prompt_form(state);
            set_text(
                state.handles.prompt_status,
                &status_text(state.language(), StatusEvent::UnsavedPromptDiscarded),
            );
        }
    }

    fn apply_prompt(
        config: &AppConfig,
        index: Option<usize>,
        old_id: Option<&str>,
        prompt: PromptConfig,
    ) -> AppConfig {
        let mut next = config.clone();
        match index {
            Some(index) => {
                next.profiles[index] = prompt;
                if let Some(old_id) = old_id {
                    let new_id = next.profiles[index].id.clone();
                    if next.defaults.selection == old_id {
                        next.defaults.selection = new_id.clone();
                    }
                    if next.defaults.hover == old_id {
                        next.defaults.hover = new_id;
                    }
                }
            }
            None => next.profiles.push(prompt),
        }
        next
    }

    fn prompt_from_form(state: &ManagerState) -> Result<PromptConfig, String> {
        let temperature =
            parse_optional_f32(&read_text(state.handles.temperature), state.language())?;
        let max_output_tokens =
            parse_optional_u32(&read_text(state.handles.max_tokens), state.language())?;
        let prompt = PromptConfig {
            id: read_text(state.handles.profile_id).trim().to_owned(),
            name: read_text(state.handles.profile_name).trim().to_owned(),
            system_prompt: read_text(state.handles.system_prompt),
            user_template: read_text(state.handles.user_template),
            model: nonempty(read_text(state.handles.profile_model)),
            temperature,
            max_output_tokens,
        };
        prompt.validate().map_err(|error| {
            status_text(
                state.language(),
                StatusEvent::PromptInvalid {
                    detail: &error.to_string(),
                },
            )
        })?;
        Ok(prompt)
    }
    fn parse_optional_f32(value: &str, language: UiLanguage) -> Result<Option<f32>, String> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        let parsed = value
            .parse::<f32>()
            .map_err(|_| status_text(language, StatusEvent::InvalidTemperature))?;
        if !parsed.is_finite() || !(0.0..=2.0).contains(&parsed) {
            return Err(status_text(language, StatusEvent::InvalidTemperature));
        }
        Ok(Some(parsed))
    }
    fn parse_optional_u32(value: &str, language: UiLanguage) -> Result<Option<u32>, String> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        let parsed = value
            .parse::<u32>()
            .map_err(|_| status_text(language, StatusEvent::InvalidMaxOutputTokens))?;
        if parsed == 0 {
            return Err(status_text(language, StatusEvent::InvalidMaxOutputTokens));
        }
        Ok(Some(parsed))
    }
    fn nonempty(value: String) -> Option<String> {
        (!value.trim().is_empty()).then_some(value.trim().to_owned())
    }
    fn save_config(state: &ManagerState, config: &AppConfig) -> Result<(), String> {
        let path = state
            .config_path
            .as_ref()
            .ok_or_else(|| status_text(state.language(), StatusEvent::ConfigPathUnavailable))?;
        save_atomic(path, config).map_err(|error| error.to_string())
    }
    fn update_credential_status(state: &mut ManagerState) {
        let target = read_text(state.handles.credential_target).trim().to_owned();
        match credentials::read_api_key(&target) {
            Ok(Some(secret)) => {
                let _secret = Secret(secret);
                state.credential_status = CredentialStatusState::Present;
            }
            Ok(None) => state.credential_status = CredentialStatusState::Absent,
            Err(error) => {
                state.credential_status = CredentialStatusState::Unavailable(error.to_string())
            }
        }
        relabel_credential_status(state);
    }

    fn relabel_credential_status(state: &ManagerState) {
        let event = credential_status_event(&state.credential_status);
        set_text(
            state.handles.credential_status,
            &status_text(state.language(), event),
        );
    }

    fn credential_status_event(status: &CredentialStatusState) -> StatusEvent<'_> {
        match status {
            CredentialStatusState::Present => StatusEvent::CredentialStatusPresent,
            CredentialStatusState::Absent => StatusEvent::CredentialStatusAbsent,
            CredentialStatusState::Unavailable(detail) => {
                StatusEvent::CredentialStatusUnavailable { detail }
            }
        }
    }
    fn set_status(state: &ManagerState, message: &str) {
        set_text(state.handles.status, message);
    }
    fn read_secret(hwnd: HWND) -> Secret {
        let length = unsafe { GetWindowTextLengthW(hwnd) } as usize;
        let mut value = vec![0u16; length.saturating_add(1)];
        let written = unsafe { GetWindowTextW(hwnd, &mut value) } as usize;
        let secret = String::from_utf16_lossy(&value[..written.min(value.len())]);
        value.fill(0);
        Secret(secret)
    }
    fn set_text(hwnd: HWND, value: &str) {
        let value = wide(value);
        unsafe {
            SetWindowTextW(hwnd, PCWSTR(value.as_ptr())).ok();
        }
    }
    fn read_text(hwnd: HWND) -> String {
        let length = unsafe { GetWindowTextLengthW(hwnd) } as usize;
        let mut value = vec![0u16; length.saturating_add(1)];
        let written = unsafe { GetWindowTextW(hwnd, &mut value) } as usize;
        String::from_utf16_lossy(&value[..written.min(value.len())])
    }
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    #[cfg(test)]
    mod tests {
        use super::{
            apply_prompt, config_refresh_status, config_with_manager_language,
            credential_refresh_status, format_history_row, history_count_text,
            history_source_for_index, parse_optional_f32, parse_optional_u32, profile_id_for_index,
            profile_option_label, resident_start_status, status_text, ui_text, valid_history_index,
            visibility_style, ManagerLayout, StatusEvent, StatusOperation, View, ALL_TEXT_KEYS,
        };
        use selection_core::{AppConfig, ExtractionSource, PromptConfig, UiLanguage};
        use selection_platform_windows::app::{RefreshOutcome, ResidentStartOutcome};
        use selection_storage::HistoryEntry;
        use windows::Win32::UI::WindowsAndMessaging::WS_VISIBLE;
        #[test]
        fn optional_numbers_are_strict_and_bounded() {
            assert_eq!(parse_optional_f32("", UiLanguage::English).unwrap(), None);
            assert_eq!(
                parse_optional_f32(" 0.5 ", UiLanguage::English).unwrap(),
                Some(0.5)
            );
            assert!(parse_optional_f32("2.1", UiLanguage::English).is_err());
            assert_eq!(
                parse_optional_u32("512", UiLanguage::English).unwrap(),
                Some(512)
            );
            assert!(parse_optional_u32("0", UiLanguage::English).is_err());
            assert!(parse_optional_f32("bad", UiLanguage::SimplifiedChinese)
                .unwrap_err()
                .contains("温度"));
        }

        #[test]
        fn page_children_are_visible_inside_hidden_containers() {
            assert_eq!(visibility_style(None), WS_VISIBLE);
            assert_eq!(visibility_style(Some(View::Settings)), WS_VISIBLE);
            assert_eq!(visibility_style(Some(View::Prompts)), WS_VISIBLE);
            assert_eq!(visibility_style(Some(View::History)), WS_VISIBLE);
        }

        #[test]
        fn page_visibility_is_exclusive() {
            for selected in [View::Settings, View::Prompts, View::History] {
                for page in [View::Settings, View::Prompts, View::History] {
                    assert_eq!(super::page_is_visible(page, selected), page == selected);
                }
            }
        }

        #[test]
        fn manager_layout_keeps_navigation_and_page_disjoint_at_supported_dpi() {
            for dpi in [96, 144, 192] {
                let width = super::scale_for_dpi(980, dpi);
                let height = super::scale_for_dpi(680, dpi);
                let layout = ManagerLayout::for_client(width, height, dpi);
                assert_eq!(layout.nav.left, 0);
                assert_eq!(layout.nav.right, layout.page.left);
                assert_eq!(layout.page.right, 980);
                assert_eq!(layout.page.bottom, 680);
                assert!(layout.status.bottom <= layout.nav_buttons[3].top - 12);
                for pair in layout.nav_buttons[..3].windows(2) {
                    assert!(pair[0].bottom < pair[1].top);
                }
            }
        }

        #[test]
        fn manager_layout_uses_extra_width_without_moving_fixed_navigation() {
            let compact = ManagerLayout::for_client(980, 680, 96);
            let wide = ManagerLayout::for_client(1240, 760, 96);
            assert_eq!(compact.nav.right, wide.nav.right);
            assert_eq!(compact.page.left, wide.page.left);
            assert!(wide.page.right > compact.page.right);
            assert!(wide.nav_buttons[3].top > compact.nav_buttons[3].top);
        }

        #[test]
        fn both_localization_catalogs_cover_every_static_key() {
            for &key in ALL_TEXT_KEYS {
                assert!(!ui_text(UiLanguage::English, key).trim().is_empty());
                assert!(!ui_text(UiLanguage::SimplifiedChinese, key)
                    .trim()
                    .is_empty());
            }
        }

        #[test]
        fn every_status_event_has_complete_english_and_chinese_text() {
            let events = vec![
                StatusEvent::ManagerInitializationFailed {
                    detail: "opaque-error",
                },
                StatusEvent::ConfigLoadFailed {
                    detail: "opaque-error",
                },
                StatusEvent::LocalAppDataUnavailable {
                    operation: StatusOperation::Save,
                },
                StatusEvent::LocalAppDataUnavailable {
                    operation: StatusOperation::History,
                },
                StatusEvent::SaveInterfaceLanguageFailed {
                    detail: "opaque-error",
                },
                StatusEvent::InterfaceLanguageSaved,
                StatusEvent::HistoryRefreshed,
                StatusEvent::HistoryUnavailable {
                    detail: "opaque-error",
                },
                StatusEvent::SelectHistoryEntry,
                StatusEvent::HistoryEntryUnavailable,
                StatusEvent::OutputCopied,
                StatusEvent::CopyOutputFailed {
                    detail: "opaque-error",
                },
                StatusEvent::DeleteHistoryConfirm {
                    target: "user text",
                },
                StatusEvent::DeletionCancelled,
                StatusEvent::HistoryEntryDeleted,
                StatusEvent::HistoryEntryAlreadyDeleted,
                StatusEvent::DeleteHistoryFailed {
                    detail: "opaque-error",
                },
                StatusEvent::OutputTooLarge,
                StatusEvent::ClipboardMemoryLockFailed,
                StatusEvent::ResidentStart(ResidentStartOutcome::AlreadyRunning),
                StatusEvent::ResidentStart(ResidentStartOutcome::Started),
                StatusEvent::ResidentStart(ResidentStartOutcome::Unavailable),
                StatusEvent::ConfigRefresh(RefreshOutcome::Acknowledged),
                StatusEvent::ConfigRefresh(RefreshOutcome::ResidentAbsent),
                StatusEvent::ConfigRefresh(RefreshOutcome::Unacknowledged),
                StatusEvent::ConfigRefresh(RefreshOutcome::Rejected),
                StatusEvent::CredentialRefresh {
                    outcome: RefreshOutcome::Acknowledged,
                    deleted: false,
                },
                StatusEvent::CredentialRefresh {
                    outcome: RefreshOutcome::Acknowledged,
                    deleted: true,
                },
                StatusEvent::CredentialRefresh {
                    outcome: RefreshOutcome::ResidentAbsent,
                    deleted: false,
                },
                StatusEvent::CredentialRefresh {
                    outcome: RefreshOutcome::ResidentAbsent,
                    deleted: true,
                },
                StatusEvent::CredentialRefresh {
                    outcome: RefreshOutcome::Unacknowledged,
                    deleted: false,
                },
                StatusEvent::CredentialRefresh {
                    outcome: RefreshOutcome::Rejected,
                    deleted: true,
                },
                StatusEvent::CannotSaveSettings {
                    detail: "opaque-error",
                },
                StatusEvent::EnterApiKey,
                StatusEvent::ApiKeySavedToCredentialManager,
                StatusEvent::ApiKeyInactiveTargetSaved,
                StatusEvent::SaveApiKeyFailed {
                    detail: "opaque-error",
                },
                StatusEvent::NoSavedApiKey,
                StatusEvent::ApiKeyInactiveTargetDeleted,
                StatusEvent::DeleteApiKeyFailed {
                    detail: "opaque-error",
                },
                StatusEvent::NoPromptProfile,
                StatusEvent::CannotSavePrompt {
                    detail: "opaque-error",
                },
                StatusEvent::NewPromptUnsaved,
                StatusEvent::NewDraft,
                StatusEvent::UnsavedPromptDiscarded,
                StatusEvent::PromptInvalid {
                    detail: "opaque-error",
                },
                StatusEvent::InvalidTemperature,
                StatusEvent::InvalidMaxOutputTokens,
                StatusEvent::ConfigPathUnavailable,
                StatusEvent::CredentialStatusPresent,
                StatusEvent::CredentialStatusAbsent,
                StatusEvent::CredentialStatusUnavailable {
                    detail: "opaque-error",
                },
                StatusEvent::HistoryCount { count: 2 },
                StatusEvent::ProfilePosition {
                    current: 1,
                    total: 2,
                },
            ];
            assert!(events.len() >= 50);
            for event in events {
                let english = status_text(UiLanguage::English, event);
                let chinese = status_text(UiLanguage::SimplifiedChinese, event);
                assert!(!english.trim().is_empty());
                assert!(!chinese.trim().is_empty());
                if english.contains("opaque-error") {
                    assert!(chinese.contains("opaque-error"));
                }
            }
        }

        #[test]
        fn credential_language_relabel_is_pure_and_preserves_opaque_detail() {
            let status = super::CredentialStatusState::Unavailable("vault-error".to_owned());
            let event = super::credential_status_event(&status);
            assert_eq!(
                status,
                super::CredentialStatusState::Unavailable("vault-error".to_owned())
            );
            assert!(status_text(UiLanguage::English, event).contains("vault-error"));
            assert!(status_text(UiLanguage::SimplifiedChinese, event).contains("vault-error"));
        }

        #[test]
        fn language_only_update_preserves_every_other_config_field() {
            let config = AppConfig::default();
            let mut expected = config.clone();
            expected.ui.manager_language = UiLanguage::SimplifiedChinese;
            let changed = config_with_manager_language(&config, UiLanguage::SimplifiedChinese);
            assert_eq!(changed, expected);
            assert_eq!(config.ui.manager_language, UiLanguage::English);
        }

        #[test]
        fn default_profile_selectors_show_names_but_save_stable_ids() {
            let mut first = PromptConfig::new("translate");
            first.name = "  Translate  ".to_owned();
            let mut second = PromptConfig::new("explain");
            second.name = "解释".to_owned();
            let profiles = vec![first.clone(), second.clone()];

            assert_eq!(profile_option_label(&first), "Translate — translate");
            assert_eq!(profile_option_label(&second), "解释 — explain");
            assert_eq!(
                profile_id_for_index(Some(0), &profiles),
                Some("translate".to_owned())
            );
            assert_eq!(
                profile_id_for_index(Some(1), &profiles),
                Some("explain".to_owned())
            );
            assert_eq!(profile_id_for_index(Some(2), &profiles), None);
            assert_eq!(profile_id_for_index(None, &profiles), None);
        }

        #[test]
        fn localized_counts_and_statuses_have_chinese_variants() {
            assert_eq!(history_count_text(UiLanguage::English, 1), "1 entry");
            assert_eq!(history_count_text(UiLanguage::English, 2), "2 entries");
            assert_eq!(
                history_count_text(UiLanguage::SimplifiedChinese, 2),
                "2 条记录"
            );
            assert!(resident_start_status(
                UiLanguage::SimplifiedChinese,
                ResidentStartOutcome::Started
            )
            .contains("驻留程序"));
            assert!(config_refresh_status(
                UiLanguage::SimplifiedChinese,
                RefreshOutcome::Acknowledged
            )
            .contains("已保存"));
        }

        #[test]
        fn draft_commit_does_not_mutate_saved_config_until_commit() {
            let config = AppConfig::default();
            let original_count = config.profiles.len();
            let draft = PromptConfig::new("draft");
            let committed = apply_prompt(&config, None, None, draft);
            assert_eq!(config.profiles.len(), original_count);
            assert_eq!(committed.profiles.len(), original_count + 1);
            assert!(config.profile("draft").is_none());
            assert!(committed.profile("draft").is_some());
        }

        #[test]
        fn history_source_filter_maps_all_and_each_source() {
            assert_eq!(history_source_for_index(None), None);
            assert_eq!(history_source_for_index(Some(0)), None);
            assert_eq!(
                history_source_for_index(Some(1)),
                Some(ExtractionSource::UiaSelection)
            );
            assert_eq!(
                history_source_for_index(Some(2)),
                Some(ExtractionSource::UiaPoint)
            );
            assert_eq!(
                history_source_for_index(Some(3)),
                Some(ExtractionSource::Clipboard)
            );
            assert_eq!(
                history_source_for_index(Some(4)),
                Some(ExtractionSource::Ocr)
            );
            assert_eq!(history_source_for_index(Some(5)), None);
        }

        #[test]
        fn history_row_is_single_line_and_keeps_target_and_output() {
            let entry = HistoryEntry {
                id: 7,
                created_at_utc: "2026-08-19T12:00:00Z".to_owned(),
                source: ExtractionSource::UiaSelection,
                target: "hello\nworld".to_owned(),
                context: None,
                output: "你好\t世界".to_owned(),
                prompt_id: "translate".to_owned(),
                model: "test".to_owned(),
                served_from_cache: false,
            };
            assert_eq!(format_history_row(&entry), "hello world  |  你好 世界");
        }

        #[test]
        fn history_selection_rejects_negative_and_out_of_range_values() {
            assert_eq!(valid_history_index(-1, 2), None);
            assert_eq!(valid_history_index(0, 2), Some(0));
            assert_eq!(valid_history_index(1, 2), Some(1));
            assert_eq!(valid_history_index(2, 2), None);
        }

        #[test]
        fn resident_and_refresh_statuses_never_claim_unacknowledged_success() {
            assert!(
                resident_start_status(UiLanguage::English, ResidentStartOutcome::Started)
                    .contains("ready")
            );
            assert!(
                resident_start_status(UiLanguage::English, ResidentStartOutcome::Unavailable)
                    .contains("translation is unavailable")
            );
            assert!(
                config_refresh_status(UiLanguage::English, RefreshOutcome::Acknowledged)
                    .contains("confirmed")
            );
            assert!(
                !config_refresh_status(UiLanguage::English, RefreshOutcome::ResidentAbsent)
                    .contains("confirmed")
            );
            assert!(
                !config_refresh_status(UiLanguage::English, RefreshOutcome::Unacknowledged)
                    .contains("confirmed")
            );
            assert!(
                !config_refresh_status(UiLanguage::English, RefreshOutcome::Rejected)
                    .contains("confirmed")
            );
            assert!(credential_refresh_status(
                UiLanguage::English,
                RefreshOutcome::Acknowledged,
                false
            )
            .contains("confirmed"));
            assert!(!credential_refresh_status(
                UiLanguage::English,
                RefreshOutcome::ResidentAbsent,
                true
            )
            .contains("confirmed"));
        }
    }
}

#[cfg(windows)]
fn main() {
    if let Err(error) = windows_app::run() {
        eprintln!("selection translate manager failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {}
