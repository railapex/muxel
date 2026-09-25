//! State for the in-app settings page.
//!
//! The page edits one preset / project at a time, reusing a fixed set of
//! [`InputState`] widgets that are repopulated on selection and read back on
//! Save. The rendering + handlers live as `MuxelApp` methods in `app.rs`.

use crate::i18n::t;
use gpui::*;
use gpui_component::input::InputState;
use muxel_core::{EnvVar, InjectionMode, PresetKind, SshAuth};
use uuid::Uuid;

/// Inline result of a "Test connection" in the SSH host editor.
#[derive(Clone)]
pub enum RemoteTestState {
    Idle,
    Testing,
    Ok(String),
    Failed(String),
}

pub use crate::action_catalog::DEFAULT_KEYBINDINGS;

/// Which settings section is shown.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Appearance,
    Editor,
    Behavior,
    Speech,
    ReadAloud,
    Agents,
    /// Setting up Grok Bot (or another agent) to drive muxel through `muxel ctl`.
    GrokBot,
    Runners,
    Snippets,
    Loops,
    Remotes,
    Identities,
    Projects,
    Keybindings,
}

/// Editable widgets + selection state for the settings page.
pub struct SettingsUi {
    pub section: SettingsSection,

    // Agent preset editor.
    pub selected_preset: Option<usize>,
    pub p_injection: InjectionMode,
    /// Terminal-agent vs browser preset (drives which editor fields show).
    pub p_kind: PresetKind,
    pub p_name: Entity<InputState>,
    /// Homepage for a browser preset.
    pub p_url: Entity<InputState>,
    pub p_program: Entity<InputState>,
    pub p_model: Entity<InputState>,
    pub p_model_flag: Entity<InputState>,
    pub p_effort: Entity<InputState>,
    pub p_effort_flag: Entity<InputState>,
    pub p_args: Entity<InputState>,
    pub p_prompt: Entity<InputState>,
    pub p_inj_flag: Entity<InputState>,
    pub p_env: Entity<InputState>,
    pub p_working_markers: Entity<InputState>,
    pub p_blocked_markers: Entity<InputState>,
    pub p_startup_delay: Entity<InputState>,

    // Runner editor.
    pub selected_runner: Option<usize>,
    pub r_preset_id: Option<Uuid>,
    pub r_presses: u8,
    pub r_name: Entity<InputState>,
    pub r_prompt: Entity<InputState>,

    // Snippet editor.
    pub selected_snippet: Option<usize>,
    pub sn_submit: bool,
    pub sn_name: Entity<InputState>,
    pub sn_text: Entity<InputState>,

    // Loop editor.
    pub selected_loop: Option<usize>,
    pub l_preset_id: Option<Uuid>,
    pub l_project_id: Option<Uuid>,
    pub l_presses: u8,
    /// Schedule kind: 0 = every N minutes, 1 = every N hours, 2 = daily at.
    pub l_sched_kind: u8,
    pub l_exit: bool,
    pub l_enabled: bool,
    pub l_name: Entity<InputState>,
    pub l_prompt: Entity<InputState>,
    pub l_interval: Entity<InputState>,
    pub l_hour: Entity<InputState>,
    pub l_minute: Entity<InputState>,

    // Grok Bot tab.
    /// Inline result of the last "Test" (running `muxel ctl` as Grok Bot would).
    pub grok_test: RemoteTestState,
    /// Whether the skill's full text is shown below its Copy button.
    pub grok_skill_preview: bool,
    /// The skill has been copied to the clipboard (shown beside the button).
    pub grok_skill_copied: bool,

    // SSH remote-host editor.
    pub selected_remote: Option<usize>,
    pub s_auth: SshAuth,
    /// Inline result of the last "Test connection".
    pub s_test: RemoteTestState,
    /// Cached "a password is stored in the keychain" flag for the open host
    /// (refreshed on open/save, so render doesn't hit the keychain every frame).
    pub s_has_password: bool,
    pub s_forward_agent: bool,
    pub s_compression: bool,
    pub s_use_tmux: bool,
    /// Which OS the selected host runs — decides the whole remote command
    /// vocabulary, and whether the tmux row is offered at all.
    pub s_remote_os: muxel_core::RemoteOs,
    /// Shell an interactive pane runs on a Windows host.
    pub s_windows_shell: muxel_core::winshell::WindowsShell,
    pub s_name: Entity<InputState>,
    pub s_host: Entity<InputState>,
    pub s_port: Entity<InputState>,
    pub s_user: Entity<InputState>,
    pub s_identity: Entity<InputState>,
    pub s_password: Entity<InputState>,
    pub s_jump: Entity<InputState>,
    pub s_keepalive: Entity<InputState>,
    pub s_strict: Entity<InputState>,
    pub s_extra: Entity<InputState>,
    /// Shared login identity chosen for the open host (None = inline credentials).
    pub s_identity_id: Option<Uuid>,

    // Shared login-identity editor.
    pub selected_identity: Option<usize>,
    pub id_auth: SshAuth,
    pub id_has_password: bool,
    pub id_name: Entity<InputState>,
    pub id_user: Entity<InputState>,
    pub id_identity: Entity<InputState>,
    pub id_password: Entity<InputState>,

    // Project editor.
    pub selected_project: Option<Uuid>,
    pub proj_name: Entity<InputState>,

    // Appearance.
    pub font_family: Entity<InputState>,

    // Editor.
    pub editor_font_family: Entity<InputState>,

    // Speech-to-text.
    pub stt_provider_url: Entity<InputState>,
    pub stt_provider_model: Entity<InputState>,
    pub stt_language: Entity<InputState>,
    pub stt_api_key: Entity<InputState>,
    /// The spoken phrase that triggers the wake command.
    pub stt_wake_phrase: Entity<InputState>,
    /// Whether a provider API key is stored (cached so render doesn't hit the
    /// keychain every frame).
    pub stt_has_key: bool,

    // Read aloud (text-to-speech).
    /// OS voice name, typed — for systems muxel can't list voices on (Linux).
    pub tts_system_voice: Entity<InputState>,
    pub tts_provider_voice: Entity<InputState>,
    pub tts_provider_model: Entity<InputState>,
    /// The OS voices, `(name, locale)`, listed once in the background the first
    /// time Settings opens (`None` until then).
    pub tts_system_voices: Option<Vec<(String, String)>>,

    // Keybindings (action name -> keystroke input).
    pub keybinds: Vec<(String, Entity<InputState>)>,
    /// Comma/space-separated chords passed through to a focused terminal.
    pub passthrough_keys: Entity<InputState>,
}

impl SettingsUi {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            section: SettingsSection::Appearance,
            selected_preset: None,
            p_injection: InjectionMode::None,
            p_kind: PresetKind::Terminal,
            p_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            p_url: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("https://duckduckgo.com"))),
            p_program: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("Program (blank = shell)"))),
            p_model: cx.new(|cx| InputState::new(window, cx).placeholder(t("Model (optional)"))),
            p_model_flag: cx.new(|cx| InputState::new(window, cx).placeholder(t("--model"))),
            p_effort: cx.new(|cx| InputState::new(window, cx).placeholder(t("Effort (optional)"))),
            p_effort_flag: cx.new(|cx| InputState::new(window, cx).placeholder(t("Effort flag"))),
            p_args: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("Extra args (quotes group words)"))
            }),
            p_prompt: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .placeholder(t("System prompt"))
            }),
            p_inj_flag: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("--append-system-prompt"))),
            p_env: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .placeholder(t("KEY=VALUE per line"))
            }),
            p_working_markers: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("comma-separated; blank = default"))
            }),
            p_blocked_markers: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("comma-separated; blank = default"))
            }),
            p_startup_delay: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("0 = auto (wait for quiet)"))),
            selected_runner: None,
            r_preset_id: None,
            r_presses: 3,
            r_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            r_prompt: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .placeholder(t("Task prompt — use {{input}} for the run-time details"))
            }),
            selected_snippet: None,
            sn_submit: false,
            sn_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            sn_text: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .placeholder(t("Text typed into the pane"))
            }),
            selected_loop: None,
            l_preset_id: None,
            l_project_id: None,
            l_presses: 0,
            l_sched_kind: 1,
            l_exit: false,
            l_enabled: true,
            l_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            l_prompt: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .placeholder(t("Prompt to run each time"))
            }),
            l_interval: cx.new(|cx| InputState::new(window, cx).placeholder("1")),
            l_hour: cx.new(|cx| InputState::new(window, cx).placeholder("9")),
            l_minute: cx.new(|cx| InputState::new(window, cx).placeholder("00")),
            grok_test: RemoteTestState::Idle,
            grok_skill_preview: false,
            grok_skill_copied: false,
            selected_remote: None,
            s_auth: SshAuth::Agent,
            s_test: RemoteTestState::Idle,
            s_has_password: false,
            s_forward_agent: false,
            s_compression: false,
            s_use_tmux: true,
            s_remote_os: muxel_core::RemoteOs::default(),
            s_windows_shell: muxel_core::winshell::WindowsShell::default(),
            s_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            s_host: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(t("host.example.com or ~/.ssh/config alias"))
            }),
            s_port: cx.new(|cx| InputState::new(window, cx).placeholder("22")),
            s_user: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("login user (optional)"))),
            s_identity: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("~/.ssh/id_ed25519"))),
            s_password: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(t("stored in the OS keychain"))
            }),
            s_jump: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("ProxyJump host (optional)"))),
            s_keepalive: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("ServerAliveInterval secs"))),
            s_strict: cx.new(|cx| InputState::new(window, cx).placeholder(t("accept-new"))),
            s_extra: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .placeholder(t("extra -o options, one KEY=VALUE per line"))
            }),
            s_identity_id: None,
            selected_identity: None,
            id_auth: SshAuth::Agent,
            id_has_password: false,
            id_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Name"))),
            id_user: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("login user (optional)"))),
            id_identity: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("~/.ssh/id_ed25519"))),
            id_password: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(t("stored in the OS keychain"))
            }),
            selected_project: None,
            proj_name: cx.new(|cx| InputState::new(window, cx).placeholder(t("Project name"))),
            font_family: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("DejaVu Sans Mono"))),
            editor_font_family: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("theme monospace"))),
            stt_provider_url: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("https://api.openai.com/v1"))),
            stt_provider_model: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("whisper-1"))),
            stt_language: cx.new(|cx| InputState::new(window, cx).placeholder(t("auto-detect"))),
            stt_api_key: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(t("stored in the OS keychain"))
            }),
            stt_wake_phrase: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(muxel_core::stt::DEFAULT_WAKE_PHRASE.to_string())
            }),
            stt_has_key: false,
            tts_system_voice: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("OS default voice"))),
            tts_provider_voice: cx.new(|cx| {
                InputState::new(window, cx).placeholder(muxel_core::tts::DEFAULT_TTS_VOICE)
            }),
            tts_provider_model: cx.new(|cx| {
                InputState::new(window, cx).placeholder(muxel_core::tts::DEFAULT_TTS_PROVIDER_MODEL)
            }),
            tts_system_voices: None,
            keybinds: DEFAULT_KEYBINDINGS
                .iter()
                .map(|(name, default, _ctx)| {
                    (
                        name.to_string(),
                        cx.new(|cx| InputState::new(window, cx).default_value(*default)),
                    )
                })
                .collect(),
            passthrough_keys: cx
                .new(|cx| InputState::new(window, cx).placeholder(t("ctrl-p, ctrl-t"))),
        }
    }
}

/// Parse a `KEY=VALUE`-per-line block into env vars (blank lines ignored).
pub fn parse_env(text: &str) -> Vec<EnvVar> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let (k, v) = line.split_once('=')?;
            Some(EnvVar {
                key: k.trim().to_string(),
                value: v.trim().to_string(),
            })
        })
        .collect()
}

/// Render env vars back to a `KEY=VALUE`-per-line block for editing.
pub fn format_env(env: &[EnvVar]) -> String {
    env.iter()
        .map(|e| format!("{}={}", e.key, e.value))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split an extra-args string into shell words — quote-aware
/// ([`muxel_core::split_words`], shared with the iOS companion), so
/// `--append-system-prompt "be terse"` stays one argument. Unbalanced quotes
/// degrade to a plain whitespace split (a preset must never fail to save);
/// `save_preset` warns via the feed in that case.
pub fn parse_args(text: &str) -> Vec<String> {
    muxel_core::split_words(text)
        .unwrap_or_else(|| text.split_whitespace().map(|s| s.to_string()).collect())
}

/// Split a status-marker string on commas (markers contain spaces), trimming
/// each and dropping blanks.
pub fn parse_markers(text: &str) -> Vec<String> {
    text.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}
