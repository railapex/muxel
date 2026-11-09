//! The OSC 133 prompt marks muxel adds to the local PowerShell it starts.
//!
//! Windows has no foreground process group, and PowerShell runs cmdlets,
//! scripts, thread jobs and event actions in its own process, so a pane can only
//! tell its shell sits idle at a prompt if the shell says so. muxel wraps
//! PowerShell's prompt function so it prints `ESC ] 133 ; A` before the user's
//! own prompt and [`BUSY_MARK`] after it, and wraps its line editor so that
//! when PowerShell asks it for a line it prints a plain `ESC ] 133 ; B` if the
//! shell found nothing running inside itself and no input waiting, and
//! `ESC ] 133 ; C` as each line it returns starts to run. These are the marks
//! Windows Terminal uses for shell integration. What the user sees is
//! unchanged. cmd isn't marked: an echoing batch file prints cmd's `PROMPT`
//! while it runs, so a mark there can't tell a prompt from a running batch. An
//! unmarked cmd or PowerShell prints no marks, so its pane's close asks.

macro_rules! busy_option {
    () => {
        "muxel=busy"
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
/// go on working; a running command can call `prompt` itself. So the prompt
/// always ends with [`BUSY_MARK`], and only the line editor grants idle. After
/// the original prompt has run, the wrapper makes sure the line editor's
/// `PSConsoleHostReadLine` (PSReadLine's, or whatever replaced it since the
/// last prompt) is wrapped. PowerShell calls it straight after drawing a
/// prompt, to read the next line; nothing else normally does. As it starts, the
/// wrap prints a plain `ESC ] 133 ; B` if the prompt PowerShell just drew
/// found nothing running and nothing is waiting to be read (below); once it
/// returns a line, just before PowerShell runs it, it prints `ESC ] 133 ; C`,
/// as VS Code's shell integration does. The prompt leaves what it found in
/// `$global:__muxel_idle`, and the wrap clears it as it reads it, so a prompt
/// drawn by a redraw or a command can only speak for a read that follows a
/// prompt PowerShell draws, which finds things afresh. The wrapper checks for
/// a line editor at every prompt, not once at startup, so one imported or
/// replaced later is wrapped too. The wrapped function is held in a closure,
/// not a global, so a hook that later chains the wrap can't loop back into
/// itself; that hook only prints the marks twice. The wrap hands the line
/// editor the same `$?`, which PSReadLine reads as the last command's status,
/// and is hidden from the debugger like PSReadLine's own. Without a
/// `PSConsoleHostReadLine` there's nothing to wrap, nothing grants idle, and
/// the pane's close asks.
///
/// Input on its way when the wrap starts, sent before the shell read it,
/// still reads idle until the line it ends runs and prints `C`. That took a
/// few milliseconds in our runs, but a key handler that works before it
/// accepts the line holds it as long as it works, and a close made then
/// counts down. The pane counts every mark the shell prints, so an Undo expiry
/// with a mark printed since the close asks.
///
/// The prompt checks for work that outlives it inside PowerShell's own
/// process, where no child process shows it:
///
/// - `$NestedPromptLevel`: a debugger stop or `$host.EnterNestedPrompt()`;
/// - an unfinished job (`Get-Job`) of any type, including a module's own job
///   types, which can run without a runspace;
/// - any event subscriber (`Get-EventSubscriber -Force`);
/// - a runspace beyond the shell's own (`Get-Runspace`).
///
/// The wrap, as it starts, checks for input the line editor would read at
/// once:
///
/// - a key waiting in the console's input buffer (`[Console]::KeyAvailable`),
///   such as keys typed while the last command ran, a whole command sent with
///   Enter included;
/// - keys PSReadLine has already read and holds. It reads a paste ahead into
///   its own queue, past the prompt the line before ends with. No public API
///   shows that queue, so the wrap reads PSReadLine's private fields
///   (`_singleton._queuedKeys`, present in 2.0.0 on 5.1 and 2.4.5 on 7); if
///   they can't be read, it doesn't grant idle.
///
/// .NET's `KeyAvailable` reads and drops the records at the head of the
/// buffer that aren't keys the line editor would read (key-ups, lone modifier
/// keys, focus and resize events) and stops at the first that is, which
/// stays. Nothing the line editor reads is lost: on PowerShell 7 the filter is
/// `Console.ReadKey`'s own, and on 5.1 the one record `ReadKey` reads that
/// `KeyAvailable` drops, an Alt key-up carrying the character an Alt+numpad
/// sequence composed, can't head the buffer while the keys before it in that
/// sequence wait there. The host's `$Host.UI.RawUI.KeyAvailable` stays true at
/// every prompt after a Ctrl+C, though the line editor has read every key.
///
/// Unless all are clear, the wrap prints no `B` of its own and the prompt's
/// [`BUSY_MARK`] stands; a check that throws leaves it standing. So a module
/// that keeps an idle runspace or a standing event subscriber, as current
/// oh-my-posh streaming does with both, always reads busy and its panes ask.
/// Measured in a real console, the wrapper costs about 0.5 ms per prompt on
/// Windows PowerShell 5.1 and 0.2 ms on PowerShell 7, the wrap's checks (under
/// 0.1 ms) included.
const POWERSHELL_WRAP: &str = concat!(
    "$global:__muxel_prompt = $function:prompt; ",
    "function global:prompt { ",
    "$ok = $global:?; $global:__muxel_idle = $false; $e = [char]27; ",
    "if (-not $ok) { Write-Error muxel -ErrorAction Ignore }; ",
    "$text = & $global:__muxel_prompt; ",
    "try { ",
    "$read = $function:PSConsoleHostReadLine; ",
    "if ($read -and -not [object]::ReferenceEquals($read, $global:__muxel_readline)) { ",
    "Set-Item Function:global:PSConsoleHostReadLine (& { param($line) { ",
    "[System.Diagnostics.DebuggerHidden()] param() ",
    "$ok = $?; $idle = $global:__muxel_idle; $global:__muxel_idle = $false; ",
    "if ($idle) { try { ",
    "$held = 0; $psrl = 'Microsoft.PowerShell.PSConsoleReadLine' -as [type]; ",
    "if ($psrl) { $one = $psrl.GetField('_singleton', 'NonPublic, Static').GetValue($null); ",
    "$held = $one.GetType().GetField('_queuedKeys', 'NonPublic, Instance').GetValue($one).Count }; ",
    "$idle = $held -eq 0 -and -not [Console]::KeyAvailable ",
    "} catch { $idle = $false } }; ",
    "if ($idle) { [Console]::Write(\"$([char]27)]133;B$([char]27)\\\") }; ",
    "if (-not $ok) { Write-Error muxel -ErrorAction Ignore }; ",
    "$typed = & $line; [Console]::Write(\"$([char]27)]133;C$([char]27)\\\"); $typed ",
    "}.GetNewClosure() } $read); ",
    "$global:__muxel_readline = $function:PSConsoleHostReadLine }; ",
    "$global:__muxel_idle = $NestedPromptLevel -eq 0 -and ",
    "-not (Get-Job -ErrorAction Ignore | Where-Object { $_.State -notin 'Completed', 'Failed', 'Stopped' }) -and ",
    "-not (Get-EventSubscriber -Force -ErrorAction Ignore) -and ",
    "@(Get-Runspace -ErrorAction Ignore).Count -eq 1 } catch { }; ",
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
