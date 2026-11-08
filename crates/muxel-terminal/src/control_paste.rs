//! Provider input encodings for outside-control paste operations.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlPaste {
    Terminal,
    WindowsCodexUnconfigured,
    WindowsCodexUnverified,
    WindowsCodex,
}

impl ControlPaste {
    pub(crate) fn for_launch(program: &str, args: &[String], windows: bool) -> Self {
        let program = program.trim().rsplit(['/', '\\']).next().unwrap_or(program);
        if !windows
            || !["codex", "codex.exe", "codex.cmd", "codex.bat"]
                .iter()
                .any(|name| program.eq_ignore_ascii_case(name))
        {
            return Self::Terminal;
        }
        let mut disabled = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "--" {
                break;
            }
            let setting = if matches!(arg.as_str(), "--config" | "-c") {
                args.next().map(String::as_str)
            } else {
                arg.strip_prefix("--config=")
                    .or_else(|| arg.strip_prefix("-c="))
            };
            if let Some((key, value)) = setting.and_then(|setting| setting.split_once('='))
                && key.trim() == "disable_paste_burst"
            {
                disabled = value.trim() == "true";
            }
        }
        if disabled {
            Self::WindowsCodexUnverified
        } else {
            Self::WindowsCodexUnconfigured
        }
    }

    pub(crate) fn with_version(self, output: &str) -> Self {
        if self != Self::WindowsCodexUnverified {
            return self;
        }
        let version = output
            .lines()
            .find_map(|line| line.trim().strip_prefix("codex-cli "));
        let supported = version
            .and_then(|version| {
                let mut parts = version.split('.');
                Some((
                    parts.next()?.parse::<u32>().ok()?,
                    parts.next()?.parse::<u32>().ok()?,
                ))
            })
            .is_some_and(|(major, minor)| major > 0 || minor >= 167);
        if supported { Self::WindowsCodex } else { self }
    }

    /// Explicit Win32 input records preserve Unicode code units and Shift+Enter
    /// through both older and newer ConPTY parsers. Ordinary text conversion
    /// can drop combining marks, and unknown CSI passthrough varies by version.
    pub(crate) fn encode(self, text: &str) -> Result<Option<String>, String> {
        if self == Self::Terminal {
            return Ok(None);
        }
        if let Some(ch) = text
            .chars()
            .find(|ch| ch.is_control() && !matches!(ch, '\r' | '\n'))
        {
            return Err(format!(
                "Windows Codex cannot insert control character U+{:04X} as prompt text; only line breaks are supported. Nothing was typed",
                ch as u32
            ));
        }
        if self == Self::WindowsCodexUnconfigured {
            return if text.contains(['\r', '\n']) || !text.is_ascii() {
                Err("Reliable multiline or Unicode input requires this Codex launch to use --config disable_paste_burst=true. Nothing was typed".into())
            } else {
                Ok(None)
            };
        }
        if self == Self::WindowsCodexUnverified && (text.contains(['\r', '\n']) || !text.is_ascii())
        {
            return Err("Multiline or Unicode input could not be verified for this Codex launch; Codex 0.167 or newer is required. Nothing was typed".into());
        }
        Ok(Some(Self::encode_records(text)))
    }

    /// Clipboard paste shares the configured decoder so disabling bursts does
    /// not drop its line breaks. Keep the existing ESC sanitization and other
    /// clipboard control handling; strict validation belongs to outside input.
    pub(crate) fn clipboard(self, text: &str) -> Option<String> {
        (self == Self::WindowsCodex).then(|| Self::encode_records(&text.replace('\x1b', "")))
    }

    fn encode_records(text: &str) -> String {
        use std::fmt::Write as _;
        let mut encoded = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    encoded.push_str("\x1b[13;0;13;1;16;1_\x1b[13;0;13;0;16;1_");
                }
                '\n' => encoded.push_str("\x1b[13;0;13;1;16;1_\x1b[13;0;13;0;16;1_"),
                ch => {
                    // Keep surrogate halves adjacent; the Windows reader pairs
                    // them before producing one character event.
                    for unit in ch.encode_utf16(&mut [0; 2]) {
                        let _ = write!(encoded, "\x1b[0;0;{unit};1;0;1_");
                    }
                    // Consecutive identical key-down records can coalesce into
                    // one repeat count that the provider does not expand.
                    // Surrogate pairs must remain adjacent and receive no
                    // releases: their parser produces a press for either kind.
                    if ch.len_utf16() == 1 {
                        let _ = write!(encoded, "\x1b[0;0;{};0;0;1_", ch as u32);
                    }
                }
            }
        }
        encoded
    }
}

#[cfg(test)]
mod tests {
    use super::ControlPaste;

    #[test]
    fn native_protocol_requires_a_known_enabled_launch() {
        let enabled = ["--config".into(), "disable_paste_burst=true".into()];
        assert_eq!(
            ControlPaste::for_launch("codex", &enabled, true),
            ControlPaste::WindowsCodexUnverified
        );
        for (program, windows) in [("codex", false), ("ssh", true), ("cmd.exe", true)] {
            assert_eq!(
                ControlPaste::for_launch(program, &enabled, windows),
                ControlPaste::Terminal
            );
        }
        assert_eq!(
            ControlPaste::for_launch("codex", &[], true),
            ControlPaste::WindowsCodexUnconfigured
        );
        let overridden = [
            "-c".into(),
            "disable_paste_burst=true".into(),
            "--config=disable_paste_burst=false".into(),
        ];
        assert_eq!(
            ControlPaste::for_launch("codex", &overridden, true),
            ControlPaste::WindowsCodexUnconfigured
        );
        let literal = ["--".into(), "--config=disable_paste_burst=true".into()];
        assert_eq!(
            ControlPaste::for_launch("codex", &literal, true),
            ControlPaste::WindowsCodexUnconfigured
        );
        assert!(
            ControlPaste::WindowsCodexUnconfigured
                .encode("one\ntwo")
                .is_err()
        );
        assert!(ControlPaste::WindowsCodexUnconfigured.encode("é").is_err());
    }

    #[test]
    fn windows_codex_preserves_line_breaks_and_unicode_input_records() {
        let encoded = ControlPaste::WindowsCodex
            .encode("AA\r\n\n🛰️")
            .unwrap()
            .unwrap();
        assert_eq!(
            encoded,
            concat!(
                "\x1b[0;0;65;1;0;1_\x1b[0;0;65;0;0;1_",
                "\x1b[0;0;65;1;0;1_\x1b[0;0;65;0;0;1_",
                "\x1b[13;0;13;1;16;1_\x1b[13;0;13;0;16;1_",
                "\x1b[13;0;13;1;16;1_\x1b[13;0;13;0;16;1_",
                "\x1b[0;0;55357;1;0;1_\x1b[0;0;57072;1;0;1_\x1b[0;0;65039;1;0;1_\x1b[0;0;65039;0;0;1_"
            )
        );
        assert_eq!(
            ControlPaste::Terminal.encode("unchanged\t\n").unwrap(),
            None
        );
        assert_eq!(
            ControlPaste::WindowsCodex.clipboard("AA\r\n\n🛰️"),
            Some(encoded)
        );
        assert_eq!(
            ControlPaste::WindowsCodex.encode("a\rb").unwrap(),
            ControlPaste::WindowsCodex.encode("a\nb").unwrap()
        );
        assert!(ControlPaste::Terminal.clipboard("first\nlast").is_none());
    }

    #[test]
    fn a_launch_flag_alone_cannot_enable_an_unknown_decoder() {
        let unverified = ControlPaste::WindowsCodexUnverified;
        for version in ["", "codex-cli 0.166.0", "unrecognised version"] {
            assert!(unverified.with_version(version).encode("one\ntwo").is_err());
            assert!(unverified.with_version(version).encode("é").is_err());
        }
        assert_eq!(
            unverified.with_version("codex-cli 0.167.0"),
            ControlPaste::WindowsCodex
        );
        assert!(unverified.encode("one line").is_ok());
    }

    #[test]
    fn editor_controls_are_refused_before_a_payload_is_returned() {
        for control in ['\0', '\t', '\u{8}', '\u{1b}', '\u{85}'] {
            assert!(
                ControlPaste::WindowsCodex
                    .encode(&format!("prefix{control}suffix"))
                    .is_err()
            );
        }
    }
}
