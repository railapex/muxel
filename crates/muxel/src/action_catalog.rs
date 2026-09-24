//! Metadata for the existing app actions. IDs are also persisted keybinding keys.
//! This catalogue does not grant external callers permission to invoke actions.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    App,
    Project,
    Pane,
    Terminal,
    Editor,
}

#[derive(Clone, Copy, Debug)]
pub struct ActionDescriptor {
    pub id: &'static str,
    pub label: &'static str,
    pub category: &'static str,
    pub target: Target,
    pub sheet_key: Option<&'static str>,
}

macro_rules! catalogue {
    ($( $(#[$attr:meta])* ($id:literal, $label:literal, $category:literal, $target:ident, $key:expr, $binding:literal, $context:expr)),* $(,)?) => {
        pub const ACTIONS: &[ActionDescriptor] = &[
            $($(#[$attr])* ActionDescriptor {
                id: $id, label: $label, category: $category,
                target: Target::$target, sheet_key: $key,
            }),*
        ];
        pub const DEFAULT_KEYBINDINGS: &[(&str, &str, Option<&str>)] = &[
            $($(#[$attr])* ($id, $binding, $context)),*
        ];
    };
}

catalogue! {
    ("NewPane", "New pane", "Layout", Project, None, "ctrl-shift-t", None),
    ("NewTab", "New tab", "Layout", Project, Some("t"), "ctrl-t", None),
    ("TabNext", "Next tab", "Navigate", Pane, Some("n"), "ctrl-tab", None),
    ("TabPrev", "Previous tab", "Navigate", Pane, Some("p"), "ctrl-shift-tab", None),
    ("SplitRight", "Split pane right", "Layout", Project, Some("r"), "ctrl-shift-l", None),
    ("SplitDown", "Split pane down", "Layout", Project, Some("d"), "ctrl-shift-j", None),
    #[cfg(target_os = "macos")]
    ("ClosePane", "Close pane", "Close", Pane, Some("w"), "cmd-w", None),
    #[cfg(not(target_os = "macos"))]
    ("ClosePane", "Close pane", "Close", Pane, Some("w"), "ctrl-w", None),
    ("FocusNext", "Focus next pane", "Navigate", Pane, Some("f"), "ctrl-shift-]", None),
    ("FocusPrev", "Focus previous pane", "Navigate", Pane, None, "ctrl-shift-[", None),
    ("FocusLeft", "Focus pane left", "Navigate", Pane, None, "ctrl-alt-left", None),
    ("FocusRight", "Focus pane right", "Navigate", Pane, None, "ctrl-alt-right", None),
    ("FocusUp", "Focus pane up", "Navigate", Pane, None, "ctrl-alt-up", None),
    ("FocusDown", "Focus pane down", "Navigate", Pane, None, "ctrl-alt-down", None),
    ("ZoomIn", "Increase terminal text size", "View", App, None, "ctrl-shift-up", None),
    ("ZoomOut", "Decrease terminal text size", "View", App, None, "ctrl-shift-down", None),
    ("ToggleSidebar", "Toggle sidebar", "View", App, Some("b"), "ctrl-shift-b", None),
    ("ToggleDashboard", "Toggle dashboard (all agents)", "View", App, Some("e"), "ctrl-shift-e", None),
    ("ToggleSettings", "Open settings", "Help", App, Some("s"), "ctrl-shift-,", None),
    ("GlobalSearch", "Search files, panes and commands", "Help", App, Some("/"), "ctrl-shift-p", None),
    ("FindInProject", "Find in project", "Navigate", Project, None, "ctrl-shift-f", None),
    ("SearchTerminal", "Search terminal scrollback", "Navigate", Terminal, None, "ctrl-shift-f", Some("Terminal")),
    ("ClearTerminal", "Clear scrollback", "View", Terminal, Some("k"), "ctrl-shift-k", None),
    ("FocusAttention", "Focus next agent needing attention", "Navigate", App, Some("a"), "ctrl-shift-a", None),
    ("ShowKeys", "Keyboard shortcuts", "Help", App, Some("?"), "ctrl-shift-/", None),
    ("ShowActionSheet", "App actions", "Help", App, None, "ctrl-shift-space", None),
    ("ToggleBroadcast", "Toggle broadcast", "View", Project, None, "ctrl-shift-i", None),
    ("ToggleSpeechToText", "Toggle speech to text", "View", Terminal, None, "ctrl-shift-m", None),
    ("HoldSpeechToText", "Hold speech to text", "View", Terminal, None, "ctrl-shift-h", None),
    ("ReadAloud", "Read the last reply aloud", "Agent", Terminal, None, "ctrl-shift-r", None),
    ("ToggleWorktree", "Toggle git worktree for new agents", "Layout", App, None, "ctrl-shift-g", None),
    ("ToggleFullScreen", "Toggle fullscreen", "View", App, Some("v"), "f11", None),
    ("ToggleDevConsole", "Toggle developer console", "View", App, None, "f12", None),
    ("SaveFile", "Save file", "File", Editor, None, "ctrl-s", None),
    ("SaveFileAs", "Save file as", "File", Editor, None, "ctrl-shift-s", None),
    ("JumpToTab1", "Go to tab 1", "Navigate", Pane, None, "alt-1", None),
    ("JumpToTab2", "Go to tab 2", "Navigate", Pane, None, "alt-2", None),
    ("JumpToTab3", "Go to tab 3", "Navigate", Pane, None, "alt-3", None),
    ("JumpToTab4", "Go to tab 4", "Navigate", Pane, None, "alt-4", None),
    ("JumpToTab5", "Go to tab 5", "Navigate", Pane, None, "alt-5", None),
    ("JumpToTab6", "Go to tab 6", "Navigate", Pane, None, "alt-6", None),
    ("JumpToTab7", "Go to tab 7", "Navigate", Pane, None, "alt-7", None),
    ("JumpToTab8", "Go to tab 8", "Navigate", Pane, None, "alt-8", None),
    ("JumpToTab9", "Go to tab 9", "Navigate", Pane, None, "alt-9", None),
    ("JumpToProject1", "Go to project 1", "Navigate", App, None, "ctrl-1", None),
    ("JumpToProject2", "Go to project 2", "Navigate", App, None, "ctrl-2", None),
    ("JumpToProject3", "Go to project 3", "Navigate", App, None, "ctrl-3", None),
    ("JumpToProject4", "Go to project 4", "Navigate", App, None, "ctrl-4", None),
    ("JumpToProject5", "Go to project 5", "Navigate", App, None, "ctrl-5", None),
    ("JumpToProject6", "Go to project 6", "Navigate", App, None, "ctrl-6", None),
    ("JumpToProject7", "Go to project 7", "Navigate", App, None, "ctrl-7", None),
    ("JumpToProject8", "Go to project 8", "Navigate", App, None, "ctrl-8", None),
    ("JumpToProject9", "Go to project 9", "Navigate", App, None, "ctrl-9", None),
    ("NewAgent1", "New agent 1", "Layout", Project, None, "ctrl-alt-1", None),
    ("NewAgent2", "New agent 2", "Layout", Project, None, "ctrl-alt-2", None),
    ("NewAgent3", "New agent 3", "Layout", Project, None, "ctrl-alt-3", None),
    ("NewAgent4", "New agent 4", "Layout", Project, None, "ctrl-alt-4", None),
    ("NewAgent5", "New agent 5", "Layout", Project, None, "ctrl-alt-5", None),
    ("NewAgent6", "New agent 6", "Layout", Project, None, "ctrl-alt-6", None),
    ("NewAgent7", "New agent 7", "Layout", Project, None, "ctrl-alt-7", None),
    ("NewAgent8", "New agent 8", "Layout", Project, None, "ctrl-alt-8", None),
    ("NewAgent9", "New agent 9", "Layout", Project, None, "ctrl-alt-9", None),
}

pub fn descriptor(id: &str) -> Option<&'static ActionDescriptor> {
    ACTIONS.iter().find(|action| action.id == id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveBinding<'a> {
    pub action: &'static str,
    pub keystroke: &'a str,
    pub context: Option<&'static str>,
}

pub fn effective_bindings(settings: &muxel_core::Settings) -> Vec<EffectiveBinding<'_>> {
    let overrides: std::collections::HashMap<_, _> = settings
        .keybindings
        .iter()
        .map(|binding| (binding.action.as_str(), binding.keystroke.as_str()))
        .collect();
    let passthrough: Vec<_> = settings
        .terminal_passthrough_keys
        .iter()
        .map(|key| key.trim())
        .filter(|key| !key.is_empty())
        .collect();
    let mut bindings: Vec<_> = DEFAULT_KEYBINDINGS
        .iter()
        .map(|&(action, default, explicit)| {
            let keystroke = overrides.get(action).copied().unwrap_or(default);
            EffectiveBinding {
                action,
                keystroke,
                context: binding_context(action, keystroke, explicit, &passthrough),
            }
        })
        .collect();
    bindings.extend([
        EffectiveBinding {
            action: "SendTab",
            keystroke: "tab",
            context: Some("Terminal"),
        },
        EffectiveBinding {
            action: "SendBackTab",
            keystroke: "shift-tab",
            context: Some("Terminal"),
        },
        EffectiveBinding {
            action: "GlobalSearch",
            keystroke: "ctrl-p",
            context: Some("!Terminal"),
        },
        EffectiveBinding {
            action: "Quit",
            keystroke: "secondary-q",
            context: None,
        },
    ]);
    // Plain Ctrl+W remains terminal input for Readline/Vim. Keep the existing
    // terminal close chord and add Windows' close-active-document alias.
    #[cfg(not(target_os = "macos"))]
    bindings.push(EffectiveBinding {
        action: "ClosePane",
        keystroke: "ctrl-shift-w",
        context: Some("Terminal"),
    });
    #[cfg(target_os = "windows")]
    bindings.push(EffectiveBinding {
        action: "ClosePane",
        keystroke: "ctrl-f4",
        context: None,
    });
    #[cfg(not(target_os = "macos"))]
    for keystroke in ["ctrl-c", "ctrl-insert"] {
        for context in ["Input", "TextView"] {
            bindings.push(EffectiveBinding {
                action: "CopySelection",
                keystroke,
                context: Some(context),
            });
        }
    }
    bindings
}

#[derive(Clone, Copy, Default)]
pub struct Selection {
    pub project: bool,
    pub pane: bool,
    pub terminal: bool,
    pub editor: bool,
}

impl ActionDescriptor {
    pub fn unavailable(&self, selection: Selection) -> Option<&'static str> {
        match self.target {
            Target::Project if !selection.project => Some("Select a project first."),
            Target::Pane if !selection.pane => Some("Select a pane first."),
            Target::Terminal if !selection.terminal => Some("Select a terminal first."),
            Target::Editor if !selection.editor => Some("Select an editable file first."),
            _ => None,
        }
    }
}

/// Explicit contexts preserve the existing terminal search override. Plain C0
/// chords and configured passthrough otherwise keep their established routing.
pub fn binding_context<'a>(
    action: &str,
    keystroke: &str,
    explicit: Option<&'a str>,
    passthrough: &[&str],
) -> Option<&'a str> {
    if let Some(context) = explicit {
        return Some(context);
    }
    if passthrough
        .iter()
        .any(|key| key.eq_ignore_ascii_case(keystroke.trim()))
        || (muxel_core::is_plain_ctrl_letter(keystroke) && action != "NewTab")
    {
        return Some("!Terminal");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{ACTIONS, Selection, binding_context, descriptor, effective_bindings};

    #[test]
    fn all_preexisting_action_ids_bindings_and_contexts_are_unchanged() {
        let original: Vec<(String, String, Option<String>)> =
            serde_json::from_str(include_str!("../tests/fixtures/keybindings-v0.1.9.json"))
                .unwrap();
        let current: std::collections::HashMap<_, _> = super::DEFAULT_KEYBINDINGS
            .iter()
            .map(|&(id, key, context)| (id, (key, context)))
            .collect();
        assert_eq!(original.len(), 59);
        assert_eq!(super::DEFAULT_KEYBINDINGS.len(), original.len() + 2);
        for (id, key, context) in original {
            let expected_key = if id == "ClosePane" {
                if cfg!(target_os = "macos") {
                    "cmd-w"
                } else {
                    "ctrl-w"
                }
            } else {
                key.as_str()
            };
            assert_eq!(
                current.get(id.as_str()),
                Some(&(expected_key, context.as_deref())),
                "{id}"
            );
        }
        assert_eq!(
            current.get("ShowActionSheet"),
            Some(&("ctrl-shift-space", None))
        );
        assert_eq!(current.get("ReadAloud"), Some(&("ctrl-shift-r", None)));
    }

    #[test]
    fn platform_close_bindings_preserve_terminal_ctrl_w() {
        let settings = muxel_core::Settings::default();
        let bindings = effective_bindings(&settings);
        let primary = bindings
            .iter()
            .find(|binding| binding.action == "ClosePane")
            .unwrap();

        #[cfg(target_os = "macos")]
        assert_eq!((primary.keystroke, primary.context), ("cmd-w", None));
        #[cfg(not(target_os = "macos"))]
        assert_eq!(
            (primary.keystroke, primary.context),
            ("ctrl-w", Some("!Terminal"))
        );
        #[cfg(not(target_os = "macos"))]
        assert!(bindings.iter().any(|binding| {
            binding.action == "ClosePane"
                && binding.keystroke == "ctrl-shift-w"
                && binding.context == Some("Terminal")
        }));
        #[cfg(target_os = "windows")]
        assert!(bindings.iter().any(|binding| {
            binding.action == "ClosePane"
                && binding.keystroke == "ctrl-f4"
                && binding.context.is_none()
        }));
    }

    #[test]
    fn saved_overrides_and_terminal_passthrough_remain_effective() {
        let settings = muxel_core::Settings {
            keybindings: vec![
                muxel_core::KeyBindingCfg {
                    action: "NewTab".into(),
                    keystroke: "alt-t".into(),
                },
                muxel_core::KeyBindingCfg {
                    action: "NewTab".into(),
                    keystroke: "ctrl-alt-t".into(),
                },
                muxel_core::KeyBindingCfg {
                    action: "ShowActionSheet".into(),
                    keystroke: "ctrl-shift-x".into(),
                },
                muxel_core::KeyBindingCfg {
                    action: "ClosePane".into(),
                    keystroke: "alt-w".into(),
                },
            ],
            terminal_passthrough_keys: vec![" CTRL-SHIFT-X ".into()],
            ..Default::default()
        };
        let bindings = effective_bindings(&settings);
        let binding = |id| {
            *bindings
                .iter()
                .find(|binding| binding.action == id)
                .unwrap()
        };
        assert_eq!(binding("NewTab").keystroke, "ctrl-alt-t");
        assert_eq!(binding("ClosePane").keystroke, "alt-w");
        assert_eq!(binding("ShowActionSheet").context, Some("!Terminal"));
        assert_eq!(binding("SaveFile").context, Some("!Terminal"));
        assert_eq!(binding("SearchTerminal").context, Some("Terminal"));
        #[cfg(not(target_os = "macos"))]
        assert!(bindings.iter().any(|binding| {
            binding.action == "ClosePane"
                && binding.keystroke == "ctrl-shift-w"
                && binding.context == Some("Terminal")
        }));
        #[cfg(target_os = "windows")]
        assert!(bindings.iter().any(|binding| {
            binding.action == "ClosePane"
                && binding.keystroke == "ctrl-f4"
                && binding.context.is_none()
        }));
        assert!(
            bindings
                .iter()
                .any(|binding| binding.action == "GlobalSearch"
                    && binding.keystroke == "ctrl-p"
                    && binding.context == Some("!Terminal"))
        );
        assert_eq!(binding_context("NewTab", "ctrl-t", None, &[]), None);
        assert_eq!(
            binding_context(
                "SearchTerminal",
                "ctrl-shift-f",
                Some("Terminal"),
                &["ctrl-shift-f"]
            ),
            Some("Terminal")
        );
    }

    #[test]
    fn action_sheet_keys_are_unique_and_only_real_targets_are_available() {
        let mut keys = std::collections::HashSet::new();
        for action in ACTIONS {
            if let Some(key) = action.sheet_key {
                assert!(keys.insert(key), "{key}");
            }
        }
        assert_eq!(
            descriptor("ClosePane")
                .unwrap()
                .unavailable(Selection::default()),
            Some("Select a pane first.")
        );
        assert_eq!(
            descriptor("SaveFile").unwrap().unavailable(Selection {
                terminal: true,
                pane: true,
                ..Selection::default()
            }),
            Some("Select an editable file first.")
        );
        assert!(
            descriptor("GlobalSearch")
                .unwrap()
                .unavailable(Selection::default())
                .is_none()
        );
    }
}
