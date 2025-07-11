use std::fmt::Write;
use std::pin::pin;
use std::{ffi::OsStr, path::Path};

use clap::Parser;
use color_eyre::eyre::ContextCompat;
use futures::TryStreamExt as _;
use octocrab::{Octocrab, models::pulls::PullRequest};
use serde::Deserialize;

use crate::graph::Graph;

mod graph;

#[derive(Parser)]
struct Cli {
    #[arg(short, long)]
    create_new: bool,
}

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    let cli = Cli::parse();

    let graph = build_branch_graph()?;

    let repo_info = repo_info()?;

    let token = command("gh", ["auth", "token"])?;
    let token = token.trim().to_owned();
    let octocrab = octocrab::OctocrabBuilder::default()
        .personal_token(token)
        .build()?;
    let mut pulls = octocrab
        .pulls(&repo_info.owner, &repo_info.name)
        .list()
        .send()
        .await?
        .into_stream(&octocrab)
        .try_collect::<Vec<_>>()
        .await?;

    let mut commands = Vec::new();

    for stack_root in graph.iter_edges_from("main") {
        let mut comment_lines = Vec::new();
        write_pr_comment(&graph, stack_root, 0, &mut comment_lines);

        process_branch(&mut commands, stack_root, "main", &graph, &comment_lines);
    }

    for command in commands {
        if let Err(err) = run_command(&command, &mut pulls, &octocrab, &repo_info, &cli).await {
            eprintln!("❌ {command:?} failed: {err:#}");
        }
    }

    Ok(())
}

fn build_branch_graph() -> color_eyre::Result<Graph> {
    fn go(graph: &mut Graph, change: &str, parent_branch: &str) -> color_eyre::Result<()> {
        let output = command(
            "jj",
            [
                "log",
                "--no-graph",
                "-r",
                &format!("children({change}, 1)"),
                "-T",
                "change_id ++ \" \" ++ bookmarks ++ \"\n\"",
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

    go(&mut graph, common_ancestor, "main")?;

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
    let output = serde_json::from_str::<Output>(&output)?;

    Ok(RepoInfo {
        owner: output.owner.login,
        name: output.name,
    })
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
    Ok(String::from_utf8(output.stdout)?)
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

#[derive(Debug)]
enum Commands {
    FindOrCreatePr {
        target: String,
        branch: String,
    },
    CreateOrUpdateComment {
        comment_lines: Vec<CommentLine>,
        branch: String,
    },
}

fn process_branch(
    commands: &mut Vec<Commands>,
    branch: &str,
    target: &str,
    graph: &Graph,
    comment_lines: &[CommentLine],
) {
    commands.push(Commands::FindOrCreatePr {
        branch: branch.to_owned(),
        target: target.to_owned(),
    });
    if comment_lines.len() > 1 {
        commands.push(Commands::CreateOrUpdateComment {
            branch: branch.to_owned(),
            comment_lines: comment_lines.to_vec(),
        });
    }

    for child in graph.iter_edges_from(branch) {
        process_branch(commands, child, branch, graph, comment_lines);
    }
}

async fn run_command(
    command: &Commands,
    pulls: &mut Vec<PullRequest>,
    octocrab: &Octocrab,
    repo_info: &RepoInfo,
    cli: &Cli,
) -> color_eyre::Result<()> {
    match command {
        Commands::FindOrCreatePr { target, branch } => {
            if let Some((idx, pull)) = pulls
                .iter()
                .enumerate()
                .find(|(_, pull)| pull.head.ref_field == **branch)
            {
                if pull.base.ref_field != **target {
                    eprintln!(
                        "updating target of PR from {branch} from {} to {target}",
                        pull.base.ref_field
                    );
                    let updated = octocrab
                        .pulls(&repo_info.owner, &repo_info.name)
                        .update(pull.number)
                        .base(target)
                        .send()
                        .await?;
                    pulls[idx] = updated;
                }
            } else if cli.create_new {
                eprintln!("creating PR from {branch} into {target}");
                let pull = octocrab
                    .pulls(&repo_info.owner, &repo_info.name)
                    .create(&**branch, target, &**branch)
                    .draft(true)
                    .send()
                    .await?;
                pulls.push(pull);
            } else {
                eprintln!("skipping creating PR from {branch} into {target}");
            }
        }
        Commands::CreateOrUpdateComment {
            comment_lines,
            branch,
        } => {
            let pull = pulls
                .iter()
                .find(|pull| pull.head.ref_field == *branch)
                .with_context(|| format!("PR from {branch} not found"))?;

            let comment = finalize_comment(branch, comment_lines, pulls)?;

            let comment_stream = octocrab
                .issues(&repo_info.owner, &repo_info.name)
                .list_comments(pull.number)
                .send()
                .await?
                .into_stream(octocrab)
                .try_filter(|comment| {
                    std::future::ready(comment.body.as_ref().is_some_and(|body| body.contains(ID)))
                });

            if let Some(existing_comment) = pin!(comment_stream).try_next().await? {
                if existing_comment.body.is_none_or(|body| body != comment) {
                    octocrab
                        .issues(&repo_info.owner, &repo_info.name)
                        .update_comment(existing_comment.id, comment)
                        .await?;
                    if let Some(url) = &pull.html_url {
                        eprintln!("updated comment on {url}");
                    }
                }
            } else {
                octocrab
                    .issues(&repo_info.owner, &repo_info.name)
                    .create_comment(pull.number, comment)
                    .await?;
                if let Some(url) = &pull.html_url {
                    eprintln!("created comment on {url}");
                }
            }
        }
    }

    Ok(())
}

fn finalize_comment(
    branch: &str,
    comment_lines: &[CommentLine],
    pulls: &[PullRequest],
) -> color_eyre::Result<String> {
    let mut comment = "This pull request is part of a stack:\n".to_owned();
    for line in comment_lines {
        line.format(branch, pulls, &mut comment)?;
        comment.push('\n');
    }
    comment.push_str("-------\n");
    write!(comment, "_This comment was auto-generated (id: {ID})_").unwrap();
    Ok(comment)
}
