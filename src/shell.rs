use anyhow::Result;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command as TokioCommand;
use tracing::{debug, info, Instrument};

pub struct Shell<'a> {
    working_dir: &'a Path,
    docker_image: Option<String>,
    mount_binds: Vec<String>,
}

impl<'a> Shell<'a> {
    pub fn new(working_dir: &'a Path) -> Self {
        Shell {
            working_dir,
            docker_image: None,
            mount_binds: Vec::new(),
        }
    }

    #[allow(unused)]
    pub fn with_image(mut self, image: &str) -> Self {
        self.docker_image = Some(image.to_string());
        self
    }

    #[allow(unused)]
    pub fn with_mount(mut self, host_path: &str, container_path: &str) -> Self {
        self.mount_binds
            .push(format!("{}:{}", host_path, container_path));
        self
    }

    fn build_command(&self, command: &str) -> Command {
        let cmd = match &self.docker_image {
            Some(image) => {
                let working_dir_str = self.working_dir.to_string_lossy();
                let mut cmd = Command::new("docker");

                let mut args = vec![
                    "run".to_string(),
                    "--rm".to_string(),
                    "-v".to_string(),
                    format!("{}:{}", working_dir_str, working_dir_str),
                ];

                // Add additional mount binds
                for mount_bind in &self.mount_binds {
                    args.push("-v".to_string());
                    args.push(mount_bind.clone());
                }

                args.extend_from_slice(&[
                    "-w".to_string(),
                    working_dir_str.to_string(),
                    image.clone(),
                    "bash".to_string(),
                    "-c".to_string(),
                    command.to_string(),
                ]);

                cmd.args(&args);
                cmd
            }
            None => {
                let mut cmd = Command::new("bash");
                cmd.args(&["-c", command]).current_dir(self.working_dir);
                cmd
            }
        };

        debug!("{:?}", cmd);

        cmd
    }

    fn build_tokio_command(&self, command: &str) -> TokioCommand {
        let cmd = match &self.docker_image {
            Some(image) => {
                let working_dir_str = self.working_dir.to_string_lossy();
                let mut cmd = TokioCommand::new("docker");

                let mut args = vec![
                    "run".to_string(),
                    "--rm".to_string(),
                    "-v".to_string(),
                    format!("{}:{}", working_dir_str, working_dir_str),
                ];

                // Add additional mount binds
                for mount_bind in &self.mount_binds {
                    args.push("-v".to_string());
                    args.push(mount_bind.clone());
                }

                args.extend_from_slice(&[
                    "-w".to_string(),
                    working_dir_str.to_string(),
                    image.clone(),
                    "bash".to_string(),
                    "-c".to_string(),
                    command.to_string(),
                ]);

                cmd.args(&args);
                cmd
            }
            None => {
                let mut cmd = TokioCommand::new("bash");
                cmd.args(&["-c", command]).current_dir(self.working_dir);
                cmd
            }
        };

        debug!("{:?}", cmd);

        cmd
    }

    #[allow(unused)]
    pub async fn run_logged(&self, command: &str) -> Result<()> {
        let mut child = self
            .build_tokio_command(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn '{}': {}", command, e))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to get stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to get stderr"))?;

        let stdout_reader = BufReader::new(stdout);
        let stderr_reader = BufReader::new(stderr);

        // Spawn tasks to read stdout and stderr concurrently
        let stdout_task = tokio::spawn(
            async move {
                let mut lines = stdout_reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    info!("{}", line);
                }
            }
            .in_current_span(),
        );

        let stderr_task = tokio::spawn(
            async move {
                let mut lines = stderr_reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!("{}", line);
                }
            }
            .in_current_span(),
        );

        // Wait for both tasks to complete and the process to finish
        let (stdout_result, stderr_result, wait_result) =
            tokio::join!(stdout_task, stderr_task, child.wait());

        // Handle any task errors
        stdout_result.map_err(|e| anyhow::anyhow!("Stdout task error: {}", e))?;
        stderr_result.map_err(|e| anyhow::anyhow!("Stderr task error: {}", e))?;

        // Check exit status
        let exit_status =
            wait_result.map_err(|e| anyhow::anyhow!("Failed to wait for '{}': {}", command, e))?;

        if !exit_status.success() {
            anyhow::bail!(
                "Command '{}' failed with exit code {:?}",
                command,
                exit_status.code()
            );
        }

        Ok(())
    }

    #[allow(unused)]
    pub fn run_sync(&self, command: &str) -> Result<()> {
        let status = self
            .build_command(command)
            .status()
            .map_err(|e| anyhow::anyhow!("Failed to execute '{}': {}", command, e))?;

        if !status.success() {
            anyhow::bail!(
                "Command '{}' failed with exit code {:?}",
                command,
                status.code()
            );
        }

        Ok(())
    }

    #[allow(unused)]
    pub fn run_with_output_sync(&self, command: &str) -> Result<String> {
        let output = self
            .build_command(command)
            .output()
            .map_err(|e| anyhow::anyhow!("Failed to execute '{}': {}", command, e))?;

        if !output.status.success() {
            anyhow::bail!(
                "Command '{}' failed with exit code {:?}: {}",
                command,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    #[allow(unused)]
    pub async fn run_with_output(&self, command: &str) -> Result<String> {
        let output = self
            .build_tokio_command(command)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to execute '{}': {}", command, e))?;

        if !output.status.success() {
            anyhow::bail!(
                "Command '{}' failed with exit code {:?}: {}",
                command,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    #[allow(unused)]
    pub async fn run_with_stdin(&self, command: &str, stdin_content: &str) -> Result<()> {
        let output = self
            .run_with_stdin_get_output(command, stdin_content)
            .await?;

        if !output.status.success() {
            anyhow::bail!(
                "Command '{}' failed with exit code {:?}: {}",
                command,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    #[allow(unused)]
    pub async fn run_with_stdin_get_output(
        &self,
        command: &str,
        stdin_content: &str,
    ) -> Result<std::process::Output> {
        let mut child = self
            .build_tokio_command(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn '{}': {}", command, e))?;

        if let Some(stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let mut stdin = stdin;
            stdin
                .write_all(stdin_content.as_bytes())
                .await
                .map_err(|e| anyhow::anyhow!("Failed to write to stdin: {}", e))?;
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to wait for '{}': {}", command, e))?;

        Ok(output)
    }

    #[allow(unused)]
    pub fn run_with_stdin_sync(&self, command: &str, stdin_content: &str) -> Result<()> {
        let mut child = self
            .build_command(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("Failed to spawn '{}': {}", command, e))?;

        if let Some(stdin) = child.stdin.take() {
            let mut stdin = stdin;
            stdin
                .write_all(stdin_content.as_bytes())
                .map_err(|e| anyhow::anyhow!("Failed to write to stdin: {}", e))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| anyhow::anyhow!("Failed to wait for '{}': {}", command, e))?;

        if !output.status.success() {
            anyhow::bail!(
                "Command '{}' failed with exit code {:?}: {}",
                command,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }
}
