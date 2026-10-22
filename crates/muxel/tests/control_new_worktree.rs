use muxel_core::Project;
use muxel_core::control::NewOptions;
use std::process::Command;
use uuid::Uuid;

// This integration fixture needs git. Keep it in the app's full CI suite,
// outside the core/store-only tests run by the Nix package build.
#[test]
fn linked_launch_worktree_survives_migration_and_close_cleanup_plan() {
    let root = std::env::temp_dir().join(format!("muxel-cwd-{}", Uuid::new_v4()));
    let repo = root.join("repo");
    let linked = root.join("linked");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(&repo)
            .env("GIT_CONFIG_GLOBAL", "")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };
    git(&["init"]);
    git(&["commit", "--allow-empty", "-m", "fixture"]);
    git(&["worktree", "add", "-b", "linked", linked.to_str().unwrap()]);
    let marker = linked.join("uncommitted.txt");
    std::fs::write(&marker, "keep this caller-owned directory").unwrap();
    let registered_before = git(&["worktree", "list", "--porcelain"]);
    let project = Project::new("repo", repo.clone());
    let instance = NewOptions {
        cwd: Some(linked.clone()),
        ..Default::default()
    }
    .instance(&project, &[], 0)
    .unwrap();
    let iid = instance.id;
    let mut ws = muxel_core::Workspace {
        projects: vec![project],
        instances: vec![instance],
        ..Default::default()
    };
    muxel_core::migrate_worktrees(&mut ws);
    assert!(ws.worktrees.is_empty());
    let cleanup = ws.instance(iid).unwrap().worktree_cleanup();
    ws.remove_instance_meta(iid);
    // This is the exact plan consumed by teardown_closed_instance. None does
    // neither registered disposal nor legacy remove_worktree/prune.
    assert_eq!(cleanup, muxel_core::WorktreeCleanup::None);
    assert!(marker.is_file());
    assert_eq!(git(&["worktree", "list", "--porcelain"]), registered_before);
    git(&["worktree", "remove", "--force", linked.to_str().unwrap()]);
    std::fs::remove_dir_all(root).unwrap();
}
