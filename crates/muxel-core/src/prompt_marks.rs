//! The OSC 133 prompt marks muxel adds to the local Windows shells it starts.
//!
//! Windows has no foreground process group, and cmd and PowerShell run batch
//! files, cmdlets and scripts in their own process, so a pane can only tell its
//! shell sits at a prompt if the shell says so. muxel asks cmd and PowerShell to
//! print `ESC ] 133 ; A` before their prompt and `ESC ] 133 ; B` after it, the
//! marks VS Code and Windows Terminal use for shell integration. Each wraps the
//! user's own prompt, so what they see is unchanged. When a launch can't be
//! marked this way, nothing is added and the pane's close asks, as before.

/// What to add to a launch so its shell marks every prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptMarks {
    /// cmd reads its prompt from `PROMPT`: set it to this.
    CmdPrompt(String),
    /// Append these to PowerShell's arguments.
    PowerShellArgs(Vec<String>),
}

/// The prompt wrapper PowerShell runs after its profiles. It reads `$?` first
/// and hands the original prompt the same value, so a prompt that shows the
/// last command's status (oh-my-posh, starship) still sees it.
const POWERSHELL_WRAP: &str = concat!(
    "$global:__muxel_prompt = $function:prompt; ",
    "function global:prompt { ",
    "$ok = $global:?; $e = [char]27; ",
    "if (-not $ok) { Write-Error muxel -ErrorAction Ignore }; ",
    "\"$e]133;A$e\\\" + (& $global:__muxel_prompt) + \"$e]133;B$e\\\" }",
);

/// PowerShell options that leave it an interactive session reading the
/// profile's prompt. Anything else (`-Command`, `-File`, `-EncodedCommand`, a
/// script path) runs something of its own, so the launch isn't marked.
const POWERSHELL_SWITCHES: &[&str] = &[
    "-nologo",
    "-nol",
    "-noprofile",
    "-nop",
    "-noexit",
    "-noe",
    "-mta",
    "-sta",
    "-login",
    "-l",
    "-interactive",
    "-i",
];
const POWERSHELL_EXECUTION_POLICY: &[&str] = &["-executionpolicy", "-ep", "-ex", "-exec"];

/// The marks for launching `program` with `args`, or `None` when it isn't a
/// shell muxel marks, or its arguments already run something else. `prompt` is
/// the `PROMPT` the shell would otherwise get (its preset's, else muxel's own
/// environment's); cmd's default is `$P$G`.
pub fn for_launch(program: &str, args: &[String], prompt: Option<&str>) -> Option<PromptMarks> {
    let leaf = program
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match leaf.strip_suffix(".exe").unwrap_or(&leaf) {
        "cmd" => {
            let inner = prompt.filter(|prompt| !prompt.is_empty()).unwrap_or("$P$G");
            Some(PromptMarks::CmdPrompt(format!(
                "$e]133;A$e\\{inner}$e]133;B$e\\"
            )))
        }
        "powershell" | "pwsh" => {
            let mut rest = args.iter().map(|arg| arg.to_ascii_lowercase());
            let mut no_exit = false;
            while let Some(arg) = rest.next() {
                if POWERSHELL_EXECUTION_POLICY.contains(&arg.as_str()) {
                    rest.next()?;
                } else if POWERSHELL_SWITCHES.contains(&arg.as_str()) {
                    no_exit |= matches!(arg.as_str(), "-noexit" | "-noe");
                } else {
                    return None;
                }
            }
            let mut add = Vec::new();
            if !no_exit {
                add.push("-NoExit".to_string());
            }
            add.push("-EncodedCommand".to_string());
            add.push(crate::winshell::encoded_command(POWERSHELL_WRAP));
            Some(PromptMarks::PowerShellArgs(add))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{PromptMarks, for_launch};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn cmd_wraps_the_prompt_it_would_have_had() {
        let marked = |prompt| match for_launch(r"C:\Windows\System32\CMD.EXE", &[], prompt) {
            Some(PromptMarks::CmdPrompt(value)) => value,
            other => panic!("{other:?}"),
        };
        assert_eq!(marked(None), "$e]133;A$e\\$P$G$e]133;B$e\\");
        assert_eq!(marked(Some("")), "$e]133;A$e\\$P$G$e]133;B$e\\");
        assert_eq!(
            marked(Some("$T $P$_$G")),
            "$e]133;A$e\\$T $P$_$G$e]133;B$e\\"
        );
    }

    #[test]
    fn powershell_is_marked_only_when_it_would_be_an_interactive_prompt() {
        for program in [
            "powershell.exe",
            "pwsh",
            r"C:\Program Files\PowerShell\7\pwsh.exe",
        ] {
            for plain in [
                args(&[]),
                args(&["-NoLogo", "-NoProfile"]),
                args(&["-ExecutionPolicy", "Bypass", "-nop"]),
                args(&["-Login"]),
            ] {
                let Some(PromptMarks::PowerShellArgs(added)) = for_launch(program, &plain, None)
                else {
                    panic!("{program} {plain:?}");
                };
                assert_eq!(added[0], "-NoExit", "{program} {plain:?}");
                assert_eq!(added[1], "-EncodedCommand");
            }
            // An -NoExit of its own isn't repeated.
            let Some(PromptMarks::PowerShellArgs(added)) =
                for_launch(program, &args(&["-NoExit"]), None)
            else {
                panic!("{program} -NoExit");
            };
            assert_eq!(added[0], "-EncodedCommand");
            for runs_something in [
                args(&["-Command", "Get-Date"]),
                args(&["-c", "dir"]),
                args(&["-File", "setup.ps1"]),
                args(&["-EncodedCommand", "AAAA"]),
                args(&["script.ps1"]),
                args(&["-NoExit", "-Command", "Import-Module x"]),
                args(&["-ExecutionPolicy"]),
            ] {
                assert_eq!(
                    for_launch(program, &runs_something, None),
                    None,
                    "{runs_something:?}"
                );
            }
        }
    }

    #[test]
    fn other_programs_are_not_marked() {
        for program in [
            "bash.exe",
            "ssh",
            "wsl.exe",
            "codex.cmd",
            "python",
            "cmd-wrapper.exe",
            "",
        ] {
            assert_eq!(for_launch(program, &[], None), None, "{program}");
        }
    }
}
