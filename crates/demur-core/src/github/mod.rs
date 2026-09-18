//! GitHub API client: pull request data, diffs, prior bot reviews, review
//! thread resolution over GraphQL, review publication, and check runs.
//! The token is least privilege and never appears in errors.

mod flow;
mod publish;

pub use publish::{Publication, publish_review};

#[cfg(test)]
mod tests;

pub use flow::{FlowError, FlowOutcome, review_pull_request};

use crate::delta::Marker;
use crate::provider::redact;
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;
use thiserror::Error;

/// Failures against the GitHub API, classified for the run.
#[derive(Debug, Error)]
pub enum GitHubError {
    /// The token was rejected. Fail fast with setup guidance.
    #[error(
        "github rejected the token: {message}\ncheck the token and its permissions: pull-requests: write, checks: write, contents: read"
    )]
    Auth {
        /// Redacted detail.
        message: String,
    },
    /// The token lacks a required permission. No partial publication.
    #[error("missing permission `{permission}`: {message}")]
    Permission {
        /// The permission that was missing.
        permission: &'static str,
        /// Redacted detail.
        message: String,
    },
    /// The request was understood and refused on its merits. Nothing about
    /// the token is wrong, so the forge's own reason is what matters.
    #[error("github refused the request: {message}")]
    Refused {
        /// What the forge said, verbatim.
        message: String,
    },
    /// Rate limited by GitHub. Retryable within bounds.
    #[error("github rate limited the request: {message}")]
    RateLimit {
        /// Redacted detail.
        message: String,
    },
    /// The referenced resource does not exist.
    #[error("github resource not found: {message}")]
    NotFound {
        /// Redacted detail.
        message: String,
    },
    /// Transport or server failure. Retryable within bounds.
    #[error("github request failed: {message}")]
    Request {
        /// Redacted detail.
        message: String,
    },
}

/// Minimal pull request data the pipeline needs.
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    /// Pull request number.
    pub number: u64,
    /// True while the pull request is a draft.
    pub draft: bool,
    /// Pull request title, which states what the change claims to do.
    #[serde(default)]
    pub title: String,
    /// Pull request body. Untrusted data like the diff, and delimited as
    /// such in prompts.
    #[serde(default)]
    pub body: Option<String>,
    /// Head commit SHA.
    #[serde(rename = "head")]
    head_refs: HeadRefs,
}

impl PullRequest {
    /// The head commit SHA under review.
    pub fn head_sha(&self) -> &str {
        &self.head_refs.sha
    }
}

#[derive(Debug, Clone, Deserialize)]
struct HeadRefs {
    sha: String,
}

/// A prior review on the pull request.
#[derive(Debug, Clone, Deserialize)]
pub struct PriorReview {
    /// Review id.
    pub id: u64,
    /// Author login.
    pub user: Option<User>,
    /// Review body text, which may carry a marker.
    pub body: Option<String>,
    /// Review state: APPROVED, CHANGES_REQUESTED, COMMENTED.
    pub state: Option<String>,
}

/// A GitHub user.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    /// Login name.
    pub login: Option<String>,
}

/// One inline review comment to publish.
#[derive(Debug, Clone)]
pub struct InlineComment {
    /// File path the comment anchors to.
    pub path: String,
    /// Line in the new file the comment anchors to.
    pub line: u32,
    /// Comment body, including any suggestion block and fingerprint
    /// marker.
    pub body: String,
}

/// The review event to submit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewEvent {
    /// Approving review.
    Approve,
    /// Request changes.
    RequestChanges,
    /// Comment only.
    Comment,
}

impl ReviewEvent {
    /// The GitHub API event name.
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewEvent::Approve => "APPROVE",
            ReviewEvent::RequestChanges => "REQUEST_CHANGES",
            ReviewEvent::Comment => "COMMENT",
        }
    }
}

/// The client. Only the endpoints this bot needs are implemented.
pub struct GitHubClient {
    http: reqwest::Client,
    api_base: String,
    token: String,
    owner: String,
    repo: String,
}

impl GitHubClient {
    /// Build a client for one repository.
    pub fn new(api_base: &str, token: String, owner: &str, repo: &str) -> GitHubClient {
        GitHubClient {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .user_agent("demur")
                .build()
                .expect("static client configuration"),
            api_base: api_base.trim_end_matches('/').to_string(),
            token,
            owner: owner.to_string(),
            repo: repo.to_string(),
        }
    }

    fn redacted(&self, text: &str) -> String {
        redact(text, &self.token)
    }

    async fn get(&self, path: &str, accept: &str) -> Result<reqwest::Response, GitHubError> {
        self.send(
            self.http
                .get(format!("{}{path}", self.api_base))
                .header(reqwest::header::ACCEPT, accept),
        )
        .await
    }

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<reqwest::Response, GitHubError> {
        self.send(
            self.http
                .post(format!("{}{path}", self.api_base))
                .json(&body),
        )
        .await
    }

    /// Post without interpreting a refusal, for a caller that has to
    /// classify one itself. The shared path turns every 403 into a
    /// permission failure, which is right for a call with one way to
    /// succeed and wrong for a review event, where a refusal costs the
    /// delivery mechanism rather than the review.
    async fn post_unclassified(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.http
            .post(format!("{}{path}", self.api_base))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
    }

    async fn send(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, GitHubError> {
        let builder = builder.bearer_auth(&self.token);
        match builder.send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                if status == 401 {
                    return Err(GitHubError::Auth {
                        message: "token rejected".to_string(),
                    });
                }
                if status == 403 || status == 429 {
                    let text = response.text().await.unwrap_or_default();
                    let secondary = text.contains("secondary rate");
                    return Err(if secondary || status == 429 {
                        GitHubError::RateLimit {
                            message: self.redacted(&crate::provider::excerpt(&text)),
                        }
                    } else {
                        GitHubError::Permission {
                            permission: "pull-requests: write or checks: write",
                            message: self.redacted(&crate::provider::excerpt(&text)),
                        }
                    });
                }
                if status == 404 {
                    return Err(GitHubError::NotFound {
                        message: "resource not found; check the repository and pull request"
                            .to_string(),
                    });
                }
                Ok(response)
            }
            Err(err) => Err(GitHubError::Request {
                message: self.redacted(&err.to_string()),
            }),
        }
    }

    async fn error_body(&self, response: reqwest::Response) -> String {
        let text = response.text().await.unwrap_or_default();
        self.redacted(&crate::provider::excerpt(&text))
    }

    /// Fetch pull request metadata.
    pub async fn pull_request(&self, number: u64) -> Result<PullRequest, GitHubError> {
        let response = self
            .get(
                &format!("/repos/{}/{}/pulls/{number}", self.owner, self.repo),
                "application/vnd.github+json",
            )
            .await?;
        if !response.status().is_success() {
            return Err(GitHubError::Request {
                message: self.error_body(response).await,
            });
        }
        response.json().await.map_err(|err| GitHubError::Request {
            message: self.redacted(&err.to_string()),
        })
    }

    /// Fetch the pull request's full unified diff.
    /// Fetch the unified diff of the pull request. When the diff exceeds
    /// GitHub's line limit (406 too large), it is assembled from the
    /// paginated per-file patches of the files API instead.
    pub async fn pull_request_diff(&self, number: u64) -> Result<String, GitHubError> {
        let response = self
            .get(
                &format!("/repos/{}/{}/pulls/{number}", self.owner, self.repo),
                "application/vnd.github.v3.diff",
            )
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            if status == 406 && is_diff_too_large(&text) {
                let files = self.pull_request_files(number).await?;
                return Ok(synthesize_diff(&files));
            }
            return Err(GitHubError::Request {
                message: self.redacted(&crate::provider::excerpt(&text)),
            });
        }
        response.text().await.map_err(|err| GitHubError::Request {
            message: self.redacted(&err.to_string()),
        })
    }

    /// The pull request's changed files with their per-file patches,
    /// paginated up to the API's 3000 file ceiling.
    pub async fn pull_request_files(&self, number: u64) -> Result<Vec<PrFile>, GitHubError> {
        let mut all = Vec::new();
        for page in 1..=30 {
            let response = self
                .get(
                    &format!(
                        "/repos/{}/{}/pulls/{number}/files?per_page=100&page={page}",
                        self.owner, self.repo
                    ),
                    "application/vnd.github+json",
                )
                .await?;
            if !response.status().is_success() {
                return Err(GitHubError::Request {
                    message: self.error_body(response).await,
                });
            }
            let mut page_files: Vec<PrFile> =
                response.json().await.map_err(|err| GitHubError::Request {
                    message: self.redacted(&err.to_string()),
                })?;
            let done = page_files.len() < 100;
            all.append(&mut page_files);
            if done {
                break;
            }
        }
        Ok(all)
    }

    /// Fetch the unified diff of the commits between two heads, used for
    /// delta reviews. When that diff exceeds GitHub's line limit, it is
    /// assembled from the compare files list; when the compare files list
    /// itself is truncated at its 300 file ceiling, the full pull request
    /// file list widens the scope instead, because a wider review is the
    /// fail-safe direction for a delta.
    pub async fn compare_diff(
        &self,
        number: u64,
        from_sha: &str,
        to_sha: &str,
    ) -> Result<String, GitHubError> {
        let response = self
            .get(
                &format!(
                    "/repos/{}/{}/compare/{from_sha}...{to_sha}",
                    self.owner, self.repo
                ),
                "application/vnd.github.v3.diff",
            )
            .await?;
        if response.status().is_success() {
            return response.text().await.map_err(|err| GitHubError::Request {
                message: self.redacted(&err.to_string()),
            });
        }
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        if status != 406 || !is_diff_too_large(&text) {
            return Err(GitHubError::Request {
                message: self.redacted(&crate::provider::excerpt(&text)),
            });
        }
        let response = self
            .get(
                &format!(
                    "/repos/{}/{}/compare/{from_sha}...{to_sha}",
                    self.owner, self.repo
                ),
                "application/vnd.github+json",
            )
            .await?;
        if !response.status().is_success() {
            return Err(GitHubError::Request {
                message: self.error_body(response).await,
            });
        }
        #[derive(Deserialize)]
        struct CompareFiles {
            files: Vec<PrFile>,
        }
        let compare: CompareFiles = response.json().await.map_err(|err| GitHubError::Request {
            message: self.redacted(&err.to_string()),
        })?;
        if compare.files.len() >= 300 {
            let files = self.pull_request_files(number).await?;
            return Ok(synthesize_diff(&files));
        }
        Ok(synthesize_diff(&compare.files))
    }

    /// True when `prior` is an ancestor of `current` (or identical).
    pub async fn is_ancestor(&self, prior: &str, current: &str) -> Result<bool, GitHubError> {
        let response = self
            .get(
                &format!(
                    "/repos/{}/{}/compare/{prior}...{current}",
                    self.owner, self.repo
                ),
                "application/vnd.github+json",
            )
            .await?;
        if !response.status().is_success() {
            return Err(GitHubError::Request {
                message: self.error_body(response).await,
            });
        }
        #[derive(Deserialize)]
        struct Compare {
            status: String,
        }
        let compare: Compare = response.json().await.map_err(|err| GitHubError::Request {
            message: self.redacted(&err.to_string()),
        })?;
        Ok(compare.status == "ahead" || compare.status == "identical")
    }

    /// List reviews on the pull request.
    pub async fn reviews(&self, number: u64) -> Result<Vec<PriorReview>, GitHubError> {
        let response = self
            .get(
                &format!("/repos/{}/{}/pulls/{number}/reviews", self.owner, self.repo),
                "application/vnd.github+json",
            )
            .await?;
        if !response.status().is_success() {
            return Err(GitHubError::Request {
                message: self.error_body(response).await,
            });
        }
        response.json().await.map_err(|err| GitHubError::Request {
            message: self.redacted(&err.to_string()),
        })
    }

    /// The newest decodable marker from the bot's own prior reviews,
    /// chosen by the highest recorded run count so the result does not
    /// depend on the API's list order. Reviews without a decodable marker
    /// are ignored, so stripped or tampered bodies degrade to a full
    /// review.
    pub async fn prior_marker(&self, number: u64) -> Result<Option<Marker>, GitHubError> {
        let reviews = self.reviews(number).await?;
        Ok(reviews
            .iter()
            .filter_map(|review| review.body.as_deref())
            .filter_map(Marker::decode)
            .max_by_key(|marker| marker.run_count))
    }

    /// The fingerprints of findings whose review threads a human resolved.
    /// When resolution state cannot be read, the set is empty, which treats
    /// every finding as unresolved: a repeated comment is safer than a
    /// silently cleared gate.
    pub async fn dismissed_fingerprints(&self, number: u64) -> HashSet<String> {
        let query = r#"query($owner:String!,$repo:String!,$number:Int!){
            repository(owner:$owner,name:$repo){
                pullRequest(number:$number){
                    reviewThreads(first:100){
                        nodes{
                            isResolved
                            comments(first:1){nodes{body}}
                        }
                    }
                }
            }
        }"#;
        let body = json!({
            "query": query,
            "variables": {
                "owner": self.owner,
                "repo": self.repo,
                "number": number as i64,
            }
        });
        let Ok(response) = self.post("/graphql", body).await else {
            return HashSet::new();
        };
        if !response.status().is_success() {
            return HashSet::new();
        }
        #[derive(Deserialize)]
        struct GraphQlResponse {
            data: Option<GraphQlData>,
        }
        #[derive(Deserialize)]
        struct GraphQlData {
            repository: Option<GraphQlRepository>,
        }
        #[derive(Deserialize)]
        struct GraphQlRepository {
            #[serde(rename = "pullRequest")]
            pull_request: Option<GraphQlPullRequest>,
        }
        #[derive(Deserialize)]
        struct GraphQlPullRequest {
            #[serde(rename = "reviewThreads")]
            review_threads: Option<GraphQlThreads>,
        }
        #[derive(Deserialize)]
        struct GraphQlThreads {
            nodes: Vec<GraphQlThread>,
        }
        #[derive(Deserialize)]
        struct GraphQlThread {
            #[serde(rename = "isResolved")]
            is_resolved: bool,
            comments: GraphQlComments,
        }
        #[derive(Deserialize)]
        struct GraphQlComments {
            nodes: Vec<GraphQlComment>,
        }
        #[derive(Deserialize)]
        struct GraphQlComment {
            body: String,
        }
        let Ok(parsed) = response.json::<GraphQlResponse>().await else {
            return HashSet::new();
        };
        let Some(data) = parsed.data else {
            return HashSet::new();
        };
        let mut dismissed = HashSet::new();
        if let Some(threads) = data
            .repository
            .and_then(|repo| repo.pull_request)
            .and_then(|pr| pr.review_threads)
        {
            for thread in threads.nodes {
                if !thread.is_resolved {
                    continue;
                }
                for comment in thread.comments.nodes {
                    if let Some(fingerprint) = comment_fingerprint(&comment.body) {
                        dismissed.insert(fingerprint);
                    }
                }
            }
        }
        dismissed
    }

    /// The login the token authenticates as, used when the CLI states the
    /// identity it would publish under.
    pub async fn authenticated_login(&self) -> Result<String, GitHubError> {
        let response = self.get("/user", "application/vnd.github+json").await?;
        if !response.status().is_success() {
            return Err(GitHubError::Request {
                message: self.error_body(response).await,
            });
        }
        #[derive(Deserialize)]
        struct AuthUser {
            login: String,
        }
        let user: AuthUser = response.json().await.map_err(|err| GitHubError::Request {
            message: self.redacted(&err.to_string()),
        })?;
        Ok(user.login)
    }

    /// Submit one review event with a body and inline comments. Returns
    /// false when the identity may not submit approvals, in which case the
    /// caller falls back to a comment review.
    pub async fn create_review(
        &self,
        number: u64,
        event: ReviewEvent,
        body: &str,
        comments: &[InlineComment],
    ) -> Result<(), GitHubError> {
        let comments: Vec<serde_json::Value> = comments
            .iter()
            .map(|comment| {
                json!({
                    "path": comment.path,
                    "line": comment.line,
                    "side": "RIGHT",
                    "body": comment.body,
                })
            })
            .collect();
        let response = self
            .post_unclassified(
                &format!("/repos/{}/{}/pulls/{number}/reviews", self.owner, self.repo),
                json!({"event": event.as_str(), "body": body, "comments": comments}),
            )
            .await
            .map_err(|err| GitHubError::Request {
                message: self.redacted(&err.to_string()),
            });
        match response {
            Ok(response) if response.status().is_success() => Ok(()),
            Ok(response) => {
                let status = response.status().as_u16();
                let detail = self.error_body(response).await;
                match status {
                    // A rejected token is not a refused event.
                    401 => Err(GitHubError::Auth {
                        message: "token rejected".to_string(),
                    }),
                    // A review event is a delivery mechanism, and the
                    // verdict is already carried by the check run, which
                    // cannot be refused. So any refusal costs the
                    // mechanism, never the review.
                    403 | 422 if event != ReviewEvent::Comment => {
                        Err(GitHubError::Refused { message: detail })
                    }
                    // A comment review is the fallback. If that is refused
                    // too there is nothing left to try.
                    403 => Err(GitHubError::Permission {
                        permission: "pull-requests: write",
                        message: detail,
                    }),
                    422 => Err(GitHubError::Refused { message: detail }),
                    _ => Err(GitHubError::Request { message: detail }),
                }
            }
            Err(err) => Err(err),
        }
    }

    /// Create the check run whose conclusion carries the verdict.
    pub async fn create_check_run(
        &self,
        head_sha: &str,
        conclusion: &str,
        title: &str,
        summary: &str,
    ) -> Result<(), GitHubError> {
        let response = self
            .post(
                &format!("/repos/{}/{}/check-runs", self.owner, self.repo),
                json!({
                    "name": "demur",
                    "head_sha": head_sha,
                    "status": "completed",
                    "conclusion": conclusion,
                    "output": {"title": title, "summary": summary},
                }),
            )
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let detail = self.error_body(response).await;
            return Err(if status == 403 {
                GitHubError::Permission {
                    permission: "checks: write",
                    message: detail,
                }
            } else {
                GitHubError::Request { message: detail }
            });
        }
        Ok(())
    }
}

/// One changed file from the files API, with its per-file patch when
/// GitHub could produce one. Binary and oversized files have no patch and
/// are skipped when a diff is assembled from this list.
#[derive(Debug, Clone, Deserialize)]
pub struct PrFile {
    /// Path after the change.
    #[serde(rename = "filename")]
    pub path: String,
    /// Path before the change, for renames.
    #[serde(rename = "previous_filename")]
    pub previous_path: Option<String>,
    /// added, removed, modified, renamed, or changed.
    pub status: String,
    /// The unified diff hunks for this file, when available.
    #[serde(default)]
    pub patch: Option<String>,
}

/// True when the API rejected the diff for exceeding its line limit.
fn is_diff_too_large(body: &str) -> bool {
    body.contains("too_large") || body.contains("exceeded the maximum number of lines")
}

/// Assemble a unified diff from per-file patches. Files without a patch
/// (binary or oversized) are skipped.
pub fn synthesize_diff(files: &[PrFile]) -> String {
    let mut out = String::new();
    for file in files {
        let Some(patch) = &file.patch else {
            continue;
        };
        let old = file.previous_path.as_ref().unwrap_or(&file.path);
        out.push_str(&format!("diff --git a/{old} b/{}\n", file.path));
        match file.status.as_str() {
            "added" => out.push_str("new file mode 100644\n"),
            "removed" => out.push_str("deleted file mode 100644\n"),
            "renamed" => {
                out.push_str(&format!("rename from {old}\n"));
                out.push_str(&format!("rename to {}\n", file.path));
            }
            _ => {}
        }
        let old_side = if file.status == "added" {
            "/dev/null".to_string()
        } else {
            format!("a/{old}")
        };
        let new_side = if file.status == "removed" {
            "/dev/null".to_string()
        } else {
            format!("b/{}", file.path)
        };
        out.push_str(&format!("--- {old_side}\n+++ {new_side}\n"));
        out.push_str(patch.trim_end());
        out.push('\n');
    }
    out
}

/// Extract a fingerprint from an inline comment's hidden marker.
pub fn comment_fingerprint(body: &str) -> Option<String> {
    const PREFIX: &str = "<!-- demur:fp ";
    let start = body.find(PREFIX)? + PREFIX.len();
    let rest = &body[start..];
    let end = rest.find(" -->")?;
    Some(rest[..end].to_string())
}
