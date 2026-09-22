//! Publication: exactly one review event carrying the synthesized verdict,
//! inline comments with suggestion blocks, and the check run whose
//! conclusion reflects that verdict. Nothing partial is published.

use super::{GitHubClient, GitHubError, InlineComment, ReviewEvent};
use crate::delta::Marker;
use crate::pipeline::synthesis::Verdict;

/// How publication went.
#[derive(Debug, Clone)]
pub struct Publication {
    /// The event actually submitted, after any fallback.
    pub event_submitted: ReviewEvent,
    /// True when the approving review was rejected and a comment review
    /// carried the same body instead.
    pub fallback_used: bool,
}

/// Publish the review and the check run. The verdict is an input here:
/// publication never recomputes it. Inline comments are only created for
/// findings anchored in the current diff; carried findings live in the
/// body so they are never posted twice.
pub async fn publish_review(
    client: &GitHubClient,
    number: u64,
    head_sha: &str,
    verdict: Verdict,
    body: &str,
    marker: Option<&Marker>,
    comments: &[InlineComment],
) -> Result<Publication, GitHubError> {
    let body_with_marker = match marker {
        // An oversized marker ships no marker at all, which makes the next
        // run a full review, rather than breaking publication.
        Some(marker) => match marker.encode_bounded() {
            Some(encoded) => format!("{}\n\n{}", body, encoded),
            None => body.to_string(),
        },
        None => body.to_string(),
    };
    let event = match verdict {
        Verdict::Approve => ReviewEvent::Approve,
        Verdict::RequestChanges => ReviewEvent::RequestChanges,
    };
    // A review event is only how the verdict is delivered. The check run
    // carries the verdict itself and cannot be refused, so a refusal costs
    // the delivery mechanism and never the review.
    let (event_submitted, fallback_used, body) = match client
        .create_review(number, event, &body_with_marker, comments)
        .await
    {
        Ok(()) => (event, false, body_with_marker),
        Err(GitHubError::Refused { message }) => {
            let comment_body = format!(
                "{body_with_marker}\n\nNote: a {} review could not be submitted, so this \
review was posted as a comment. The check run carries the verdict.\nGitHub said: {message}",
                event.as_str()
            );
            client
                .create_review(number, ReviewEvent::Comment, &comment_body, comments)
                .await?;
            (ReviewEvent::Comment, true, comment_body)
        }
        Err(err) => return Err(err),
    };

    let (title, conclusion) = match verdict {
        Verdict::Approve => ("demur: no case against merging", "success"),
        Verdict::RequestChanges => ("demur: changes requested", "failure"),
    };
    client
        .create_check_run(head_sha, conclusion, title, &body)
        .await?;
    Ok(Publication {
        event_submitted,
        fallback_used,
    })
}

/// Publish an explanatory notice as a comment review carrying the state
/// marker when one fits, plus a check run whose title and conclusion say
/// what happened. The marker keeps carried state and recorded spend alive
/// for the next run.
pub async fn publish_notice(
    client: &GitHubClient,
    number: u64,
    head_sha: &str,
    body: &str,
    marker: Option<&Marker>,
    title: &'static str,
    conclusion: &'static str,
) -> Result<(), GitHubError> {
    let body_with_marker = match marker {
        Some(marker) => match marker.encode_bounded() {
            Some(encoded) => format!("{}\n\n{}", body, encoded),
            None => body.to_string(),
        },
        None => body.to_string(),
    };
    client
        .create_review(number, ReviewEvent::Comment, &body_with_marker, &[])
        .await?;
    client
        .create_check_run(head_sha, conclusion, title, &body_with_marker)
        .await?;
    Ok(())
}
