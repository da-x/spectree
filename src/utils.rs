use crate::shell::{Shell, ShellEscaped};
use anyhow::{Context, Result};
use std::path::Path;
use tracing::{debug, info};

pub(crate) fn check_git_clean(repo_path: &Path) -> Result<bool> {
    let shell = Shell::new(repo_path);
    let output = shell.run_with_output_sync("git status --porcelain")?;
    Ok(output.is_empty())
}

pub(crate) fn get_git_tree_hash(repo_path: &Path, subpath: Option<&str>) -> Result<String> {
    let shell = Shell::new(repo_path);
    let command = match subpath {
        Some(subpath) => format!("git rev-parse HEAD:{}", subpath),
        None => "git rev-parse HEAD^{tree}".to_string(),
    };

    let output = shell.run_with_output_sync(&command).with_context(|| {
        format!(
            "Failed to get git hash{}",
            subpath.map(|s| format!(" for subpath '{}'", s)).unwrap_or_default()
        )
    })?;

    Ok(output)
}

pub(crate) fn get_git_revision(repo_path: &Path) -> Result<String> {
    let shell = Shell::new(repo_path);
    let output = shell.run_with_output_sync("git rev-parse HEAD")?;
    Ok(output)
}

// Helper function to recursively copy directories with hardlinks when possible
pub(crate) fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    // Remove destination if it exists to ensure clean copy
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }

    // First try to hardlink the entire directory tree with cp -al
    let current_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let shell = Shell::new(&current_dir);
    let cp_result = shell.run_sync(&format!("cp -al {} {}", src.shell_escaped(), dst.shell_escaped()));

    match cp_result {
        Ok(()) => {
            debug!(
                "Successfully hardlinked directory {} to {}",
                src.display(),
                dst.display()
            );
            return Ok(());
        }
        Err(e) => {
            debug!("cp -al failed: {}, falling back to regular copy", e);
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

pub(crate) fn export_git_revision(
    repo_path: &Path, revision: &str, export_path: &Path, subpath: Option<&str>,
) -> Result<()> {
    // Ensure export directory exists
    if let Some(parent) = export_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Build git archive command
    let mut args = vec!["archive", "--format=tar", revision];

    // Add subpath if specified
    if let Some(subpath) = subpath {
        args.push(subpath);
    }

    info!(
        "Exporting git revision '{}' from {} to {}{}",
        revision,
        repo_path.display(),
        export_path.display(),
        subpath.map(|s| format!(" (subpath: {})", s)).unwrap_or_default()
    );

    let shell = Shell::new(repo_path);
    let command = args.join(" ");
    let output = shell.run_with_output_sync(&command).with_context(|| {
        format!(
            "Failed to export git revision '{}'{}",
            revision,
            subpath.map(|s| format!(" from subpath '{}'", s)).unwrap_or_default()
        )
    })?;

    // Create parent directory if it doesn't exist
    if let Some(parent) = export_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::create_dir_all(export_path)?;

    // Use a simpler approach: write archive to temp file then extract
    let temp_archive = export_path.with_extension("tar.tmp");
    std::fs::write(&temp_archive, output.as_bytes())?;

    // Extract the tar archive to the export path
    let tar_command = format!(
        "tar -xf {} -C {}",
        temp_archive.shell_escaped(),
        export_path.shell_escaped()
    );
    let tar_result = shell.run_sync(&tar_command);

    // Clean up temp file
    let _ = std::fs::remove_file(&temp_archive);

    tar_result.with_context(|| format!("Failed to extract git archive to {}", export_path.display()))?;

    debug!("Successfully exported git revision to {}", export_path.display());
    Ok(())
}
