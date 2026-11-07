//! Parsing for identity queries that must finish before GUI initialization.

use std::ffi::OsStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityCommand {
    Version,
    Json,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityArgsError {
    ExtraArguments,
    UnsupportedForm,
}

impl std::fmt::Display for IdentityArgsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = match self {
            Self::ExtraArguments => "use exactly one of --version or --build-info-json",
            Self::UnsupportedForm => "identity flags do not take a value or a suffix",
        };
        write!(formatter, "invalid_build_identity_arguments: {reason}")
    }
}

impl std::error::Error for IdentityArgsError {}

/// Root flags are consumed before this is called. Unknown GUI arguments retain
/// their existing behavior; everything following ctl belongs to that command.
pub fn parse_identity_args(
    args: &[impl AsRef<OsStr>],
) -> Result<Option<IdentityCommand>, IdentityArgsError> {
    if args.first().is_some_and(|arg| arg.as_ref() == "ctl") {
        return Ok(None);
    }
    let has_identity = args.iter().any(|arg| {
        crate::startup_flags::names_flag(arg.as_ref(), "--version")
            || crate::startup_flags::names_flag(arg.as_ref(), "--build-info-json")
    });
    if !has_identity {
        return Ok(None);
    }
    if args.len() != 1 {
        return Err(IdentityArgsError::ExtraArguments);
    }
    match args[0].as_ref().as_encoded_bytes() {
        b"--version" => Ok(Some(IdentityCommand::Version)),
        b"--build-info-json" => Ok(Some(IdentityCommand::Json)),
        _ => Err(IdentityArgsError::UnsupportedForm),
    }
}

#[cfg(test)]
mod tests {
    use super::{IdentityArgsError, IdentityCommand, parse_identity_args};

    #[test]
    fn identity_requests_are_selected() {
        assert_eq!(
            parse_identity_args(&["--version"]),
            Ok(Some(IdentityCommand::Version))
        );
        assert_eq!(
            parse_identity_args(&["--build-info-json"]),
            Ok(Some(IdentityCommand::Json))
        );
    }

    #[test]
    fn conflicting_identity_flags_are_refused() {
        for args in [
            ["--version", "--build-info-json"],
            ["--build-info-json", "--version"],
        ] {
            assert_eq!(
                parse_identity_args(&args),
                Err(IdentityArgsError::ExtraArguments)
            );
        }
    }

    #[test]
    fn repeated_identity_flags_are_refused() {
        for flag in ["--version", "--build-info-json"] {
            assert_eq!(
                parse_identity_args(&[flag, flag]),
                Err(IdentityArgsError::ExtraArguments)
            );
        }
    }

    #[test]
    fn identity_flags_with_other_startup_arguments_are_refused() {
        assert_eq!(
            parse_identity_args(&["--version", "--settings"]),
            Err(IdentityArgsError::ExtraArguments)
        );
        assert_eq!(
            parse_identity_args(&["--settings", "--build-info-json"]),
            Err(IdentityArgsError::ExtraArguments)
        );
    }

    #[test]
    fn unconsumed_identity_forms_are_refused() {
        for flag in [
            "--version=",
            "--version=1",
            "--build-info-json=",
            "--build-info-json:json",
        ] {
            assert_eq!(
                parse_identity_args(&[flag]),
                Err(IdentityArgsError::UnsupportedForm)
            );
        }
    }

    #[test]
    fn case_and_dash_identity_variants_are_refused() {
        for flag in [
            "--VERSION",
            "-version",
            "---version",
            "--BUILD-INFO-JSON",
            "-build-info-json",
            "---build-info-json",
            "--Version=1",
        ] {
            assert_eq!(
                parse_identity_args(&[flag]),
                Err(IdentityArgsError::UnsupportedForm),
                "{flag}"
            );
            assert_eq!(
                parse_identity_args(&["ctl", "send", "pane", flag]),
                Ok(None)
            );
        }
    }

    #[test]
    fn ctl_keeps_ownership_of_identity_payload() {
        assert_eq!(
            parse_identity_args(&["ctl", "send", "pane", "--version"]),
            Ok(None)
        );
    }

    #[test]
    fn ordinary_gui_arguments_keep_their_existing_behavior() {
        assert_eq!(parse_identity_args(&["--settings"]), Ok(None));
        assert_eq!(parse_identity_args(&["versioned-project"]), Ok(None));
        assert_eq!(parse_identity_args(&[] as &[&str]), Ok(None));
    }
}
