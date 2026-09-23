//! Exercise Cargo's real invalidation behavior; a path-list unit test cannot
//! establish whether a cached binary refreshes when Git creates a loose ref.
use std::{fs, path::Path, process::Command};

fn run(dir: &Path, program: &str, args: &[&str]) -> String {
    let output = Command::new(program)
        .current_dir(dir)
        .args(args)
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .output()
        .expect("run fixture command");
    assert!(
        output.status.success(),
        "{program} {args:?}:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 output").trim().to_owned()
}

fn check_packed_ref_transition(root: &Path) {
    let crate_dir = root.join("crates/cleat");
    let old_head = run(root, "git", &["rev-parse", "HEAD"]);
    // A first build's git status can refresh index stat metadata. Warm Cargo's
    // cache before the ref-only update so that cannot mask a missing ref watch.
    for _ in 0..3 {
        assert_eq!(run(&crate_dir, "cargo", &["run", "--quiet", "--offline"]), old_head);
    }
    let tree = run(root, "git", &["rev-parse", "HEAD^{tree}"]);
    let next_head = run(root, "git", &["commit-tree", &tree, "-p", "HEAD", "-m", "ref-only change"]);
    run(root, "git", &["update-ref", "HEAD", &next_head]);
    assert_ne!(old_head, next_head);
    assert_eq!(
        run(&crate_dir, "cargo", &["run", "--quiet", "--offline"]),
        next_head,
        "build identity must follow a packed ref becoming loose"
    );
}

#[test]
fn cached_build_follows_packed_refs_becoming_loose_in_checkout_and_worktree() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("repo");
    let crate_dir = root.join("crates/cleat");
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    fs::write(crate_dir.join("Cargo.toml"), "[package]\nname = \"build-identity-fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n")
        .unwrap();
    fs::write(crate_dir.join("src/main.rs"), "fn main() { println!(\"{}\", env!(\"CLEAT_GIT_SHA\")); }\n").unwrap();
    fs::write(crate_dir.join("build.rs"), include_str!("../build.rs")).unwrap();
    fs::write(root.join(".gitignore"), "target/\n").unwrap();
    run(&root, "git", &["init", "--quiet", "--initial-branch=main"]);
    run(&root, "git", &["config", "user.name", "Build identity test"]);
    run(&root, "git", &["config", "user.email", "build-test@example.invalid"]);
    run(&root, "git", &["config", "commit.gpgsign", "false"]);
    let hooks = temp.path().join("empty-hooks");
    fs::create_dir(&hooks).unwrap();
    run(&root, "git", &["config", "core.hooksPath", hooks.to_str().unwrap()]);
    run(&crate_dir, "cargo", &["generate-lockfile", "--offline"]);
    run(&root, "git", &["add", "."]);
    run(&root, "git", &["commit", "--quiet", "-m", "fixture"]);
    run(&root, "git", &["pack-refs", "--all", "--prune"]);
    check_packed_ref_transition(&root);

    let worktree = temp.path().join("worktree");
    run(&root, "git", &["worktree", "add", "--quiet", "-b", "nested/branch", worktree.to_str().unwrap()]);
    run(&root, "git", &["pack-refs", "--all", "--prune"]);
    check_packed_ref_transition(&worktree);
}
