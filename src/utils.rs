use anyhow::Result;
use std::{path::Path, process::Command};

pub(crate) fn check_git_clean(repo_path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(&["status", "--porcelain"])
        .current_dir(repo_path)
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to check git status: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(output.stdout.is_empty())
}

pub(crate) fn get_git_tree_hash(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(&["rev-parse", "HEAD^{tree}"])
        .current_dir(repo_path)
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to get git hash: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}
