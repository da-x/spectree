use crate::shell::Shell;
use anyhow::Result;
use std::{path::Path, process::Output};

pub fn get_builder_dockerfile_for_os(os: &str) -> Result<String> {
    match os {
        "epel10" => Ok(r#"FROM rockylinux:10

RUN dnf install -y 'dnf-command(config-manager)'
RUN dnf config-manager --set-enabled crb appstream extras

# Install EPEL repository
RUN dnf install -y epel-release

# Install build dependencies
RUN dnf install -y bash bzip2 cpio diffutils findutils gawk glibc-minimal-langpack grep gzip info patch redhat-rpm-config rocky-release rpm-build sed tar unzip util-linux which xz

#
# Not wanted for podman:
#
# Create build user and directories
# RUN useradd -m builder && \
#     mkdir -p /build/workspace && \
#    chown -R builder:builder /build
#
# Set up rpmbuild directories
# USER builder
# RUN rpmdev-setuptree
# WORKDIR /build/workspace
"#
        .to_string()),
        _ => anyhow::bail!("Unsupported OS: {}", os),
    }
}

pub async fn ensure_image(
    target: &str,
    dockerfile_content: &str,
    args: &str,
) -> anyhow::Result<Result<String, Output>> {
    let prefix = "spectree.ops/";
    let image_name = if !target.starts_with(prefix) {
        format!("{}{}", prefix, target)
    } else {
        target.to_owned()
    };

    // Check if image already exists
    let shell = Shell::new(Path::new("."));
    let check_result = shell
        .run_with_output(&format!("docker images -q {}", image_name))
        .await;

    match check_result {
        Ok(output) if !output.trim().is_empty() => {
            // Image exists, no need to build
            return Ok(Ok(image_name));
        }
        _ => {
            // Image doesn't exist or error checking, proceed with build
        }
    }

    let build_command = format!("docker build {args} --no-cache -t {} -", image_name);

    let output = shell
        .run_with_stdin_get_output(&build_command, &dockerfile_content)
        .await?;

    if !output.status.success() {
        return Ok(Err(output));
    }

    return Ok(Ok(image_name));
}
