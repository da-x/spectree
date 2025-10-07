use anyhow::{Context, Result};
use clap::Parser;
use nutype::nutype;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::time::Duration;
use std::{fs, path};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, span, Instrument, Level};

mod docker;
mod logging;
mod shell;
mod utils;

use shell::Shell;

use crate::utils::{
    check_git_clean, copy_dir_all, export_git_revision, get_git_revision, get_git_tree_hash,
};

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
        (Some("rocky"), Some(version)) if version.starts_with("9") => Ok("epel9".to_string()),
        (Some("rocky"), Some(version)) if version.starts_with("8") => Ok("epel8".to_string()),
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
    Copr,
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
            "copr" => Ok(BuilderBackend::Copr),
            _ => anyhow::bail!(
                "Invalid builder backend: {}. Valid options: mock, null, docker, copr",
                s
            ),
        }
    }
}

impl std::fmt::Display for BuilderBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuilderBackend::Mock => write!(f, "mock"),
            BuilderBackend::Null => write!(f, "null"),
            BuilderBackend::Docker => write!(f, "docker"),
            BuilderBackend::Copr => write!(f, "copr"),
        }
    }
}

impl BuilderBackend {
    pub fn is_remote(&self) -> bool {
        matches!(self, BuilderBackend::Copr)
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
        subpath: Option<String>,
        revision: Option<String>,
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
pub struct CoprBuildState {
    pub build_key: String, // Using string instead of BuildKey for serialization simplicity
    pub build_id: u64,
    pub status: CoprBuildStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub enum CoprBuildStatus {
    Submitted,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CoprStateFile {
    pub builds: BTreeMap<String, CoprBuildState>, // key is build_key.to_string()
}

impl CoprStateFile {
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read Copr state file: {}", path.display()))?;
            serde_yaml::from_str(&content)
                .with_context(|| format!("Failed to parse Copr state file: {}", path.display()))
        } else {
            Ok(Self {
                builds: Default::default(),
            })
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let content =
            serde_yaml::to_string(self).context("Failed to serialize Copr state to YAML")?;
        fs::write(path, content)
            .with_context(|| format!("Failed to write Copr state file: {}", path.display()))?;
        Ok(())
    }

    pub fn get_build_state(&self, build_key: &BuildKey) -> Option<&CoprBuildState> {
        self.builds.get(&build_key.to_string())
    }

    pub fn set_build_state(&mut self, build_key: &BuildKey, build_state: CoprBuildState) {
        self.builds.insert(build_key.to_string(), build_state);
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
    pub params: Vec<String>,
    #[serde(default)]
    pub network: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildInfo {
    pub source: Source,
    pub git_revision: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SpecTree {
    #[serde(flatten)]
    pub sources: HashMap<SourceKey, Source>,
}

#[derive(Parser, Clone)]
#[command(name = "spectree")]
#[command(about = "A tool for building dependent RPM packages from a YAML specification")]
struct Args {
    #[arg(help = "Path to the YAML specification file")]
    spec_file: PathBuf,

    #[arg(short, long, help = "Workspace directory for builds and Git clones")]
    workspace: PathBuf,

    #[arg(help = "Root sources to start building from (can specify multiple)")]
    root_sources: Vec<SourceKey>,

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

    #[arg(long, help = "Copr project name (required for Copr backend)")]
    copr_project: Option<String>,

    #[arg(
        long,
        help = "YAML file to store Copr build state mappings (required for Copr backend)"
    )]
    copr_state_file: Option<PathBuf>,

    #[arg(
        long,
        action = clap::ArgAction::Append,
        help = "Exclude chroot for Copr builds (can be specified multiple times)"
    )]
    exclude_chroot: Vec<String>,

    #[arg(
        long,
        help = "Regex pattern for source keys to assume are already built in Copr (skip building)"
    )]
    copr_assume_built: Option<String>,

    #[arg(
        long,
        help = "Debug mode: only prepare sources (rpmbuild -bp) and leave them for inspection. Build will fail intentionally."
    )]
    debug_prepare: bool,

    #[arg(
        long,
        help = "Output directory to copy build results (root sources and their dependencies)"
    )]
    output_dir: Option<PathBuf>,

    #[command(flatten)]
    logging: logging::LoggingArgs,
}

fn setup_workspace(workspace: &Path) -> Result<()> {
    fs::create_dir_all(&workspace).with_context(|| {
        format!(
            "Failed to create workspace directory: {}",
            workspace.display()
        )
    })?;
    fs::create_dir_all(workspace.join("sources")).with_context(|| {
        format!(
            "Failed to create sources directory: {}",
            workspace.join("sources").display()
        )
    })?;
    fs::create_dir_all(workspace.join("builds")).with_context(|| {
        format!(
            "Failed to create builds directory: {}",
            workspace.join("builds").display()
        )
    })?;
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
            .output()
            .with_context(|| {
                format!(
                    "Failed to execute git fetch in repo: {}",
                    repo_path.display()
                )
            })?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to fetch in repo {}: {}",
                repo_path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let output = Command::new("git")
            .args(&["reset", "--hard", "origin/HEAD"])
            .current_dir(&repo_path)
            .output()
            .with_context(|| {
                format!(
                    "Failed to execute git reset in repo: {}",
                    repo_path.display()
                )
            })?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to reset in repo {}: {}",
                repo_path.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    } else {
        info!("Cloning repo for {} from {}", key, url);
        let output = Command::new("git")
            .args(&["clone", url, &repo_path.to_string_lossy()])
            .output()
            .with_context(|| {
                format!(
                    "Failed to execute git clone from {} to {}",
                    url,
                    repo_path.display()
                )
            })?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to clone from {} to {}: {}",
                url,
                repo_path.display(),
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
            dep_hashes.push((dependency.key().to_string(), dep_hash.clone(), false));
        }
    }
    dep_hashes.sort_by(|(a, _, _), (b, _, _)| a.cmp(b));
    hasher.update(format!("{:?}", dep_hashes).as_bytes());

    hasher.update(format!("{:?}", source.params).as_bytes());
    BuildHash::new(format!("{:x}", hasher.finalize()))
}

impl Source {
    fn get_repo_path(&self, key: &SourceKey, workspace: &Path, update: bool) -> Result<PathBuf> {
        let repo_path = match &self.typ {
            SourceType::Git { url, path, .. } => {
                if let Some(path) = path {
                    let path = path.replace("${NAME}", key.as_ref());
                    path::absolute(&path)
                        .with_context(|| format!("Failed to get absolute path for: {}", path))?
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

    fn get_working_path(&self, key: &SourceKey, workspace: &Path, update: bool) -> Result<PathBuf> {
        match &self.typ {
            SourceType::Git {
                revision, subpath, ..
            } => {
                if let Some(revision) = revision {
                    // For specific revisions, export to a revision-specific directory
                    let source_repo_path = self.get_repo_path(key, workspace, update)?;

                    // Resolve the revision to its full commit hash
                    let output = Command::new("git")
                        .args(&["rev-parse", revision])
                        .current_dir(&source_repo_path)
                        .output()?;

                    if !output.status.success() {
                        anyhow::bail!(
                            "Failed to resolve git revision '{}' for source {}: {}",
                            revision,
                            key,
                            String::from_utf8_lossy(&output.stderr)
                        );
                    }

                    let full_revision = String::from_utf8(output.stdout)?.trim().to_string();
                    let export_key = format!("{}-{}", key.as_ref(), full_revision);
                    let export_path = workspace.join("sources").join(&export_key);

                    // Only export if the directory doesn't already exist
                    if !export_path.exists() {
                        info!("Exporting revision {} for source {}", revision, key);
                        let subpath_ref =
                            subpath.as_ref().map(|s| s.replace("${NAME}", key.as_ref()));
                        export_git_revision(
                            &source_repo_path,
                            revision,
                            &export_path,
                            subpath_ref.as_deref(),
                        )?;

                        // Run spectool -g on the exported sources if there's a spec file
                        self.run_spectool_on_exported_sources(&export_path)?;
                    }

                    Ok(export_path)
                } else {
                    // For HEAD/current revision, use the repo path directly
                    self.get_repo_path(key, workspace, update)
                }
            }
            _ => self.get_repo_path(key, workspace, update),
        }
    }

    fn run_spectool_on_exported_sources(&self, export_path: &Path) -> Result<()> {
        // Find spec files in the exported directory
        let spec_files: Vec<_> = std::fs::read_dir(export_path)?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.is_file() && path.extension()? == "spec" {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();

        if spec_files.is_empty() {
            debug!("No spec files found in exported directory, skipping spectool");
            return Ok(());
        }

        for spec_file in spec_files {
            info!("Running spectool -g on {}", spec_file.display());
            let output = Command::new("spectool")
                .args(&["-g", spec_file.to_str().unwrap()])
                .current_dir(export_path)
                .output();

            match output {
                Ok(output) if output.status.success() => {
                    info!("Successfully ran spectool -g on {}", spec_file.display());
                    if !output.stdout.is_empty() {
                        debug!(
                            "spectool output: {}",
                            String::from_utf8_lossy(&output.stdout)
                        );
                    }
                }
                Ok(output) => {
                    info!(
                        "spectool -g completed with warnings for {}: {}",
                        spec_file.display(),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                Err(e) => {
                    info!(
                        "spectool command not available or failed for {}: {}",
                        spec_file.display(),
                        e
                    );
                }
            }
        }

        Ok(())
    }
}

fn calc_source_hash(key: &SourceKey, source: &Source, workspace: &Path) -> Result<SourceHash> {
    // Check if using a specific revision
    let using_revision = match &source.typ {
        SourceType::Git { revision, .. } => revision.is_some(),
        _ => false,
    };

    let repo_path = source.get_repo_path(key, workspace, true)?;

    // Skip git clean check when using a specific revision
    if !using_revision {
        if !check_git_clean(&repo_path)? {
            anyhow::bail!("Git repository for {} has uncommitted changes", key);
        }
    }

    // For specific revisions, we need to use the revision instead of the tree hash
    let git_hash = match &source.typ {
        SourceType::Git {
            revision: Some(revision),
            subpath,
            ..
        } => {
            info!("Using specified revision '{}' for source {}", revision, key);
            // For specific revisions, we use the revision as part of the hash
            // But we still need to resolve it to a full commit hash for consistency
            let output = Command::new("git")
                .args(&["rev-parse", revision])
                .current_dir(&repo_path)
                .output()?;

            if !output.status.success() {
                anyhow::bail!(
                    "Failed to resolve git revision '{}' for source {}: {}",
                    revision,
                    key,
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            let full_revision = String::from_utf8(output.stdout)?.trim().to_string();

            // If there's a subpath, we need to get the tree hash for that specific path at the revision
            if let Some(subpath) = subpath {
                let subpath = subpath.replace("${NAME}", key.as_ref());
                let output = Command::new("git")
                    .args(&["rev-parse", &format!("{}:{}", full_revision, subpath)])
                    .current_dir(&repo_path)
                    .output()?;

                if !output.status.success() {
                    anyhow::bail!(
                        "Failed to get tree hash for subpath '{}' at revision '{}' for source {}: {}",
                        subpath,
                        revision,
                        key,
                        String::from_utf8_lossy(&output.stderr)
                    );
                }

                String::from_utf8(output.stdout)?.trim().to_string()
            } else {
                // Use the tree hash of the full revision
                let output = Command::new("git")
                    .args(&["rev-parse", &format!("{}^{{tree}}", full_revision)])
                    .current_dir(&repo_path)
                    .output()?;

                if !output.status.success() {
                    anyhow::bail!(
                        "Failed to get tree hash for revision '{}' for source {}: {}",
                        revision,
                        key,
                        String::from_utf8_lossy(&output.stderr)
                    );
                }

                String::from_utf8(output.stdout)?.trim().to_string()
            }
        }
        SourceType::Git { subpath, .. } => {
            // Original behavior for HEAD/current revision
            let subpath = subpath.as_ref().map(|s| s.replace("${NAME}", key.as_ref()));
            get_git_tree_hash(&repo_path, subpath.as_deref())?
        }
        _ => {
            anyhow::bail!("Non-git sources not supported in calc_source_hash");
        }
    };

    // Extract subpath for debug message
    let subpath = match &source.typ {
        SourceType::Git { subpath, .. } => {
            subpath.as_ref().map(|s| s.replace("${NAME}", key.as_ref()))
        }
        _ => None,
    };

    debug!(
        "Processed sources for source: {} (git: {}){}",
        key,
        git_hash,
        subpath
            .map(|s| format!(" subpath: {}", s))
            .unwrap_or_default()
    );

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

fn format_params_for_command(params: &[String], prefix: &str) -> String {
    if params.is_empty() {
        String::new()
    } else {
        let escaped_params: Vec<String> = params.iter().map(|p| format!("{:?}", p)).collect();
        format!("{}{}", prefix, escaped_params.join(" "))
    }
}

fn create_build_info_file(
    build_key: &BuildKey,
    source: &Source,
    workspace: &Path,
    build_dir: &Path,
) -> Result<()> {
    let git_revision = match &source.typ {
        SourceType::Git { revision, .. } => {
            // If a specific revision is provided, use that; otherwise get current revision
            if let Some(rev) = revision {
                Some(rev.clone())
            } else {
                let repo_path = source.get_repo_path(&build_key.source_key, workspace, false)?;
                match get_git_revision(&repo_path) {
                    Ok(revision) => Some(revision),
                    Err(e) => {
                        debug!("Failed to get git revision: {}", e);
                        None
                    }
                }
            }
        }
        _ => None,
    };

    let build_info = BuildInfo {
        source: source.clone(),
        git_revision,
    };

    let build_info_path = build_dir.join("build_info.yaml");
    let build_info_content = serde_yaml::to_string(&build_info)
        .with_context(|| "Failed to serialize build info to YAML")?;
    fs::write(&build_info_path, build_info_content).with_context(|| {
        format!(
            "Failed to write build info file: {}",
            build_info_path.display()
        )
    })?;
    debug!("Created build info file: {}", build_info_path.display());

    Ok(())
}

async fn build_source(
    build_key: &BuildKey,
    source: &Source,
    all_dependencies: &HashMap<SourceKey, BuildHash>,
    args: &Args,
    copr_state_mutex: &Mutex<()>,
) -> Result<()> {
    // For remote builds, check Copr state instead of local directories
    if args.backend.is_remote() {
        let copr_state_file = args
            .copr_state_file
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Copr state file is required for remote backend"))?;

        // Atomically check build state
        let existing_build_info = {
            let _guard = copr_state_mutex.lock().await;
            let state = CoprStateFile::load_or_create(copr_state_file)?;
            state.get_build_state(build_key).cloned()
        };

        if let Some(existing_build) = existing_build_info {
            match existing_build.status {
                CoprBuildStatus::Completed => {
                    info!(
                        "Remote build {} already completed for {}",
                        existing_build.build_id, build_key
                    );
                    return Ok(());
                }
                CoprBuildStatus::Failed => {
                    info!(
                        "Previous Copr build {} failed for {}, will retry",
                        existing_build.build_id, build_key
                    );
                    // Continue to generate SRPM and submit new build
                }
                CoprBuildStatus::Submitted | CoprBuildStatus::InProgress => {
                    info!(
                        "Copr build {} is in progress for {}, waiting...",
                        existing_build.build_id, build_key
                    );
                    // Wait for existing build (no SRPM generation needed)
                    wait_for_copr_build(
                        existing_build.build_id,
                        build_key,
                        copr_state_file,
                        copr_state_mutex,
                    )
                    .await?;
                    return Ok(());
                }
            }
        }
    } else {
        // For local builds, check if build directory already exists
        let build_dir_final = args
            .workspace
            .join("builds")
            .join(build_key.build_dir_name());
        let build_subdir_final = build_dir_final.join("build");
        if build_subdir_final.exists() {
            info!("Build already exists, skipping");
            return Ok(());
        }
    }

    let build_dir = args
        .workspace
        .join("builds")
        .join(format!("{}.tmp", build_key.build_dir_name()));

    let _ = fs::remove_dir_all(&build_dir);

    // Check if build already exists - if so, do nothing
    let build_subdir = build_dir.join("build");

    // Create build subdirectory
    fs::create_dir_all(&build_subdir).with_context(|| {
        format!(
            "Failed to create build subdirectory: {}",
            build_subdir.display()
        )
    })?;
    debug!("Created build subdirectory: {}", build_subdir.display());

    // Create build information file
    create_build_info_file(build_key, source, &args.workspace, &build_subdir)?;

    // If there are dependencies, create deps directory and hardlink them (skip for remote builds)
    if !all_dependencies.is_empty() && !args.backend.is_remote() {
        let deps_dir = build_dir.join("deps");
        fs::create_dir_all(&deps_dir)
            .with_context(|| format!("Failed to create deps directory: {}", deps_dir.display()))?;
        info!("Created deps directory: {}", deps_dir.display());

        // Hardlink each dependency's build directory
        for (dep_key, dep_hash) in all_dependencies.iter() {
            let dep_build_key = BuildKey::new(dep_key.clone(), dep_hash.clone());
            let dep_build_dir = args
                .workspace
                .join("builds")
                .join(dep_build_key.build_dir_name())
                .join("build");

            if !dep_build_dir.exists() {
                anyhow::bail!(
                    "Dependency build directory does not exist: {}",
                    dep_build_dir.display()
                );
            }

            let target_dir = deps_dir.join(dep_build_key.build_dir_name());
            copy_dir_all(&dep_build_dir, &target_dir).with_context(|| {
                format!(
                    "Failed to copy dependency from {} to {}",
                    dep_build_dir.display(),
                    target_dir.display()
                )
            })?;
            debug!("Hardlinked dependency {} to deps directory", dep_key);
        }

        // Run createrepo_c to create repository metadata (skip for Docker backend)
        if args.backend != BuilderBackend::Docker {
            let shell = Shell::new(&deps_dir);
            shell
                .run_with_output("createrepo_c .")
                .await
                .context("Failed to create repository metadata with createrepo_c")?;
            info!("Created repository metadata in deps directory");
        }
    }

    // Get source working path (exported revision if specified, or repo path)
    let repo_path = source.get_working_path(&build_key.source_key, &args.workspace, false)?;

    // Extract subpath from source type if it's a Git source
    let subpath = match &source.typ {
        SourceType::Git { subpath, .. } => subpath
            .as_ref()
            .map(|s| s.replace("${NAME}", build_key.source_key.as_ref())),
        _ => None,
    };
    let subpath = subpath.as_deref();

    // Determine the working directory for fedpkg
    let fedpkg_working_dir = if let Some(subpath) = subpath {
        let subpath_dir = repo_path.join(subpath);
        if !subpath_dir.exists() {
            anyhow::bail!(
                "Subpath '{}' does not exist in repository at {}",
                subpath,
                repo_path.display()
            );
        }
        if !subpath_dir.is_dir() {
            anyhow::bail!(
                "Subpath '{}' is not a directory in repository at {}",
                subpath,
                repo_path.display()
            );
        }
        subpath_dir
    } else {
        repo_path
    };

    let srpm_path = generate_srpm(
        build_key,
        source,
        args.target_os.as_deref(),
        &build_dir,
        subpath,
        "srpm",
        fedpkg_working_dir,
        false, // use_rpmbuild = false for regular fedpkg generation
    )
    .await?;

    // Build command based on backend
    match &args.backend {
        BuilderBackend::Mock => {
            build_with_mock(
                source,
                all_dependencies,
                &args.workspace,
                build_dir.clone(),
                build_subdir,
                &srpm_path,
            )
            .await?;
        }
        BuilderBackend::Null => {
            info!("🚫 Null backend");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        BuilderBackend::Docker => {
            build_under_docker(
                &args.workspace,
                args.target_os.as_deref(),
                build_dir.clone(),
                &source.params,
                args.debug_prepare,
                source.network,
            )
            .await
            .with_context(|| format!("Docker build failed for {}", build_key))?;
        }
        BuilderBackend::Copr => {
            let copr_project = args
                .copr_project
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Copr project name is required for Copr backend"))?;
            let copr_state_file = args
                .copr_state_file
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Copr state file is required for Copr backend"))?;

            // If we reach here, we need to submit a new build (state already checked earlier)
            build_with_copr(
                build_key,
                source,
                &srpm_path,
                copr_project,
                &args.exclude_chroot,
                copr_state_file,
                copr_state_mutex,
                &build_dir,
                args.target_os.as_deref(),
            )
            .await?;
        }
    }

    // For remote builds, we don't need to rename directories since builds happen remotely
    if !args.backend.is_remote() {
        let build_dir_final = args
            .workspace
            .join("builds")
            .join(build_key.build_dir_name());
        std::fs::rename(&build_dir, &build_dir_final).with_context(|| {
            format!(
                "Failed to rename build directory from {} to {}",
                build_dir.display(),
                build_dir_final.display()
            )
        })?;
    }

    Ok(())
}

async fn generate_srpm(
    build_key: &BuildKey,
    source: &Source,
    target_os: Option<&str>,
    build_dir: &PathBuf,
    subpath: Option<&str>,
    dirname: &str,
    fedpkg_working_dir: PathBuf,
    use_rpmbuild: bool,
) -> Result<PathBuf, anyhow::Error> {
    let base_os = match target_os {
        Some(os) => os.to_string(),
        None => get_base_os()?,
    };

    // Check for RHEL Git packaging mode (SOURCES subdirectory exists)
    let sources_dir = fedpkg_working_dir.join("SOURCES");
    let specs_dir = fedpkg_working_dir.join("SPECS");
    let is_rhel_packaging = sources_dir.exists();

    let fedpkg_defines = if is_rhel_packaging {
        info!("Detected RHEL Git packaging mode (SOURCES directory found)");
        format!(
            " --define \"_sourcedir {}\" --define \"_specdir {}\"",
            sources_dir.display(),
            specs_dir.display()
        )
    } else {
        String::new()
    };

    // Build the params string for fedpkg srpm (pass as extra args after --)
    let fedpkg_params = format_params_for_command(&source.params, " -- ");

    let shell = Shell::new(&fedpkg_working_dir);
    let build_srpm_dir = build_dir.join(dirname);
    let build_srpm_dir_disp = build_srpm_dir.display();

    if use_rpmbuild {
        // Use rpmbuild -bs directly (for repacking with baked parameters)
        info!("Generating SRPM using rpmbuild -bs");

        // Find the spec file
        let specs_dir = fedpkg_working_dir.join("SPECS");
        let spec_files: Vec<_> = std::fs::read_dir(&specs_dir)
            .with_context(|| format!("Failed to read SPECS directory: {}", specs_dir.display()))?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.extension()? == "spec" {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();

        if spec_files.is_empty() {
            anyhow::bail!("No spec file found for rpmbuild");
        }

        if spec_files.len() > 1 {
            anyhow::bail!("Multiple spec files found for rpmbuild: {:?}", spec_files);
        }

        let spec_file = &spec_files[0];

        shell
            .run_with_output(&format!(
                "rpmbuild -bs --define \"_topdir {}\" --define \"_srcrpmdir {}\" \"{}\"",
                fedpkg_working_dir.display(),
                build_srpm_dir_disp,
                spec_file.display()
            ))
            .await
            .with_context(|| {
                format!(
                    "Failed to generate SRPM with rpmbuild for {}",
                    build_key.source_key
                )
            })?;
    } else {
        // Use fedpkg srpm (original behavior)
        info!(
            "Generating source RPM using fedpkg{}{}",
            subpath
                .map(|s| format!(" from subpath '{}'", s))
                .unwrap_or_default(),
            if is_rhel_packaging {
                " (RHEL mode)"
            } else {
                ""
            }
        );

        shell
            .run_with_output(&format!(
                "fedpkg --release {base_os} srpm --define \"_srcrpmdir {build_srpm_dir_disp}\"{}{}",
                fedpkg_defines, fedpkg_params
            ))
            .await
            .with_context(|| {
                format!(
                    "Failed to generate SRPM with fedpkg for {}",
                    build_key.source_key
                )
            })?;
    }

    // Find the generated SRPM file
    let srpm_files: Vec<_> = std::fs::read_dir(&build_srpm_dir)
        .with_context(|| {
            format!(
                "Failed to read SRPM directory: {}",
                build_srpm_dir.display()
            )
        })?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? == "rpm" && path.file_name()?.to_str()?.ends_with(".src.rpm") {
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
    Ok(srpm_path.clone())
}

async fn build_under_docker(
    workspace: &Path,
    target_os: Option<&str>,
    build_dir: PathBuf,
    params: &[String],
    debug_prepare: bool,
    network_enabled: bool,
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

    let shell = Shell::new(workspace)
        .with_image(&image)
        .with_mount(
            &build_dir.to_string_lossy().as_ref().to_owned(),
            "/workspace",
        )
        .with_network(network_enabled);

    // Build the params string for rpmbuild
    let params_str = format_params_for_command(params, " ");

    let missing_deps = shell
        .run_with_output(&format!(
            r#"
rpm -D "_topdir /workspace/build" -i /workspace/srpm/*.src.rpm

list-missing-deps() {{
    local param="-br"
    if ! rpmbuild -br 2>/dev/null ; then
        param="-bp"
    fi

    (rpmbuild ${{param}} "-D _topdir /workspace/build"{} /workspace/build/SPECS/*.spec 2>&1 || true) \
        | (grep -v ^error: || true) \
        | grep -E '([^ ]*) is needed by [^ ]+$' \
        | sed -E 's/[\t]/ /g' \
        | sed -E 's/ +(.*) is needed by [^ ]+$/\1/g'
}}

# >&2
list-missing-deps
        "#,
            params_str
        ))
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
        let deps_image = format!("{}:{:x}", image, hasher.finalize());
        let dockerfile = if dep_repo {
            format!(
                r#"FROM {image}
COPY --from=deps / /deps
RUN createrepo_c /deps
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

    let shell = Shell::new(workspace)
        .with_image(&image)
        .with_mount(
            &build_dir.to_string_lossy().as_ref().to_owned(),
            "/workspace",
        )
        .with_network(network_enabled);

    if debug_prepare {
        info!("🔍 Debug mode: Running rpmbuild -bp (prepare only)");
        shell
            .run_logged(&format!(
                r#"
rpmbuild -bp -D "_topdir /workspace/build"{} /workspace/build/SPECS/*.spec
            "#,
                params_str
            ))
            .await
            .context("Failed to prepare sources with rpmbuild -bp")?;

        // Print the prepared source path
        let build_sources_path = build_dir.join("build/BUILD");
        info!(
            "📁 Prepared sources available at: {}",
            build_sources_path.display()
        );
        info!("🔍 You can inspect the prepared sources by examining the BUILD directory");
        info!("💡 The sources are left in the workspace for debugging purposes");

        // Intentionally fail the build as requested
        anyhow::bail!(
            "Build intentionally stopped after prepare phase for debugging (--debug-prepare mode)"
        );
    } else {
        shell
            .run_logged(&format!(
                r#"
rpmbuild -ba -D "_topdir /workspace/build"{} /workspace/build/SPECS/*.spec
            "#,
                params_str
            ))
            .await?;
    }

    Ok(())
}

async fn repack_srpm_with_params(
    build_key: &BuildKey,
    source: &Source,
    srpm_path: &PathBuf,
    build_dir: &PathBuf,
    target_os: Option<&str>,
) -> Result<PathBuf> {
    let repack_dir = build_dir.join("repack");

    // Remove existing repack directory if it exists
    if repack_dir.exists() {
        fs::remove_dir_all(&repack_dir).with_context(|| {
            format!(
                "Failed to remove existing repack directory: {}",
                repack_dir.display()
            )
        })?;
    }

    // Create repack directory
    fs::create_dir_all(&repack_dir).with_context(|| {
        format!(
            "Failed to create repack directory: {}",
            repack_dir.display()
        )
    })?;

    info!("📦 Extracting SRPM to repack directory");

    // Extract SRPM to repack directory using rpm -i
    let shell = Shell::new(&repack_dir);
    shell
        .run_with_output(&format!(
            "rpm -i --define \"_topdir {}\" \"{}\"",
            repack_dir.display(),
            srpm_path.display()
        ))
        .await
        .with_context(|| format!("Failed to extract SRPM: {}", srpm_path.display()))?;

    // Find and edit the spec file
    let specs_dir = repack_dir.join("SPECS");
    let spec_files: Vec<_> = std::fs::read_dir(&specs_dir)
        .with_context(|| format!("Failed to read SPECS directory: {}", specs_dir.display()))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? == "spec" {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    if spec_files.is_empty() {
        anyhow::bail!("No spec file found in extracted SRPM");
    }

    if spec_files.len() > 1 {
        anyhow::bail!(
            "Multiple spec files found in extracted SRPM: {:?}",
            spec_files
        );
    }

    let spec_file = &spec_files[0];
    info!("📝 Editing spec file: {}", spec_file.display());

    // Read and modify spec file
    let spec_content = fs::read_to_string(spec_file)
        .with_context(|| format!("Failed to read spec file: {}", spec_file.display()))?;

    let modified_spec_content = modify_spec_for_params(&spec_content, &source.params)?;

    // Write modified spec file
    fs::write(spec_file, modified_spec_content).with_context(|| {
        format!(
            "Failed to write modified spec file: {}",
            spec_file.display()
        )
    })?;

    info!("🔧 Repacking SRPM with modified spec");

    // Repack the SRPM using generate_srpm with rpmbuild
    let repacked_srpm_path = generate_srpm(
        build_key,
        source,
        target_os,
        build_dir,
        None, // subpath = None for repack
        "srpm-params",
        repack_dir.clone(),
        true, // use_rpmbuild = true for repacking with baked parameters
    )
    .await?;

    // Clean up repack directory
    fs::remove_dir_all(&repack_dir).with_context(|| {
        format!(
            "Failed to remove repack directory: {}",
            repack_dir.display()
        )
    })?;

    info!("✅ Successfully repacked SRPM with parameters");
    Ok(repacked_srpm_path)
}

fn modify_spec_for_params(spec_content: &str, params: &[String]) -> Result<String> {
    let lines: Vec<&str> = spec_content.lines().collect();
    let mut modified_lines = Vec::new();

    // Build parameter maps for features to enable/disable and defines to set
    let mut with_features = HashSet::new();
    let mut without_features = HashSet::new();
    let mut defines = HashMap::new();

    let mut i = 0;
    while i < params.len() {
        if params[i] == "--with" && i + 1 < params.len() {
            with_features.insert(params[i + 1].clone());
            i += 2;
        } else if params[i] == "--without" && i + 1 < params.len() {
            without_features.insert(params[i + 1].clone());
            i += 2;
        } else if (params[i] == "--define" || params[i] == "-D") && i + 1 < params.len() {
            // Parse define parameter: "name value"
            let define_str = &params[i + 1];
            if let Some(space_pos) = define_str.find(' ') {
                let name = define_str[..space_pos].trim().to_string();
                let value = define_str[space_pos + 1..].trim().to_string();
                defines.insert(name, value);
            } else {
                // No value provided, set to empty string
                defines.insert(define_str.trim().to_string(), String::new());
            }
            i += 2;
        } else {
            // Skip other parameters
            i += 1;
        }
    }

    // Compile regex patterns for bcond directives and %global definitions
    let bcond_with_regex = Regex::new(r"^(%bcond_with)[\t ]+([^\t ]+)[\t ]*(.*)")
        .context("Failed to compile bcond_with regex")?;
    let bcond_without_regex = Regex::new(r"^(%bcond_without)[\t ]+([^\t ]+)[\t ]*(.*)")
        .context("Failed to compile bcond_without regex")?;
    let global_regex = Regex::new(r"^(%global)[\t ]+([^\t ]+)[\t ]+(.*)")
        .context("Failed to compile global regex")?;

    // Process each line
    for line in lines {
        let mut modified_line = line.to_string();

        // Check for %bcond_with patterns
        if let Some(captures) = bcond_with_regex.captures(line) {
            let feature = captures.get(2).unwrap().as_str();
            let trailing = captures.get(3).map(|m| m.as_str()).unwrap_or("");

            if with_features.contains(feature) {
                info!(
                    "🔄 Changing %bcond_with {} to %bcond_without {}",
                    feature, feature
                );
                // Reconstruct the line with %bcond_without
                if trailing.is_empty() {
                    modified_line = format!("%bcond_without {}", feature);
                } else {
                    modified_line = format!("%bcond_without {} {}", feature, trailing);
                }
            }
        }
        // Check for %bcond_without patterns
        else if let Some(captures) = bcond_without_regex.captures(line) {
            let feature = captures.get(2).unwrap().as_str();
            let trailing = captures.get(3).map(|m| m.as_str()).unwrap_or("");

            if without_features.contains(feature) {
                info!(
                    "🔄 Changing %bcond_without {} to %bcond_with {}",
                    feature, feature
                );
                // Reconstruct the line with %bcond_with
                if trailing.is_empty() {
                    modified_line = format!("%bcond_with {}", feature);
                } else {
                    modified_line = format!("%bcond_with {} {}", feature, trailing);
                }
            }
        }
        // Check for %global patterns
        else if let Some(captures) = global_regex.captures(line) {
            let var_name = captures.get(2).unwrap().as_str();

            if let Some(new_value) = defines.get(var_name) {
                info!(
                    "🔄 Replacing %global {} with new value: {}",
                    var_name, new_value
                );
                modified_line = format!("%global {} {}", var_name, new_value);
            }
        }

        modified_lines.push(modified_line);
    }

    Ok(modified_lines.join("\n"))
}

async fn build_with_copr(
    build_key: &BuildKey,
    source: &Source,
    srpm_path: &PathBuf,
    copr_project: &str,
    exclude_chroots: &[String],
    copr_state_file: &Path,
    state_mutex: &Mutex<()>,
    build_dir: &PathBuf,
    target_os: Option<&str>,
) -> Result<()> {
    // Repack SRPM with baked-in build parameters for Copr
    let final_srpm_path = if !source.params.is_empty() {
        info!("🔄 Repacking SRPM with build parameters for Copr");
        repack_srpm_with_params(build_key, source, srpm_path, build_dir, target_os).await?
    } else {
        srpm_path.clone()
    };

    // Submit new build
    info!("Submitting Copr build for {}", build_key);
    let mut copr_cmd = vec![
        "copr".to_string(),
        "build".to_string(),
        "--nowait".to_string(),
        copr_project.to_string(),
        final_srpm_path.to_string_lossy().to_string(),
    ];

    // Add exclude-chroot arguments
    for chroot in exclude_chroots {
        copr_cmd.push("--exclude-chroot".to_string());
        copr_cmd.push(chroot.clone());
    }

    // Add network flag if network access is enabled
    if source.network {
        copr_cmd.push("--enable-net".to_string());
        copr_cmd.push("on".to_string());
    }

    let copr_command = copr_cmd.join(" ");
    info!("Executing Copr command: {}", copr_command);

    let current_dir = std::env::current_dir().context("Failed to get current working directory")?;
    let shell = Shell::new(current_dir.as_path());
    let output = shell
        .run_with_output(&copr_command)
        .await
        .with_context(|| format!("Failed to execute Copr build command: {}", copr_command))?;

    // Parse build ID from output
    let build_id = extract_copr_build_id(&output)?;
    info!("Copr build submitted with ID: {}", build_id);

    // Atomically save build state
    {
        let _guard = state_mutex.lock().await;
        let mut state = CoprStateFile::load_or_create(copr_state_file)?;
        let build_state = CoprBuildState {
            build_key: build_key.to_string(),
            build_id,
            status: CoprBuildStatus::Submitted,
        };
        state.set_build_state(build_key, build_state);
        state.save(copr_state_file)?;
    }

    // Wait for build completion
    wait_for_copr_build(build_id, build_key, copr_state_file, state_mutex).await
}

async fn wait_for_copr_build(
    build_id: u64,
    build_key: &BuildKey,
    copr_state_file: &Path,
    state_mutex: &Mutex<()>,
) -> Result<()> {
    info!("Waiting for Copr build {} to complete", build_id);

    // Atomically update status to InProgress
    {
        let _guard = state_mutex.lock().await;
        let mut state = CoprStateFile::load_or_create(copr_state_file)?;
        if let Some(build_state) = state.builds.get_mut(&build_key.to_string()) {
            build_state.status = CoprBuildStatus::InProgress;
            state.save(copr_state_file)?;
        }
    }

    let watch_command = format!("copr watch-build {}", build_id);
    let current_dir = std::env::current_dir().context("Failed to get current working directory")?;
    let shell = Shell::new(current_dir.as_path());

    match shell
        .run_with_output(&watch_command)
        .await
        .with_context(|| format!("Failed to execute Copr watch command: {}", watch_command))
    {
        Ok(_) => {
            info!("✅ Copr build {} completed successfully", build_id);
            // Atomically update status to Completed
            {
                let _guard = state_mutex.lock().await;
                let mut state = CoprStateFile::load_or_create(copr_state_file)?;
                if let Some(build_state) = state.builds.get_mut(&build_key.to_string()) {
                    build_state.status = CoprBuildStatus::Completed;
                    state.save(copr_state_file)?;
                }
            }
            Ok(())
        }
        Err(e) => {
            error!("❌ Copr build {} failed: {}", build_id, e);
            // Atomically update status to Failed
            {
                let _guard = state_mutex.lock().await;
                let mut state = CoprStateFile::load_or_create(copr_state_file)?;
                if let Some(build_state) = state.builds.get_mut(&build_key.to_string()) {
                    build_state.status = CoprBuildStatus::Failed;
                    state.save(copr_state_file)?;
                }
            }
            Err(e)
        }
    }
}

fn extract_copr_build_id(output: &str) -> Result<u64> {
    for line in output.lines() {
        if line.starts_with("Created builds: ") {
            let id_str = line.strip_prefix("Created builds: ").unwrap().trim();
            return id_str
                .parse::<u64>()
                .map_err(|e| anyhow::anyhow!("Failed to parse build ID '{}': {}", id_str, e));
        }
    }
    anyhow::bail!("No 'Created builds:' line found in Copr output");
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
    for param in &source.params {
        mock_cmd.push(param.clone());
    }
    let mock_command = mock_cmd.join(" ");
    info!("Executing mock: {}", mock_command);
    let shell = Shell::new(workspace);
    shell
        .run_logged(&mock_command)
        .await
        .with_context(|| format!("Failed to execute mock build command: {}", mock_command))?;
    info!("✅ Successfully built with mock");
    Ok(())
}

async fn build_source_task(
    build_key: BuildKey,
    source: Source,
    all_dependencies: HashMap<SourceKey, BuildHash>,
    args: Args,
    copr_state_mutex: std::sync::Arc<Mutex<()>>,
    direct_dependency_receivers: Vec<(SourceKey, mpsc::Receiver<bool>)>,
    direct_completion_senders: Vec<mpsc::Sender<bool>>,
) -> Result<()> {
    info!("🚀 Starting build task");

    // Check if this source should be skipped based on copr_assume_built regex
    if let Some(pattern) = &args.copr_assume_built {
        if args.backend.is_remote() {
            let regex = Regex::new(pattern).with_context(|| {
                format!("Invalid regex pattern for copr_assume_built: {}", pattern)
            })?;

            if regex.is_match(build_key.source_key.as_ref()) {
                info!(
                    "⏭️  Skipping build for {} (matches copr_assume_built pattern: {})",
                    build_key.source_key, pattern
                );

                // Notify all waiting tasks that this build is "complete"
                for sender in direct_completion_senders {
                    if let Err(e) = sender.send(true).await {
                        error!("Failed to notify completion: {}", e);
                    }
                }
                return Ok(());
            }
        }
    }

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
        &args,
        &*copr_state_mutex,
    )
    .await;

    // Determine success/failure and notify all waiting tasks
    let success = match &build_result {
        Ok(()) => {
            info!("✅ Build completed successfully");
            true
        }
        Err(e) => {
            error!("❌ Build failed, error chain:");
            let mut index = 0;
            e.chain().for_each(|cause| {
                tracing::error!("  [{}]: {}", index, cause);
                index += 1;
            });

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

fn copy_build_results_to_output_dir(
    output_dir: &Path,
    root_sources: &[SourceKey],
    all_dependencies_map: &HashMap<SourceKey, HashMap<SourceKey, BuildHash>>,
    build_hashes: &HashMap<SourceKey, BuildHash>,
    workspace: &Path,
) -> Result<()> {
    info!(
        "Copying build results to output directory: {}",
        output_dir.display()
    );

    // Create output directory if it doesn't exist
    std::fs::create_dir_all(output_dir).with_context(|| {
        format!(
            "Failed to create output directory: {}",
            output_dir.display()
        )
    })?;

    // Collect all sources to copy: root sources and their dependencies
    let mut sources_to_copy = HashSet::new();

    // Add all root sources
    for root_source in root_sources {
        sources_to_copy.insert(root_source.clone());

        // Add all dependencies of this root source
        if let Some(dependencies) = all_dependencies_map.get(root_source) {
            for dep_key in dependencies.keys() {
                sources_to_copy.insert(dep_key.clone());
            }
        }
    }

    // Copy each source's build directory
    for source_key in sources_to_copy {
        let build_hash = build_hashes.get(&source_key).unwrap();
        let build_key = BuildKey::new(source_key.clone(), build_hash.clone());
        let source_build_dir = workspace.join("builds").join(build_key.build_dir_name());

        if source_build_dir.exists() {
            let dest_dir = output_dir.join(build_key.build_dir_name());
            info!(
                "Copying {} to {}",
                source_build_dir.display(),
                dest_dir.display()
            );

            // Copy the entire build directory
            copy_dir_all(&source_build_dir, &dest_dir).with_context(|| {
                format!(
                    "Failed to copy build directory from {} to {}",
                    source_build_dir.display(),
                    dest_dir.display()
                )
            })?;
        } else {
            info!(
                "Build directory does not exist (remote build?): {}",
                source_build_dir.display()
            );
        }
    }

    info!("✅ Build results copied to output directory successfully!");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Initialize logging
    logging::start(&args.logging)?;

    // Always create the mutex (simpler than conditional logic)
    let copr_state_mutex = std::sync::Arc::new(Mutex::new(()));

    // Validate Copr arguments if using Copr backend
    if args.backend == BuilderBackend::Copr {
        if args.copr_project.is_none() {
            anyhow::bail!("--copr-project is required when using Copr backend");
        }
        if args.copr_state_file.is_none() {
            anyhow::bail!("--copr-state-file is required when using Copr backend");
        }
    }

    // Validate debug_prepare is only used with Docker backend
    if args.debug_prepare && args.backend != BuilderBackend::Docker {
        anyhow::bail!("--debug-prepare can only be used with Docker backend");
    }

    setup_workspace(&args.workspace)?;

    let yaml_content = fs::read_to_string(&args.spec_file)
        .with_context(|| format!("Failed to read spec file: {}", args.spec_file.display()))?;
    let spec_tree: SpecTree = serde_yaml::from_str(&yaml_content)
        .with_context(|| format!("Failed to parse spec file: {}", args.spec_file.display()))?;

    info!(
        "Successfully read YAML file with {} sources",
        spec_tree.sources.len()
    );

    // Verify all root sources exist
    if args.root_sources.is_empty() {
        anyhow::bail!("At least one root source must be specified");
    }

    for root_source in &args.root_sources {
        if !spec_tree.sources.contains_key(root_source) {
            anyhow::bail!("Root source '{}' not found in spec tree", root_source);
        }
    }

    // Find all dependency pairs starting from the root sources
    let dependency_pairs = find_all_dependency_pairs(&args.root_sources, &spec_tree)?;

    info!(
        "Found {} dependency relationships for {} root sources",
        dependency_pairs.len(),
        args.root_sources.len()
    );

    // Log all dependency pairs for visibility
    for (source, dependency) in &dependency_pairs {
        debug!("Dependency: {} -> {}", source, dependency);
    }

    let mut all_sources = HashSet::new();
    for root_source in &args.root_sources {
        all_sources.insert(root_source.clone());
    }
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
        let task_args = args.clone();
        let task_copr_state_mutex = copr_state_mutex.clone();

        let task = tokio::spawn(async move {
            let key = task_source_key.clone();
            let task_build_key = BuildKey::new(task_source_key, source_build_hash);
            build_source_task(
                task_build_key,
                source,
                source_deps,
                task_args,
                task_copr_state_mutex,
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

    // Wait for leaf sources to complete (or root sources if they are specified and are leaves)
    let mut sources_to_wait_for = Vec::new();
    for root_source in &args.root_sources {
        if leaf_sources.contains(root_source) {
            sources_to_wait_for.push(root_source.clone());
        }
    }

    // If none of the root sources are leaves, wait for all leaf sources
    if sources_to_wait_for.is_empty() {
        sources_to_wait_for = leaf_sources;
    }

    info!("Waiting for sources to complete: {:?}", sources_to_wait_for);

    let mut completed_root_sources = HashSet::new();
    for (source_key, task) in source_tasks {
        if sources_to_wait_for.contains(&source_key) {
            match task.await {
                Ok(Ok(())) => {
                    info!("✅ Source '{}' completed successfully!", source_key);
                    if args.root_sources.contains(&source_key) {
                        completed_root_sources.insert(source_key.clone());
                        // Check if all root sources are completed
                        if completed_root_sources.len() == args.root_sources.len() {
                            break; // All root sources completed, we're done
                        }
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

    // Copy build results to output directory if specified
    if let Some(output_dir) = &args.output_dir {
        copy_build_results_to_output_dir(
            output_dir,
            &args.root_sources,
            &all_dependencies_map,
            &build_hashes,
            &args.workspace,
        )?;
    }

    Ok(())
}
