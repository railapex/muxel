use std::path::{Path, PathBuf};
use std::process::Command;

struct BuildIdentity {
    commit: String,
    branch: String,
    profile: String,
    source_policy_clean: bool,
}

fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn git_success(root: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .is_ok_and(|status| status.success())
}

fn build_affecting(path: &str) -> bool {
    let path = path.replace('\\', "/");
    path.starts_with(".cargo/")
        || path.starts_with("crates/")
        || path == "Cargo.toml"
        || path == "Cargo.lock"
        || path.ends_with("/Cargo.toml")
        || path.ends_with("/build.rs")
}

fn source_policy_clean(root: &Path) -> bool {
    let tracked = git_output(
        root,
        &[
            "diff",
            "--name-only",
            "HEAD",
            "--",
            ".cargo",
            "crates",
            "Cargo.toml",
            "Cargo.lock",
        ],
    );
    let untracked = git_output(root, &["ls-files", "--others", "--exclude-standard"]);
    tracked.is_none()
        && untracked
            .as_deref()
            .map(|paths| paths.lines().all(|path| !build_affecting(path)))
            .unwrap_or(true)
        && git_success(
            root,
            &[
                "diff",
                "--quiet",
                "HEAD",
                "--",
                ".cargo",
                "crates",
                "Cargo.toml",
                "Cargo.lock",
            ],
        )
}

fn git_path(root: &Path, name: &str) -> Option<PathBuf> {
    let raw = git_output(root, &["rev-parse", "--git-path", name])?;
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    path.exists().then_some(path)
}

fn emit_build_identity() -> BuildIdentity {
    println!("cargo:rerun-if-env-changed=MUXEL_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=MUXEL_BUILD_BRANCH");

    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let root = manifest.join("../..");
    let commit = std::env::var("MUXEL_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| git_output(&root, &["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    let branch = std::env::var("MUXEL_BUILD_BRANCH")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| git_output(&root, &["branch", "--show-current"]))
        .unwrap_or_else(|| "unknown".to_string());
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());
    let source_policy_clean = source_policy_clean(&root);

    println!("cargo:rustc-env=MUXEL_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=MUXEL_BUILD_BRANCH={branch}");
    println!("cargo:rustc-env=MUXEL_BUILD_PROFILE={profile}");
    println!("cargo:rustc-env=MUXEL_BUILD_SOURCE_POLICY_CLEAN={source_policy_clean}");

    // Recompute cleanliness whenever build-affecting source appears, changes,
    // or disappears. These directories exclude Cargo output.
    for path in [".cargo", "crates", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }

    // The per-worktree HEAD reflog moves on commit, checkout, merge, reset, and
    // rebase even when the branch ref is packed. Resolve every Git path against
    // the repository root because main checkouts return relative `.git/...` paths.
    for name in ["HEAD", "logs/HEAD", "packed-refs"] {
        if let Some(path) = git_path(&root, name) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    BuildIdentity {
        commit,
        branch,
        profile,
        source_policy_clean,
    }
}

fn main() {
    let identity = emit_build_identity();

    // Bump the Windows main-thread stack to 8 MiB. The MSVC linker defaults the
    // executable's stack reserve to 1 MiB, but GPUI's layout/paint recurse deep
    // enough — especially when rendering a full-screen agent TUI (e.g. Claude) —
    // to overflow that, crashing with a "stack overflow". Linux/macOS default the
    // main thread to 8 MiB, so this only bites on Windows. gpui-component and Zed
    // do the same: gpui-component sets `/STACK:8000000` in its own
    // `.cargo/config.toml`, but that does NOT propagate to downstream crates, so
    // muxel must set it on its own binary. `/STACK` is MSVC-linker syntax, so gate
    // on the windows-msvc target (these CARGO_CFG_* vars describe the target).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg=/STACK:8388608");
    }

    // Embed the app icon into the Windows .exe. Only runs when building on
    // Windows (CI builds the Windows targets on a windows runner); a no-op on
    // Linux/macOS, so it doesn't affect the .deb/.rpm/AppImage/.app builds.
    #[cfg(windows)]
    {
        println!("cargo:rerun-if-changed=assets/muxel.ico");
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/muxel.ico");
        res.set("Comments", "muxel-build-identity-v1");
        res.set("PrivateBuild", &identity.commit);
        res.set(
            "SpecialBuild",
            &format!(
                "branch={};profile={};sourcePolicyClean={}",
                identity.branch, identity.profile, identity.source_policy_clean
            ),
        );
        if let Err(e) = res.compile() {
            println!("cargo:warning=winresource (exe metadata) failed: {e}");
        }
    }
}
