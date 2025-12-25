use std::fmt::Write;
use std::io::Write as _;
use std::path::PathBuf;
use std::{ffi::OsStr, path::Path};

use clap::Parser;
use color_eyre::eyre::{Context as _, ContextCompat, bail};
use dialoguer::{Confirm, Editor};
use futures::TryStreamExt as _;
use octocrab::{Octocrab, models::pulls::PullRequest};
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::graph::Graph;

mod graph;

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
            let branch_at_root_of_stack = branch_at_root_of_stack();

            let graph = tokio::task::spawn_blocking(|| {
                build_branch_graph(branch_at_root_of_stack).context("failed to build graph")
            });

            let repo_info = repo_info().context("failed to find repo info")?;

            let octocrab = octocrab::OctocrabBuilder::default()
                .personal_token(&*github_token)
                .build()
                .context("failed to build github client")?;

            let mut pulls = octocrab
                .pulls(&repo_info.owner, &repo_info.name)
                .list()
                .send()
                .await
                .context("failed to fetch pull requests")?
                .into_stream(&octocrab)
                .try_collect::<Vec<_>>()
                .await
                .context("failed to fetch all pull requests")?;

            let graph = graph.await??;

            for stack_root in graph.iter_edges_from(branch_at_root_of_stack) {
                find_or_create_prs(
                    stack_root,
                    branch_at_root_of_stack,
                    &graph,
                    &repo_info,
                    &octocrab,
                    &mut pulls,
                )
                .await
                .context("failed to sync prs")?;
            }

            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1024);

            for stack_root in graph.iter_edges_from(branch_at_root_of_stack) {
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
                        &octocrab,
                        &repo_info,
                        tx.clone(),
                    )
                    .context("failed to sync stack comment")?;
                }
            }
            drop(tx);
            while rx.recv().await.is_some() {}
        }
        Subcommand::Graph { out } => {
            let branch_at_root_of_stack = branch_at_root_of_stack();
            let graph =
                build_branch_graph(branch_at_root_of_stack).context("failed to build graph")?;
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
                "change_id ++ \" \" ++ local_bookmarks ++ \"\n\"",
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

    let output = command("jj", ["log", "--no-graph", "-T", "change_id ++ \"\n\""])?;
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

fn repo_info() -> color_eyre::Result<RepoInfo> {
    #[derive(Deserialize)]
    struct Output {
        name: String,
        owner: Owner,
    }

    #[derive(Deserialize)]
    struct Owner {
        login: String,
    }

    let output = command("gh", ["repo", "view", "--json", "name,owner"])?;
    let output =
        serde_json::from_str::<Output>(&output).context("failed to parse json output from gh")?;

    Ok(RepoInfo {
        owner: output.owner.login,
        name: output.name,
    })
}

fn branch_at_root_of_stack() -> &'static str {
    if command("jj", ["show", "dev"]).is_ok() {
        "dev"
    } else {
        "main"
    }
}

fn command<I>(command: &str, args: I) -> color_eyre::Result<String>
where
    I: IntoIterator<Item: AsRef<OsStr>>,
{
    let mut cmd = std::process::Command::new(command);
    cmd.args(args);
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
        pulls: &[PullRequest],
        out: &mut String,
    ) -> color_eyre::Result<()> {
        let (pull_title, pull_url) = pulls
            .iter()
            .find(|pull| pull.head.ref_field == self.branch)
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
    repo_info: &RepoInfo,
    octocrab: &Octocrab,
    pulls: &mut Vec<PullRequest>,
) -> color_eyre::Result<()> {
    find_or_create_pr(target, branch, pulls, octocrab, repo_info)
        .await
        .with_context(|| format!("failed to find or create pr from {branch} into {target}"))?;

    for child in graph.iter_edges_from(branch) {
        Box::pin(find_or_create_prs(
            child, branch, graph, repo_info, octocrab, pulls,
        ))
        .await
        .with_context(|| format!("failed to find or create pr from {branch} into {target}"))?;
    }

    Ok(())
}

fn finalize_comment(
    branch: &str,
    comment_lines: &[CommentLine],
    pulls: &[PullRequest],
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
    writeln!(
        &mut comment,
        "> <sup>Stack auto generated by jj-sync-prs</sup>"
    )?;
    Ok(comment)
}

async fn find_or_create_pr(
    target: &str,
    branch: &str,
    pulls: &mut Vec<PullRequest>,
    octocrab: &Octocrab,
    repo_info: &RepoInfo,
) -> color_eyre::Result<()> {
    if let Some((idx, pull)) = pulls
        .iter()
        .enumerate()
        .find(|(_, pull)| pull.head.ref_field == branch)
    {
        if pull.base.ref_field != target {
            eprintln!(
                "updating target of PR #{number} from {prev_target} <- {branch} to {new_target} <- {branch}",
                number = pull.number,
                prev_target = pull.base.ref_field,
                new_target = target,
            );
            let updated = octocrab
                .pulls(&repo_info.owner, &repo_info.name)
                .update(pull.number)
                .base(target)
                .send()
                .await
                .with_context(|| {
                    format!(
                        "failed updating target of PR #{number} from {prev_target} <- {branch} to {new_target} <- {branch}",
                        number = pull.number,
                        prev_target = pull.base.ref_field,
                        new_target = target,
                    )
                })?;
            pulls[idx] = updated;
        }
    } else if Confirm::new()
        .with_prompt(format!(
            "PR from {target} <- {branch} doesn't exist. Do you want to create it?"
        ))
        .default(true)
        .interact()?
    {
        let repo_pulls = octocrab.pulls(&repo_info.owner, &repo_info.name);

        let pull = if let Some((title, body)) = get_pr_title_and_body(branch, target)? {
            repo_pulls.create(title, branch, target).body(body)
        } else {
            repo_pulls.create(branch, branch, target)
        }
        .draft(true)
        .send()
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

    Ok(())
}

fn create_or_update_comments(
    comment_lines: &[CommentLine],
    branch: &str,
    graph: &Graph,
    pulls: &[PullRequest],
    octocrab: &Octocrab,
    repo_info: &RepoInfo,
    tx: mpsc::Sender<()>,
) -> color_eyre::Result<()> {
    tokio::spawn({
        let octocrab = octocrab.clone();
        let tx = tx.clone();
        let comment_lines = comment_lines.to_vec();
        let branch = branch.to_owned();
        let pulls = pulls.to_vec();
        let repo_info = repo_info.clone();
        async move {
            if let Err(err) =
                create_or_update_comment(comment_lines, branch, pulls, octocrab, repo_info).await
            {
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
            octocrab,
            repo_info,
            tx.clone(),
        )
        .context("failed to sync stack comment")?;
    }

    Ok(())
}

async fn create_or_update_comment(
    comment_lines: Vec<CommentLine>,
    branch: String,
    pulls: Vec<PullRequest>,
    octocrab: Octocrab,
    repo_info: RepoInfo,
) -> color_eyre::Result<()> {
    let pull = pulls
        .iter()
        .find(|pull| pull.head.ref_field == *branch)
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

    octocrab
        .issues(&repo_info.owner, &repo_info.name)
        .update(pull.number)
        .body(&new_body)
        .send()
        .await
        .with_context(|| format!("failed to update comment on #{}", pull.number))?;

    Ok(())
}

fn get_pr_title_and_body(
    branch: &str,
    target: &str,
) -> color_eyre::Result<Option<(String, String)>> {
    use std::fmt::Write;

    let body = std::fs::read_to_string(".github/pull_request_template.md")
        .unwrap_or_else(|_| "...and description here".to_owned());

    let diff = command(
        "jj",
        ["diff", "--git", "-r", &format!("{target}..{branch}")],
    )?;

    let count = command(
        "jj",
        ["log", "--count", "-r", &format!("{target}..{branch}")],
    )?
    .trim()
    .parse::<i32>()?;

    let ignored_marker = "Everything below this line will be ignored";

    let commit_descriptions = command(
        "jj",
        [
            "log",
            "--no-graph",
            "-r",
            &format!("{target}..{branch}"),
            "-T",
            "description ++ \"\n\"",
        ],
    )?;
    let commit_descriptions = commit_descriptions.trim().to_owned();

    let mut template = if count == 1 {
        commit_descriptions.clone()
    } else {
        format!("Enter PR title...\n\n{body}")
    };
    writeln!(&mut template)?;
    writeln!(&mut template)?;
    writeln!(&mut template, "JJ: {ignored_marker}")?;
    if count != 1 {
        writeln!(&mut template)?;
        write!(&mut template, "{commit_descriptions}")?;
    }
    if !diff.trim().is_empty() {
        writeln!(&mut template)?;
        writeln!(&mut template)?;
        writeln!(&mut template, "{diff}")?;
    }

    if let Some(text) = Editor::new().extension(".jjdescription").edit(&template)? {
        if text.trim().is_empty() {
            bail!("empty PR title and description");
        }

        let mut lines = text.lines();
        let Some(title) = lines.next() else {
            return Ok(None);
        };
        let msg = lines
            .skip(1)
            .take_while(|line| !line.contains(ignored_marker))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Some((title.to_owned(), msg)))
    } else {
        Ok(None)
    }
}
