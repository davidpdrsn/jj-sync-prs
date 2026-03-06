use std::path::{Path, PathBuf};
use std::process::Command;

use color_eyre::eyre::{Context as _, bail};
use tempfile::TempDir;

pub struct TempJjRepo {
    _temp_dir: TempDir,
    path: PathBuf,
}

impl TempJjRepo {
    pub fn new() -> color_eyre::Result<Self> {
        let temp_dir = tempfile::tempdir().context("failed creating temporary directory")?;
        let path = temp_dir.path().to_path_buf();

        run_cmd(path.as_path(), "jj", ["git", "init", "."])?;
        run_cmd(
            path.as_path(),
            "jj",
            ["config", "set", "--repo", "user.name", "Test User"],
        )?;
        run_cmd(
            path.as_path(),
            "jj",
            ["config", "set", "--repo", "user.email", "test@example.com"],
        )?;

        Ok(Self {
            _temp_dir: temp_dir,
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write_file(&self, rel_path: &str, contents: &str) -> color_eyre::Result<()> {
        let file = self.path.join(rel_path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(file, contents)?;
        Ok(())
    }

    pub fn commit_all(&self, message: &str) -> color_eyre::Result<()> {
        run_cmd(self.path.as_path(), "jj", ["describe", "-m", message])?;
        run_cmd(self.path.as_path(), "jj", ["new"])?;
        run_cmd(self.path.as_path(), "jj", ["describe", "-m", ""])?;
        Ok(())
    }

    pub fn run_jj(&self, args: &[&str]) -> color_eyre::Result<String> {
        run_cmd(self.path.as_path(), "jj", args.iter().copied())
    }
}

fn run_cmd(
    cwd: &Path,
    program: &str,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> color_eyre::Result<String> {
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_owned())
        .collect::<Vec<_>>();

    let output = Command::new(program)
        .current_dir(cwd)
        .args(&args)
        .output()
        .with_context(|| format!("failed to run {program}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{program} {:?} failed: {stderr}", args);
    }

    String::from_utf8(output.stdout).context("command returned invalid utf-8")
}
