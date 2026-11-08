//! The OSC 133 prompt marks muxel adds to the local PowerShell it starts.
//!
//! Windows has no foreground process group, and PowerShell runs cmdlets,
//! scripts, thread jobs and event actions in its own process, so a pane can only
//! tell its shell sits idle at a prompt if the shell says so. muxel wraps
//! PowerShell's prompt function so it prints `ESC ] 133 ; A` before the user's
//! own prompt and `ESC ] 133 ; B` after it, the marks Windows Terminal uses for
//! shell integration. That `B` is [`BUSY_MARK`] unless the shell found nothing
//! running inside itself. What the user sees is unchanged. cmd isn't marked: an
//! echoing batch file prints cmd's `PROMPT` while it runs, so a mark there
//! can't tell a prompt from a running batch. An unmarked cmd or PowerShell
//! prints no marks, so its pane's close asks.

macro_rules! busy_option {
    () => {
        "muxel=busy"
    };
}

/// The OSC payload of the wrapper's closing mark when the shell found work
/// running inside itself, or couldn't check. The wrapper's mark is the
/// prompt's last, so it decides over any marks the user's own prompt prints.
pub const BUSY_MARK: &str = concat!("133;B;", busy_option!());

/// The prompt wrapper PowerShell runs after its profiles. It reads `$?` first
/// and hands the original prompt the same value, so a prompt that shows the
/// last command's status (oh-my-posh, starship) still sees it. After the
/// original prompt has run, it checks for work that outlives the prompt inside
/// PowerShell's own process, where no child process shows it, and for input
/// the shell hasn't read:
///
/// - `$NestedPromptLevel`: a debugger stop or `$host.EnterNestedPrompt()`;
/// - an unfinished job (`Get-Job`) of any type, including a module's own job
///   types, which can run without a runspace;
/// - any event subscriber (`Get-EventSubscriber -Force`);
/// - a runspace beyond the shell's own (`Get-Runspace`);
/// - a key waiting in the console's input buffer (`[Console]::KeyAvailable`).
///
/// The prompt runs before the line editor reads anything, so keys typed while
/// the last command ran, a whole command sent with Enter included, are still
/// waiting there. .NET's `KeyAvailable` skips key-up and lone modifier
/// records; the host's `$Host.UI.RawUI.KeyAvailable` stays true at every
/// prompt after a Ctrl+C, though the line editor has read every key. It's
/// read last, as late as the prompt can.
///
/// Unless all are clear, the closing mark is [`BUSY_MARK`]; a check that
/// throws leaves it busy. The wrapper costs about 0.15 ms per prompt on
/// PowerShell 7 and 0.6 ms on Windows PowerShell 5.1.
const POWERSHELL_WRAP: &str = concat!(
    "$global:__muxel_prompt = $function:prompt; ",
    "function global:prompt { ",
    "$ok = $global:?; $e = [char]27; ",
    "if (-not $ok) { Write-Error muxel -ErrorAction Ignore }; ",
    "$text = & $global:__muxel_prompt; ",
    "$busy = $true; ",
    "try { $busy = -not ($NestedPromptLevel -eq 0 -and ",
    "-not (Get-Job -ErrorAction Ignore | Where-Object { $_.State -notin 'Completed', 'Failed', 'Stopped' }) -and ",
    "-not (Get-EventSubscriber -Force -ErrorAction Ignore) -and ",
    "@(Get-Runspace -ErrorAction Ignore).Count -eq 1 -and ",
    "-not [Console]::KeyAvailable) } catch { }; ",
    "\"$e]133;A$e\\\" + $text + \"$e]133;B$(if ($busy) { ';",
    busy_option!(),
    "' })$e\\\" }",
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

/// The arguments to append so launching `program` with `args` marks every
/// prompt, or `None` when it isn't PowerShell, or its arguments already run
/// something else.
pub fn for_launch(program: &str, args: &[String]) -> Option<Vec<String>> {
    let leaf = program
        .trim()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(
        leaf.strip_suffix(".exe").unwrap_or(&leaf),
        "powershell" | "pwsh"
    ) {
        return None;
    }
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
    Some(add)
}

#[cfg(test)]
mod tests {
    use super::for_launch;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
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
                let Some(added) = for_launch(program, &plain) else {
                    panic!("{program} {plain:?}");
                };
                assert_eq!(added[0], "-NoExit", "{program} {plain:?}");
                assert_eq!(added[1], "-EncodedCommand");
            }
            // An -NoExit of its own isn't repeated.
            let Some(added) = for_launch(program, &args(&["-NoExit"])) else {
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
                    for_launch(program, &runs_something),
                    None,
                    "{runs_something:?}"
                );
            }
        }
    }

    #[test]
    fn other_programs_are_not_marked() {
        for program in [
            "cmd.exe",
            r"C:\Windows\System32\CMD.EXE",
            "bash.exe",
            "ssh",
            "wsl.exe",
            "codex.cmd",
            "python",
            "cmd-wrapper.exe",
            "",
        ] {
            assert_eq!(for_launch(program, &[]), None, "{program}");
        }
    }
}
