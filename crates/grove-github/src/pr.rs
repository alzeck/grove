use crate::cli::gh;
use crate::error::GhError;
use crate::repo::RepoRef;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub repo: RepoRef,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: PrState,
    pub is_draft: bool,
    /// The branch name in the head repo (the fork, for cross-repo PRs).
    pub head_ref: String,
    pub base_ref: String,
    /// `None` when the fork has been deleted.
    pub head_repo: Option<RepoRef>,
    pub is_cross_repository: bool,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

const PR_FIELDS: &str = "number,title,url,state,isDraft,headRefName,baseRefName,\
                         headRepository,headRepositoryOwner,isCrossRepository,body";

pub async fn pr_view(repo: &RepoRef, number: u64) -> Result<PullRequest, GhError> {
    let json = gh(&[
        "pr",
        "view",
        &number.to_string(),
        "--repo",
        &repo.to_string(),
        "--json",
        PR_FIELDS,
    ])
    .await?;
    parse_pr_view(repo, &json)
}

/// What to fetch from the PR's base repo, and a suggested local branch name.
/// Same-repo PRs use the real branch (so you can push to it); fork PRs use
/// GitHub's `refs/pull/<n>/head` and a local `pr-<n>` branch.
pub fn fetch_refspec(pr: &PullRequest) -> (String, String) {
    if pr.is_cross_repository {
        (
            format!("refs/pull/{}/head", pr.number),
            format!("pr-{}", pr.number),
        )
    } else {
        (format!("refs/heads/{}", pr.head_ref), pr.head_ref.clone())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPr {
    number: u64,
    title: String,
    url: String,
    state: PrState,
    is_draft: bool,
    head_ref_name: String,
    base_ref_name: String,
    head_repository: Option<RawRepo>,
    head_repository_owner: Option<RawOwner>,
    is_cross_repository: bool,
    body: Option<String>,
}

#[derive(Deserialize)]
struct RawRepo {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct RawOwner {
    #[serde(default)]
    login: String,
}

/// Parses `gh pr view --json <PR_FIELDS>` output. `repo` is the repo the PR
/// was requested from.
fn parse_pr_view(repo: &RepoRef, json: &str) -> Result<PullRequest, GhError> {
    let raw: RawPr = serde_json::from_str(json)
        .map_err(|e| GhError::Parse(format!("pull request JSON: {e}")))?;
    let head_repo = match (raw.head_repository_owner, raw.head_repository) {
        (Some(owner), Some(repo)) if !owner.login.is_empty() && !repo.name.is_empty() => {
            Some(RepoRef::new(owner.login, repo.name))
        }
        _ => None,
    };
    Ok(PullRequest {
        repo: repo.clone(),
        number: raw.number,
        title: raw.title,
        url: raw.url,
        state: raw.state,
        is_draft: raw.is_draft,
        head_ref: raw.head_ref_name,
        base_ref: raw.base_ref_name,
        head_repo,
        is_cross_repository: raw.is_cross_repository,
        body: raw.body.unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAME_REPO: &str = r#"{
      "baseRefName": "main",
      "body": "Pairs with acme/web#12",
      "headRefName": "feat/login",
      "headRepository": {"id": "R_kgDOAbCdEf", "name": "api"},
      "headRepositoryOwner": {"id": "O_kgDOAbCdEf", "login": "acme"},
      "isCrossRepository": false,
      "isDraft": true,
      "number": 123,
      "state": "OPEN",
      "title": "Add login",
      "url": "https://github.com/acme/api/pull/123"
    }"#;

    const FORK: &str = r#"{
      "baseRefName": "main",
      "body": "",
      "headRefName": "main",
      "headRepository": {"id": "R_kgDOXyZ", "name": "api-fork"},
      "headRepositoryOwner": {"id": "MDQ6VXNlcjE=", "login": "someone", "name": "Some One"},
      "isCrossRepository": true,
      "isDraft": false,
      "number": 45,
      "state": "MERGED",
      "title": "Fix typo",
      "url": "https://github.com/acme/api/pull/45"
    }"#;

    const DELETED_FORK: &str = r#"{
      "baseRefName": "main",
      "body": null,
      "headRefName": "patch-1",
      "headRepository": null,
      "headRepositoryOwner": {"id": "", "login": ""},
      "isCrossRepository": true,
      "isDraft": false,
      "number": 7,
      "state": "CLOSED",
      "title": "Old",
      "url": "https://github.com/acme/api/pull/7"
    }"#;

    fn acme_api() -> RepoRef {
        RepoRef::new("acme", "api")
    }

    #[test]
    fn same_repo_pr() {
        let pr = parse_pr_view(&acme_api(), SAME_REPO).unwrap();
        assert_eq!(
            pr,
            PullRequest {
                repo: acme_api(),
                number: 123,
                title: "Add login".into(),
                url: "https://github.com/acme/api/pull/123".into(),
                state: PrState::Open,
                is_draft: true,
                head_ref: "feat/login".into(),
                base_ref: "main".into(),
                head_repo: Some(acme_api()),
                is_cross_repository: false,
                body: "Pairs with acme/web#12".into(),
            }
        );
        assert_eq!(
            fetch_refspec(&pr),
            (
                "refs/heads/feat/login".to_string(),
                "feat/login".to_string()
            )
        );
    }

    #[test]
    fn fork_pr() {
        let pr = parse_pr_view(&acme_api(), FORK).unwrap();
        assert_eq!(pr.state, PrState::Merged);
        assert_eq!(pr.head_repo, Some(RepoRef::new("someone", "api-fork")));
        assert_eq!(
            fetch_refspec(&pr),
            ("refs/pull/45/head".to_string(), "pr-45".to_string())
        );
    }

    #[test]
    fn deleted_fork_pr() {
        let pr = parse_pr_view(&acme_api(), DELETED_FORK).unwrap();
        assert_eq!(pr.state, PrState::Closed);
        assert_eq!(pr.head_repo, None);
        assert_eq!(pr.body, "");
        assert_eq!(fetch_refspec(&pr).1, "pr-7");

        let no_owner = DELETED_FORK.replace(r#"{"id": "", "login": ""}"#, "null");
        assert_eq!(
            parse_pr_view(&acme_api(), &no_owner).unwrap().head_repo,
            None
        );
    }

    #[test]
    fn bad_json() {
        let err = parse_pr_view(&acme_api(), r#"{"number": 1}"#).unwrap_err();
        assert!(matches!(err, GhError::Parse(_)), "{err:?}");
        let weird_state = SAME_REPO.replace("\"OPEN\"", "\"DRAFT\"");
        assert!(parse_pr_view(&acme_api(), &weird_state).is_err());
    }
}
