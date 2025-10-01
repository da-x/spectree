use anyhow::Result;
use clap::Parser;
use nutype::nutype;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::time::Duration;
use std::{fs, path};
use tokio::sync::mpsc;
use tokio::task;
use tracing::{debug, error, info, span, Instrument, Level};

mod docker;
mod logging;
mod shell;
mod utils;

use shell::Shell;

use crate::utils::{check_git_clean, get_git_tree_hash};

fn get_base_os() -> Result<String> {
    let os_release_content = fs::read_to_string("/etc/os-release")?;

    let mut id = None;
    let mut version_id = None;

    for line in os_release_content.lines() {
        if let Some(value) = line.strip_prefix("ID=") {
            id = Some(value.trim_matches('"'));
        } else if let Some(value) = line.strip_prefix("VERSION_ID=") {
            version_id = Some(value.trim_matches('"'));
        }
    }

    match (id, version_id) {
        (Some("rocky"), Some(version)) if version.starts_with("10") => Ok("epel10".to_string()),
        (Some(id), Some(version)) => {
            anyhow::bail!("Unsupported OS: ID={}, VERSION_ID={}", id, version)
        }
        _ => anyhow::bail!("Could not parse /etc/os-release"),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BuilderBackend {
    Mock,
    Docker,
    Null,
}

impl Default for BuilderBackend {
    fn default() -> Self {
        BuilderBackend::Mock
    }
}

impl FromStr for BuilderBackend {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "mock" => Ok(BuilderBackend::Mock),
            "null" => Ok(BuilderBackend::Null),
            "docker" => Ok(BuilderBackend::Docker),
            _ => anyhow::bail!("Invalid builder backend: {}. Valid options: mock, null", s),
        }
    }
}

impl std::fmt::Display for BuilderBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuilderBackend::Mock => write!(f, "mock"),
            BuilderBackend::Null => write!(f, "null"),
            BuilderBackend::Docker => write!(f, "docker"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Dependency {
    Regular(String),
    OnlyDirect(String),
}

impl Dependency {
    pub fn parse(dep_str: &str) -> Self {
        if dep_str.starts_with('~') {
            Self::OnlyDirect(dep_str[1..].to_string())
        } else {
            Self::Regular(dep_str.to_string())
        }
    }

    pub fn key(&self) -> &str {
        match self {
            Self::Regular(key) => key,
            Self::OnlyDirect(key) => key,
        }
    }

    pub fn is_direct_only(&self) -> bool {
        matches!(self, Self::OnlyDirect(_))
    }

    pub fn parse_list(dep_strings: &[String]) -> Vec<Self> {
        dep_strings.iter().map(|s| Self::parse(s)).collect()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "source", deny_unknown_fields)]
pub enum SourceType {
    #[serde(rename = "git")]
    Git {
        url: Option<String>,
        path: Option<String>,
    },

    #[serde(rename = "srpm")]
    Srpm { path: String },
}

#[nutype(derive(
    Debug,
    PartialEq,
    Eq,
    Clone,
    Hash,
    Serialize,
    Deserialize,
    Display,
    From,
    Into,
    Borrow,
    AsRef
))]
pub struct SourceKey(String);

#[nutype(derive(
    Debug,
    PartialEq,
    Eq,
    Clone,
    Hash,
    Serialize,
    Deserialize,
    Display,
    From,
    Into,
    Borrow,
    AsRef
))]
pub struct SourceHash(String);

#[nutype(derive(
    Debug,
    PartialEq,
    Eq,
    Clone,
    Hash,
    Serialize,
    Deserialize,
    Display,
    From,
    Into,
    Borrow,
    AsRef
))]
pub struct BuildHash(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BuildKey {
    pub source_key: SourceKey,
    pub build_hash: BuildHash,
}

impl BuildKey {
    pub fn new(source_key: SourceKey, build_hash: BuildHash) -> Self {
        Self {
            source_key,
            build_hash,
        }
    }

    pub fn build_dir_name(&self) -> String {
        format!("{}-{}", self.source_key.as_ref(), self.build_hash.as_ref())
    }
}

impl std::fmt::Display for BuildKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.build_dir_name())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    #[serde(rename = "type")]
    pub typ: SourceType,
    #[serde(default)]
    pub dependencies: Vec<SourceKey>,
    #[serde(default)]
    pub build_params: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SpecTree {
    #[serde(flatten)]
    pub sources: HashMap<SourceKey, Source>,
}

#[derive(Parser)]
#[command(name = "spectree")]
#[command(about = "A tool for building dependent RPM packages from a YAML specification")]
struct Args {
    #[arg(help = "Path to the YAML specification file")]
    spec_file: PathBuf,

    #[arg(short, long, help = "Workspace directory for builds and Git clones")]
    workspace: PathBuf,

    #[arg(help = "Root source to start building from")]
    root_source: SourceKey,

    #[arg(
        short,
        long,
        help = "Builder backend to use for RPM generation",
        default_value = "mock"
    )]
    backend: BuilderBackend,

    #[arg(
        long,
        help = "Target OS for Docker backend (e.g., fedora-39, centos-stream-9)"
    )]
    target_os: Option<String>,

    #[command(flatten)]
    logging: logging::LoggingArgs,
}

fn setup_workspace(workspace: &Path) -> Result<()> {
    fs::create_dir_all(&workspace)?;
    fs::create_dir_all(workspace.join("sources"))?;
    fs::create_dir_all(workspace.join("builds"))?;
    info!("Workspace setup at: {}", workspace.display());
    Ok(())
}

fn clone_or_update_repo(url: &str, workspace: &Path, key: &str) -> Result<PathBuf> {
    let sources_dir = workspace.join("sources");
    let repo_path = sources_dir.join(key);

    if repo_path.exists() {
        info!("Updating existing repo for {}", key);
        let output = Command::new("git")
            .args(&["fetch", "origin"])
            .current_dir(&repo_path)
            .output()?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to fetch: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let output = Command::new("git")
            .args(&["reset", "--hard", "origin/HEAD"])
            .current_dir(&repo_path)
            .output()?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to reset: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    } else {
        info!("Cloning repo for {} from {}", key, url);
        let output = Command::new("git")
            .args(&["clone", url, &repo_path.to_string_lossy()])
            .output()?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to clone: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    Ok(repo_path)
}

fn calculate_build_hash(
    key: &SourceKey,
    source: &Source,
    source_content_hash: &SourceHash,
    dependency_hashes: &HashMap<SourceKey, BuildHash>,
) -> BuildHash {
    let mut hasher = Sha256::new();
    hasher.update(key.as_ref().as_bytes());
    hasher.update(source_content_hash.as_ref().as_bytes());

    // Include dependency hashes in sorted order for consistent hashing
    let mut dep_hashes = Vec::new();
    for dep_str in &source.dependencies {
        let dependency = Dependency::parse(dep_str.as_ref());
        if let Some(dep_hash) = dependency_hashes.get(dependency.key()) {
            dep_hashes.push((
                dependency.key().to_string(),
                dep_hash.clone(),
                dependency.is_direct_only(),
            ));
        }
    }
    dep_hashes.sort_by(|(a, _, _), (b, _, _)| a.cmp(b));
    hasher.update(format!("{:?}", dep_hashes).as_bytes());

    hasher.update(format!("{:?}", source.build_params).as_bytes());
    BuildHash::new(format!("{:x}", hasher.finalize()))
}

impl Source {
    fn get_repo_path(&self, key: &SourceKey, workspace: &Path, update: bool) -> Result<PathBuf> {
        let repo_path = match &self.typ {
            SourceType::Git { url, path } => {
                if let Some(path) = path {
                    let path = path.replace("${NAME}", key.as_ref());
                    path::absolute(path)?
                } else if let Some(url) = url {
                    let url = url.replace("${NAME}", key.as_ref());
                    if url.starts_with("file://") {
                        PathBuf::from(&url[7..])
                    } else {
                        if !update {
                            workspace.join("sources").join(key.as_ref())
                        } else {
                            clone_or_update_repo(&url, workspace, key.as_ref())?
                        }
                    }
                } else {
                    anyhow::bail!("Invalid Git source");
                }
            }
            SourceType::Srpm { path: _ } => {
                anyhow::bail!("SRPM sources not yet implemented");
            }
        };

        Ok(repo_path)
    }
}

fn calc_source_hash(key: &SourceKey, source: &Source, workspace: &Path) -> Result<SourceHash> {
    let repo_path = source.get_repo_path(key, workspace, true)?;

    if !check_git_clean(&repo_path)? {
        anyhow::bail!("Git repository for {} has uncommitted changes", key);
    }

    let git_hash = get_git_tree_hash(&repo_path)?;
    debug!("Processed sources for source: {} (git: {})", key, git_hash);

    Ok(SourceHash::new(git_hash))
}

struct SourceHashes {
    hashes: HashMap<SourceKey, SourceHash>,
}

fn get_source_hashes(
    args: &Args,
    spec_tree: &SpecTree,
    all_sources: &Vec<SourceKey>,
) -> Result<SourceHashes> {
    let mut hashes = HashMap::new();
    for key in all_sources {
        let source = spec_tree.sources.get(key).unwrap();
        match calc_source_hash(key, source, &args.workspace) {
            Ok(hash) => {
                hashes.insert(key.clone(), hash);
                info!("✅ Source {} processed successfully", key);
            }
            Err(e) => {
                error!("❌ Failed to process sources for source {}: {}", key, e);
                return Err(e);
            }
        }
    }
    Ok(SourceHashes { hashes })
}

fn find_all_dependency_pairs(
    sources: &[SourceKey],
    spec_tree: &SpecTree,
) -> Result<Vec<(SourceKey, SourceKey)>> {
    let mut pairs = Vec::new();
    let mut visited = HashSet::new();
    let mut recursion_stack = HashSet::new();

    for source_key in sources {
        find_dependency_pairs_recursive(
            source_key,
            spec_tree,
            &mut pairs,
            &mut visited,
            &mut recursion_stack,
        )?;
    }

    Ok(pairs)
}

fn find_dependency_pairs_recursive(
    source_key: &SourceKey,
    spec_tree: &SpecTree,
    pairs: &mut Vec<(SourceKey, SourceKey)>,
    visited: &mut HashSet<SourceKey>,
    recursion_stack: &mut HashSet<SourceKey>,
) -> Result<()> {
    // Check for cycles - if this source is already in the recursion stack
    if recursion_stack.contains(source_key) {
        anyhow::bail!(
            "Circular dependency detected involving source: {}",
            source_key
        );
    }

    // If we've already processed this source completely, skip it
    if visited.contains(source_key) {
        return Ok(());
    }

    // Add to recursion stack to detect cycles
    recursion_stack.insert(source_key.clone());

    // Get the source definition
    let source = spec_tree
        .sources
        .get(source_key)
        .ok_or_else(|| anyhow::anyhow!("Source '{}' not found in spec tree", source_key))?;

    // Process all dependencies of this source
    for dep_key in &source.dependencies {
        // Parse the dependency to handle ~ prefix
        let dependency = Dependency::parse(dep_key.as_ref());
        let actual_dep_key = SourceKey::from(dependency.key().to_string());

        // Add the dependency pair (source -> dependency)
        pairs.push((source_key.clone(), actual_dep_key.clone()));

        // Recursively process the dependency
        find_dependency_pairs_recursive(
            &actual_dep_key,
            spec_tree,
            pairs,
            visited,
            recursion_stack,
        )?;
    }

    // Remove from recursion stack and mark as visited
    recursion_stack.remove(source_key);
    visited.insert(source_key.clone());

    Ok(())
}

fn resolve_dependencies(key: &SourceKey, spec_tree: &SpecTree) -> Result<Vec<SourceKey>> {
    let mut resolved = Vec::new();
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();

    queue.push_back((SourceKey::from(key.to_string()), true));

    while let Some((key, direct)) = queue.pop_front() {
        if visited.contains(&key) {
            continue;
        }
        visited.insert(key.clone());

        let source = spec_tree
            .sources
            .get(&key)
            .ok_or_else(|| anyhow::anyhow!("Source '{}' not found in spec tree", key))?;

        // Add current source to resolved list
        resolved.push(key);

        // Process dependencies
        for dep_str in &source.dependencies {
            let dependency = Dependency::parse(dep_str.as_ref());

            // If dependency has ~ prefix, only include if this is a direct dependency
            if direct || !dependency.is_direct_only() {
                queue.push_back((SourceKey::from(dependency.key().to_string()), false));
            }
        }
    }

    // Remove the root source from the resolved list (we don't depend on ourselves)
    resolved.retain(|k| k != key);

    Ok(resolved)
}

async fn build_source(
    build_key: &BuildKey,
    source: &Source,
    all_dependencies: &HashMap<SourceKey, BuildHash>,
    workspace: &Path,
    backend: &BuilderBackend,
    target_os: Option<&str>,
) -> Result<()> {
    let build_dir_final = workspace.join("builds").join(build_key.build_dir_name());

    // Check if build already exists - if so, do nothing
    let build_subdir_final = build_dir_final.join("build");
    if build_subdir_final.exists() {
        info!("Build already exists, skipping");
        return Ok(());
    }

    let build_dir = workspace
        .join("builds")
        .join(format!("{}.tmp", build_key.build_dir_name()));

    let _ = fs::remove_dir_all(&build_dir);

    // Check if build already exists - if so, do nothing
    let build_subdir = build_dir.join("build");

    // Create build subdirectory
    fs::create_dir_all(&build_subdir)?;
    debug!("Created build subdirectory: {}", build_subdir.display());

    // If there are dependencies, create deps directory and hardlink them
    if !all_dependencies.is_empty() {
        let deps_dir = build_dir.join("deps");
        fs::create_dir_all(&deps_dir)?;
        info!("Created deps directory: {}", deps_dir.display());

        let shell = Shell::new(&deps_dir);

        // Hardlink each dependency's build directory
        for (dep_key, dep_hash) in all_dependencies.iter() {
            let dep_build_key = BuildKey::new(dep_key.clone(), dep_hash.clone());
            let dep_build_dir = workspace
                .join("builds")
                .join(dep_build_key.build_dir_name())
                .join("build");

            if !dep_build_dir.exists() {
                anyhow::bail!(
                    "Dependency build directory does not exist: {}",
                    dep_build_dir.display()
                );
            }

            let target_dir = dep_build_key.build_dir_name();
            shell
                .run_with_output(&format!("mkdir -p \"{}\"", target_dir))
                .await?;
            shell
                .run_with_output(&format!(
                    "cp -al \"{}\"/* \"{}\"/ 2>/dev/null || true",
                    dep_build_dir.display(),
                    target_dir
                ))
                .await?;
            debug!("Hardlinked dependency {} to deps directory", dep_key);
        }

        // Run createrepo_c to create repository metadata
        shell.run_with_output("createrepo_c .").await?;
        info!("Created repository metadata in deps directory");
    }

    // Get source repository path
    let repo_path = source.get_repo_path(&build_key.source_key, workspace, false)?;

    let base_os = match target_os {
        Some(os) => os.to_string(),
        None => get_base_os()?,
    };

    info!("Generating source RPM using fedpkg");
    let fedpkg_shell = Shell::new(&repo_path);
    let build_srpm_dir = build_dir.join("srpm");
    let build_srpm_dir_disp = build_srpm_dir.display();
    task::block_in_place(|| {
        fedpkg_shell.run_with_output_sync(&format!(
            "fedpkg --release {base_os} srpm --define \"_srcrpmdir {build_srpm_dir_disp}\""
        ))
    })?;

    // Find the generated SRPM file
    let srpm_files: Vec<_> = std::fs::read_dir(&build_srpm_dir)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? == "rpm"
                && path.file_name()?.to_str()?.contains(".src.")
                && path.file_name()?.to_str()?.starts_with(build_key.source_key.as_ref())
            {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    if srpm_files.is_empty() {
        anyhow::bail!("No source RPM found after fedpkg srpm");
    }

    if srpm_files.len() > 1 {
        anyhow::bail!(
            "Multiple source RPMs found for {}: {:?}",
            build_key.source_key.as_ref(),
            srpm_files
        );
    }

    let srpm_path = &srpm_files[0];
    debug!("Found source RPM: {}", srpm_path.display());

    // Build command based on backend
    match backend {
        BuilderBackend::Mock => {
            build_with_mock(
                source,
                all_dependencies,
                workspace,
                build_dir.clone(),
                build_subdir,
                srpm_path,
            )
            .await?;
        }
        BuilderBackend::Null => {
            info!("🚫 Null backend");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        BuilderBackend::Docker => {
            build_under_docker(workspace, target_os, build_dir.clone()).await?;
        }
    }

    std::fs::rename(&build_dir, build_dir_final)?;

    Ok(())
}

async fn build_under_docker(
    workspace: &Path,
    target_os: Option<&str>,
    build_dir: PathBuf,
) -> Result<(), anyhow::Error> {
    let base_os = match target_os {
        Some(os) => os.to_string(),
        None => get_base_os()?,
    };

    info!("Using base OS: {}", base_os);

    let dockerfile = docker::get_builder_dockerfile_for_os(&base_os)?;
    let mut image = match docker::ensure_image(&base_os, &dockerfile, "").await? {
        Ok(image) => image,
        Err(output) => anyhow::bail!(
            "error creating base os image: {:?}",
            String::from_utf8_lossy(&output.stderr)
        ),
    };

    let shell = Shell::new(workspace).with_image(&image).with_mount(
        &build_dir.to_string_lossy().as_ref().to_owned(),
        "/workspace",
    );

    let missing_deps = shell
        .run_with_output(
            r#"
rpm -D "_topdir /workspace/build" -i /workspace/srpm/*.src.rpm

list-missing-deps() {
    local param="-br"
    if ! rpmbuild -br 2>/dev/null ; then
        param="-bp"
    fi

    (rpmbuild ${param} "-D _topdir /workspace/build" /workspace/build/SPECS/*.spec 2>&1 || true) \
        | (grep -v ^error: || true) \
        | grep -E '([^ ]*) is needed by [^ ]+$' \
        | sed -E 's/[\t]/ /g' \
        | sed -E 's/ +(.*) is needed by [^ ]+$/\1/g'
}

# >&2
list-missing-deps
        "#,
        )
        .await?;

    let mut deps: Vec<_> = missing_deps.lines().collect();
    if !deps.is_empty() {
        info!("Found {} dependencies", deps.len());
        debug!("Dependencies: {:?}", deps);

        let dep_repo = build_dir.join("deps").exists();

        deps.sort();
        let deps = deps
            .iter()
            .map(|x| format!("{:?}", x))
            .collect::<Vec<_>>()
            .join(" ");

        let mut hasher = Sha256::new();
        hasher.update(deps.as_bytes());
        let deps_image = format!("{image}-{:x}", hasher.finalize());
        let dockerfile = if dep_repo {
            format!(
                r#"FROM {image}
COPY --from=deps / /deps
RUN dnf install --repofrompath=deps,file:///deps --setopt=deps.gpgcheck=0 --enablerepo=deps -y {deps}
RUN rm -rf /deps
"#
            )
        } else {
            format!(
                r#"FROM {image}
RUN dnf install -y {deps}
"#
            )
        };
        debug!("image with deps Dockerfile: {:?}", dockerfile);
        image = match docker::ensure_image(
            &deps_image,
            &dockerfile,
            &if dep_repo {
                format!(
                    "--layers=false --build-context deps={}/deps",
                    build_dir.display()
                )
            } else {
                format!("--layers=false")
            },
        )
        .await?
        {
            Ok(image) => image,
            Err(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let mut found = false;
                for line in stderr.lines() {
                    if let Some(package_start) = line.find("Error: Unable to find a match: ") {
                        let package =
                            &line[package_start + "Error: Unable to find a match: ".len()..];
                        error!(
                            "Error: Unable to find a match: {}",
                            package.replace(" \\t", " ")
                        );
                        found = true;
                        break;
                    }
                }
                if !found {
                    error!("Error: {}", stderr);
                }
                anyhow::bail!("not being able to install dependencies");
            }
        };

        info!("Building on image {image}");
    }

    let shell = Shell::new(workspace).with_image(&image).with_mount(
        &build_dir.to_string_lossy().as_ref().to_owned(),
        "/workspace",
    );

    shell
        .run_logged(
            r#"
rpmbuild -ba -D "_topdir /workspace/build" /workspace/build/SPECS/*.spec
        "#,
        )
        .await?;

    Ok(())
}

async fn build_with_mock(
    source: &Source,
    all_dependencies: &HashMap<SourceKey, BuildHash>,
    workspace: &Path,
    build_dir: PathBuf,
    build_subdir: PathBuf,
    srpm_path: &PathBuf,
) -> Result<(), anyhow::Error> {
    let mut mock_cmd = vec![
        "mock".to_string(),
        "--resultdir".to_string(),
        build_subdir.to_string_lossy().to_string(),
        srpm_path.to_string_lossy().to_string(),
    ];
    if !all_dependencies.is_empty() {
        let deps_dir = build_dir.join("deps");
        mock_cmd.push("--addrepo".to_string());
        mock_cmd.push(deps_dir.to_string_lossy().to_string());
    }
    for param in &source.build_params {
        mock_cmd.push(param.clone());
    }
    let mock_command = mock_cmd.join(" ");
    info!("Executing mock: {}", mock_command);
    let shell = Shell::new(workspace);
    shell.run_logged(&mock_command).await?;
    info!("✅ Successfully built with mock");
    Ok(())
}

async fn build_source_task(
    build_key: BuildKey,
    source: Source,
    all_dependencies: HashMap<SourceKey, BuildHash>,
    workspace: PathBuf,
    backend: BuilderBackend,
    target_os: Option<String>,
    direct_dependency_receivers: Vec<(SourceKey, mpsc::Receiver<bool>)>,
    direct_completion_senders: Vec<mpsc::Sender<bool>>,
) -> Result<()> {
    info!("🚀 Starting build task");

    // Wait for all dependencies to complete successfully
    for (dep_key, mut receiver) in direct_dependency_receivers {
        info!("⏳ Waiting for dependency {} to complete...", dep_key);
        match receiver.recv().await {
            Some(true) => {
                info!("✅ Dependency {} completed successfully", dep_key);
            }
            Some(false) => {
                error!("❌ Dependency {} failed to build", dep_key);
                // Notify all waiting tasks that this build failed
                for sender in direct_completion_senders {
                    let _ = sender.send(false).await;
                }
                anyhow::bail!("Dependency {} failed, cannot build", dep_key);
            }
            None => {
                error!("❌ Dependency {} channel closed unexpectedly", dep_key);
                // Notify all waiting tasks that this build failed
                for sender in direct_completion_senders {
                    let _ = sender.send(false).await;
                }
                anyhow::bail!(
                    "Dependency {} channel closed, cannot build {}",
                    dep_key,
                    build_key.source_key
                );
            }
        }
    }

    info!("🔨 All dependencies ready");

    // Use block_in_place to call the synchronous build_source function
    let build_result = build_source(
        &build_key,
        &source,
        &all_dependencies,
        &workspace,
        &backend,
        target_os.as_deref(),
    )
    .await;

    // Determine success/failure and notify all waiting tasks
    let success = match &build_result {
        Ok(()) => {
            info!("✅ Build completed successfully");
            true
        }
        Err(e) => {
            error!("❌ Build failed for {}", e);
            false
        }
    };

    // Notify all tasks waiting for this build to complete
    for sender in direct_completion_senders {
        if let Err(e) = sender.send(success).await {
            error!("Failed to notify completion: {}", e);
        }
    }

    if success {
        info!("🎉 Build task completed successfully");
    }

    build_result
}

fn compute_all_build_hashes(
    sources: &[SourceKey],
    spec_tree: &SpecTree,
    source_hashes: &SourceHashes,
) -> Result<HashMap<SourceKey, BuildHash>> {
    let mut build_hashes = HashMap::new();
    let mut visited = HashSet::new();
    let mut recursion_stack = HashSet::new();

    for source_key in sources {
        let _ = compute_build_hash_recursive(
            source_key,
            spec_tree,
            source_hashes,
            &mut build_hashes,
            &mut visited,
            &mut recursion_stack,
        )?;
    }

    Ok(build_hashes)
}

fn compute_build_hash_recursive(
    source_key: &SourceKey,
    spec_tree: &SpecTree,
    source_hashes: &SourceHashes,
    build_hashes: &mut HashMap<SourceKey, BuildHash>,
    visited: &mut HashSet<SourceKey>,
    recursion_stack: &mut HashSet<SourceKey>,
) -> Result<BuildHash> {
    // Check for cycles - if this source is already in the recursion stack
    if recursion_stack.contains(source_key) {
        anyhow::bail!(
            "Circular dependency detected during build hash calculation involving source: {}",
            source_key
        );
    }

    // If we've already processed this source completely, return the cached hash
    if let Some(existing_hash) = build_hashes.get(source_key) {
        return Ok(existing_hash.clone());
    }

    // Add to recursion stack to detect cycles
    recursion_stack.insert(source_key.clone());

    // Get the source definition
    let source = spec_tree
        .sources
        .get(source_key)
        .ok_or_else(|| anyhow::anyhow!("Source '{}' not found in spec tree", source_key))?;

    // First, recursively compute build hashes for all dependencies and collect them
    let mut dep_build_hashes = HashMap::new();
    for dep_key in &source.dependencies {
        // Parse the dependency to handle ~ prefix
        let dependency = Dependency::parse(dep_key.as_ref());
        let actual_dep_key = SourceKey::from(dependency.key().to_string());

        // Recursively compute the dependency's build hash
        let dep_build_hash = compute_build_hash_recursive(
            &actual_dep_key,
            spec_tree,
            source_hashes,
            build_hashes,
            visited,
            recursion_stack,
        )?;

        dep_build_hashes.insert(actual_dep_key, dep_build_hash);
    }

    // Now compute the build hash for this source
    let source_hash = source_hashes
        .hashes
        .get(source_key)
        .ok_or_else(|| anyhow::anyhow!("Source hash not found for source: {}", source_key))?;

    // Calculate the build hash
    let build_hash = calculate_build_hash(source_key, source, source_hash, &dep_build_hashes);
    build_hashes.insert(source_key.clone(), build_hash.clone());

    // Remove from recursion stack and mark as visited
    recursion_stack.remove(source_key);
    visited.insert(source_key.clone());

    Ok(build_hash)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    logging::start(&args.logging)?;

    setup_workspace(&args.workspace)?;

    let yaml_content = fs::read_to_string(&args.spec_file)?;
    let spec_tree: SpecTree = serde_yaml::from_str(&yaml_content)?;

    info!(
        "Successfully read YAML file with {} sources",
        spec_tree.sources.len()
    );

    // Verify root source exists
    if !spec_tree.sources.contains_key(&args.root_source) {
        anyhow::bail!("Root source '{}' not found in spec tree", args.root_source);
    }

    // Find all dependency pairs starting from the root source
    let dependency_pairs = find_all_dependency_pairs(&[args.root_source.clone()], &spec_tree)?;

    info!(
        "Found {} dependency relationships for root source '{}'",
        dependency_pairs.len(),
        args.root_source
    );

    // Log all dependency pairs for visibility
    for (source, dependency) in &dependency_pairs {
        debug!("Dependency: {} -> {}", source, dependency);
    }

    let mut all_sources = HashSet::new();
    all_sources.insert(args.root_source.clone());
    for (source, dependency) in &dependency_pairs {
        all_sources.insert(source.clone());
        all_sources.insert(dependency.clone());
    }

    let all_sources: Vec<SourceKey> = all_sources.into_iter().collect();
    info!(
        "Total sources to build: {} (including root and all dependencies)",
        all_sources.len()
    );

    // Calculate source hashes for all sources
    let source_hashes = get_source_hashes(&args, &spec_tree, &all_sources)?;
    info!(
        "Calculated source hashes for {} sources",
        source_hashes.hashes.len()
    );

    // Calculate build hashes for all sources using recursion
    let build_hashes = compute_all_build_hashes(&all_sources, &spec_tree, &source_hashes)?;
    info!("Calculated build hashes for {} sources", build_hashes.len());

    // Create all_dependencies mapping: HashMap<SourceKey, HashMap<SourceKey, BuildHash>>
    let mut all_dependencies_map: HashMap<SourceKey, HashMap<SourceKey, BuildHash>> =
        HashMap::new();

    for source_key in &all_sources {
        // Get all dependencies for this source
        let source_deps = resolve_dependencies(source_key, &spec_tree)?;

        // Create the dependency mapping with their build hashes
        let dependencies_with_hashes: HashMap<SourceKey, BuildHash> = source_deps
            .iter()
            .filter_map(|dep_key| {
                build_hashes
                    .get(dep_key)
                    .map(|hash| (dep_key.clone(), hash.clone()))
            })
            .collect();

        all_dependencies_map.insert(source_key.clone(), dependencies_with_hashes);

        debug!(
            "Source {} has {} dependencies: {:?}",
            source_key,
            source_deps.len(),
            source_deps
        );
    }

    info!(
        "Created dependency mappings for {} sources",
        all_dependencies_map.len()
    );

    // Log summary information
    for (source_key, deps) in &all_dependencies_map {
        info!("Source '{}' depends on {} sources", source_key, deps.len());
    }

    // Create channels for each dependency pair and organize by source
    let mut source_completion_senders: HashMap<SourceKey, Vec<mpsc::Sender<bool>>> = HashMap::new();
    let mut source_dependency_receivers: HashMap<
        SourceKey,
        Vec<(SourceKey, mpsc::Receiver<bool>)>,
    > = HashMap::new();

    // Initialize empty vectors for all sources
    for source_key in &all_sources {
        source_completion_senders.insert(source_key.clone(), Vec::new());
        source_dependency_receivers.insert(source_key.clone(), Vec::new());
    }

    // Create channels for each dependency pair
    for (dependent, dependency) in &dependency_pairs {
        let (tx, rx) = mpsc::channel::<bool>(1);

        // The dependency source gets the sender to notify when it completes
        source_completion_senders
            .get_mut(dependency)
            .unwrap()
            .push(tx);

        // The dependent source gets the receiver to wait for the dependency
        source_dependency_receivers
            .get_mut(dependent)
            .unwrap()
            .push((dependency.clone(), rx));

        info!(
            "Created channel for dependency pair: {} -> {}",
            dependency, dependent
        );
    }

    // For each source, create the direct dependency receivers and completion senders
    let mut source_tasks = Vec::new();

    for source_key in &all_sources {
        let source = spec_tree.sources.get(source_key).unwrap().clone();
        let source_build_hash = build_hashes.get(source_key).unwrap().clone();
        let source_deps = all_dependencies_map
            .get(source_key)
            .cloned()
            .unwrap_or_default();

        // Get dependency receivers for this source (to wait for dependencies)
        let direct_dependency_receivers = source_dependency_receivers
            .remove(source_key)
            .unwrap_or_default();
        // Get completion senders for this source (to notify dependents)
        let direct_completion_senders = source_completion_senders
            .remove(source_key)
            .unwrap_or_default();

        info!(
            "Source {} has {} dependency receivers and {} completion senders",
            source_key,
            direct_dependency_receivers.len(),
            direct_completion_senders.len()
        );

        // Spawn the build task
        let task_source_key = source_key.clone();
        let task_workspace = args.workspace.clone();
        let task_backend = args.backend.clone();
        let task_target_os = args.target_os.clone();

        let task = tokio::spawn(async move {
            let key = task_source_key.clone();
            let task_build_key = BuildKey::new(task_source_key, source_build_hash);
            build_source_task(
                task_build_key,
                source,
                source_deps,
                task_workspace,
                task_backend,
                task_target_os,
                direct_dependency_receivers,
                direct_completion_senders,
            )
            .instrument(span!(Level::INFO, "task", key = %key))
            .await
        });

        source_tasks.push((source_key.clone(), task));
    }

    info!("Spawned {} build tasks", source_tasks.len());

    // Identify leaf sources (sources that no one depends on)
    let mut dependency_sources: HashSet<SourceKey> = HashSet::new();
    for (_, dependency) in &dependency_pairs {
        dependency_sources.insert(dependency.clone());
    }

    let leaf_sources: Vec<SourceKey> = all_sources
        .iter()
        .filter(|source| !dependency_sources.contains(*source))
        .cloned()
        .collect();

    info!("Leaf sources (no one depends on them): {:?}", leaf_sources);

    // Wait for leaf sources to complete (or root source if it's specified and is a leaf)
    let sources_to_wait_for = if leaf_sources.contains(&args.root_source) {
        vec![args.root_source.clone()]
    } else {
        leaf_sources
    };

    info!("Waiting for sources to complete: {:?}", sources_to_wait_for);

    for (source_key, task) in source_tasks {
        if sources_to_wait_for.contains(&source_key) {
            match task.await {
                Ok(Ok(())) => {
                    info!("✅ Source '{}' completed successfully!", source_key);
                    if source_key == args.root_source {
                        break; // Root source completed, we're done
                    }
                }
                Ok(Err(e)) => {
                    anyhow::bail!("❌ Source '{}' failed: {}", source_key, e);
                }
                Err(e) => {
                    anyhow::bail!("❌ Source '{}' task panicked: {}", source_key, e);
                }
            }
        }
    }

    Ok(())
}
