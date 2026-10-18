use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentSurface {
    Desktop,
    Terminal,
    HerdrPane,
    Web,
    Unknown,
}

/// Default target selection. This is intentionally adapter-owned so a future
/// user profile can override the preference without changing event routing.
pub fn preferred_surface(agent_id: &str) -> Option<AgentSurface> {
    match agent_id {
        "chatgpt" => Some(AgentSurface::Desktop),
        "codex" => Some(AgentSurface::Terminal),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAction {
    NewSession,
    QuickQuestion,
    BranchSession,
    OpenCommands,
    Stop,
    Previous,
    Next,
    Approve,
    Reject,
    ToggleView,
    Find,
    ShowHelp,
    SplitVertical,
    SplitHorizontal,
    PreviousTab,
    NextTab,
    ToggleSidebar,
    Zoom,
    Compact,
    ResumeSession,
    ToggleTranscript,
    CyclePermissionMode,
    Plan,
    Export,
    ShowGoal,
    AgentPanel1,
    AgentPanel2,
    AgentPanel3,
    Send,
    VoiceInput,
    QuickAction,
    SessionNavigator,
}

impl AgentAction {
    pub const fn id(self) -> &'static str {
        match self {
            Self::NewSession => "new_session",
            Self::QuickQuestion => "quick_question",
            Self::BranchSession => "branch_session",
            Self::OpenCommands => "open_commands",
            Self::Stop => "stop",
            Self::Previous => "previous",
            Self::Next => "next",
            Self::Approve => "approve",
            Self::Reject => "reject",
            Self::ToggleView => "toggle_view",
            Self::Find => "find",
            Self::ShowHelp => "show_help",
            Self::SplitVertical => "split_vertical",
            Self::SplitHorizontal => "split_horizontal",
            Self::PreviousTab => "previous_tab",
            Self::NextTab => "next_tab",
            Self::ToggleSidebar => "toggle_sidebar",
            Self::Zoom => "zoom",
            Self::Compact => "compact",
            Self::ResumeSession => "resume_session",
            Self::ToggleTranscript => "toggle_transcript",
            Self::CyclePermissionMode => "cycle_permission_mode",
            Self::Plan => "plan",
            Self::Export => "export",
            Self::ShowGoal => "show_goal",
            Self::AgentPanel1 => "agent_panel_1",
            Self::AgentPanel2 => "agent_panel_2",
            Self::AgentPanel3 => "agent_panel_3",
            Self::Send => "send",
            Self::VoiceInput => "voice_input",
            Self::QuickAction => "quick_action",
            Self::SessionNavigator => "session_navigator",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyModifiers(u8);

impl KeyModifiers {
    pub const NONE: Self = Self(0);
    pub const COMMAND: Self = Self(1 << 0);
    pub const CONTROL: Self = Self(1 << 1);
    pub const SHIFT: Self = Self(1 << 2);
    pub const OPTION: Self = Self(1 << 3);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionPlan {
    Shortcut {
        key_code: u16,
        modifiers: KeyModifiers,
    },
    TextInput {
        text: &'static str,
        submit: bool,
    },
    /// Types a slash command into a web composer whose autocomplete popup
    /// swallows the first Enter (accepting the suggestion), then presses
    /// Enter again to actually submit the command.
    SlashCommand {
        text: &'static str,
    },
    HerdrPrefixedShortcut {
        key_code: u16,
        modifiers: KeyModifiers,
    },
    LeaderSequence {
        leader: &'static str,
        key_code: u16,
        modifiers: KeyModifiers,
    },
    ChatgptFork,
    ChatgptVoiceHold,
    ChatgptApprove,
    ChatgptReject,
    ChatgptFast,
    ActivePanel {
        slot: u8,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FunctionKeyState {
    pub key: String,
    pub label: String,
    pub hint: String,
    pub supported: bool,
    pub action_id: Option<String>,
    pub source: String,
    pub customized: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ActionCatalogItem {
    pub action_id: String,
    pub label: String,
    pub hint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedAgentBinding {
    pub action: AgentAction,
    pub action_id: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub plan: ActionPlan,
}

#[derive(Debug, Clone, Copy)]
struct Binding {
    key: &'static str,
    label: &'static str,
    hint: &'static str,
    action: AgentAction,
    plan: Option<ActionPlan>,
}

trait AgentAdapter {
    fn bindings(&self, surface: AgentSurface) -> &'static [Binding];
}

struct CodexAdapter;
struct QwenAdapter;
struct ClaudeAdapter;
struct HerdrAdapter;
struct DshAdapter;
struct UnsupportedAdapter;

const COMMAND: KeyModifiers = KeyModifiers::COMMAND;
const COMMAND_OPTION: KeyModifiers = KeyModifiers::COMMAND.union(KeyModifiers::OPTION);
const COMMAND_SHIFT: KeyModifiers = KeyModifiers::COMMAND.union(KeyModifiers::SHIFT);

const CODEX_DESKTOP_BINDINGS: [Binding; 10] = [
    binding(
        "K1",
        "NEW",
        "⌘N",
        AgentAction::NewSession,
        shortcut(45, COMMAND),
    ),
    binding(
        "K2",
        "QUICK",
        "⌘⌥N",
        AgentAction::QuickQuestion,
        shortcut(45, COMMAND_OPTION),
    ),
    binding(
        "K3",
        "TEMP",
        "⌘⇧N",
        AgentAction::BranchSession,
        shortcut(45, COMMAND_SHIFT),
    ),
    binding(
        "K4",
        "OK",
        "Approve",
        AgentAction::Approve,
        Some(ActionPlan::ChatgptApprove),
    ),
    binding(
        "K5",
        "NO",
        "Reject",
        AgentAction::Reject,
        Some(ActionPlan::ChatgptReject),
    ),
    binding(
        "K6",
        "FORK",
        "ACT09 Fork",
        AgentAction::BranchSession,
        Some(ActionPlan::ChatgptFork),
    ),
    binding(
        "K7",
        "FAST",
        "ACT06 Fast",
        AgentAction::QuickAction,
        Some(ActionPlan::ChatgptFast),
    ),
    binding(
        "K8",
        "SIDEBAR",
        "⌘B",
        AgentAction::ToggleView,
        shortcut(11, COMMAND),
    ),
    binding(
        "K9",
        "SEND",
        "ACT12 Send",
        AgentAction::Send,
        shortcut(36, KeyModifiers::NONE),
    ),
    binding(
        "K10",
        "MIC",
        "ACT10 Mic",
        AgentAction::VoiceInput,
        Some(ActionPlan::ChatgptVoiceHold),
    ),
];

const QWEN_BINDINGS: [Binding; 10] = [
    binding(
        "K1",
        "NEW",
        "/clear",
        AgentAction::NewSession,
        text_input("/clear", true),
    ),
    binding(
        "K2",
        "QUICK",
        "/btw …",
        AgentAction::QuickQuestion,
        text_input("/btw ", false),
    ),
    binding(
        "K3",
        "BRANCH",
        "/branch",
        AgentAction::BranchSession,
        text_input("/branch", true),
    ),
    binding(
        "K4",
        "OK",
        "y + Enter",
        AgentAction::Approve,
        text_input("y", true),
    ),
    binding(
        "K5",
        "NO",
        "n + Enter",
        AgentAction::Reject,
        text_input("n", true),
    ),
    binding(
        "K6",
        "HIST",
        "Ctrl+R",
        AgentAction::ToggleView,
        shortcut(15, KeyModifiers::CONTROL),
    ),
    binding(
        "K7",
        "STOP",
        "Esc",
        AgentAction::Stop,
        shortcut(53, KeyModifiers::NONE),
    ),
    binding(
        "K8",
        "SEND",
        "Enter",
        AgentAction::Send,
        shortcut(36, KeyModifiers::NONE),
    ),
    binding(
        "K9",
        "FORK",
        "/branch",
        AgentAction::BranchSession,
        text_input("/branch", true),
    ),
    unsupported("K10", "VOICE"),
];

const CLAUDE_TERMINAL_BINDINGS: [Binding; 10] = [
    binding(
        "K1",
        "CLEAR",
        "/clear",
        AgentAction::NewSession,
        text_input("/clear", true),
    ),
    binding(
        "K2",
        "COMPACT",
        "/compact",
        AgentAction::Compact,
        text_input("/compact", true),
    ),
    binding(
        "K3",
        "RESUME",
        "/resume",
        AgentAction::ResumeSession,
        text_input("/resume", true),
    ),
    binding(
        "K4",
        "OK",
        "y + Enter",
        AgentAction::Approve,
        text_input("y", true),
    ),
    binding(
        "K5",
        "NO",
        "n + Enter",
        AgentAction::Reject,
        text_input("n", true),
    ),
    binding(
        "K6",
        "VIEW",
        "Ctrl+O",
        AgentAction::ToggleView,
        shortcut(31, KeyModifiers::CONTROL),
    ),
    binding(
        "K7",
        "STOP",
        "Ctrl+C",
        AgentAction::Stop,
        shortcut(8, KeyModifiers::CONTROL),
    ),
    binding(
        "K8",
        "SEND",
        "Enter",
        AgentAction::Send,
        shortcut(36, KeyModifiers::NONE),
    ),
    unsupported("K9", "FORK"),
    unsupported("K10", "VOICE"),
];

const HERDR_BINDINGS: [Binding; 10] = [
    binding(
        "K1",
        "AGENT 1",
        "聚焦 Herdr Agent panel 1",
        AgentAction::AgentPanel1,
        Some(ActionPlan::ActivePanel { slot: 1 }),
    ),
    binding(
        "K2",
        "AGENT 2",
        "聚焦 Herdr Agent panel 2",
        AgentAction::AgentPanel2,
        Some(ActionPlan::ActivePanel { slot: 2 }),
    ),
    binding(
        "K3",
        "AGENT 3",
        "聚焦 Herdr Agent panel 3",
        AgentAction::AgentPanel3,
        Some(ActionPlan::ActivePanel { slot: 3 }),
    ),
    binding(
        "K4",
        "OK",
        "选中",
        AgentAction::Approve,
        shortcut(36, KeyModifiers::NONE),
    ),
    binding(
        "K5",
        "NO",
        "退出",
        AgentAction::Reject,
        shortcut(53, KeyModifiers::NONE),
    ),
    binding(
        "K6",
        "VIEW",
        "prefix+b",
        AgentAction::ToggleView,
        herdr_shortcut(11, KeyModifiers::NONE),
    ),
    binding(
        "K7",
        "VSP",
        "prefix+v",
        AgentAction::SplitVertical,
        herdr_shortcut(9, KeyModifiers::NONE),
    ),
    binding(
        "K8",
        "HSP",
        "prefix+-",
        AgentAction::SplitHorizontal,
        herdr_shortcut(27, KeyModifiers::NONE),
    ),
    unsupported("K9", "FORK"),
    binding(
        "K10",
        "SESSION",
        "prefix+g session navigator",
        AgentAction::SessionNavigator,
        herdr_shortcut(5, KeyModifiers::NONE),
    ),
];

const DSH_WEB_BINDINGS: [Binding; 10] = [
    binding(
        "K1",
        "NEW",
        "/goal ",
        AgentAction::NewSession,
        text_input("/goal ", false),
    ),
    binding(
        "K2",
        "PLAN",
        "/plan",
        AgentAction::Plan,
        slash_command("/plan"),
    ),
    binding(
        "K3",
        "GOAL",
        "/goal",
        AgentAction::ShowGoal,
        slash_command("/goal"),
    ),
    binding(
        "K4",
        "OK",
        "y + Enter",
        AgentAction::Approve,
        text_input("y", true),
    ),
    binding(
        "K5",
        "NO",
        "n + Enter",
        AgentAction::Reject,
        text_input("n", true),
    ),
    binding(
        "K6",
        "COMPACT",
        "/compact",
        AgentAction::Compact,
        slash_command("/compact"),
    ),
    binding(
        "K7",
        "PLAN",
        "/plan",
        AgentAction::Plan,
        slash_command("/plan"),
    ),
    binding(
        "K8",
        "EXPORT",
        "/export",
        AgentAction::Export,
        slash_command("/export"),
    ),
    binding(
        "K9",
        "SEND",
        "Enter",
        AgentAction::Send,
        shortcut(36, KeyModifiers::NONE),
    ),
    unsupported("K10", "VOICE"),
];

const UNSUPPORTED_BINDINGS: [Binding; 10] = [
    unsupported("K1", "NEW"),
    unsupported("K2", "QUICK"),
    unsupported("K3", "TEMP"),
    unsupported("K4", "COMMAND"),
    unsupported("K5", "STOP"),
    unsupported("K6", "PREV"),
    unsupported("K7", "NEXT"),
    unsupported("K8", "VIEW"),
    unsupported("K9", "FIND"),
    unsupported("K10", "KEYS"),
];

const fn binding(
    key: &'static str,
    label: &'static str,
    hint: &'static str,
    action: AgentAction,
    plan: Option<ActionPlan>,
) -> Binding {
    Binding {
        key,
        label,
        hint,
        action,
        plan,
    }
}

const fn unsupported(key: &'static str, label: &'static str) -> Binding {
    binding(key, label, "暂不支持", AgentAction::OpenCommands, None)
}

const fn shortcut(key_code: u16, modifiers: KeyModifiers) -> Option<ActionPlan> {
    Some(ActionPlan::Shortcut {
        key_code,
        modifiers,
    })
}

const fn text_input(text: &'static str, submit: bool) -> Option<ActionPlan> {
    Some(ActionPlan::TextInput { text, submit })
}

const fn slash_command(text: &'static str) -> Option<ActionPlan> {
    Some(ActionPlan::SlashCommand { text })
}

const fn herdr_shortcut(key_code: u16, modifiers: KeyModifiers) -> Option<ActionPlan> {
    Some(ActionPlan::HerdrPrefixedShortcut {
        key_code,
        modifiers,
    })
}

impl AgentAdapter for CodexAdapter {
    fn bindings(&self, surface: AgentSurface) -> &'static [Binding] {
        if surface == AgentSurface::Desktop {
            &CODEX_DESKTOP_BINDINGS
        } else {
            &UNSUPPORTED_BINDINGS
        }
    }
}

impl AgentAdapter for QwenAdapter {
    fn bindings(&self, _surface: AgentSurface) -> &'static [Binding] {
        &QWEN_BINDINGS
    }
}

impl AgentAdapter for ClaudeAdapter {
    fn bindings(&self, surface: AgentSurface) -> &'static [Binding] {
        if matches!(surface, AgentSurface::Terminal | AgentSurface::HerdrPane) {
            &CLAUDE_TERMINAL_BINDINGS
        } else {
            &UNSUPPORTED_BINDINGS
        }
    }
}

impl AgentAdapter for HerdrAdapter {
    fn bindings(&self, _surface: AgentSurface) -> &'static [Binding] {
        &HERDR_BINDINGS
    }
}

impl AgentAdapter for DshAdapter {
    fn bindings(&self, surface: AgentSurface) -> &'static [Binding] {
        if surface == AgentSurface::Web {
            &DSH_WEB_BINDINGS
        } else {
            &UNSUPPORTED_BINDINGS
        }
    }
}

impl AgentAdapter for UnsupportedAdapter {
    fn bindings(&self, _surface: AgentSurface) -> &'static [Binding] {
        &UNSUPPORTED_BINDINGS
    }
}

static CODEX_ADAPTER: CodexAdapter = CodexAdapter;
static QWEN_ADAPTER: QwenAdapter = QwenAdapter;
static CLAUDE_ADAPTER: ClaudeAdapter = ClaudeAdapter;
static HERDR_ADAPTER: HerdrAdapter = HerdrAdapter;
static DSH_ADAPTER: DshAdapter = DshAdapter;
static UNSUPPORTED_ADAPTER: UnsupportedAdapter = UnsupportedAdapter;

fn adapter(agent_id: Option<&str>) -> &'static dyn AgentAdapter {
    match agent_id {
        Some("codex") => &CODEX_ADAPTER,
        Some("qwen") => &QWEN_ADAPTER,
        Some("claude") => &CLAUDE_ADAPTER,
        Some("herdr") => &HERDR_ADAPTER,
        Some("dsh") => &DSH_ADAPTER,
        _ => &UNSUPPORTED_ADAPTER,
    }
}

#[cfg(test)]
fn function_keys(agent_id: Option<&str>, surface: AgentSurface) -> Vec<FunctionKeyState> {
    adapter(agent_id)
        .bindings(surface)
        .iter()
        .map(|binding| FunctionKeyState {
            key: binding.key.into(),
            label: binding.label.into(),
            hint: binding.hint.into(),
            supported: binding.plan.is_some(),
            action_id: binding.plan.map(|_| binding.action.id().into()),
            source: "adapter_default".into(),
            customized: false,
        })
        .collect()
}

fn resolved(binding: &Binding) -> Option<ResolvedAgentBinding> {
    binding.plan.map(|plan| ResolvedAgentBinding {
        action: binding.action,
        action_id: binding.action.id(),
        label: binding.label,
        hint: binding.hint,
        plan,
    })
}

pub fn default_binding(
    agent_id: Option<&str>,
    surface: AgentSurface,
    key: &str,
) -> Option<ResolvedAgentBinding> {
    adapter(agent_id)
        .bindings(surface)
        .iter()
        .find(|binding| binding.key.eq_ignore_ascii_case(key))
        .and_then(resolved)
}

/// Returns (label, hint, action_id) for a key even if it's unsupported (plan=None).
/// This preserves the original label (e.g. "VOICE") instead of falling back to "UNAVAILABLE".
pub fn default_binding_unresolved(
    agent_id: Option<&str>,
    surface: AgentSurface,
    key: &str,
) -> Option<(&'static str, &'static str, &'static str)> {
    adapter(agent_id)
        .bindings(surface)
        .iter()
        .find(|binding| binding.key.eq_ignore_ascii_case(key))
        .map(|binding| (binding.label, binding.hint, binding.action.id()))
}

pub fn binding_for_action(
    agent_id: Option<&str>,
    surface: AgentSurface,
    action_id: &str,
) -> Option<ResolvedAgentBinding> {
    adapter(agent_id)
        .bindings(surface)
        .iter()
        .find(|binding| binding.action.id() == action_id && binding.plan.is_some())
        .and_then(resolved)
}

pub fn action_catalog(agent_id: Option<&str>, surface: AgentSurface) -> Vec<ActionCatalogItem> {
    let mut catalog = Vec::new();
    for binding in adapter(agent_id).bindings(surface) {
        let Some(binding) = resolved(binding) else {
            continue;
        };
        if catalog
            .iter()
            .any(|item: &ActionCatalogItem| item.action_id == binding.action_id)
        {
            continue;
        }
        catalog.push(ActionCatalogItem {
            action_id: binding.action_id.into(),
            label: binding.label.into(),
            hint: binding.hint.into(),
        });
    }
    catalog
}

#[cfg(test)]
fn action_plan(
    agent_id: Option<&str>,
    surface: AgentSurface,
    key: &str,
) -> Option<(AgentAction, ActionPlan)> {
    adapter(agent_id)
        .bindings(surface)
        .iter()
        .find(|binding| binding.key.eq_ignore_ascii_case(key))
        .and_then(|binding| binding.plan.map(|plan| (binding.action, plan)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen_keys_are_allowlisted_plans() {
        let keys = function_keys(Some("qwen"), AgentSurface::HerdrPane);
        assert_eq!(keys.len(), 10);
        // K10 (VOICE) is unsupported for Qwen
        assert!(keys[0].supported); // K1
        assert!(keys[3].supported); // K4 OK
        assert!(!keys[9].supported); // K10 disabled
        assert_eq!(keys[0].hint, "/clear");
        assert_eq!(
            action_plan(Some("qwen"), AgentSurface::Terminal, "k7"),
            Some((
                AgentAction::Stop,
                ActionPlan::Shortcut {
                    key_code: 53,
                    modifiers: KeyModifiers::NONE,
                }
            ))
        );
    }

    #[test]
    fn codex_cli_does_not_reuse_desktop_shortcuts() {
        assert!(action_plan(Some("codex"), AgentSurface::Terminal, "K1").is_none());
        assert!(function_keys(Some("codex"), AgentSurface::Terminal)
            .iter()
            .all(|key| !key.supported));
        assert!(action_plan(Some("codex"), AgentSurface::Desktop, "K1").is_some());
    }

    #[test]
    fn chatgpt_and_codex_use_separate_surfaces() {
        assert_eq!(preferred_surface("chatgpt"), Some(AgentSurface::Desktop));
        assert_eq!(preferred_surface("codex"), Some(AgentSurface::Terminal));
        assert_eq!(preferred_surface("qwen"), None);
    }

    #[test]
    fn claude_terminal_keys_are_allowlisted_plans() {
        let keys = function_keys(Some("claude"), AgentSurface::Terminal);
        assert_eq!(keys.len(), 10);
        // K9 (FORK), K10 (VOICE) are unsupported for Claude
        assert!(keys[0].supported); // K1
        assert!(keys[3].supported); // K4 OK
        assert!(!keys[8].supported); // K9 disabled
        assert!(!keys[9].supported); // K10 disabled
        assert_eq!(keys[0].hint, "/clear");
        assert_eq!(
            action_plan(Some("claude"), AgentSurface::Terminal, "k7"),
            Some((
                AgentAction::Stop,
                ActionPlan::Shortcut {
                    key_code: 8,
                    modifiers: KeyModifiers::CONTROL,
                }
            ))
        );
        // K10 is now unsupported for Claude
        assert!(action_plan(Some("claude"), AgentSurface::HerdrPane, "K10").is_none());
    }

    #[test]
    fn claude_non_terminal_surfaces_are_explicitly_unsupported() {
        assert!(function_keys(Some("claude"), AgentSurface::Desktop)
            .iter()
            .all(|key| !key.supported));
        assert!(action_plan(Some("claude"), AgentSurface::Web, "K1").is_none());
    }

    #[test]
    fn unknown_agents_are_explicitly_unsupported() {
        assert!(function_keys(Some("unknown"), AgentSurface::Terminal)
            .iter()
            .all(|key| !key.supported));
    }

    #[test]
    fn herdr_first_keys_focus_agent_panels() {
        let keys = function_keys(Some("herdr"), AgentSurface::Terminal);
        assert_eq!(keys.len(), 10);
        assert_eq!(keys[0].hint, "聚焦 Herdr Agent panel 1");
        assert_eq!(
            action_plan(Some("herdr"), AgentSurface::Terminal, "K1"),
            Some((
                AgentAction::AgentPanel1,
                ActionPlan::ActivePanel { slot: 1 }
            ))
        );
        // K4 is OK (Enter), K5 is NO (Escape), K6 is VIEW, K7 is VSP, K8 is HSP, K9 is unsupported, K10 is session navigator
        assert_eq!(
            action_plan(Some("herdr"), AgentSurface::Terminal, "K4"),
            Some((
                AgentAction::Approve,
                ActionPlan::Shortcut {
                    key_code: 36,
                    modifiers: KeyModifiers::NONE
                }
            ))
        );
        assert_eq!(
            action_plan(Some("herdr"), AgentSurface::Terminal, "K5"),
            Some((
                AgentAction::Reject,
                ActionPlan::Shortcut {
                    key_code: 53,
                    modifiers: KeyModifiers::NONE
                }
            ))
        );
        assert!(action_plan(Some("herdr"), AgentSurface::Terminal, "K6").is_some());
        assert_eq!(
            action_plan(Some("herdr"), AgentSurface::Terminal, "K7"),
            Some((
                AgentAction::SplitVertical,
                ActionPlan::HerdrPrefixedShortcut {
                    key_code: 9,
                    modifiers: KeyModifiers::NONE
                }
            ))
        );
        assert_eq!(
            action_plan(Some("herdr"), AgentSurface::Terminal, "K8"),
            Some((
                AgentAction::SplitHorizontal,
                ActionPlan::HerdrPrefixedShortcut {
                    key_code: 27,
                    modifiers: KeyModifiers::NONE
                }
            ))
        );
        assert!(action_plan(Some("herdr"), AgentSurface::Terminal, "K9").is_none());
        assert_eq!(
            action_plan(Some("herdr"), AgentSurface::Terminal, "K10"),
            Some((
                AgentAction::SessionNavigator,
                ActionPlan::HerdrPrefixedShortcut {
                    key_code: 5,
                    modifiers: KeyModifiers::NONE
                }
            ))
        );
    }

    #[test]
    fn dsh_web_keys_are_goal_slash_commands() {
        let keys = function_keys(Some("dsh"), AgentSurface::Web);
        assert_eq!(keys.len(), 10);
        // K10 (VOICE) is unsupported for DSH
        assert!(keys[3].supported); // K4 OK
        assert!(!keys[9].supported); // K10 disabled
        assert_eq!(keys[0].hint, "/goal ");
        assert_eq!(
            action_plan(Some("dsh"), AgentSurface::Web, "k6"),
            Some((
                AgentAction::Compact,
                ActionPlan::SlashCommand { text: "/compact" }
            ))
        );
        assert_eq!(
            action_plan(Some("dsh"), AgentSurface::Web, "K7"),
            Some((
                AgentAction::Plan,
                ActionPlan::SlashCommand { text: "/plan" }
            ))
        );
        assert_eq!(
            action_plan(Some("dsh"), AgentSurface::Web, "K8"),
            Some((
                AgentAction::Export,
                ActionPlan::SlashCommand { text: "/export" }
            ))
        );
        assert_eq!(
            action_plan(Some("dsh"), AgentSurface::Web, "K9"),
            Some((
                AgentAction::Send,
                ActionPlan::Shortcut {
                    key_code: 36,
                    modifiers: KeyModifiers::NONE,
                }
            ))
        );
    }

    #[test]
    fn dsh_terminal_does_not_reuse_web_commands() {
        assert!(action_plan(Some("dsh"), AgentSurface::Terminal, "K1").is_none());
        assert!(function_keys(Some("dsh"), AgentSurface::Terminal)
            .iter()
            .all(|key| !key.supported));
    }
}
