use std::fmt::Write;
use std::{ffi::OsStr, path::Path};

use futures::TryStreamExt as _;
use octocrab::{Octocrab, models::pulls::PullRequest};
use serde::Deserialize;

use crate::graph::Graph;

mod graph;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    let graph = build_branch_graph()?;

    let repo_info = repo_info()?;

    let token = command("gh", ["auth", "token"])?;
    let token = token.trim().to_owned();
    let octocrab = octocrab::OctocrabBuilder::default()
        .personal_token(token)
        .build()?;
    let pulls = octocrab
        .pulls(&repo_info.owner, &repo_info.name)
        .list()
        .send()
        .await?
        .into_stream(&octocrab)
        .try_collect::<Vec<_>>()
        .await?;

    for stack_root in graph.iter_edges_from("main") {
        let mut comment_lines = Vec::new();
        write_pr_comment(&graph, stack_root, 0, &mut comment_lines);

        process_branch(
            stack_root,
            &graph,
            &pulls,
            &comment_lines,
            &octocrab,
            &repo_info,
        )
        .await?;
    }

    Ok(())
}

fn build_branch_graph() -> color_eyre::Result<Graph> {
    let mut graph = Graph::default();

    let mut branches = command(
        "jj",
        [
            "log",
            "--no-graph",
            "-r",
            "bookmarks()",
            "-T",
            "bookmarks ++ \"\\n\"",
        ],
    )?
    .lines()
    .map(|s| s.to_owned())
    .collect::<Vec<_>>();
    branches.reverse();

    for parent in branches {
        let parent_node = graph.get_or_insert(&parent);

        let children = command(
            "jj",
            [
                "log",
                "--no-graph",
                "-r",
                &format!("children({parent}) & bookmarks()"),
                "-T",
                "bookmarks ++ \"\\n\"",
            ],
        )?;

        for child in children.lines() {
            let child_node = graph.get_or_insert(child);
            graph.add_edge(parent_node, child_node);
        }
    }

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

#[derive(Debug)]
struct CommentLine {
    branch: String,
    indent: usize,
}

impl CommentLine {
    fn format(
        &self,
        branch: &str,
        pulls: &[PullRequest],
        out: &mut String,
    ) -> color_eyre::Result<()> {
        let Some((pull_title, pull_url)) = pulls
            .iter()
            .find(|pull| pull.head.ref_field == self.branch)
            .and_then(|pull| {
                let url = pull.html_url.as_ref()?;
                let title = pull.title.as_deref()?;
                Some((title, url))
            })
        else {
            color_eyre::eyre::bail!("no pr");
        };

        for c in std::iter::repeat_n(' ', self.indent) {
            write!(out, "{c}").unwrap();
        }
        write!(out, "- ").unwrap();

        write!(out, "[{pull_title}]({pull_url})").unwrap();
        if branch == self.branch {
            write!(out, " 👈 you are here").unwrap();
        }

        Ok(())
    }
}

const ID: &str = "e39f85cc-4589-41f7-9bae-d491c1ee2eda";

async fn process_branch(
    branch: &str,
    graph: &Graph,
    pulls: &[PullRequest],
    comment_lines: &[CommentLine],
    octocrab: &Octocrab,
    repo_info: &RepoInfo,
) -> color_eyre::Result<()> {
    if let Some(pull) = pulls.iter().find(|pull| pull.head.ref_field == branch) {
        let mut comment = "This pull request is part of a stack:\n".to_owned();
        for line in comment_lines {
            if line.format(branch, pulls, &mut comment).is_ok() {
                comment.push('\n');
            }
        }
        comment.push_str("-------\n");
        write!(comment, "_This comment was auto-generated (id: {ID})_").unwrap();

        let mut stream = std::pin::pin!(
            octocrab
                .issues(&repo_info.owner, &repo_info.name)
                .list_comments(pull.number)
                .send()
                .await?
                .into_stream(octocrab)
                .try_filter(|comment| {
                    std::future::ready(comment.body.as_ref().is_some_and(|body| body.contains(ID)))
                })
        );

        if let Some(existing_comment) = stream.try_next().await? {
            if existing_comment.body.is_none_or(|body| body != comment) {
                octocrab
                    .issues("lun-energy", "web-main")
                    .update_comment(existing_comment.id, comment)
                    .await?;
                if let Some(url) = &pull.html_url {
                    println!("Updated comment on {url}");
                }
            } else if let Some(url) = &pull.html_url {
                println!("{url} is up to date");
            }
        } else {
            octocrab
                .issues("lun-energy", "web-main")
                .create_comment(pull.number, comment)
                .await?;
            if let Some(url) = &pull.html_url {
                println!("Created comment on {url}");
            }
        }
    } else {
        println!("`{branch}` has no pull request");
    }

    for child in graph.iter_edges_from(branch) {
        Box::pin(process_branch(
            child,
            graph,
            pulls,
            comment_lines,
            octocrab,
            repo_info,
        ))
        .await?;
    }

    Ok(())
}
