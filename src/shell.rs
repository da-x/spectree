use anyhow::Result;
use std::path::Path;
use std::process::Command;

pub struct Shell<'a> {
    working_dir: &'a Path,
}

impl<'a> Shell<'a> {
    pub fn new(working_dir: &'a Path) -> Self {
        Shell { working_dir }
    }

    pub fn run(&self, command: &str) -> Result<()> {
        let output = Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(self.working_dir)
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

        Ok(())
    }

    #[allow(unused)]
    pub fn run_interactive(&self, command: &str) -> Result<()> {
        let status = Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(self.working_dir)
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
    pub fn run_with_output(&self, command: &str) -> Result<String> {
        let output = Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(self.working_dir)
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
}

#[macro_export]
macro_rules! ishell {
    ($($arg:tt)*) => { crate::shell::shell(format!($($arg)*)) }
}

#[macro_export]
macro_rules! ishell_stdout {
    ($($arg:tt)*) => { crate::shell::shell_stdout(format!($($arg)*)) }
}
