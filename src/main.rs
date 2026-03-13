use std::borrow::Cow;
use std::fmt::Write;
use std::io::Write as _;
use std::path::PathBuf;
use std::{
    ffi::{OsStr, OsString},
    path::Path,
};

use clap::Parser;
use color_eyre::eyre::{Context as _, ContextCompat, bail};
use dialoguer::{Confirm, Editor};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::github::{GithubClient, OctocrabGithubClient, PullRequestInfo};
use crate::graph::Graph;

mod github;
mod graph;

#[cfg(test)]
#[derive(Default, Clone)]
struct TestHooks {
    command: Option<Arc<dyn Fn(&str, &[String]) -> color_eyre::Result<String> + Send + Sync>>,
    confirm: Option<Arc<dyn Fn(&str, bool) -> color_eyre::Result<bool> + Send + Sync>>,
    editor: Option<Arc<dyn Fn(&str) -> color_eyre::Result<Option<String>> + Send + Sync>>,
}

#[cfg(test)]
fn test_hooks() -> &'static std::sync::Mutex<TestHooks> {
    static HOOKS: std::sync::OnceLock<std::sync::Mutex<TestHooks>> = std::sync::OnceLock::new();
    HOOKS.get_or_init(|| std::sync::Mutex::new(TestHooks::default()))
}

#[derive(Parser, Debug)]
#[command(name = "jj-sync-prs")]
#[command(about = "Sync jujutsu branches with GitHub pull requests")]
struct Args {
    #[command(subcommand)]
    subcommand: Option<Subcommand>,
}

#[derive(clap::Subcommand, Debug)]
enum Subcommand {
    /// Sync branches with pull requests
    Sync {
        /// GitHub authentication token
        #[arg(long, env = "GH_AUTH_TOKEN")]
        github_token: String,
    },
    /// Save the branch graph as an image
    Graph {
        /// File to write the image to
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let args = Args::parse();

    if let Some(subcommand) = args.subcommand {
        run_subcommand(subcommand).await?;
    } else {
        run_subcommand(Subcommand::Sync {
            github_token: std::env::var("GH_AUTH_TOKEN").context("GH_AUTH_TOKEN is not set")?,
        })
        .await?;
    }

    Ok(())
}

async fn run_subcommand(subcommand: Subcommand) -> color_eyre::Result<()> {
    match subcommand {
        Subcommand::Sync { github_token } => {
            let branch_at_root_of_stack = branch_at_root_of_stack()?;
            let graph_branch_root = branch_at_root_of_stack.clone();

            let graph = tokio::task::spawn_blocking(move || {
                build_branch_graph(&graph_branch_root).context("failed to build graph")
            });

            let repo_info = repo_info().context("failed to find repo info")?;

            let octocrab = octocrab::OctocrabBuilder::default()
                .personal_token(&*github_token)
                .build()
                .context("failed to build github client")?;

            let github: Arc<dyn GithubClient> = Arc::new(OctocrabGithubClient::new(
                octocrab,
                repo_info.owner.clone(),
                repo_info.name.clone(),
            ));

            let mut pulls = github.list_pulls().await?;

            let graph = graph.await??;

            for stack_root in graph.iter_edges_from(&branch_at_root_of_stack) {
                find_or_create_prs(
                    stack_root,
                    &branch_at_root_of_stack,
                    &graph,
                    github.as_ref(),
                    &mut pulls,
                    true,
                )
                .await
                .context("failed to sync prs")?;
            }

            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1024);

            for stack_root in graph.iter_edges_from(&branch_at_root_of_stack) {
                let mut comment_lines = Vec::new();
                write_pr_comment(&graph, stack_root, 0, &mut comment_lines);

                if comment_lines.len() > 1 {
                    // if the comment contains just one line then its not
                    // part of a stack
                    create_or_update_comments(
                        &comment_lines,
                        stack_root,
                        &graph,
                        &pulls,
                        github.clone(),
                        tx.clone(),
                    )
                    .context("failed to sync stack comment")?;
                }
            }
            drop(tx);
            while rx.recv().await.is_some() {}
        }
        Subcommand::Graph { out } => {
            let branch_at_root_of_stack = branch_at_root_of_stack()?;
            let graph =
                build_branch_graph(&branch_at_root_of_stack).context("failed to build graph")?;
            let dot = graph.to_dot();
            let (read, mut write) = std::io::pipe()?;
            let out = out.as_deref().unwrap_or_else(|| Path::new("branches.png"));
            let mut cmd = std::process::Command::new("dot");
            cmd.arg("-Tpng");
            cmd.arg("-o");
            cmd.args(out);
            cmd.stdin(read);
            let mut child = cmd.spawn().with_context(|| format!("{cmd:?} failed"))?;
            write.write_all(dot.as_bytes())?;
            drop(write);
            color_eyre::eyre::ensure!(child.wait()?.success());
            eprintln!("Wrote {out:?}");
        }
    }

    Ok(())
}

fn build_branch_graph(branch_at_root_of_stack: &str) -> color_eyre::Result<Graph> {
    fn go(graph: &mut Graph, change: &str, parent_branch: &str) -> color_eyre::Result<()> {
        let output = command(
            "jj",
            [
                "log",
                "--no-graph",
                "-r",
                &format!("children({change}, 1)"),
                "-T",
                "change_id ++ \" \" ++ local_bookmarks ++ \"\\n\"",
            ],
        )?;

        for line in output.lines() {
            let (change, branch) = if let Some((change, branch)) = line.trim().split_once(' ') {
                (change, Some(branch.trim_matches('*')))
            } else {
                (line, None)
            };

            if let Some(branch) = branch {
                if parent_branch != branch {
                    let parent_branch_node = graph.get_or_insert(parent_branch);
                    let branch_node = graph.get_or_insert(branch);
                    graph.add_edge(parent_branch_node, branch_node);
                }
                go(graph, change, branch)?;
            } else {
                go(graph, change, parent_branch)?;
            }
        }

        Ok(())
    }

    let mut graph = Graph::default();

    let output = command("jj", ["log", "--no-graph", "-T", "change_id ++ \"\\n\""])?;
    let mut output = output.lines();
    let common_ancestor = output.next_back().context("no lines")?;

    go(&mut graph, common_ancestor, branch_at_root_of_stack)?;

    Ok(graph)
}

#[derive(Debug, Clone)]
struct RepoInfo {
    owner: String,
    name: String,
}

fn parse_repo_info_output(output: &str) -> color_eyre::Result<RepoInfo> {
    #[derive(Deserialize)]
    struct Output {
        name: String,
        owner: Owner,
    }

    #[derive(Deserialize)]
    struct Owner {
        login: String,
    }

    let output =
        serde_json::from_str::<Output>(output).context("failed to parse json output from gh")?;

    Ok(RepoInfo {
        owner: output.owner.login,
        name: output.name,
    })
}

fn repo_info() -> color_eyre::Result<RepoInfo> {
    let output = command("gh", ["repo", "view", "--json", "name,owner"])?;
    parse_repo_info_output(&output)
}

fn parse_trunk_bookmark_output(output: &str) -> Option<String> {
    let mut names = output
        .split_whitespace()
        .map(|name| name.trim_matches('*').trim())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();

    names.dedup();

    if names.len() == 1 {
        Some(names[0].to_owned())
    } else {
        None
    }
}

fn branch_at_root_of_stack() -> color_eyre::Result<String> {
    if let Ok(output) = command("jj", ["show", "-r", "trunk()", "-T", "local_bookmarks"]) {
        if let Some(branch) = parse_trunk_bookmark_output(&output) {
            return Ok(branch);
        }
    }

    if command("jj", ["show", "main"]).is_ok() {
        return Ok("main".to_owned());
    }

    if command("jj", ["show", "master"]).is_ok() {
        return Ok("master".to_owned());
    }

    Ok("main".to_owned())
}

fn command<I>(command: &str, args: I) -> color_eyre::Result<String>
where
    I: IntoIterator<Item: AsRef<OsStr>>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<OsString>>();

    #[cfg(test)]
    {
        if let Some(mock) = test_hooks().lock().unwrap().command.clone() {
            let args = args
                .iter()
                .map(|arg| arg.to_string_lossy().to_string())
                .collect::<Vec<_>>();
            return mock(command, &args);
        }
    }

    let mut cmd = std::process::Command::new(command);
    cmd.args(&args);
    if Path::new(".jj/repo/store/git").exists() {
        cmd.env("GIT_DIR", ".jj/repo/store/git");
    }
    let output = cmd.output()?;
    color_eyre::eyre::ensure!(output.status.success(), "{cmd:?} failed");
    String::from_utf8(output.stdout).context("command returned invalid utf-8")
}

fn write_pr_comment(graph: &Graph, branch: &str, indent: usize, out: &mut Vec<CommentLine>) {
    out.push(CommentLine {
        branch: branch.to_owned(),
        indent,
    });

    for child in graph.iter_edges_from(branch) {
        write_pr_comment(graph, child, indent + 2, out);
    }
}

#[derive(Debug, Clone)]
struct CommentLine {
    branch: String,
    indent: usize,
}

impl CommentLine {
    fn format(
        &self,
        head_branch: &str,
        pulls: &[PullRequestInfo],
        out: &mut String,
    ) -> color_eyre::Result<()> {
        let (pull_title, pull_url) = pulls
            .iter()
            .find(|pull| pull.head_branch == self.branch)
            .and_then(|pull| {
                let url = pull.html_url.as_ref()?;
                let title = pull.title.as_deref()?;
                Some((title, url))
            })
            .with_context(|| format!("PR from {} not found", self.branch))?;

        for c in std::iter::repeat_n(' ', self.indent) {
            write!(out, "{c}").unwrap();
        }
        write!(out, "- ").unwrap();

        write!(out, "[{pull_title}]({pull_url})").unwrap();
        if head_branch == self.branch {
            write!(out, " 👈 you are here").unwrap();
        }

        Ok(())
    }
}

const ID: &str = "e39f85cc-4589-41f7-9bae-d491c1ee2eda";

async fn find_or_create_prs(
    branch: &str,
    target: &str,
    graph: &Graph,
    github: &dyn GithubClient,
    pulls: &mut Vec<PullRequestInfo>,
    is_first: bool,
) -> color_eyre::Result<()> {
    if !is_first {
        println!();
    }

    find_or_create_pr(target, branch, pulls, github)
        .await
        .with_context(|| format!("failed to find or create pr from {branch} into {target}"))?;

    for child in graph.iter_edges_from(branch) {
        Box::pin(find_or_create_prs(
            child, branch, graph, github, pulls, false,
        ))
        .await
        .with_context(|| format!("failed to find or create pr from {branch} into {target}"))?;
    }

    Ok(())
}

fn finalize_comment(
    branch: &str,
    comment_lines: &[CommentLine],
    pulls: &[PullRequestInfo],
) -> color_eyre::Result<String> {
    let mut comment = String::new();
    writeln!(&mut comment, "<!-- jj-sync-prs: {ID} -->")?;
    writeln!(&mut comment, "---")?;
    writeln!(&mut comment, "> [!NOTE]")?;
    writeln!(&mut comment, "> This pull request is part of a stack:")?;
    for line in comment_lines {
        write!(&mut comment, "> ")?;
        line.format(branch, pulls, &mut comment)?;
        writeln!(&mut comment)?;
    }
    writeln!(&mut comment, ">")?;
    writeln!(&mut comment, ">")?;
    Ok(comment)
}

fn confirm_prompt(prompt: &str, default: bool) -> color_eyre::Result<bool> {
    #[cfg(test)]
    {
        if let Some(mock) = test_hooks().lock().unwrap().confirm.clone() {
            return mock(prompt, default);
        }
    }

    Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact()
        .map_err(Into::into)
}

fn edit_text(template: &str) -> color_eyre::Result<Option<String>> {
    #[cfg(test)]
    {
        if let Some(mock) = test_hooks().lock().unwrap().editor.clone() {
            return mock(template);
        }
    }

    Editor::new()
        .extension(".jjdescription")
        .edit(template)
        .map_err(Into::into)
}

async fn find_or_create_pr(
    target: &str,
    branch: &str,
    pulls: &mut Vec<PullRequestInfo>,
    github: &dyn GithubClient,
) -> color_eyre::Result<()> {
    if let Some((idx, pull)) = pulls
        .iter()
        .enumerate()
        .find(|(_, pull)| pull.head_branch == branch)
    {
        if pull.base_branch != target {
            eprintln!(
                "updating target of PR #{number} from {prev_target} <- {branch} to {new_target} <- {branch}",
                number = pull.number,
                prev_target = pull.base_branch,
                new_target = target,
            );
            let updated = github.update_pull_base(pull.number, target).await.with_context(|| {
                format!(
                    "failed updating target of PR #{number} from {prev_target} <- {branch} to {new_target} <- {branch}",
                    number = pull.number,
                    prev_target = pull.base_branch,
                    new_target = target,
                )
            })?;
            pulls[idx] = updated;
        }
    } else {
        let _ = command("jj", ["log", "-r", &format!("{target}::{branch}")]);
        if confirm_prompt(
            &format!("PR from {target} <- {branch} doesn't exist. Do you want to create it?"),
            true,
        )? {
            let (title, body) = if let Some((title, body)) = get_pr_title_and_body(branch, target)?
            {
                (title, Some(body))
            } else {
                (branch.to_owned(), None)
            };

            let pull = github
                .create_pull(&title, branch, target, body.as_deref(), true)
                .await
                .with_context(|| format!("failed to create PR from {target} <- {branch}"))?;

            if let Some(url) = &pull.html_url {
                eprintln!("Created PR from {target} <- {branch}: {url}");
            } else {
                eprintln!("Created PR from {target} <- {branch}");
            }
            pulls.push(pull);
        } else {
            eprintln!("skipping creating PR from {target} <- {branch}");
        }
    }

    Ok(())
}

fn create_or_update_comments(
    comment_lines: &[CommentLine],
    branch: &str,
    graph: &Graph,
    pulls: &[PullRequestInfo],
    github: Arc<dyn GithubClient>,
    tx: mpsc::Sender<()>,
) -> color_eyre::Result<()> {
    tokio::spawn({
        let github = github.clone();
        let tx = tx.clone();
        let comment_lines = comment_lines.to_vec();
        let branch = branch.to_owned();
        let pulls = pulls.to_vec();
        async move {
            if let Err(err) = create_or_update_comment(comment_lines, branch, pulls, github).await {
                eprintln!("{err:#}");
            }
            drop(tx);
        }
    });

    for child in graph.iter_edges_from(branch) {
        create_or_update_comments(
            comment_lines,
            child,
            graph,
            pulls,
            github.clone(),
            tx.clone(),
        )
        .context("failed to sync stack comment")?;
    }

    Ok(())
}

async fn create_or_update_comment(
    comment_lines: Vec<CommentLine>,
    branch: String,
    pulls: Vec<PullRequestInfo>,
    github: Arc<dyn GithubClient>,
) -> color_eyre::Result<()> {
    let pull = pulls
        .iter()
        .find(|pull| pull.head_branch == *branch)
        .with_context(|| format!("PR from {branch} not found"))?;

    let comment =
        finalize_comment(&branch, &comment_lines, &pulls).context("failed to finalize comment")?;

    let new_body = if let Some(body) = pull.body.as_deref() {
        let body_without_comment = body
            .lines()
            .take_while(|line| !line.contains(ID))
            .collect::<Vec<_>>()
            .join("\n");
        format!("{body_without_comment}\n\n{comment}")
    } else {
        comment
    };

    github
        .update_issue_body(pull.number, &new_body)
        .await
        .with_context(|| format!("failed to update comment on #{}", pull.number))?;

    Ok(())
}

struct GetPrTitleAndBody {
    title: Option<Cow<'static, str>>,
    message: Option<Cow<'static, str>>,
    additional: Option<String>,
    diff: Option<String>,
    log: String,
}

impl GetPrTitleAndBody {
    fn get(branch: &str, target: &str) -> color_eyre::Result<Self> {
        let title: Option<Cow<'static, str>>;
        let message: Option<Cow<'static, str>>;
        let additional: Option<String>;

        let count = command(
            "jj",
            ["log", "--count", "-r", &format!("{target}..{branch}")],
        )?
        .trim()
        .parse::<i32>()?;

        let descriptions = command(
            "jj",
            [
                "log",
                "--no-graph",
                "-r",
                &format!("{target}..{branch}"),
                "-T",
                "description ++ \"\\n\"",
            ],
        )?;
        let mut descriptions_lines = descriptions.lines();

        if let Ok(pr_template) = std::fs::read_to_string(".github/pull_request_template.md") {
            title = if count == 1 {
                descriptions_lines.next().map(|line| line.to_owned().into())
            } else {
                None
            };
            message = Some(pr_template.into());
            additional = Some(descriptions_lines.collect::<Vec<_>>().join("\n"));
        } else if count == 1 {
            title = descriptions_lines.next().map(|line| line.to_owned().into());
            message = Some(descriptions_lines.collect::<Vec<_>>().join("\n").into());
            additional = None;
        } else {
            title = None;
            message = None;
            additional = Some(descriptions);
        }

        let diff = command(
            "jj",
            ["diff", "--git", "-r", &format!("{target}..{branch}")],
        )?;
        let diff = if diff.trim().is_empty() {
            None
        } else {
            Some(diff)
        };

        let log = command(
            "jj",
            [
                "log",
                "--color",
                "never",
                "-r",
                &format!("{target}..{branch}"),
                "-T",
                "builtin_log_detailed",
            ],
        )?;

        Ok(GetPrTitleAndBody {
            title,
            message,
            additional,
            diff,
            log,
        })
    }
}

const IGNORED_MARKER: &str = "Everything below this line will be ignored";

fn parse_pr_editor_text(text: &str) -> color_eyre::Result<Option<(String, String)>> {
    if text.trim().is_empty() {
        bail!("empty PR title and description");
    }

    let mut lines = text.lines();
    let Some(title) = lines.next() else {
        return Ok(None);
    };
    let msg = lines
        .skip(1)
        .take_while(|line| !line.contains(IGNORED_MARKER))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(Some((title.to_owned(), msg)))
}

fn get_pr_title_and_body(
    branch: &str,
    target: &str,
) -> color_eyre::Result<Option<(String, String)>> {
    use std::fmt::Write;

    let mut template = String::new();

    let GetPrTitleAndBody {
        title,
        message,
        additional,
        diff,
        log,
    } = GetPrTitleAndBody::get(branch, target)?;

    if let Some(title) = title {
        writeln!(&mut template, "{title}")?;
        writeln!(&mut template)?;
    } else {
        writeln!(&mut template, "Enter PR title...")?;
        writeln!(&mut template)?;
    }

    if let Some(message) = message {
        writeln!(&mut template, "{message}")?;
        writeln!(&mut template)?;
    }

    writeln!(&mut template, "JJ: {IGNORED_MARKER}")?;
    writeln!(&mut template)?;

    writeln!(&mut template, "{log}")?;

    if let Some(additional) = additional {
        writeln!(&mut template, "{additional}")?;
        writeln!(&mut template)?;
    }

    if let Some(diff) = diff {
        writeln!(&mut template, "{diff}")?;
    }

    if let Some(text) = edit_text(&template)? {
        parse_pr_editor_text(&text)
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;

    #[derive(Default, Clone)]
    struct MockGithubClient {
        updated_bases: Arc<Mutex<Vec<(u64, String)>>>,
        updated_bodies: Arc<Mutex<Vec<(u64, String)>>>,
        created_pulls: Arc<Mutex<Vec<(String, String, String, Option<String>, bool)>>>,
        fail_create_pull: Arc<Mutex<Option<String>>>,
    }

    #[async_trait]
    impl GithubClient for MockGithubClient {
        async fn list_pulls(&self) -> color_eyre::Result<Vec<PullRequestInfo>> {
            Ok(Vec::new())
        }

        async fn update_pull_base(
            &self,
            pull_number: u64,
            base_branch: &str,
        ) -> color_eyre::Result<PullRequestInfo> {
            self.updated_bases
                .lock()
                .unwrap()
                .push((pull_number, base_branch.to_owned()));

            Ok(PullRequestInfo {
                number: pull_number,
                head_branch: "feature".to_owned(),
                base_branch: base_branch.to_owned(),
                html_url: Some("https://example.com/pr/1".to_owned()),
                title: Some("title".to_owned()),
                body: None,
            })
        }

        async fn create_pull(
            &self,
            title: &str,
            head_branch: &str,
            base_branch: &str,
            body: Option<&str>,
            draft: bool,
        ) -> color_eyre::Result<PullRequestInfo> {
            if let Some(err) = self.fail_create_pull.lock().unwrap().clone() {
                bail!("{err}");
            }

            self.created_pulls.lock().unwrap().push((
                title.to_owned(),
                head_branch.to_owned(),
                base_branch.to_owned(),
                body.map(str::to_owned),
                draft,
            ));

            Ok(PullRequestInfo {
                number: 999,
                head_branch: head_branch.to_owned(),
                base_branch: base_branch.to_owned(),
                html_url: Some("https://example.com/pr/999".to_owned()),
                title: Some(title.to_owned()),
                body: body.map(str::to_owned),
            })
        }

        async fn update_issue_body(&self, pull_number: u64, body: &str) -> color_eyre::Result<()> {
            self.updated_bodies
                .lock()
                .unwrap()
                .push((pull_number, body.to_owned()));
            Ok(())
        }
    }

    struct HookGuard {
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for HookGuard {
        fn drop(&mut self) {
            *test_hooks().lock().unwrap() = TestHooks::default();
        }
    }

    fn install_hooks(hooks: TestHooks) -> HookGuard {
        static SERIAL: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let serial = SERIAL
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        *test_hooks().lock().unwrap() = hooks;
        HookGuard { _serial: serial }
    }

    struct CwdGuard {
        prev: std::path::PathBuf,
    }

    impl CwdGuard {
        fn set(path: &Path) -> color_eyre::Result<Self> {
            let prev = std::env::current_dir()?;
            std::env::set_current_dir(path)?;
            Ok(Self { prev })
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.prev);
        }
    }

    fn sample_pull(number: u64, head_branch: &str, base_branch: &str) -> PullRequestInfo {
        PullRequestInfo {
            number,
            head_branch: head_branch.to_owned(),
            base_branch: base_branch.to_owned(),
            html_url: Some(format!("https://example.com/pull/{number}")),
            title: Some(format!("PR {head_branch}")),
            body: None,
        }
    }

    #[test]
    fn write_pr_comment_collects_depth_first_with_indentation() {
        let mut graph = Graph::default();
        let main = graph.get_or_insert("main");
        let feat1 = graph.get_or_insert("feat1");
        let feat2 = graph.get_or_insert("feat2");
        let feat3 = graph.get_or_insert("feat3");
        graph.add_edge(main, feat1);
        graph.add_edge(feat1, feat2);
        graph.add_edge(feat1, feat3);

        let mut lines = Vec::new();
        write_pr_comment(&graph, "feat1", 0, &mut lines);

        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].branch, "feat1");
        assert_eq!(lines[0].indent, 0);
        assert_eq!(lines[1].branch, "feat2");
        assert_eq!(lines[1].indent, 2);
        assert_eq!(lines[2].branch, "feat3");
        assert_eq!(lines[2].indent, 2);
    }

    #[test]
    fn comment_line_format_includes_here_marker_on_head_branch() -> color_eyre::Result<()> {
        let line = CommentLine {
            branch: "feat".to_owned(),
            indent: 2,
        };
        let pulls = vec![sample_pull(1, "feat", "main")];

        let mut out = String::new();
        line.format("feat", &pulls, &mut out)?;

        assert_eq!(
            out,
            "  - [PR feat](https://example.com/pull/1) 👈 you are here"
        );
        Ok(())
    }

    #[test]
    fn comment_line_format_errors_when_pull_missing() {
        let line = CommentLine {
            branch: "missing".to_owned(),
            indent: 0,
        };
        let pulls = vec![sample_pull(1, "feat", "main")];

        let err = line.format("feat", &pulls, &mut String::new()).unwrap_err();
        assert!(format!("{err:#}").contains("PR from missing not found"));
    }

    #[test]
    fn finalize_comment_includes_all_metadata() -> color_eyre::Result<()> {
        let lines = vec![
            CommentLine {
                branch: "feat1".to_owned(),
                indent: 0,
            },
            CommentLine {
                branch: "feat2".to_owned(),
                indent: 2,
            },
        ];
        let pulls = vec![
            sample_pull(1, "feat1", "main"),
            sample_pull(2, "feat2", "feat1"),
        ];

        let comment = finalize_comment("feat2", &lines, &pulls)?;

        assert!(comment.contains("<!-- jj-sync-prs:"));
        assert!(comment.contains("> [!NOTE]"));
        assert!(comment.contains("[PR feat1](https://example.com/pull/1)"));
        assert!(comment.contains("[PR feat2](https://example.com/pull/2) 👈 you are here"));
        Ok(())
    }

    #[tokio::test]
    async fn find_or_create_pr_updates_base_when_needed() -> color_eyre::Result<()> {
        let github = MockGithubClient::default();
        let mut pulls = vec![sample_pull(1, "feature", "old-base")];

        find_or_create_pr("new-base", "feature", &mut pulls, &github).await?;

        let updates = github.updated_bases.lock().unwrap().clone();
        assert_eq!(updates, vec![(1, "new-base".to_owned())]);
        assert_eq!(pulls[0].base_branch, "new-base");
        Ok(())
    }

    #[tokio::test]
    async fn find_or_create_pr_noop_when_base_matches() -> color_eyre::Result<()> {
        let github = MockGithubClient::default();
        let mut pulls = vec![sample_pull(1, "feature", "main")];

        find_or_create_pr("main", "feature", &mut pulls, &github).await?;

        assert!(github.updated_bases.lock().unwrap().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn create_or_update_comment_appends_when_no_existing_body() -> color_eyre::Result<()> {
        let github = Arc::new(MockGithubClient::default());
        let lines = vec![CommentLine {
            branch: "feature".to_owned(),
            indent: 0,
        }];
        let pulls = vec![sample_pull(42, "feature", "main")];

        create_or_update_comment(lines, "feature".to_owned(), pulls, github.clone()).await?;

        let bodies = github.updated_bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0].0, 42);
        assert!(bodies[0].1.contains("<!-- jj-sync-prs:"));
        Ok(())
    }

    #[tokio::test]
    async fn create_or_update_comment_replaces_existing_generated_comment() -> color_eyre::Result<()>
    {
        let github = Arc::new(MockGithubClient::default());
        let lines = vec![CommentLine {
            branch: "feature".to_owned(),
            indent: 0,
        }];
        let mut pull = sample_pull(7, "feature", "main");
        pull.body = Some(format!(
            "Manual content\n<!-- jj-sync-prs: {ID} -->\nold generated comment"
        ));

        create_or_update_comment(lines, "feature".to_owned(), vec![pull], github.clone()).await?;

        let body = &github.updated_bodies.lock().unwrap()[0].1;
        assert!(body.starts_with("Manual content\n\n<!-- jj-sync-prs:"));
        assert!(!body.contains("old generated comment"));
        Ok(())
    }

    #[tokio::test]
    async fn create_or_update_comment_errors_for_unknown_branch() {
        let github = Arc::new(MockGithubClient::default());
        let lines = vec![CommentLine {
            branch: "feature".to_owned(),
            indent: 0,
        }];

        let err = create_or_update_comment(lines, "missing".to_owned(), Vec::new(), github)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("PR from missing not found"));
    }

    #[test]
    fn args_parse_sync_and_graph() {
        let args = Args::try_parse_from(["jj-sync-prs", "sync", "--github-token", "t"]).unwrap();
        assert!(matches!(args.subcommand, Some(Subcommand::Sync { .. })));

        let args = Args::try_parse_from(["jj-sync-prs", "graph", "--out", "x.png"]).unwrap();
        assert!(matches!(args.subcommand, Some(Subcommand::Graph { .. })));
    }

    #[test]
    fn parse_repo_info_output_parses_json() -> color_eyre::Result<()> {
        let info = parse_repo_info_output(r#"{"name":"repo","owner":{"login":"me"}}"#)?;
        assert_eq!(info.owner, "me");
        assert_eq!(info.name, "repo");
        Ok(())
    }

    #[test]
    fn parse_repo_info_output_errors_for_invalid_json() {
        let err = parse_repo_info_output("not json").unwrap_err();
        assert!(format!("{err:#}").contains("failed to parse json output from gh"));
    }

    #[test]
    fn parse_trunk_bookmark_output_parses_single_bookmark() {
        assert_eq!(parse_trunk_bookmark_output("master\n"), Some("master".to_owned()));
    }

    #[test]
    fn parse_trunk_bookmark_output_ignores_markers_and_whitespace() {
        assert_eq!(
            parse_trunk_bookmark_output("  *main  \n"),
            Some("main".to_owned())
        );
    }

    #[test]
    fn parse_trunk_bookmark_output_rejects_ambiguous_output() {
        assert_eq!(parse_trunk_bookmark_output("main master\n"), None);
    }

    #[test]
    fn branch_at_root_of_stack_uses_trunk_bookmark() -> color_eyre::Result<()> {
        let _guard = install_hooks(TestHooks {
            command: Some(Arc::new(|command, args| {
                if command == "jj"
                    && args
                        == [
                            "show".to_string(),
                            "-r".to_string(),
                            "trunk()".to_string(),
                            "-T".to_string(),
                            "local_bookmarks".to_string(),
                        ]
                {
                    Ok("master\n".to_owned())
                } else {
                    bail!("unexpected command: {command} {args:?}")
                }
            })),
            ..Default::default()
        });

        assert_eq!(branch_at_root_of_stack()?, "master");
        Ok(())
    }

    #[test]
    fn branch_at_root_of_stack_falls_back_to_master_when_trunk_is_unavailable(
    ) -> color_eyre::Result<()> {
        let _guard = install_hooks(TestHooks {
            command: Some(Arc::new(|command, args| {
                if command != "jj" {
                    bail!("unexpected command: {command} {args:?}");
                }

                if args
                    == [
                        "show".to_string(),
                        "-r".to_string(),
                        "trunk()".to_string(),
                        "-T".to_string(),
                        "local_bookmarks".to_string(),
                    ]
                {
                    bail!("trunk unavailable");
                }

                if args == ["show".to_string(), "main".to_string()] {
                    bail!("main missing");
                }

                if args == ["show".to_string(), "master".to_string()] {
                    return Ok("ok".to_owned());
                }

                bail!("unexpected command: {command} {args:?}")
            })),
            ..Default::default()
        });

        assert_eq!(branch_at_root_of_stack()?, "master");
        Ok(())
    }

    #[test]
    fn branch_at_root_of_stack_falls_back_to_main() -> color_eyre::Result<()> {
        let _guard = install_hooks(TestHooks {
            command: Some(Arc::new(|command, args| {
                if command != "jj" {
                    bail!("unexpected command: {command} {args:?}");
                }

                if args
                    == [
                        "show".to_string(),
                        "-r".to_string(),
                        "trunk()".to_string(),
                        "-T".to_string(),
                        "local_bookmarks".to_string(),
                    ]
                {
                    return Ok("\n".to_owned());
                }

                if args == ["show".to_string(), "main".to_string()] {
                    bail!("main missing");
                }

                if args == ["show".to_string(), "master".to_string()] {
                    bail!("master missing");
                }

                bail!("unexpected command: {command} {args:?}")
            })),
            ..Default::default()
        });

        assert_eq!(branch_at_root_of_stack()?, "main");
        Ok(())
    }

    #[test]
    fn parse_pr_editor_text_handles_marker() -> color_eyre::Result<()> {
        let parsed = parse_pr_editor_text(
            "My title\n\nBody line\nJJ: Everything below this line will be ignored\nignored",
        )?
        .unwrap();
        assert_eq!(parsed.0, "My title");
        assert_eq!(parsed.1, "Body line");
        Ok(())
    }

    #[test]
    fn parse_pr_editor_text_errors_on_empty() {
        let err = parse_pr_editor_text("  \n\t").unwrap_err();
        assert!(format!("{err:#}").contains("empty PR title and description"));
    }

    #[test]
    fn build_branch_graph_parses_recursive_children() -> color_eyre::Result<()> {
        let _guard = install_hooks(TestHooks {
            command: Some(Arc::new(|command, args| {
                if command != "jj" {
                    bail!("unexpected command");
                }

                if args == ["log", "--no-graph", "-T", "change_id ++ \"\\n\""] {
                    return Ok("c3\nc2\nc1\n".to_owned());
                }

                if args.iter().any(|arg| arg == "children(c1, 1)") {
                    return Ok("c2 feat1\n".to_owned());
                }

                if args.iter().any(|arg| arg == "children(c2, 1)") {
                    return Ok("c3\n".to_owned());
                }

                if args.iter().any(|arg| arg == "children(c3, 1)") {
                    return Ok("c4 feat2\n".to_owned());
                }

                if args.iter().any(|arg| arg == "children(c4, 1)") {
                    return Ok(String::new());
                }

                bail!("unexpected args: {args:?}")
            })),
            ..Default::default()
        });

        let graph = build_branch_graph("main")?;
        let main_children = graph.iter_edges_from("main").collect::<Vec<_>>();
        let feat1_children = graph.iter_edges_from("feat1").collect::<Vec<_>>();

        assert_eq!(main_children, vec!["feat1"]);
        assert_eq!(feat1_children, vec!["feat2"]);
        Ok(())
    }

    #[tokio::test]
    async fn find_or_create_pr_creates_when_missing_and_confirmed() -> color_eyre::Result<()> {
        let github = MockGithubClient::default();
        let _guard = install_hooks(TestHooks {
            command: Some(Arc::new(|command, args| {
                if command != "jj" {
                    bail!("unexpected command")
                }

                if args.iter().any(|arg| arg == "--count") {
                    return Ok("1\n".to_owned());
                }
                if args.iter().any(|arg| arg == "description ++ \"\\n\"") {
                    return Ok("Title\nBody\n".to_owned());
                }
                if args.iter().any(|arg| arg == "--git") {
                    return Ok(String::new());
                }
                if args.iter().any(|arg| arg == "builtin_log_detailed") {
                    return Ok("log".to_owned());
                }

                Ok(String::new())
            })),
            confirm: Some(Arc::new(|_, _| Ok(true))),
            editor: Some(Arc::new(|_| Ok(Some("A title\n\nA body".to_owned())))),
        });

        let mut pulls = Vec::new();
        find_or_create_pr("main", "feature", &mut pulls, &github).await?;

        let created = github.created_pulls.lock().unwrap().clone();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].0, "A title");
        assert_eq!(created[0].1, "feature");
        assert_eq!(created[0].2, "main");
        assert_eq!(pulls.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn find_or_create_pr_skips_create_when_not_confirmed() -> color_eyre::Result<()> {
        let github = MockGithubClient::default();
        let _guard = install_hooks(TestHooks {
            command: Some(Arc::new(|_, _| Ok(String::new()))),
            confirm: Some(Arc::new(|_, _| Ok(false))),
            ..Default::default()
        });

        let mut pulls = Vec::new();
        find_or_create_pr("main", "feature", &mut pulls, &github).await?;

        assert!(github.created_pulls.lock().unwrap().is_empty());
        assert!(pulls.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn find_or_create_pr_propagates_create_errors() {
        let github = MockGithubClient::default();
        *github.fail_create_pull.lock().unwrap() = Some("boom".to_owned());
        let _guard = install_hooks(TestHooks {
            command: Some(Arc::new(|_, args| {
                if args.iter().any(|arg| arg == "--count") {
                    return Ok("1\n".to_owned());
                }
                if args.iter().any(|arg| arg == "description ++ \"\\n\"") {
                    return Ok("Title\nBody\n".to_owned());
                }
                if args.iter().any(|arg| arg == "--git") {
                    return Ok(String::new());
                }
                if args.iter().any(|arg| arg == "builtin_log_detailed") {
                    return Ok("log".to_owned());
                }

                Ok(String::new())
            })),
            confirm: Some(Arc::new(|_, _| Ok(true))),
            editor: Some(Arc::new(|_| Ok(Some("A title\n\nA body".to_owned())))),
        });

        let mut pulls = Vec::new();
        let err = find_or_create_pr("main", "feature", &mut pulls, &github)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("failed to create PR from main <- feature"));
    }

    #[test]
    fn get_pr_title_and_body_handles_editor_cancel() -> color_eyre::Result<()> {
        let _guard = install_hooks(TestHooks {
            command: Some(Arc::new(|_, args| {
                if args.iter().any(|arg| arg == "--count") {
                    return Ok("1\n".to_owned());
                }
                if args.iter().any(|arg| arg == "description ++ \"\\n\"") {
                    return Ok("Title\nBody\n".to_owned());
                }
                if args.iter().any(|arg| arg == "--git") {
                    return Ok(String::new());
                }
                if args.iter().any(|arg| arg == "builtin_log_detailed") {
                    return Ok("log".to_owned());
                }
                Ok(String::new())
            })),
            editor: Some(Arc::new(|_| Ok(None))),
            ..Default::default()
        });

        let out = get_pr_title_and_body("feature", "main")?;
        assert!(out.is_none());
        Ok(())
    }

    #[test]
    fn get_pr_title_and_body_uses_pr_template_when_present() -> color_eyre::Result<()> {
        let captured = Arc::new(Mutex::new(String::new()));
        let captured_clone = captured.clone();
        let _guard = install_hooks(TestHooks {
            command: Some(Arc::new(|_, args| {
                if args.iter().any(|arg| arg == "--count") {
                    return Ok("1\n".to_owned());
                }
                if args.iter().any(|arg| arg == "description ++ \"\\n\"") {
                    return Ok("Commit title\nMore details\n".to_owned());
                }
                if args.iter().any(|arg| arg == "--git") {
                    return Ok("diff --git a b\n".to_owned());
                }
                if args.iter().any(|arg| arg == "builtin_log_detailed") {
                    return Ok("log output".to_owned());
                }
                Ok(String::new())
            })),
            editor: Some(Arc::new(move |template| {
                *captured_clone.lock().unwrap() = template.to_owned();
                Ok(Some("Edited title\n\nEdited body".to_owned()))
            })),
            ..Default::default()
        });

        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join(".github"))?;
        std::fs::write(
            dir.path().join(".github/pull_request_template.md"),
            "Template body",
        )?;
        let _cwd = CwdGuard::set(dir.path())?;

        let out = get_pr_title_and_body("feature", "main")?.unwrap();
        assert_eq!(out.0, "Edited title");

        let template = captured.lock().unwrap().clone();
        assert!(template.contains("Template body"));
        assert!(template.contains("log output"));
        assert!(template.contains("diff --git a b"));
        Ok(())
    }

    #[tokio::test]
    async fn find_or_create_prs_recurses_with_parent_as_target() -> color_eyre::Result<()> {
        let github = MockGithubClient::default();

        let mut graph = Graph::default();
        let main = graph.get_or_insert("main");
        let a = graph.get_or_insert("a");
        let b = graph.get_or_insert("b");
        let c = graph.get_or_insert("c");
        let d = graph.get_or_insert("d");
        graph.add_edge(main, a);
        graph.add_edge(a, b);
        graph.add_edge(a, c);
        graph.add_edge(b, d);

        let mut pulls = vec![
            sample_pull(10, "a", "old"),
            sample_pull(11, "b", "old"),
            sample_pull(12, "c", "old"),
            sample_pull(13, "d", "old"),
        ];

        find_or_create_prs("a", "main", &graph, &github, &mut pulls, true).await?;

        let mut updates = github.updated_bases.lock().unwrap().clone();
        updates.sort_by_key(|(number, _)| *number);
        assert_eq!(
            updates,
            vec![
                (10, "main".to_owned()),
                (11, "a".to_owned()),
                (12, "a".to_owned()),
                (13, "b".to_owned()),
            ]
        );

        Ok(())
    }

    #[tokio::test]
    async fn create_or_update_comments_recurses_for_entire_stack() -> color_eyre::Result<()> {
        let github = Arc::new(MockGithubClient::default());

        let mut graph = Graph::default();
        let main = graph.get_or_insert("main");
        let a = graph.get_or_insert("a");
        let b = graph.get_or_insert("b");
        let c = graph.get_or_insert("c");
        let d = graph.get_or_insert("d");
        graph.add_edge(main, a);
        graph.add_edge(a, b);
        graph.add_edge(a, c);
        graph.add_edge(c, d);

        let mut comment_lines = Vec::new();
        write_pr_comment(&graph, "a", 0, &mut comment_lines);

        let pulls = vec![
            sample_pull(1, "a", "main"),
            sample_pull(2, "b", "a"),
            sample_pull(3, "c", "a"),
            sample_pull(4, "d", "c"),
        ];

        let (tx, mut rx) = mpsc::channel::<()>(64);
        create_or_update_comments(&comment_lines, "a", &graph, &pulls, github.clone(), tx)?;

        while rx.recv().await.is_some() {}

        let mut updated_numbers = github
            .updated_bodies
            .lock()
            .unwrap()
            .iter()
            .map(|(number, _)| *number)
            .collect::<Vec<_>>();
        updated_numbers.sort_unstable();

        assert_eq!(updated_numbers, vec![1, 2, 3, 4]);

        Ok(())
    }
}
