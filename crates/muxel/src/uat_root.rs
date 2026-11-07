//! `muxel --uat-root DIR`: `next`'s private seam for scripted native UAT
//! (`b0368b4`), kept as an alias over the public `--user-data-dir`.
//!
//! `--uat-root DIR` and `--uat-root=DIR` take an absolute path. They keep config
//! and data under
//! `DIR/config` and `DIR/data`, as `--user-data-dir DIR` does, and marks the run
//! with [`UAT_ROOT_ENV`] so the app enters the seeded fixture's current workspace
//! instead of waiting at the selector after first-run terms. An
//! inherited [`UAT_ROOT_ENV`] still isolates on its own, as it did before
//! `--user-data-dir` existed, so nothing started under a UAT root falls back to
//! the real profile.

use std::ffi::OsString;
use std::path::PathBuf;

/// Set for a UAT run and inherited by everything it starts.
pub const UAT_ROOT_ENV: &str = "MUXEL_UAT_ROOT";
const FLAG: &str = "--uat-root";
const MISSING: &str = "invalid_uat_root: --uat-root requires an absolute path";

/// Turn `--uat-root DIR` into `--user-data-dir DIR` plus [`UAT_ROOT_ENV`], or map
/// an inherited [`UAT_ROOT_ENV`] onto the user data dir. Runs first in `main`,
/// before [`crate::user_data_dir::configure`], while the process is still
/// single-threaded.
pub fn configure(args: Vec<OsString>) -> Result<Vec<OsString>, String> {
    let (flag, rest) = take_flag(args)?;
    if let Some(root) = flag {
        // SAFETY: called first in `main`, before any thread is spawned.
        unsafe { std::env::set_var(UAT_ROOT_ENV, &root) };
        let mut args = vec![OsString::from("--user-data-dir"), root.into_os_string()];
        args.extend(rest);
        return Ok(args);
    }
    let inherited_dir = std::env::var_os(muxel_store::USER_DATA_DIR_ENV);
    if let Some(root) = inherited_root(std::env::var_os(UAT_ROOT_ENV), inherited_dir) {
        // SAFETY: called first in `main`, before any thread is spawned.
        unsafe { std::env::set_var(muxel_store::USER_DATA_DIR_ENV, root) };
    }
    Ok(rest)
}

/// Whether this run was started under a UAT root.
pub fn active() -> bool {
    std::env::var_os(UAT_ROOT_ENV).is_some_and(|root| !root.is_empty())
}

/// Split either `--uat-root DIR` or `--uat-root=DIR` out before any ctl subcommand.
fn take_flag(args: Vec<OsString>) -> Result<(Option<PathBuf>, Vec<OsString>), String> {
    let mut root = None;
    let mut rest = Vec::with_capacity(args.len());
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "ctl" {
            rest.push(arg);
            rest.extend(args);
            break;
        }
        let value = if arg == FLAG {
            args.next().ok_or(MISSING)?
        } else if arg.as_encoded_bytes().starts_with(b"--uat-root=") {
            let text = arg.to_str().ok_or(
                "invalid_uat_root: --uat-root=DIR needs a Unicode path; use --uat-root DIR",
            )?;
            OsString::from(&text[FLAG.len() + 1..])
        } else if muxel_core::startup_flags::names_flag(arg.as_os_str(), FLAG) {
            return Err("invalid_uat_root: unsupported --uat-root form".into());
        } else {
            rest.push(arg);
            continue;
        };
        let value = PathBuf::from(value);
        if !value.is_absolute() {
            return Err(MISSING.into());
        }
        if root.is_some() {
            return Err(format!("invalid_uat_root: {FLAG} was given more than once"));
        }
        root = Some(value);
    }
    Ok((root, rest))
}

/// The inherited UAT root to use as the user data dir, unless a user data dir is
/// already inherited.
fn inherited_root(uat_root: Option<OsString>, user_data_dir: Option<OsString>) -> Option<OsString> {
    if user_data_dir.is_some_and(|dir| !dir.is_empty()) {
        return None;
    }
    uat_root.filter(|root| !root.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{inherited_root, take_flag};
    use std::ffi::OsString;

    #[test]
    fn takes_an_absolute_uat_root_anywhere_before_ctl() {
        let root = std::env::temp_dir().join("muxel-uat-test");
        let args = vec![
            OsString::from("--other"),
            "--uat-root".into(),
            root.clone().into_os_string(),
            "ctl".into(),
            "--uat-root".into(),
        ];
        assert_eq!(
            take_flag(args),
            Ok((
                Some(root),
                vec!["--other".into(), "ctl".into(), "--uat-root".into()]
            ))
        );
    }

    #[test]
    fn refuses_a_missing_relative_or_repeated_uat_root() {
        let root = std::env::temp_dir().join("muxel-uat-test").into_os_string();
        assert!(take_flag(vec!["--uat-root".into()]).is_err());
        assert!(take_flag(vec!["--uat-root".into(), "relative".into()]).is_err());
        assert!(
            take_flag(vec![
                "--uat-root".into(),
                root.clone(),
                "--uat-root".into(),
                root,
            ])
            .is_err()
        );
    }

    #[test]
    fn accepts_an_absolute_equals_form_uat_root() {
        let root = std::env::temp_dir().join("muxel-uat-test");
        let mut flag = OsString::from("--uat-root=");
        flag.push(&root);
        assert_eq!(take_flag(vec![flag]), Ok((Some(root), vec![])));
    }

    #[test]
    fn refuses_unconsumed_uat_root_forms() {
        for flag in [
            "--uat-root=",
            "--uat-root=relative",
            "--uat-root:root",
            "--uat-rooted=root",
        ] {
            assert!(take_flag(vec![flag.into()]).is_err(), "{flag}");
        }
    }

    #[test]
    fn refuses_case_and_dash_uat_root_variants() {
        for flag in ["--UAT-ROOT", "-uat-root", "---uat-root", "--Uat-Root=dir"] {
            assert!(
                take_flag(vec![flag.into(), std::env::temp_dir().into_os_string()]).is_err(),
                "{flag}"
            );
            let payload = vec!["ctl".into(), "send".into(), "pane".into(), flag.into()];
            assert_eq!(take_flag(payload.clone()), Ok((None, payload)));
        }
        assert_eq!(
            take_flag(vec!["uat-root".into()]),
            Ok((None, vec!["uat-root".into()]))
        );
    }

    #[test]
    fn refuses_identity_flags_as_uat_root_values() {
        for flag in ["--version", "--build-info-json"] {
            assert!(take_flag(vec!["--uat-root".into(), flag.into()]).is_err());
        }
    }

    #[test]
    fn no_uat_root_preserves_normal_startup() {
        assert_eq!(
            take_flag(vec!["--other".into()]),
            Ok((None, vec!["--other".into()]))
        );
        assert_eq!(inherited_root(None, None), None);
        assert_eq!(inherited_root(Some("".into()), None), None);
    }

    #[test]
    fn an_inherited_uat_root_isolates_unless_a_user_data_dir_is_inherited() {
        let root = OsString::from("D:/tmp/muxel-uat/test");
        assert_eq!(inherited_root(Some(root.clone()), None), Some(root.clone()));
        assert_eq!(
            inherited_root(Some(root.clone()), Some("".into())),
            Some(root.clone())
        );
        assert_eq!(
            inherited_root(Some(root), Some("D:/elsewhere".into())),
            None
        );
    }
}
