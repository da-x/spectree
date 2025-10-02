use anyhow::Result;
use std::{path::Path, process::Command};
use tracing::debug;

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

pub(crate) fn get_git_tree_hash(repo_path: &Path, subpath: Option<&str>) -> Result<String> {
    let output = match subpath {
        Some(subpath) => {
            // Get the tree hash of the specific subdirectory
            Command::new("git")
                .args(&["rev-parse", &format!("HEAD:{}", subpath)])
                .current_dir(repo_path)
                .output()?
        }
        None => {
            // Get the tree hash of the entire repository
            Command::new("git")
                .args(&["rev-parse", "HEAD^{tree}"])
                .current_dir(repo_path)
                .output()?
        }
    };

    if !output.status.success() {
        anyhow::bail!(
            "Failed to get git hash{}: {}",
            subpath
                .map(|s| format!(" for subpath '{}'", s))
                .unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

pub(crate) fn get_git_revision(repo_path: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(&["rev-parse", "HEAD"])
        .current_dir(repo_path)
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "Failed to get git revision: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

// Helper function to recursively copy directories with hardlinks when possible
pub(crate) fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    // Remove destination if it exists to ensure clean copy
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }

    // First try to hardlink the entire directory tree with cp -al
    let cp_result = Command::new("cp")
        .args(&["-al", &src.to_string_lossy(), &dst.to_string_lossy()])
        .output();

    match cp_result {
        Ok(output) if output.status.success() => {
            debug!(
                "Successfully hardlinked directory {} to {}",
                src.display(),
                dst.display()
            );
            return Ok(());
        }
        Ok(output) => {
            debug!(
                "cp -al failed: {}, falling back to regular copy",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Err(e) => {
            debug!("cp command failed: {}, falling back to regular copy", e);
        }
    }

    // Fallback to regular recursive copy
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst.join(entry.file_name()))?;
        } else {
            std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}
