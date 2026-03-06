mod common;

use common::TempJjRepo;

#[test]
fn can_create_temp_repo_and_commit() -> color_eyre::Result<()> {
    let repo = TempJjRepo::new()?;
    assert!(repo.path().exists());
    repo.write_file("hello.txt", "hello")?;
    repo.commit_all("initial")?;

    let log = repo.run_jj(&[
        "log",
        "--no-graph",
        "-n",
        "3",
        "-T",
        "description ++ \"\\n\"",
    ])?;
    assert!(log.lines().any(|line| line == "initial"));

    Ok(())
}

#[test]
fn write_file_creates_parent_directories() -> color_eyre::Result<()> {
    let repo = TempJjRepo::new()?;

    repo.write_file("nested/deep/file.txt", "hello")?;

    let file = repo.path().join("nested/deep/file.txt");
    assert!(file.exists());
    assert_eq!(std::fs::read_to_string(file)?, "hello");

    Ok(())
}

#[test]
fn run_jj_returns_error_for_failing_command() -> color_eyre::Result<()> {
    let repo = TempJjRepo::new()?;

    let err = repo.run_jj(&["this-command-does-not-exist"]).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("failed"));

    Ok(())
}

#[test]
fn commit_all_leaves_empty_working_copy_description() -> color_eyre::Result<()> {
    let repo = TempJjRepo::new()?;

    repo.write_file("hello.txt", "hello")?;
    repo.commit_all("initial")?;

    let description = repo.run_jj(&["log", "-r", "@", "--no-graph", "-T", "description"])?;
    assert_eq!(description.trim(), "");

    Ok(())
}
