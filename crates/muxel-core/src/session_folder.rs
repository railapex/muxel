//! Launch-folder chip decisions and the sidebar's filter box. Paths are lexical
//! identities, never filesystem probes.

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

/// Root rows only repeat a branch when it differs from the project header.
/// Other folders remain identifiable without Git.
pub fn chip_label(
    root: &str,
    folder: &str,
    branch: Option<&str>,
    project_branch: Option<&str>,
    windows: bool,
) -> Option<String> {
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
        (false, Some(branch)) if Some(branch) != project_branch => Some(format!("⎇ {branch}")),
        (false, _) => None,
    }
}

/// One sidebar row as the filter box sees it.
#[derive(Clone, Copy, Debug)]
pub struct FilterRow<'a> {
    /// Its project's name.
    pub project: &'a str,
    /// The name the row shows.
    pub name: &'a str,
    /// Its launch folder.
    pub folder: &'a str,
    /// That folder's known branch.
    pub branch: Option<&'a str>,
    /// Whether the folder is a Windows path, matched without case.
    pub windows: bool,
}

/// The sidebar's filter box, read. Each word must appear, ignoring case, in
/// the row's name, folder path or branch. `project:`, `folder:` and `branch:`
/// are what a row's menu writes for "this folder and branch": a row then needs
/// that project, and that folder as a full path (never a basename) or that
/// known branch. Values with spaces are quoted. Several of one kind are
/// alternatives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowFilter {
    words: Vec<String>,
    projects: Vec<String>,
    folders: Vec<String>,
    branches: Vec<String>,
}

impl RowFilter {
    pub fn parse(query: &str) -> Self {
        let mut filter = Self::default();
        for term in terms(query) {
            let (list, value) = if let Some(project) = term.strip_prefix("project:") {
                (&mut filter.projects, project.to_lowercase())
            } else if let Some(folder) = term.strip_prefix("folder:") {
                (&mut filter.folders, folder.to_string())
            } else if let Some(branch) = term.strip_prefix("branch:") {
                (&mut filter.branches, branch.to_string())
            } else {
                (&mut filter.words, term.to_lowercase())
            };
            // A bare `folder:` is one being typed, so it filters nothing yet.
            if !value.is_empty() {
                list.push(value);
            }
        }
        filter
    }

    /// What a row's menu puts in the box for its `folder` and that folder's
    /// known `branch`, in `project`.
    pub fn for_folder(project: &str, folder: &str, branch: Option<&str>) -> String {
        let mut text = format!("project:{} folder:{}", quoted(project), quoted(folder));
        if let Some(branch) = branch.filter(|b| !b.is_empty()) {
            text.push_str(" branch:");
            text.push_str(&quoted(branch));
        }
        text
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
            && self.projects.is_empty()
            && self.folders.is_empty()
            && self.branches.is_empty()
    }

    /// Whether the box names `project`, as a row's menu writes it.
    pub fn names_project(&self, project: &str) -> bool {
        let project = project.to_lowercase();
        self.projects.contains(&project)
    }

    /// Whether the box names `folder` in `project`, so the row there offers to
    /// clear it, though its branch may have moved since.
    pub fn names_folder(&self, project: &str, folder: &str, windows: bool) -> bool {
        let key = folder_key(folder, windows);
        (self.projects.is_empty() || self.names_project(project))
            && self.folders.iter().any(|f| folder_key(f, windows) == key)
    }

    pub fn matches(&self, row: FilterRow) -> bool {
        let project = self.projects.is_empty() || self.names_project(row.project);
        let place = (self.folders.is_empty() && self.branches.is_empty())
            || self.names_folder(row.project, row.folder, row.windows)
            || row
                .branch
                .is_some_and(|b| self.branches.iter().any(|want| want == b));
        project
            && place
            && self.words.iter().all(|word| {
                [row.name, row.folder]
                    .into_iter()
                    .chain(row.branch)
                    .any(|field| field.to_lowercase().contains(word.as_str()))
            })
    }
}

/// `query` split at whitespace, a double-quoted stretch kept whole.
fn terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut term = String::new();
    let mut quoted = false;
    for c in query.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !term.is_empty() {
                    terms.push(std::mem::take(&mut term));
                }
            }
            c => term.push(c),
        }
    }
    if !term.is_empty() {
        terms.push(term);
    }
    terms
}

/// `value` as one term: quoted when it holds whitespace.
fn quoted(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{FilterRow, RowFilter, chip_label, folder_key};

    #[test]
    fn root_and_external_folder_chip_decisions() {
        assert_eq!(chip_label("/repo", "/repo/", None, None, false), None);
        assert_eq!(
            chip_label("/repo", "/repo", Some("main"), None, false).as_deref(),
            Some("⎇ main")
        );
        assert_eq!(
            chip_label("/repo", "/env/ember/", None, None, false).as_deref(),
            Some("ember")
        );
        assert_eq!(
            chip_label("/repo", "/env/ember", Some("fix/dates"), None, false).as_deref(),
            Some("ember  ⎇ fix/dates")
        );
        assert_eq!(chip_label("C:/Repo", "c:\\repo\\", None, None, true), None);
        assert_eq!(chip_label("/", "/", None, None, false), None);
        assert_eq!(folder_key("C:/", true), "c:");
        assert_eq!(
            chip_label("/repo", "/", None, None, false).as_deref(),
            Some("/")
        );
        assert_eq!(
            chip_label("/repo", "/tmp/one\\two", None, None, false).as_deref(),
            Some("one\\two")
        );
    }

    #[test]
    fn root_rows_only_show_branch_when_it_differs_from_header() {
        let cases = [
            ("/repo/", Some("main"), Some("main"), None),
            ("/repo", Some("topic"), Some("main"), Some("⎇ topic")),
            (
                "/env/ember",
                Some("main"),
                Some("main"),
                Some("ember  ⎇ main"),
            ),
            ("/repo", None, Some("main"), None),
            ("/repo", Some("topic"), None, Some("⎇ topic")),
        ];
        for (folder, branch, header, expected) in cases {
            assert_eq!(
                chip_label("/repo", folder, branch, header, false).as_deref(),
                expected,
                "folder={folder}, branch={branch:?}, header={header:?}"
            );
        }
        assert_eq!(
            chip_label("C:/Repo", "c:\\repo\\", Some("main"), Some("main"), true),
            None
        );
    }

    fn row<'a>(
        project: &'a str,
        name: &'a str,
        folder: &'a str,
        branch: Option<&'a str>,
    ) -> FilterRow<'a> {
        FilterRow {
            project,
            name,
            folder,
            branch,
            windows: false,
        }
    }

    /// The row menu's text, read back: that project's rows in that full folder
    /// or on that known branch, never a basename match, an unknown branch or
    /// another project's same-named branch.
    #[test]
    fn menu_filter_is_folder_or_known_branch_in_its_project() {
        let filter = RowFilter::parse(&RowFilter::for_folder("orez", "/one/ember", Some("topic")));
        let rows = [
            row("orez", "a", "/one/ember", None),
            row("orez", "b", "/two/glimmer", Some("topic")),
            row("orez", "c", "/two/ember", Some("other")),
            row("orez", "d", "/repo", None),
            row("muxel", "e", "/elsewhere", Some("topic")),
            row("muxel", "f", "/one/ember", None),
        ];
        assert_eq!(
            rows.map(|r| filter.matches(r)),
            [true, true, false, false, false, false]
        );
        let unknown = RowFilter::parse(&RowFilter::for_folder("orez", "/one/ember", None));
        assert!(!unknown.matches(row("orez", "a", "/two/ember", None)));
        assert!(unknown.matches(row("orez", "a", "/one/ember/", Some("new"))));
        let windows = RowFilter::parse(&RowFilter::for_folder("Repo", "C:/Repo", None));
        assert!(windows.matches(FilterRow {
            windows: true,
            ..row("repo", "a", "c:\\repo\\", None)
        }));
        let posix = RowFilter::parse(&RowFilter::for_folder("repo", "/Repo", None));
        assert!(!posix.matches(row("repo", "a", "/repo", None)));
    }

    /// A folder, branch or project with spaces survives the box, so the row
    /// that wrote it is the row that offers to clear it, and only in its
    /// project.
    #[test]
    fn menu_text_round_trips_through_the_box() {
        let text = RowFilter::for_folder("My Project", "/work/my folder", Some("fix/a b"));
        assert_eq!(
            text,
            "project:\"My Project\" folder:\"/work/my folder\" branch:\"fix/a b\""
        );
        let filter = RowFilter::parse(&text);
        assert!(filter.names_project("my project"));
        assert!(filter.names_folder("My Project", "/work/my folder/", false));
        assert!(!filter.names_folder("Other", "/work/my folder", false));
        assert!(!filter.names_folder("My Project", "/work/my", false));
        assert!(filter.matches(row("My Project", "x", "/else", Some("fix/a b"))));
        assert!(RowFilter::parse("  ").is_empty());
        assert!(
            RowFilter::parse("folder: branch:\"\" project:").is_empty(),
            "a token still being typed filters nothing"
        );
    }

    /// Typed words narrow by name, folder path and branch, each word somewhere,
    /// ignoring case, together with whatever the menu wrote.
    #[test]
    fn words_match_name_folder_or_branch_ignoring_case() {
        let rows = [
            row("orez", "Brownout review", "/repo", Some("main")),
            row("orez", "Dates fix", "/env/ember", Some("fix/dates")),
            row("muxel", "Sidebar filter", "/dev/muxel-wt-box", None),
        ];
        let shown = |query: &str| {
            let filter = RowFilter::parse(query);
            rows.iter()
                .filter(|r| filter.matches(**r))
                .map(|r| r.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(shown("BROWNOUT"), ["Brownout review"]);
        assert_eq!(shown("ember"), ["Dates fix"]);
        assert_eq!(shown("dates"), ["Dates fix"]);
        assert_eq!(shown("wt-box"), ["Sidebar filter"]);
        assert_eq!(shown("fix ember"), ["Dates fix"]);
        assert_eq!(shown("fix brownout"), Vec::<&str>::new());
        assert_eq!(shown("\"sidebar filter\""), ["Sidebar filter"]);
        assert_eq!(shown("project:OREZ fix"), ["Dates fix"]);
        assert_eq!(
            shown(""),
            ["Brownout review", "Dates fix", "Sidebar filter"]
        );
    }
}
