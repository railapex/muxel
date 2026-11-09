//! What a terminal pane was started to do, which decides whether its process
//! exiting is its finish.

/// What a terminal pane was started to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchPurpose {
    /// An interactive agent conversation or shell. It finishes turns while it
    /// runs, so its process exiting (`/exit`, a kill, a crash, `exit` typed in
    /// a shell) finishes nothing.
    Session,
    /// A command run to completion: a literal command, a custom command preset,
    /// or an agent's one-shot run (`claude -p`, `codex exec`). Its exit is its
    /// finish, failing or not.
    Job,
}

impl LaunchPurpose {
    /// The purpose of a pane running `program` (`None`: the user's default
    /// shell) with `args`, as saved on its instance. The provider comes from the
    /// program and the purpose from its arguments, so a literal `claude` is
    /// still an interactive agent. An agent muxel doesn't know, like any other
    /// program, counts as a command.
    pub fn of(program: Option<&str>, args: &[String]) -> Self {
        let Some(name) = program_name(program) else {
            return Self::Session;
        };
        if let Some(shell) = shell_of(&name) {
            return if shell.runs_a_command(args) {
                Self::Job
            } else {
                Self::Session
            };
        }
        match AGENTS.iter().find(|(agent, _)| *agent == name) {
            Some((_, one_shot)) if one_shot.matches(args) => Self::Job,
            Some(_) => Self::Session,
            None => Self::Job,
        }
    }
}

/// Whether `program` is a shell (`None` or empty: the user's default shell).
pub fn is_shell_program(program: Option<&str>) -> bool {
    program_name(program).is_none_or(|name| shell_of(&name).is_some())
}

/// `program`'s file name, lowercased, without its directory or a Windows
/// launcher extension. `None` for the default shell.
fn program_name(program: Option<&str>) -> Option<String> {
    let program = program.map(str::trim).filter(|p| !p.is_empty())?;
    let leaf = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let stem = [".exe", ".cmd", ".bat"]
        .iter()
        .find_map(|ext| leaf.strip_suffix(ext))
        .unwrap_or(&leaf);
    Some(stem.to_string())
}

#[derive(Clone, Copy)]
enum ShellFamily {
    /// `sh`-style: `-c` alone or in a group (`-lc`), or `--command(s)`.
    Posix,
    PowerShell,
    Cmd,
}

fn shell_of(name: &str) -> Option<ShellFamily> {
    match name {
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "tcsh" | "csh" | "nu" | "elvish"
        | "xonsh" => Some(ShellFamily::Posix),
        "pwsh" | "powershell" => Some(ShellFamily::PowerShell),
        "cmd" => Some(ShellFamily::Cmd),
        _ => None,
    }
}

impl ShellFamily {
    /// Whether these arguments start the shell to run a command and exit.
    /// PowerShell's `-NoExit` keeps it at its prompt after the command. A
    /// POSIX shell's `-c` exits after its command even with `-i`.
    fn runs_a_command(self, args: &[String]) -> bool {
        if matches!(self, Self::PowerShell)
            && args
                .iter()
                .any(|arg| matches!(arg.to_ascii_lowercase().as_str(), "-noexit" | "-noe"))
        {
            return false;
        }
        args.iter().any(|arg| match self {
            Self::Posix => {
                matches!(arg.as_str(), "--command" | "--commands")
                    || arg
                        .strip_prefix('-')
                        .is_some_and(|flags| !flags.starts_with('-') && flags.contains('c'))
            }
            Self::PowerShell => matches!(
                arg.to_ascii_lowercase().as_str(),
                "-c" | "-command"
                    | "-commandwithargs"
                    | "-cwa"
                    | "-f"
                    | "-file"
                    | "-e"
                    | "-ec"
                    | "-encodedcommand"
            ),
            // `/k` runs a command and stays open.
            Self::Cmd => matches!(arg.to_ascii_lowercase().as_str(), "/c" | "/r"),
        })
    }
}

/// How an agent is started for a single run instead of a conversation.
enum OneShot {
    /// Any of these flags, alone or as `--flag=value`.
    Flags(&'static [&'static str]),
    /// Any of these subcommands.
    Subcommand(&'static [&'static str]),
    /// `ollama`: interactive only for `launch <agent>`, or `run <model>`
    /// without a prompt. Its other subcommands (`pull`, `serve`, a `run` with
    /// a prompt) are jobs.
    Ollama,
    /// No one-shot form known: always a session.
    Never,
}

impl OneShot {
    fn matches(&self, args: &[String]) -> bool {
        match self {
            Self::Flags(flags) => args.iter().any(|arg| {
                flags.iter().any(|flag| {
                    arg == flag
                        || (flag.starts_with("--")
                            && arg
                                .strip_prefix(flag)
                                .is_some_and(|rest| rest.starts_with('=')))
                })
            }),
            Self::Subcommand(names) => args.iter().any(|arg| names.contains(&arg.as_str())),
            Self::Ollama => {
                let mut positional = args.iter().filter(|arg| !arg.starts_with('-'));
                match positional.next().map(String::as_str) {
                    Some("launch") => false,
                    Some("run") => positional.count() != 1,
                    _ => true,
                }
            }
            Self::Never => false,
        }
    }
}

/// The agent providers muxel knows, the programs of its built-in agent presets
/// ([`crate::AgentPreset::builtin_agents`]), with their one-shot forms. Claude,
/// Codex and Grok's are from their `--help`; opencode `run`, Amp `--execute`,
/// Pi `--print` and Ollama's `run` with a prompt from their documentation.
const AGENTS: &[(&str, OneShot)] = &[
    ("claude", OneShot::Flags(&["-p", "--print"])),
    ("codex", OneShot::Subcommand(&["exec", "e"])),
    (
        "grok",
        OneShot::Flags(&["-p", "--single", "--prompt-file", "--prompt-json"]),
    ),
    ("opencode", OneShot::Subcommand(&["run"])),
    ("amp", OneShot::Flags(&["-x", "--execute"])),
    ("pi", OneShot::Flags(&["-p", "--print"])),
    ("ollama", OneShot::Ollama),
    ("muse", OneShot::Never),
    ("hermes", OneShot::Never),
];

#[cfg(test)]
mod tests {
    use super::{LaunchPurpose, is_shell_program};
    use crate::AgentPreset;

    fn purpose(program: Option<&str>, args: &[&str]) -> LaunchPurpose {
        let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
        LaunchPurpose::of(program, &args)
    }

    /// A new built-in agent needs its one-shot form decided here, or its
    /// conversations would count as commands.
    #[test]
    fn the_agent_table_is_the_built_in_agents() {
        let mut built_in: Vec<String> = AgentPreset::builtin_agents()
            .iter()
            .filter_map(|preset| super::program_name(preset.program.as_deref()))
            .collect();
        built_in.sort();
        built_in.dedup();
        let mut table: Vec<String> = super::AGENTS
            .iter()
            .map(|(name, _)| name.to_string())
            .collect();
        table.sort();
        assert_eq!(table, built_in);
    }

    #[test]
    fn every_built_in_preset_starts_a_session() {
        for preset in AgentPreset::defaults() {
            assert_eq!(
                LaunchPurpose::of(preset.program.as_deref(), &preset.args),
                LaunchPurpose::Session,
                "{}",
                preset.name
            );
        }
    }

    #[test]
    fn a_command_is_a_job() {
        for (program, args) in [
            // A literal command and a custom command preset alike.
            ("cargo", &["test", "--workspace"][..]),
            ("npm", &["run", "build"]),
            ("C:\\tools\\build.exe", &[]),
            ("/usr/bin/make", &["-j8"]),
            // An agent muxel doesn't know counts as a command.
            ("aider", &[]),
        ] {
            assert_eq!(
                purpose(Some(program), args),
                LaunchPurpose::Job,
                "{program}"
            );
        }
    }

    #[test]
    fn an_agents_one_shot_run_is_a_job_and_its_conversation_a_session() {
        for (program, args, expected) in [
            ("claude", &["-p", "fix the build"][..], LaunchPurpose::Job),
            ("claude.exe", &["--print", "hi"], LaunchPurpose::Job),
            ("claude", &[], LaunchPurpose::Session),
            ("claude", &["--resume", "abc"], LaunchPurpose::Session),
            ("codex", &["exec", "fix it"], LaunchPurpose::Job),
            (
                "codex",
                &["--config=x=y", "e", "fix it"],
                LaunchPurpose::Job,
            ),
            ("codex", &["resume", "--last"], LaunchPurpose::Session),
            ("grok", &["-p", "hi"], LaunchPurpose::Job),
            ("grok", &["--single=hi"], LaunchPurpose::Job),
            ("grok", &["--prompt-file", "task.md"], LaunchPurpose::Job),
            ("grok", &["hi"], LaunchPurpose::Session),
            ("opencode", &["run", "hi"], LaunchPurpose::Job),
            ("amp", &["-x", "hi"], LaunchPurpose::Job),
            ("pi", &["-p", "hi"], LaunchPurpose::Job),
            ("ollama", &["run", "llama3.2", "hi"], LaunchPurpose::Job),
            ("ollama", &["pull", "llama3.2"], LaunchPurpose::Job),
            ("ollama", &["run", "llama3.2"], LaunchPurpose::Session),
            ("muse", &["-p", "hi"], LaunchPurpose::Session),
        ] {
            assert_eq!(purpose(Some(program), args), expected, "{program} {args:?}");
        }
    }

    #[test]
    fn a_shell_is_a_session_unless_started_with_a_command() {
        for (program, args, expected) in [
            (None, &[][..], LaunchPurpose::Session),
            (Some(""), &[], LaunchPurpose::Session),
            (Some("bash"), &["-i", "-l"], LaunchPurpose::Session),
            (Some("bash"), &["--rcfile", "x"], LaunchPurpose::Session),
            (Some("bash"), &["-lc", "make"], LaunchPurpose::Job),
            // `-c` exits after its command, interactive or not.
            (Some("bash"), &["-i", "-c", "make"], LaunchPurpose::Job),
            (Some("fish"), &["--command", "make"], LaunchPurpose::Job),
            (Some("pwsh.exe"), &["-NoLogo"], LaunchPurpose::Session),
            (Some("pwsh"), &["-Command", "make"], LaunchPurpose::Job),
            (Some("powershell"), &["-File", "x.ps1"], LaunchPurpose::Job),
            // `-NoExit` keeps the prompt after the command.
            (
                Some("pwsh"),
                &["-NoExit", "-Command", "Import-Module x"],
                LaunchPurpose::Session,
            ),
            (
                Some("powershell.exe"),
                &["-noe", "-File", "profile.ps1"],
                LaunchPurpose::Session,
            ),
            (Some("cmd.exe"), &[], LaunchPurpose::Session),
            (
                Some("cmd.exe"),
                &["/k", "vcvars.bat"],
                LaunchPurpose::Session,
            ),
            (
                Some("cmd.exe"),
                &["/d", "/C", "build.bat"],
                LaunchPurpose::Job,
            ),
        ] {
            assert_eq!(purpose(program, args), expected, "{program:?} {args:?}");
        }
        assert!(is_shell_program(None));
        assert!(is_shell_program(Some("C:\\Windows\\System32\\cmd.exe")));
        assert!(!is_shell_program(Some("claude")));
    }
}
