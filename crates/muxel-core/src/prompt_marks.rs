//! The OSC 133 prompt marks muxel adds to the local PowerShell it starts.
//!
//! Windows has no foreground process group, and PowerShell runs cmdlets,
//! scripts, thread jobs and event actions in its own process, so a pane can only
//! tell its shell sits idle at a prompt if the shell says so. muxel wraps
//! PowerShell's prompt function so it prints `ESC ] 133 ; A` before the user's
//! own prompt and [`BUSY_MARK`] after it, and wraps its line editor so that a
//! plain `ESC ] 133 ; B` follows once the line editor has waited for a key, if
//! the prompt found nothing running inside the shell, and `ESC ] 133 ; C` as
//! each line it returns starts to run. These are the marks
//! Windows Terminal uses for shell integration. What the user sees is
//! unchanged. cmd isn't marked: an echoing batch file prints cmd's `PROMPT`
//! while it runs, so a mark there can't tell a prompt from a running batch. An
//! unmarked cmd or PowerShell prints no marks, so its pane's close asks.

macro_rules! busy_option {
    () => {
        "muxel=busy"
    };
}

/// Whether nothing is running inside the shell: no nested prompt, unfinished
/// job, other event subscriber or other runspace. `$mine` is the subscription
/// the check may ignore: none at the prompt, the one-shot itself as it fires.
macro_rules! nothing_running {
    () => {
        "$NestedPromptLevel -eq 0 -and -not (Get-Job -ErrorAction Ignore | Where-Object { $_.State -notin 'Completed', 'Failed', 'Stopped' }) -and -not (Get-EventSubscriber -Force -ErrorAction Ignore | Where-Object { $_.SubscriptionId -ne $mine }) -and @(Get-Runspace -ErrorAction Ignore).Count -eq 1"
    };
}

/// The OSC payload of the closing mark muxel's PowerShell wrapper prints on
/// every prompt it draws. A prompt being drawn doesn't mean the shell waits
/// for input: PSReadLine draws it again from inside a key handler, and a
/// command can call it. The wrapper's mark is the prompt's last, so it decides
/// over any marks the user's own prompt prints.
pub const BUSY_MARK: &str = concat!("133;B;", busy_option!());

/// The prompt wrapper PowerShell runs after its profiles. It reads `$?` first
/// and hands the original prompt the same value, so a prompt that shows the
/// last command's status (oh-my-posh, starship) still sees it.
///
/// A prompt being drawn doesn't mean the shell waits for input. PSReadLine
/// draws it again from inside a key handler, as a transient prompt (Starship,
/// oh-my-posh) does on Enter before it accepts the line, and the handler can
/// go on working; a running command can call `prompt` itself; a line editor
/// can work before it reads; keys can be on their way. So the prompt always
/// ends with [`BUSY_MARK`], and only the line editor's own wait for a key
/// grants idle.
///
/// After the original prompt has run, the wrapper makes sure the line editor's
/// `PSConsoleHostReadLine` (PSReadLine's, or whatever replaced it since the
/// last prompt) is wrapped. PowerShell calls it straight after drawing a
/// prompt, to read the next line. The wrap sets `$global:__muxel_reading` as
/// it starts and prints nothing; once the line editor returns a line, just
/// before PowerShell runs it, the wrap clears the flag and prints
/// `ESC ] 133 ; C`, as VS Code's shell integration does. The wrapper checks
/// for a line editor at every prompt, not once at startup, so one imported or
/// replaced later is wrapped too. The wrapped function is held in a closure,
/// not a global, so a hook that later chains the wrap can't loop back into
/// itself; that hook only prints `C` twice. The wrap hands the line editor
/// the same `$?`, which PSReadLine reads as the last command's status, and is
/// hidden from the debugger like PSReadLine's own.
///
/// When the prompt finds nothing running (below), it registers a hidden
/// one-shot handler for `PowerShell.OnIdle` (`-MaxTriggerCount 1
/// -SupportEvent`). PSReadLine raises that event only from its wait for a
/// key, once it has waited about 300 ms with no key available and none it read
/// ahead still queued. It raises none while a key handler works, while a
/// command runs, between the lines of a paste, or while a replacement line
/// editor works before it reads. The handler prints a plain `ESC ] 133 ; B`
/// only while the wrap's flag is set, so a PSReadLine read that a command
/// makes itself grants nothing, and only if the prompt's checks (below) still
/// pass as it fires. Work can start after the prompt has checked: a pipeline
/// started with `BeginInvoke` opens its runspace on another thread, which on
/// PowerShell 7 can come after the next prompt has drawn. A one-shot left
/// pending, by a prompt a command drew or by prompts quicker than PSReadLine's
/// wait, stays the only one: each later prompt sees it as a subscriber and
/// registers none, and it grants for whichever read it fires in. Without a
/// `PSConsoleHostReadLine` there's nothing to wrap, nothing grants idle, and
/// the pane's close asks.
///
/// The prompt, and the one-shot again as it fires, check for work that
/// outlives a command inside PowerShell's own process, where no child process
/// shows it:
///
/// - `$NestedPromptLevel`: a debugger stop or `$host.EnterNestedPrompt()`;
/// - an unfinished job (`Get-Job`) of any type, including a module's own job
///   types, which can run without a runspace;
/// - any event subscriber (`Get-EventSubscriber -Force`) but the one-shot
///   that is firing;
/// - a runspace beyond the shell's own (`Get-Runspace`).
///
/// Unless all are clear, the prompt registers no one-shot, or the one-shot
/// prints nothing, and the prompt's [`BUSY_MARK`] stands; a check that throws
/// leaves it standing. So a module that keeps an idle runspace or a standing
/// event subscriber, as current oh-my-posh streaming does with both, always
/// reads busy and its panes ask; so does a user's own `PowerShell.OnIdle`
/// subscriber.
///
/// Known limits:
///
/// - Idle comes about 300 ms after each prompt, once PSReadLine has waited
///   that long; a close before then asks. Work still starting as the one-shot
///   checks, such as a runspace that hasn't opened 300 ms after the
///   `BeginInvoke` that asked for it, isn't seen.
/// - A script that draws the prompt and then reads a line through
///   `PSConsoleHostReadLine`, as the host does, reads idle while it waits for
///   that line. Nothing runs while it waits.
/// - `Unregister-Event -Force` naming `PowerShell.OnIdle`, or every
///   subscriber (`Get-EventSubscriber -Force | Unregister-Event -Force`),
///   removes the one-shot, and that prompt asks. Without `-Force`,
///   `Unregister-Event` leaves hidden subscribers alone.
/// - .NET work started directly, such as `[System.Threading.Tasks.Task]::Run`
///   or a `System.Threading.Timer`, shows in none of these checks and runs on
///   while the pane reads idle. No check here proves that all work in the
///   shell has stopped.
/// - An Enter sent in the moment the one-shot fires lands before its `B`, and
///   that line's key handler may then work with nothing more printed. The
///   pane doesn't take that `B` as idle: after an Enter it reads busy until
///   the wrap's `C`. A line a command reads for itself never gets one, so the
///   pane stays busy until the next line goes through the line editor. The
///   pane also counts every mark the shell prints, so an Undo expiry with a
///   mark printed since the close asks.
///
/// Measured in a real console on a loaded machine, the wrapper costs about
/// 1 to 1.5 ms per prompt on Windows PowerShell 5.1 and 0.5 ms on PowerShell
/// 7. The one `OnIdle` event that grants each prompt idle takes another
/// 2.5 ms and 1.2 ms of CPU, and its checks 0.7 to 1.5 ms and 0.4 to 1 ms,
/// while the shell waits rather than as it draws the prompt.
const POWERSHELL_WRAP: &str = concat!(
    "$global:__muxel_prompt = $function:prompt; ",
    "function global:prompt { ",
    "$ok = $global:?; $e = [char]27; ",
    "if (-not $ok) { Write-Error muxel -ErrorAction Ignore }; ",
    "$text = & $global:__muxel_prompt; ",
    "try { ",
    "$read = $function:PSConsoleHostReadLine; ",
    "if ($read -and -not [object]::ReferenceEquals($read, $global:__muxel_readline)) { ",
    "Set-Item Function:global:PSConsoleHostReadLine (& { param($line) { ",
    "[System.Diagnostics.DebuggerHidden()] param() ",
    "$ok = $?; $global:__muxel_reading = $true; ",
    "if (-not $ok) { Write-Error muxel -ErrorAction Ignore }; ",
    "try { $typed = & $line } finally { $global:__muxel_reading = $false }; ",
    "[Console]::Write(\"$([char]27)]133;C$([char]27)\\\"); $typed ",
    "}.GetNewClosure() } $read); ",
    "$global:__muxel_readline = $function:PSConsoleHostReadLine }; ",
    "$mine = $null; ",
    "if ($read -and ",
    nothing_running!(),
    ") { ",
    "$null = Register-EngineEvent -SourceIdentifier PowerShell.OnIdle -MaxTriggerCount 1 -SupportEvent -Action { ",
    "$mine = $EventSubscriber.SubscriptionId; ",
    "try { if ($global:__muxel_reading -and ",
    nothing_running!(),
    ") { [Console]::Write(\"$([char]27)]133;B$([char]27)\\\") } } catch { } } } ",
    "} catch { }; ",
    "\"$e]133;A$e\\\" + $text + \"$e]133;B;",
    busy_option!(),
    "$e\\\" }",
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
