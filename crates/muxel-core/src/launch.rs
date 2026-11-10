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
    /// program, counts as a command, and so does a wrapper that starts one
    /// (`npx`, `uvx`, `wsl`, `ssh`): the program it wraps isn't looked at.
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

/// What a terminal pane's child process is to the command it runs, which
/// decides whose exit the child's is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PaneTransport {
    /// The child is the command.
    #[default]
    Direct,
    /// `ssh … exec <command>`, which exits with the command's status, or with
    /// 255 when ssh itself fails.
    Ssh,
    /// `sshpass … ssh … exec <command>`, which exits 0 only when the command
    /// did. Its other codes may be the command's, ssh's 255, or sshpass's own
    /// failures (1 to 6).
    SshPass,
    /// A tmux client in front of the command, local or remote: a lost
    /// server, a detach and the session ending all end it without passing on
    /// the command's status.
    Client,
}

impl PaneTransport {
    /// A remote pane's transport: through tmux, or ssh straight to the
    /// command, with or without `sshpass` in front.
    pub fn remote(tmux: bool, sshpass: bool) -> Self {
        match (tmux, sshpass) {
            (true, _) => Self::Client,
            (false, true) => Self::SshPass,
            (false, false) => Self::Ssh,
        }
    }

    /// Whether the child's exit, with `code` (`None`: unknown) or killed by a
    /// signal, is the command's own: it ended, and how. A job's own exit is
    /// its finish; a transport's is no evidence about the job, and neither is
    /// a code the transport could have returned itself.
    pub fn exit_is_the_commands(self, code: Option<i32>, signalled: bool) -> bool {
        match self {
            Self::Direct => true,
            Self::Ssh => !signalled && code.is_some_and(|code| code != 255),
            Self::SshPass => !signalled && code == Some(0),
            Self::Client => false,
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
    /// POSIX `sh` and the shells that keep its command line (bash, zsh, dash,
    /// ksh): options, then `-c`'s command or a script file.
    Sh,
    /// fish, nu, xonsh, tcsh and csh, known by their command flag alone: `-c`
    /// alone or in a group (`-lc`), or `--command(s)`. A script they run by
    /// path reads as a session.
    CommandFlag,
    /// elvish, whose options are single-dash words (`-norc`): `-c` alone.
    Elvish,
    /// pwsh and Windows PowerShell: always a session, as on master. Their
    /// command lines (two hosts with different options, prefixes and
    /// stdin rules) kept turning up cases read the wrong way, and a
    /// session read as a job finishes falsely when it quits.
    PowerShell,
    Cmd,
}

fn shell_of(name: &str) -> Option<ShellFamily> {
    match name {
        "sh" | "bash" | "zsh" | "dash" | "ksh" => Some(ShellFamily::Sh),
        "fish" | "nu" | "xonsh" | "tcsh" | "csh" => Some(ShellFamily::CommandFlag),
        "elvish" => Some(ShellFamily::Elvish),
        "pwsh" | "powershell" => Some(ShellFamily::PowerShell),
        "cmd" => Some(ShellFamily::Cmd),
        _ => None,
    }
}

impl ShellFamily {
    /// Whether these arguments start the shell to run a command or a script
    /// and exit. Each shell reads its own options only up to the command, so
    /// a word after it (`/c` after `cmd /k`) is the command's.
    fn runs_a_command(self, args: &[String]) -> bool {
        match self {
            Self::Sh => sh_runs_a_command(args),
            Self::CommandFlag => args.iter().any(|arg| {
                matches!(arg.as_str(), "--command" | "--commands")
                    || arg
                        .strip_prefix('-')
                        .is_some_and(|flags| !flags.starts_with('-') && flags.contains('c'))
            }),
            Self::Elvish => args.iter().any(|arg| arg == "-c"),
            Self::PowerShell => false,
            Self::Cmd => cmd_runs_a_command(args),
        }
    }
}

/// POSIX `sh` (and bash): options, then the first operand, which is `-c`'s
/// command string or else a script file. `-c` exits after its command even
/// with `-i`; `-s` reads commands from the terminal, its operands only
/// setting `$1`…. `-o`/`+o` (and bash's `-O`/`+O`) take an option name,
/// and bash's `--rcfile`/`--init-file` a file.
fn sh_runs_a_command(args: &[String]) -> bool {
    let (mut command, mut stdin) = (false, false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let arg = arg.as_str();
        if matches!(arg, "--" | "-") {
            // The end of the options: what follows is the first operand.
            return command || (!stdin && args.next().is_some());
        }
        if let Some(long) = arg.strip_prefix("--") {
            if matches!(long, "rcfile" | "init-file") {
                args.next();
            }
            continue;
        }
        let Some(group) = arg
            .strip_prefix(['-', '+'])
            .filter(|group| !group.is_empty())
        else {
            // The first operand: `-c`'s command, a script, or `-s`'s `$1`.
            return command || !stdin;
        };
        if arg.starts_with('-') {
            command |= group.contains('c');
            stdin |= group.contains('s');
        }
        for _ in group.chars().filter(|letter| matches!(letter, 'o' | 'O')) {
            args.next();
        }
    }
    command
}

/// cmd reads its switches up to the first `/c` or `/r` (run the command and
/// exit) or `/k` (run it and stay open); the rest is the command's.
fn cmd_runs_a_command(args: &[String]) -> bool {
    for arg in args {
        match arg
            .strip_prefix('/')
            .and_then(|switch| switch.chars().next())
            .map(|letter| letter.to_ascii_lowercase())
        {
            Some('c' | 'r') => return true,
            Some('k') => return false,
            _ => {}
        }
    }
    false
}

/// How an agent is started for a single run instead of a conversation.
enum OneShot {
    /// Any of these flags, alone or as `--flag=value`.
    Flags(&'static [&'static str]),
    /// One of `names` as the agent's subcommand: its first operand, once the
    /// `values` options' values are skipped. A prompt is an operand too, so
    /// a word after one (`codex resume <id> e`) is never the subcommand.
    Subcommand {
        names: &'static [&'static str],
        values: &'static [&'static str],
    },
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
            Self::Subcommand { names, values } => {
                first_operand(args, values).is_some_and(|operand| names.contains(&operand))
            }
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

/// An agent's first operand, skipping the options and the values of those
/// in `values` (`-m o3`; `--model=o3` carries its own). `None` without one,
/// or after `--`, which makes the rest prompt text.
fn first_operand<'a>(args: &'a [String], values: &[&str]) -> Option<&'a str> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            return None;
        }
        if !arg.starts_with('-') || arg == "-" {
            return Some(arg);
        }
        if values.contains(&arg.as_str()) {
            args.next();
        }
    }
    None
}

/// The agent providers muxel knows, the programs of its built-in agent presets
/// ([`crate::AgentPreset::builtin_agents`]), with their one-shot forms. Claude,
/// Codex and Grok's are from their `--help` (Codex 0.167's for its options
/// that take a value); opencode `run` and its options, Amp `--execute`, Pi
/// `--print` and Ollama's `run` with a prompt from their documentation.
const AGENTS: &[(&str, OneShot)] = &[
    ("claude", OneShot::Flags(&["-p", "--print"])),
    (
        "codex",
        OneShot::Subcommand {
            names: &["exec", "e", "review"],
            values: &[
                "-c",
                "--config",
                "--enable",
                "--disable",
                "--remote",
                "--remote-auth-token-env",
                "-i",
                "--image",
                "-m",
                "--model",
                "--local-provider",
                "-p",
                "--profile",
                "-s",
                "--sandbox",
                "-C",
                "--cd",
                "--add-dir",
                "-a",
                "--ask-for-approval",
            ],
        },
    ),
    (
        "grok",
        OneShot::Flags(&["-p", "--single", "--prompt-file", "--prompt-json"]),
    ),
    (
        "opencode",
        OneShot::Subcommand {
            names: &["run"],
            values: &[
                "-m",
                "--model",
                "-s",
                "--session",
                "-p",
                "--prompt",
                "--agent",
                "--port",
                "--hostname",
                "--log-level",
            ],
        },
    ),
    ("amp", OneShot::Flags(&["-x", "--execute"])),
    ("pi", OneShot::Flags(&["-p", "--print"])),
    ("ollama", OneShot::Ollama),
    ("muse", OneShot::Never),
    ("hermes", OneShot::Never),
];

#[cfg(test)]
mod tests {
    use super::{LaunchPurpose, PaneTransport, is_shell_program};
    use crate::AgentPreset;

    /// Only a child that is the command, or ssh passing on its status, exits
    /// for it. A tmux client's exit 0 can be a detach; its 1, a lost server.
    #[test]
    fn only_the_commands_own_exit_is_its_exit() {
        for (transport, code, signalled, expected) in [
            (PaneTransport::Direct, Some(3), false, true),
            (PaneTransport::Direct, None, true, true),
            (PaneTransport::Ssh, Some(0), false, true),
            (PaneTransport::Ssh, Some(3), false, true),
            (PaneTransport::Ssh, Some(255), false, false),
            (PaneTransport::Ssh, None, true, false),
            (PaneTransport::Ssh, None, false, false),
            // sshpass exits 0 only when the command did; its other codes
            // can be its own, or ssh's.
            (PaneTransport::remote(false, true), Some(0), false, true),
            (PaneTransport::remote(false, true), Some(3), false, false),
            (PaneTransport::remote(false, true), Some(5), false, false),
            (PaneTransport::remote(false, true), Some(255), false, false),
            (PaneTransport::remote(false, true), None, true, false),
            (PaneTransport::remote(false, false), Some(0), false, true),
            // A tmux client's exit is never the command's, sshpass or not.
            (PaneTransport::remote(true, true), Some(0), false, false),
            (PaneTransport::remote(true, false), Some(0), false, false),
            (PaneTransport::Client, Some(0), false, false),
            (PaneTransport::Client, Some(1), false, false),
        ] {
            assert_eq!(
                transport.exit_is_the_commands(code, signalled),
                expected,
                "{transport:?} {code:?} {signalled}"
            );
        }
    }

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
            // An agent muxel doesn't know counts as a command, and so does a
            // wrapper, whatever it starts.
            ("aider", &[]),
            ("npx", &["@openai/codex"]),
            ("wsl", &["claude"]),
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

    /// Codex's subcommand is its first operand: a prompt is one too, and an
    /// option's value isn't. `codex resume <id> e` resumes a conversation
    /// with the prompt `e`; `codex review` runs once (Codex 0.167 `--help`).
    #[test]
    fn an_agents_subcommand_is_its_first_operand() {
        for (program, args, expected) in [
            (
                "codex",
                &["resume", "0199a213-81c0-7800-8aa1-bbab2a035a53", "e"][..],
                LaunchPurpose::Session,
            ),
            ("codex", &["fix the exec path"], LaunchPurpose::Session),
            ("codex", &["--", "exec"], LaunchPurpose::Session),
            ("codex", &["--profile", "e"], LaunchPurpose::Session),
            ("codex", &["review", "--uncommitted"], LaunchPurpose::Job),
            (
                "codex",
                &[
                    "-m",
                    "o3",
                    "-c",
                    "model_reasoning_effort=high",
                    "exec",
                    "fix it",
                ],
                LaunchPurpose::Job,
            ),
            (
                "opencode",
                &["--model", "x/y", "run", "hi"],
                LaunchPurpose::Job,
            ),
            ("opencode", &["--session", "run"], LaunchPurpose::Session),
            ("opencode", &["./run"], LaunchPurpose::Session),
        ] {
            assert_eq!(purpose(Some(program), args), expected, "{program} {args:?}");
        }
    }

    /// PowerShell is a session whatever it's started with, as on master: a
    /// pane running a PowerShell command doesn't finish when it exits.
    #[test]
    fn powershell_is_always_a_session() {
        for (program, args) in [
            ("pwsh", &[][..]),
            ("pwsh.exe", &["-NoLogo"]),
            ("pwsh", &["-Command", "make"]),
            ("pwsh", &["build.ps1"]),
            ("pwsh", &["-File", "-"]),
            ("powershell", &["-File", "x.ps1"]),
            ("powershell.exe", &["Get-Date"]),
            ("powershell.exe", &["-i", "Text", "-Command", "Get-Date"]),
            ("pwsh", &["-ec", "ZQB4AGkAdAA="]),
        ] {
            assert_eq!(
                purpose(Some(program), args),
                LaunchPurpose::Session,
                "{program} {args:?}"
            );
        }
    }

    /// A POSIX shell runs a script given by path, and
    /// each shell's options end where its command begins.
    #[test]
    fn a_shell_runs_a_script_and_its_options_end_at_the_command() {
        for (program, args, expected) in [
            ("bash", &["build.sh"][..], LaunchPurpose::Job),
            ("sh", &["-e", "build.sh", "--fast"], LaunchPurpose::Job),
            ("bash", &["--", "build.sh"], LaunchPurpose::Job),
            ("bash", &["-o", "vi"], LaunchPurpose::Session),
            ("bash", &["-O", "extglob", "-l"], LaunchPurpose::Session),
            ("zsh", &["-s", "first"], LaunchPurpose::Session),
            ("bash", &["--init-file", "dev.rc"], LaunchPurpose::Session),
            // elvish's `-norc` is a word, not a group holding `c`.
            ("elvish", &["-norc"], LaunchPurpose::Session),
            ("elvish", &["-c", "make"], LaunchPurpose::Job),
            // cmd's `/k` stays open; a `/c` after it is the command's.
            ("cmd", &["/k", "echo", "/c"], LaunchPurpose::Session),
            ("cmd", &["/d", "/s", "/c", "build.bat"], LaunchPurpose::Job),
        ] {
            assert_eq!(purpose(Some(program), args), expected, "{program} {args:?}");
        }
    }
}
