//! Starting Git for Windows' git without its launcher.
//!
//! A Git for Windows install puts `<top>\cmd` on `PATH`, and the `git.exe` there is not git. It is a
//! launcher (`git-wrapper.c` in git-for-windows/MINGW-packages) that sets a few environment variables,
//! starts `<top>\<msystem>\bin\git.exe` and waits for it. Each git call made through it is three
//! processes (the launcher, the console host and git) where git itself is two. [`resolve`] finds the
//! git the launcher would start and the environment it would start it with, so a caller can start that
//! git directly. Any other layout is left to `PATH`, as before.
//!
//! The launcher's `setup_environment()` is all it changes for git:
//! - `MSYSTEM` is always set to the launcher build's value (`MINGW64`, `UCRT64`, `CLANGARM64`);
//! - `PLINK_PROTOCOL=ssh` when unset;
//! - `HOME`, when unset, is `%HOMEDRIVE%%HOMEPATH%` if that is a directory other than System32, else
//!   `%USERPROFILE%`;
//! - `PATH` becomes `<top>\<msystem>\bin;<top>\usr\bin;<HOME>\bin;` followed by `PATH`;
//! - `MSYS` is prefixed with each `MSYS=` line of `<top>\etc\git-bash.config`.
//!
//! git.exe does most of this itself when `MSYSTEM` is unset (`setup_windows_environment()` in
//! compat/mingw.c), but it puts `<HOME>\bin` first, and it leaves an inherited `MSYSTEM` and `PATH`
//! alone. Setting the launcher's values keeps hooks, textconv drivers and credential helpers on the
//! same `PATH` they get through the launcher.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Git for Windows' `MSYSTEM`s. Each keeps its git at `<top>\<msystem, lowercased>\bin\git.exe`.
const MSYSTEMS: [&str; 4] = ["MINGW64", "UCRT64", "CLANGARM64", "MINGW32"];

/// What [`resolve`] reads from the machine.
pub trait Host {
    fn var(&self, key: &str) -> Option<OsString>;
    fn is_file(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
    fn read_to_string(&self, path: &Path) -> Option<String>;
    /// Windows' system directory (`GetSystemDirectoryW`), where the launcher won't put `HOME`.
    fn system_dir(&self) -> Option<PathBuf>;
}

/// The git to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitProgram {
    /// Plain `git`, found when it starts, as before.
    Path,
    /// Git for Windows' git, started with the variables its launcher would have set.
    Direct {
        exe: PathBuf,
        env: Vec<(OsString, OsString)>,
    },
}

/// What a plain `git` amounts to when `std::process::Command` would search `dirs`, in that order,
/// for `git.exe`.
pub fn resolve(dirs: impl IntoIterator<Item = PathBuf>, host: &impl Host) -> GitProgram {
    let found = dirs
        .into_iter()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("git.exe"))
        .find(|candidate| host.is_file(candidate));
    match found
        .as_deref()
        .and_then(|found| launcher_target(found, host))
    {
        Some((top, msystem, exe)) => GitProgram::Direct {
            env: launcher_env(&top, msystem, host),
            exe,
        },
        None => GitProgram::Path,
    }
}

/// When `found` is the launcher (`<top>\cmd\git.exe`, or its copy `<top>\bin\git.exe`) in an MSYS2-based
/// Git for Windows tree: the tree, its `MSYSTEM` and the git the launcher starts.
fn launcher_target(found: &Path, host: &impl Host) -> Option<(PathBuf, &'static str, PathBuf)> {
    let dir = found.parent()?;
    let name = dir.file_name()?;
    if !(name.eq_ignore_ascii_case("cmd") || name.eq_ignore_ascii_case("bin")) {
        return None;
    }
    let top = dir.parent()?;
    if !host.is_dir(&top.join("usr").join("bin")) {
        return None;
    }
    // A real install has its git and its `libexec\git-core` side by side. Since v2.56 the old
    // `mingw64\bin` may hold only delegating launchers, so a lone `bin\git.exe` doesn't count.
    let mut installs = MSYSTEMS.iter().filter_map(|&msystem| {
        let prefix = top.join(msystem.to_ascii_lowercase());
        let exe = prefix.join("bin").join("git.exe");
        (host.is_file(&exe) && host.is_dir(&prefix.join("libexec").join("git-core")))
            .then_some((msystem, exe))
    });
    let (msystem, exe) = installs.next()?;
    // Two installs side by side (an interrupted upgrade): only the launcher knows which it starts.
    if installs.next().is_some() {
        return None;
    }
    Some((top.to_path_buf(), msystem, exe))
}

/// The variables the launcher sets before starting git, in the order it sets them.
fn launcher_env(top: &Path, msystem: &str, host: &impl Host) -> Vec<(OsString, OsString)> {
    let mut env = vec![("MSYSTEM".into(), msystem.into())];
    if host.var("PLINK_PROTOCOL").is_none() {
        env.push(("PLINK_PROTOCOL".into(), "ssh".into()));
    }
    let home = match host.var("HOME") {
        Some(home) => Some(home),
        None => {
            let home = launcher_home(host);
            if let Some(home) = &home {
                env.push(("HOME".into(), home.clone()));
            }
            home
        }
    };

    let mut path = OsString::new();
    for dir in [
        top.join(msystem.to_ascii_lowercase()).join("bin"),
        top.join("usr").join("bin"),
    ] {
        path.push(dir);
        path.push(";");
    }
    // `<HOME>\bin`, even when there is no HOME at all (the launcher then adds `\bin`).
    let home = home.unwrap_or_default();
    path.push(&home);
    if !home.as_encoded_bytes().ends_with(b"\\") {
        path.push("\\");
    }
    path.push("bin;");
    if let Some(inherited) = host.var("PATH") {
        path.push(inherited);
    }
    env.push(("PATH".into(), path));

    if let Some(config) = host.read_to_string(&top.join("etc").join("git-bash.config"))
        && let Some(msys) = configured_msys(&config, host.var("MSYS"))
    {
        env.push(("MSYS".into(), msys));
    }
    env
}

/// `HOME` as the launcher picks it when none is set.
fn launcher_home(host: &impl Host) -> Option<OsString> {
    if let Some(home_path) = host.var("HOMEPATH") {
        let mut home = host.var("HOMEDRIVE").unwrap_or_default();
        home.push(home_path);
        let system32 = host
            .system_dir()
            .is_some_and(|dir| dir.as_os_str().eq_ignore_ascii_case(&home));
        if !system32 && host.is_dir(Path::new(&home)) {
            return Some(home);
        }
    }
    host.var("USERPROFILE")
}

/// `MSYS` after the launcher reads `git-bash.config`: each `MSYS=` line goes in front of what is there,
/// space-separated. `None` when the file sets nothing.
fn configured_msys(config: &str, inherited: Option<OsString>) -> Option<OsString> {
    let mut msys = None;
    let mut current = inherited;
    for value in config.lines().filter_map(|line| line.strip_prefix("MSYS=")) {
        let mut next = OsString::from(value);
        if let Some(old) = current {
            next.push(" ");
            next.push(old);
        }
        current = Some(next.clone());
        msys = Some(next);
    }
    msys
}

#[cfg(test)]
mod tests {
    use super::{GitProgram, Host, resolve};
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    /// A machine made of the files, directories and variables a test lists.
    #[derive(Default)]
    struct Machine {
        files: HashSet<PathBuf>,
        dirs: HashSet<PathBuf>,
        vars: HashMap<&'static str, &'static str>,
        texts: HashMap<PathBuf, &'static str>,
    }

    impl Machine {
        fn file(mut self, path: &str) -> Self {
            self.files.insert(path.into());
            self
        }
        fn dir(mut self, path: &str) -> Self {
            self.dirs.insert(path.into());
            self
        }
        fn var(mut self, key: &'static str, value: &'static str) -> Self {
            self.vars.insert(key, value);
            self
        }
        fn text(mut self, path: &str, text: &'static str) -> Self {
            self.texts.insert(path.into(), text);
            self
        }
        /// A Git for Windows tree at `top` whose real git lives under `msystem`.
        fn git_for_windows(self, top: &str, msystem: &str) -> Self {
            self.file(&format!(r"{top}\cmd\git.exe"))
                .file(&format!(r"{top}\bin\git.exe"))
                .dir(&format!(r"{top}\usr\bin"))
                .file(&format!(r"{top}\{msystem}\bin\git.exe"))
                .dir(&format!(r"{top}\{msystem}\libexec\git-core"))
        }
        /// A user with a profile directory and the variables Windows sets for one.
        fn user(self) -> Self {
            self.dir(r"C:\Users\u")
                .var("HOMEDRIVE", "C:")
                .var("HOMEPATH", r"\Users\u")
                .var("USERPROFILE", r"C:\Users\u")
        }
        fn resolve(&self, dirs: &[&str]) -> GitProgram {
            resolve(dirs.iter().map(PathBuf::from), self)
        }
    }

    impl Host for Machine {
        fn var(&self, key: &str) -> Option<OsString> {
            self.vars.get(key).map(OsString::from)
        }
        fn is_file(&self, path: &Path) -> bool {
            self.files.contains(path)
        }
        fn is_dir(&self, path: &Path) -> bool {
            self.dirs.contains(path)
        }
        fn read_to_string(&self, path: &Path) -> Option<String> {
            self.texts.get(path).map(|text| text.to_string())
        }
        fn system_dir(&self) -> Option<PathBuf> {
            Some(r"C:\WINDOWS\system32".into())
        }
    }

    fn direct(exe: &str, env: &[(&str, &str)]) -> GitProgram {
        GitProgram::Direct {
            exe: exe.into(),
            env: env.iter().map(|&(k, v)| (k.into(), v.into())).collect(),
        }
    }

    // Expected environments come from git-wrapper.c's setup_environment(), not from this module.
    #[test]
    fn the_launcher_resolves_to_the_git_it_starts_with_its_environment() {
        let path = r"C:\Program Files\Git\cmd;C:\WINDOWS";
        let mingw64 = Machine::default()
            .git_for_windows(r"C:\Program Files\Git", "mingw64")
            .user()
            .var("PATH", path);
        let expected = direct(
            r"C:\Program Files\Git\mingw64\bin\git.exe",
            &[
                ("MSYSTEM", "MINGW64"),
                ("PLINK_PROTOCOL", "ssh"),
                ("HOME", r"C:\Users\u"),
                (
                    "PATH",
                    r"C:\Program Files\Git\mingw64\bin;C:\Program Files\Git\usr\bin;C:\Users\u\bin;C:\Program Files\Git\cmd;C:\WINDOWS",
                ),
            ],
        );
        assert_eq!(
            mingw64.resolve(&[r"C:\Program Files\Git\cmd", r"C:\WINDOWS"]),
            expected
        );
        // `bin\git.exe` is the same launcher.
        assert_eq!(mingw64.resolve(&[r"C:\Program Files\Git\bin"]), expected);

        // v2.56 moved git to UCRT64.
        let ucrt64 = Machine::default()
            .git_for_windows(r"D:\PortableGit", "ucrt64")
            .user()
            .var("PATH", r"D:\PortableGit\cmd");
        assert_eq!(
            ucrt64.resolve(&[r"D:\PortableGit\cmd"]),
            direct(
                r"D:\PortableGit\ucrt64\bin\git.exe",
                &[
                    ("MSYSTEM", "UCRT64"),
                    ("PLINK_PROTOCOL", "ssh"),
                    ("HOME", r"C:\Users\u"),
                    (
                        "PATH",
                        r"D:\PortableGit\ucrt64\bin;D:\PortableGit\usr\bin;C:\Users\u\bin;D:\PortableGit\cmd",
                    ),
                ],
            )
        );
    }

    #[test]
    fn the_environment_follows_the_launchers_rules() {
        let git = r"C:\Git\mingw64\bin\git.exe";
        let base = || Machine::default().git_for_windows(r"C:\Git", "mingw64");
        let cases: Vec<(&str, Machine, GitProgram)> = vec![
            (
                "an inherited HOME, PLINK_PROTOCOL and MSYSTEM: HOME and PLINK stay, MSYSTEM is replaced",
                base()
                    .user()
                    .var("HOME", r"D:\home")
                    .var("PLINK_PROTOCOL", "plink")
                    .var("MSYSTEM", "UCRT64")
                    .var("PATH", r"C:\Git\cmd"),
                direct(
                    git,
                    &[
                        ("MSYSTEM", "MINGW64"),
                        (
                            "PATH",
                            r"C:\Git\mingw64\bin;C:\Git\usr\bin;D:\home\bin;C:\Git\cmd",
                        ),
                    ],
                ),
            ),
            (
                "HOMEDRIVE+HOMEPATH not a directory: USERPROFILE",
                base()
                    .dir(r"C:\Users\u")
                    .var("HOMEDRIVE", "H:")
                    .var("HOMEPATH", r"\")
                    .var("USERPROFILE", r"C:\Users\u"),
                direct(
                    git,
                    &[
                        ("MSYSTEM", "MINGW64"),
                        ("PLINK_PROTOCOL", "ssh"),
                        ("HOME", r"C:\Users\u"),
                        ("PATH", r"C:\Git\mingw64\bin;C:\Git\usr\bin;C:\Users\u\bin;"),
                    ],
                ),
            ),
            (
                "HOMEDRIVE+HOMEPATH is System32: USERPROFILE",
                base()
                    .dir(r"C:\Windows\System32")
                    .var("HOMEDRIVE", "C:")
                    .var("HOMEPATH", r"\Windows\System32")
                    .var("USERPROFILE", r"C:\Users\svc")
                    .var("PATH", r"C:\Git\cmd"),
                direct(
                    git,
                    &[
                        ("MSYSTEM", "MINGW64"),
                        ("PLINK_PROTOCOL", "ssh"),
                        ("HOME", r"C:\Users\svc"),
                        (
                            "PATH",
                            r"C:\Git\mingw64\bin;C:\Git\usr\bin;C:\Users\svc\bin;C:\Git\cmd",
                        ),
                    ],
                ),
            ),
            (
                "a HOME ending in a backslash",
                base().var("HOME", r"E:\").var("PATH", r"C:\Git\cmd"),
                direct(
                    git,
                    &[
                        ("MSYSTEM", "MINGW64"),
                        ("PLINK_PROTOCOL", "ssh"),
                        (
                            "PATH",
                            r"C:\Git\mingw64\bin;C:\Git\usr\bin;E:\bin;C:\Git\cmd",
                        ),
                    ],
                ),
            ),
            (
                "no HOME, HOMEPATH or USERPROFILE",
                base().var("PATH", r"C:\Git\cmd"),
                direct(
                    git,
                    &[
                        ("MSYSTEM", "MINGW64"),
                        ("PLINK_PROTOCOL", "ssh"),
                        ("PATH", r"C:\Git\mingw64\bin;C:\Git\usr\bin;\bin;C:\Git\cmd"),
                    ],
                ),
            ),
            (
                "git-bash.config: each MSYS= line goes in front",
                base().var("HOME", r"C:\h").var("MSYS", "noglob").text(
                    r"C:\Git\etc\git-bash.config",
                    "MSYS=winsymlinks:nativestrict\r\nOTHER=1\nMSYS=enable_pcon\n",
                ),
                direct(
                    git,
                    &[
                        ("MSYSTEM", "MINGW64"),
                        ("PLINK_PROTOCOL", "ssh"),
                        ("PATH", r"C:\Git\mingw64\bin;C:\Git\usr\bin;C:\h\bin;"),
                        ("MSYS", "enable_pcon winsymlinks:nativestrict noglob"),
                    ],
                ),
            ),
        ];
        for (case, machine, expected) in cases {
            assert_eq!(machine.resolve(&[r"C:\Git\cmd"]), expected, "{case}");
        }
    }

    #[test]
    fn any_other_git_is_left_to_path() {
        let gfw = || {
            Machine::default()
                .git_for_windows(r"C:\Git", "mingw64")
                .user()
        };
        let cases: Vec<(&str, Machine, &[&str])> = vec![
            (
                "scoop's shim",
                gfw().file(r"C:\Users\u\scoop\shims\git.exe"),
                &[r"C:\Users\u\scoop\shims", r"C:\Git\cmd"],
            ),
            (
                "the real git first on PATH",
                gfw(),
                &[r"C:\Git\mingw64\bin", r"C:\Git\cmd"],
            ),
            (
                "MSYS2's own git",
                Machine::default()
                    .file(r"C:\msys64\usr\bin\git.exe")
                    .dir(r"C:\msys64\usr\bin"),
                &[r"C:\msys64\usr\bin"],
            ),
            (
                "a git beside muxel.exe, found before PATH",
                gfw().file(r"C:\Apps\muxel\git.exe"),
                &[r"C:\Apps\muxel", r"C:\Git\cmd"],
            ),
            (
                "a cmd\\git.exe outside an MSYS2 tree",
                Machine::default()
                    .file(r"C:\Tools\cmd\git.exe")
                    .file(r"C:\Tools\mingw64\bin\git.exe")
                    .dir(r"C:\Tools\mingw64\libexec\git-core"),
                &[r"C:\Tools\cmd"],
            ),
            (
                "mingw64 holds only a delegating launcher (v2.56)",
                Machine::default()
                    .file(r"C:\Git\cmd\git.exe")
                    .dir(r"C:\Git\usr\bin")
                    .file(r"C:\Git\mingw64\bin\git.exe"),
                &[r"C:\Git\cmd"],
            ),
            (
                "two installs side by side",
                gfw()
                    .file(r"C:\Git\ucrt64\bin\git.exe")
                    .dir(r"C:\Git\ucrt64\libexec\git-core"),
                &[r"C:\Git\cmd"],
            ),
            (
                "no git anywhere",
                Machine::default().user(),
                &[r"C:\WINDOWS"],
            ),
        ];
        for (case, machine, dirs) in cases {
            assert_eq!(machine.resolve(dirs), GitProgram::Path, "{case}");
        }
    }
}
