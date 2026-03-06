use async_trait::async_trait;
use color_eyre::eyre::Context as _;
use futures::TryStreamExt as _;
use octocrab::{Octocrab, models::pulls::PullRequest};

#[derive(Debug, Clone)]
pub struct PullRequestInfo {
    pub number: u64,
    pub head_branch: String,
    pub base_branch: String,
    pub html_url: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
}

impl TryFrom<PullRequest> for PullRequestInfo {
    type Error = color_eyre::Report;

    fn try_from(value: PullRequest) -> Result<Self, Self::Error> {
        let number = value.number;

        Ok(Self {
            number,
            head_branch: value.head.ref_field,
            base_branch: value.base.ref_field,
            html_url: value.html_url.map(|url| url.to_string()),
            title: value.title,
            body: value.body,
        })
    }
}

#[async_trait]
pub trait GithubClient: Send + Sync {
    async fn list_pulls(&self) -> color_eyre::Result<Vec<PullRequestInfo>>;

    async fn update_pull_base(
        &self,
        pull_number: u64,
        base_branch: &str,
    ) -> color_eyre::Result<PullRequestInfo>;

    async fn create_pull(
        &self,
        title: &str,
        head_branch: &str,
        base_branch: &str,
        body: Option<&str>,
        draft: bool,
    ) -> color_eyre::Result<PullRequestInfo>;

    async fn update_issue_body(&self, pull_number: u64, body: &str) -> color_eyre::Result<()>;
}

#[derive(Debug, Clone)]
pub struct OctocrabGithubClient {
    octocrab: Octocrab,
    owner: String,
    name: String,
}

impl OctocrabGithubClient {
    pub fn new(octocrab: Octocrab, owner: String, name: String) -> Self {
        Self {
            octocrab,
            owner,
            name,
        }
    }
}

#[async_trait]
impl GithubClient for OctocrabGithubClient {
    async fn list_pulls(&self) -> color_eyre::Result<Vec<PullRequestInfo>> {
        self.octocrab
            .pulls(&self.owner, &self.name)
            .list()
            .send()
            .await
            .context("failed to fetch pull requests")?
            .into_stream(&self.octocrab)
            .try_collect::<Vec<_>>()
            .await
            .context("failed to fetch all pull requests")?
            .into_iter()
            .map(TryInto::try_into)
            .collect::<color_eyre::Result<Vec<_>>>()
    }

    async fn update_pull_base(
        &self,
        pull_number: u64,
        base_branch: &str,
    ) -> color_eyre::Result<PullRequestInfo> {
        self.octocrab
            .pulls(&self.owner, &self.name)
            .update(pull_number)
            .base(base_branch)
            .send()
            .await
            .context("failed updating pull request base")?
            .try_into()
    }

    async fn create_pull(
        &self,
        title: &str,
        head_branch: &str,
        base_branch: &str,
        body: Option<&str>,
        draft: bool,
    ) -> color_eyre::Result<PullRequestInfo> {
        let repo_pulls = self.octocrab.pulls(&self.owner, &self.name);

        let mut create = repo_pulls.create(title, head_branch, base_branch);
        if let Some(body) = body {
            create = create.body(body);
        }

        if draft {
            create = create.draft(true);
        }

        create
            .send()
            .await
            .context("failed creating pull request")?
            .try_into()
    }

    async fn update_issue_body(&self, pull_number: u64, body: &str) -> color_eyre::Result<()> {
        self.octocrab
            .issues(&self.owner, &self.name)
            .update(pull_number)
            .body(body)
            .send()
            .await
            .context("failed to update issue body")?;

        Ok(())
    }
}
