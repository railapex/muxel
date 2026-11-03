//! Launch-folder chip decisions. Paths are lexical identities, never filesystem probes.

/// Normalize separators/trailing slashes, and Windows' case-insensitive spelling.
pub fn folder_key(path: &str, windows: bool) -> String {
    let path = if windows {
        path.replace('\\', "/")
    } else {
        path.to_string()
    };
    let trimmed = path.trim_end_matches('/');
    let path = if trimmed.is_empty() { "/" } else { trimmed };
    if windows {
        path.to_lowercase()
    } else {
        path.to_string()
    }
}

/// A root session needs only a branch; other folders remain identifiable without Git.
pub fn chip_label(root: &str, folder: &str, branch: Option<&str>, windows: bool) -> Option<String> {
    let different = folder_key(root, windows) != folder_key(folder, windows);
    let separator = |c| c == '/' || (windows && c == '\\');
    let short = folder
        .trim_end_matches(separator)
        .rsplit(separator)
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(folder);
    match (different, branch.filter(|b| !b.is_empty())) {
        (true, Some(branch)) => Some(format!("{short}  ⎇ {branch}")),
        (true, None) => Some(short.to_string()),
        (false, Some(branch)) => Some(format!("⎇ {branch}")),
        (false, None) => None,
    }
}

/// Project-scoped union of the clicked folder and its known branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderFilter {
    folder: String,
    branch: Option<String>,
}

impl FolderFilter {
    pub fn new(folder: &str, branch: Option<&str>, windows: bool) -> Self {
        Self {
            folder: folder_key(folder, windows),
            branch: branch.filter(|b| !b.is_empty()).map(str::to_owned),
        }
    }

    pub fn matches(&self, folder: &str, branch: Option<&str>, windows: bool) -> bool {
        self.folder == folder_key(folder, windows)
            || self.branch.as_deref().is_some_and(|b| Some(b) == branch)
    }

    pub fn is_source(&self, folder: &str, windows: bool) -> bool {
        self.folder == folder_key(folder, windows)
    }

    /// Clicking the same chip clears; another chip replaces the selection.
    pub fn toggle(current: &mut Option<Self>, clicked: Self) {
        *current = if current.as_ref().is_some_and(|f| f.folder == clicked.folder) {
            None
        } else {
            Some(clicked)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::{FolderFilter, chip_label, folder_key};

    #[test]
    fn root_and_external_folder_chip_decisions() {
        assert_eq!(chip_label("/repo", "/repo/", None, false), None);
        assert_eq!(
            chip_label("/repo", "/repo", Some("main"), false).as_deref(),
            Some("⎇ main")
        );
        assert_eq!(
            chip_label("/repo", "/env/ember/", None, false).as_deref(),
            Some("ember")
        );
        assert_eq!(
            chip_label("/repo", "/env/ember", Some("fix/dates"), false).as_deref(),
            Some("ember  ⎇ fix/dates")
        );
        assert_eq!(chip_label("C:/Repo", "c:\\repo\\", None, true), None);
        assert_eq!(chip_label("/", "/", None, false), None);
        assert_eq!(folder_key("C:/", true), "c:");
        assert_eq!(chip_label("/repo", "/", None, false).as_deref(), Some("/"));
        assert_eq!(
            chip_label("/repo", "/tmp/one\\two", None, false).as_deref(),
            Some("one\\two")
        );
    }

    #[test]
    fn filter_is_folder_or_known_branch_not_basename_or_unknown_branch() {
        let filter = FolderFilter::new("/one/ember", Some("topic"), false);
        let rows = [
            ("/one/ember", None),
            ("/two/glimmer", Some("topic")),
            ("/two/ember", Some("other")),
            ("/repo", None),
        ];
        assert_eq!(
            rows.map(|(p, b)| filter.matches(p, b, false)),
            [true, true, false, false]
        );
        let unknown = FolderFilter::new("/one/ember", None, false);
        assert!(!unknown.matches("/two/ember", None, false));
        assert!(unknown.matches("/one/ember/", Some("new"), false));
        assert!(FolderFilter::new("C:/Repo", None, true).matches("c:\\repo\\", None, true));
        assert!(!FolderFilter::new("/Repo", None, false).matches("/repo", None, false));
    }

    #[test]
    fn clicking_same_chip_clears_and_another_replaces() {
        let first = FolderFilter::new("/a", Some("one"), false);
        let second = FolderFilter::new("/b", Some("two"), false);
        let mut current = None;
        FolderFilter::toggle(&mut current, first.clone());
        assert_eq!(current, Some(first.clone()));
        FolderFilter::toggle(&mut current, first);
        assert_eq!(current, None);
        FolderFilter::toggle(&mut current, FolderFilter::new("/a", Some("old"), false));
        FolderFilter::toggle(&mut current, FolderFilter::new("/a", Some("new"), false));
        assert_eq!(
            current, None,
            "the source chip must still clear after checkout"
        );
        FolderFilter::toggle(&mut current, second.clone());
        let third = FolderFilter::new("/c", None, false);
        FolderFilter::toggle(&mut current, third.clone());
        assert_eq!(current, Some(third));
    }
}
