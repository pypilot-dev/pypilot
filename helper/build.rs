//! Build metadata for `pypilot version`: the target triple, and, when built
//! from a git checkout, the commit and the date the data tables last changed.
//! Without git (a crates.io source build) those are simply absent.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=data");
    println!("cargo:rerun-if-env-changed=PYPILOT_GIT_COMMIT");

    if let Ok(target) = std::env::var("TARGET") {
        println!("cargo:rustc-env=PYPILOT_TARGET={target}");
    }

    // Rebuild when HEAD moves, so the commit is never stale. In a worktree
    // `--git-dir` is per-worktree and refs live in the common dir.
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!(
            "cargo:rerun-if-changed={}",
            Path::new(&dir).join("HEAD").display()
        );
    }
    if let Some(dir) = git(&["rev-parse", "--git-common-dir"]) {
        let common = Path::new(&dir);
        println!(
            "cargo:rerun-if-changed={}",
            common.join("packed-refs").display()
        );
        if let Some(head_ref) = git(&["symbolic-ref", "-q", "HEAD"]) {
            println!("cargo:rerun-if-changed={}", common.join(head_ref).display());
        }
    }

    let commit = std::env::var("PYPILOT_GIT_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| git(&["rev-parse", "--short=12", "HEAD"]));
    if let Some(commit) = commit {
        println!("cargo:rustc-env=PYPILOT_GIT_COMMIT={commit}");
    }
    if let Some(date) = git(&["log", "-1", "--format=%cs"]) {
        println!("cargo:rustc-env=PYPILOT_GIT_DATE={date}");
    }
    // A shallow clone (the release workflow's) sees one commit, so this is
    // "no later than"; a full clone gives the last data change.
    if let Some(date) = git(&["log", "-1", "--format=%cs", "--", "data"]) {
        println!("cargo:rustc-env=PYPILOT_DATA_DATE={date}");
    }
}
