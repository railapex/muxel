//! Identity baked into the executable at compile time.

use serde_json::{Value, json};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const COMMIT: &str = env!("MUXEL_BUILD_COMMIT");
pub const BRANCH: &str = env!("MUXEL_BUILD_BRANCH");
pub const PROFILE: &str = env!("MUXEL_BUILD_PROFILE");
pub const SOURCE_POLICY_CLEAN: &str = env!("MUXEL_BUILD_SOURCE_POLICY_CLEAN");

const CHANNEL_ENV: &str = "MUXEL_DOGFOOD_CHANNEL";
const RELEASE_ENV: &str = "MUXEL_DOGFOOD_RELEASE_ID";

pub fn short_commit() -> &'static str {
    COMMIT.get(..COMMIT.len().min(10)).unwrap_or(COMMIT)
}

pub fn channel() -> Option<String> {
    if !is_launcher_binary() {
        return None;
    }
    std::env::var(CHANNEL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn release_id() -> Option<String> {
    if !is_launcher_binary() {
        return None;
    }
    std::env::var(RELEASE_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn is_launcher_binary() -> bool {
    let Some(expected) = std::env::var_os("MUXEL_DOGFOOD_EXE") else {
        return false;
    };
    let Ok(actual) = std::env::current_exe() else {
        return false;
    };
    let expected = std::path::PathBuf::from(expected);
    match (actual.canonicalize(), expected.canonicalize()) {
        (Ok(actual), Ok(expected)) => actual == expected,
        _ => actual == expected,
    }
}

fn source_policy_clean() -> bool {
    SOURCE_POLICY_CLEAN == "true"
}

pub fn compact_label() -> String {
    let identity = format!("v{VERSION} · {} · {PROFILE}", short_commit());
    match channel() {
        Some(channel) => format!("{channel} · {identity}"),
        None => identity,
    }
}

pub fn json_value() -> Value {
    json!({
        "version": VERSION,
        "commit": COMMIT,
        "shortCommit": short_commit(),
        "branch": BRANCH,
        "profile": PROFILE,
        "sourcePolicyClean": source_policy_clean(),
        "channel": channel(),
        "releaseId": release_id(),
    })
}

#[cfg(test)]
mod tests {
    use super::{COMMIT, compact_label, json_value, short_commit};

    #[test]
    fn compact_identity_contains_exact_embedded_build() {
        let label = compact_label();
        assert!(label.contains(short_commit()));
        assert!(label.contains(super::PROFILE));
        assert!(label.contains(super::VERSION));
    }

    #[test]
    fn machine_identity_keeps_the_full_commit() {
        let value = json_value();
        assert_eq!(value["commit"], COMMIT);
        assert_eq!(value["shortCommit"], short_commit());
    }
}
