use anyhow::Result;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;
use tracing::{debug, error, info};

mod logging;
mod shell;

use shell::Shell;

fn setup_git_repo(temp_dir: &Path, package_name: &str, spec_content: &str) -> Result<String> {
    let repo_path = temp_dir.join(package_name);
    fs::create_dir_all(&repo_path)?;

    // Initialize git repo
    debug!("Initializing git repo at: {}", repo_path.display());
    let shell = Shell::new(&repo_path);
    shell.run("git init")?;

    // Create spec file
    let spec_path = repo_path.join(format!("{}.spec", package_name));
    debug!("Writing spec file: {}", spec_path.display());
    fs::write(&spec_path, spec_content)?;

    // Create a dummy source file
    let src_dir = repo_path.join(format!("{}-1.0", package_name));
    debug!("Creating source directory: {}", src_dir.display());
    fs::create_dir_all(&src_dir)?;
    fs::write(src_dir.join("dummy.txt"), "dummy content")?;

    // Create tarball
    debug!("Creating tarball for {}", package_name);
    shell.run(&format!(
        "tar czf {}-1.0.tar.gz {}-1.0",
        package_name, package_name
    ))?;

    // Add and commit files
    debug!("Adding and committing files for {}", package_name);
    shell.run("git add .")?;
    shell.run("git commit -m 'Initial commit'")?;

    Ok(format!("file://{}", repo_path.display()))
}

fn create_test_yaml(
    temp_dir: &Path,
    hello_repo: &str,
    hello_extended_repo: &str,
    hello_other_extended_repo: &str,
    combined_repo: &str,
) -> Result<String> {
    let yaml_content = format!(
        r#"hello:
  type:
    source: git
    url: {}

hello-extended:
  type:
    source: git
    url: {}
  dependencies: ["hello"]

hello-other-extended: {{type: {{source: git, url: {}}}, dependencies: ["hello"]}}

combined:
  type:
    source: git
    url: {}
  dependencies: ["hello-extended", "hello-other-extended"]
"#,
        hello_repo, hello_extended_repo, hello_other_extended_repo, combined_repo
    );

    let yaml_path = temp_dir.join("test-spec.yaml");
    debug!("Writing test YAML to: {}", yaml_path.display());
    debug!("YAML content:\n{}", yaml_content);
    fs::write(&yaml_path, yaml_content)?;

    Ok(yaml_path.to_string_lossy().to_string())
}

fn main() -> Result<()> {
    // Initialize debug logging for test runner
    let logging_args = logging::LoggingArgs {
        log_level: Some("debug".to_string()),
        log_dir: None,
        log_dir_level: None,
    };
    logging::start(&logging_args)?;

    info!("Setting up test environment...");

    // Create temporary directory
    let temp_dir = TempDir::new()?;
    let temp_path = temp_dir.path();

    info!("Using temporary directory: {}", temp_path.display());

    // Read fixture files
    let hello_spec = fs::read_to_string("tests/fixtures/hello.spec")?;
    let hello_extended_spec = fs::read_to_string("tests/fixtures/hello-extended.spec")?;
    let hello_other_extended_spec = fs::read_to_string("tests/fixtures/hello-other-extended.spec")?;
    let combined_spec = fs::read_to_string("tests/fixtures/combined.spec")?;

    // Setup git repositories
    info!("Setting up hello package git repository...");
    let hello_repo = setup_git_repo(temp_path, "hello", &hello_spec)?;

    info!("Setting up hello-extended package git repository...");
    let hello_extended_repo = setup_git_repo(temp_path, "hello-extended", &hello_extended_spec)?;
    let hello_other_extended_repo = setup_git_repo(
        temp_path,
        "hello-other-extended",
        &hello_other_extended_spec,
    )?;
    let combined_repo = setup_git_repo(temp_path, "combined", &combined_spec)?;

    // Create test YAML
    info!("Creating test YAML specification...");
    let yaml_path = create_test_yaml(
        temp_path,
        &hello_repo,
        &hello_extended_repo,
        &hello_other_extended_repo,
        &combined_repo,
    )?;

    // Create workspace directory
    let workspace_path = temp_path.join("workspace");
    debug!("Workspace path: {}", workspace_path.display());

    // Get backend from environment variable, default to "null" for testing
    let backend = std::env::var("TEST_SPECTREE_BACKEND").unwrap_or_else(|_| "null".to_string());
    info!("Using backend: {}", backend);

    // Run spectree
    info!("Running spectree with test specification...");
    debug!(
        "Command: ./target/debug/spectree {} --workspace {} hello-extended --backend {} --log-level debug",
        yaml_path,
        workspace_path.display(),
        backend
    );
    let status = Command::new("./target/debug/spectree")
        .arg(&yaml_path)
        .arg("--workspace")
        .arg(&workspace_path)
        .arg("combined") // Use hello-extended as root since it depends on hello
        .arg("--backend")
        .arg(&backend)
        .arg("--log-level")
        .arg("debug")
        .status()?;

    if status.success() {
        info!("✅ Test completed successfully!");
    } else {
        error!("❌ Test failed with exit code: {}", status);
        anyhow::bail!("Test failed");
    }

    Ok(())
}
