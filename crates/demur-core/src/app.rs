//! Authorizing demur to act on a user's behalf.
//!
//! A review published with an authorization obtained this way is attributed
//! to the user who granted it and carries demur's mark beside their name.
//! A credential belonging to demur itself would publish reviews authored by
//! demur instead, which is the opposite of what this is for.

use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Where the code-entry flow starts and finishes on github.com.
const DEFAULT_ENDPOINT: &str = "https://github.com";

/// The web host an authorization flow runs against, derived from what the
/// environment says about which GitHub this is. An installation that is not
/// github.com serves the flow from its own host, so hardcoding the public
/// one locks enterprise users out of authorizing entirely.
///
/// Taken as arguments rather than read here so the derivation can be tested
/// without an environment.
pub fn web_host(server_url: Option<&str>, api_url: Option<&str>) -> String {
    // Workflows are told the server URL outright.
    if let Some(server) = server_url.map(str::trim).filter(|value| !value.is_empty()) {
        return server.trim_end_matches('/').to_string();
    }
    let Some(api) = api_url.map(str::trim).filter(|value| !value.is_empty()) else {
        return DEFAULT_ENDPOINT.to_string();
    };
    let api = api.trim_end_matches('/');
    if api == "https://api.github.com" {
        return DEFAULT_ENDPOINT.to_string();
    }
    // An enterprise instance serves its API under the same host as its web
    // interface.
    api.strip_suffix("/api/v3").unwrap_or(api).to_string()
}

/// The web host for this process, from the environment.
pub fn web_host_from_env() -> String {
    web_host(
        std::env::var("GITHUB_SERVER_URL").ok().as_deref(),
        std::env::var("GITHUB_API_URL").ok().as_deref(),
    )
}

/// Longest a run waits for someone to finish authorizing.
const AUTHORIZE_TIMEOUT: Duration = Duration::from_secs(300);

/// Renew this long before expiry, so a publication does not race the clock.
const RENEW_MARGIN: Duration = Duration::from_secs(120);

/// Failures of authorizing or of using an authorization.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// The user declined, or did not finish in time.
    #[error("authorization was not granted: {0}")]
    NotGranted(String),
    /// The service refused the request.
    #[error("authorization failed: {0}")]
    Refused(String),
    /// Transport failure.
    #[error("could not reach the authorization service: {0}")]
    Unreachable(String),
}

/// An authorization, with what is needed to renew it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Authorization {
    /// The credential used for calls. Never logged, never published.
    pub token: String,
    /// Used to renew without asking again. Absent when the service issues
    /// authorizations that do not expire.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// When the token stops working, as seconds since the epoch. Absent
    /// means it does not expire.
    #[serde(default)]
    pub expires_at: Option<u64>,
}

impl Authorization {
    /// True when this should be renewed before use. An authorization about
    /// to expire is treated as expired, so a publication does not fail
    /// halfway through on a clock it could have checked first.
    pub fn needs_renewal(&self) -> bool {
        let Some(expires_at) = self.expires_at else {
            return false;
        };
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0);
        expires_at <= now.saturating_add(RENEW_MARGIN.as_secs())
    }

    /// Read a kept authorization. Anything unreadable or damaged reads as
    /// absent, because asking the user again is better than failing a
    /// publication they already paid for.
    pub fn read(path: &Path) -> Option<Authorization> {
        let text = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Keep an authorization where the user named, readable only by them.
    /// Failure to write is not failure to publish.
    pub fn write(&self, path: &Path) -> Result<(), std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)?;
        restrict_to_owner(path)
    }
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<(), std::io::Error> {
    Ok(())
}

/// What the user must do to grant an authorization.
#[derive(Debug, Clone)]
pub struct Prompt {
    /// Where to go.
    pub verification_uri: String,
    /// What to enter there.
    pub user_code: String,
}

/// Obtains and renews authorizations through the code-entry flow, which
/// needs no redirect, no local server, and no browser on the machine
/// running it.
pub struct Authorizer {
    http: reqwest::Client,
    endpoint: String,
    client_id: String,
}

impl Authorizer {
    /// Build an authorizer for an application, against whichever GitHub
    /// this process is pointed at.
    pub fn new(client_id: &str) -> Authorizer {
        Authorizer {
            http: reqwest::Client::new(),
            endpoint: web_host_from_env(),
            client_id: client_id.to_string(),
        }
    }

    /// Point the flow somewhere else. For tests.
    pub fn with_endpoint(mut self, endpoint: &str) -> Authorizer {
        self.endpoint = endpoint.trim_end_matches('/').to_string();
        self
    }

    /// Ask the service to start an authorization, returning what to show
    /// the user and the handle to wait on.
    pub async fn begin(&self) -> Result<(Prompt, Pending), AppError> {
        let response: DeviceCode = self
            .form(
                "/login/device/code",
                &[("client_id", self.client_id.as_str())],
            )
            .await?;
        Ok((
            Prompt {
                verification_uri: response.verification_uri.clone(),
                user_code: response.user_code.clone(),
            },
            Pending {
                device_code: response.device_code,
                interval: Duration::from_secs(response.interval.unwrap_or(5).max(1)),
            },
        ))
    }

    /// Wait for the user to finish. Returns when granted, when declined,
    /// or when they have taken too long.
    pub async fn wait(&self, pending: &Pending) -> Result<Authorization, AppError> {
        let deadline = SystemTime::now() + AUTHORIZE_TIMEOUT;
        loop {
            tokio::time::sleep(pending.interval).await;
            let response: TokenResponse = self
                .form(
                    "/login/oauth/access_token",
                    &[
                        ("client_id", self.client_id.as_str()),
                        ("device_code", pending.device_code.as_str()),
                        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ],
                )
                .await?;
            match response.into_outcome() {
                Outcome::Granted(authorization) => return Ok(authorization),
                Outcome::KeepWaiting => {
                    if SystemTime::now() >= deadline {
                        return Err(AppError::NotGranted(
                            "no response within the time allowed".to_string(),
                        ));
                    }
                }
                Outcome::Declined(reason) => return Err(AppError::NotGranted(reason)),
                Outcome::Failed(reason) => return Err(AppError::Refused(reason)),
            }
        }
    }

    /// Renew without involving the user. An expired authorization is a fact
    /// about elapsed time, not a problem they caused.
    pub async fn renew(&self, authorization: &Authorization) -> Result<Authorization, AppError> {
        let Some(refresh) = authorization.refresh_token.as_deref() else {
            return Err(AppError::NotGranted(
                "this authorization cannot be renewed".to_string(),
            ));
        };
        let response: TokenResponse = self
            .form(
                "/login/oauth/access_token",
                &[
                    ("client_id", self.client_id.as_str()),
                    ("refresh_token", refresh),
                    ("grant_type", "refresh_token"),
                ],
            )
            .await?;
        match response.into_outcome() {
            Outcome::Granted(renewed) => Ok(renewed),
            Outcome::Declined(reason) | Outcome::Failed(reason) => {
                Err(AppError::NotGranted(reason))
            }
            Outcome::KeepWaiting => Err(AppError::Refused("renewal did not complete".to_string())),
        }
    }

    async fn form<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<T, AppError> {
        let response = self
            .http
            .post(format!("{}{path}", self.endpoint))
            .header("accept", "application/json")
            .form(params)
            .send()
            .await
            .map_err(|err| AppError::Unreachable(err.to_string()))?;
        response
            .json::<T>()
            .await
            .map_err(|err| AppError::Refused(err.to_string()))
    }
}

/// An authorization in progress.
#[derive(Debug, Clone)]
pub struct Pending {
    device_code: String,
    interval: Duration,
}

#[derive(Debug, Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

enum Outcome {
    Granted(Authorization),
    KeepWaiting,
    Declined(String),
    Failed(String),
}

impl TokenResponse {
    fn into_outcome(self) -> Outcome {
        if let Some(token) = self.access_token {
            let expires_at = self.expires_in.map(|seconds| {
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|since| since.as_secs())
                    .unwrap_or(0)
                    .saturating_add(seconds)
            });
            return Outcome::Granted(Authorization {
                token,
                refresh_token: self.refresh_token,
                expires_at,
            });
        }
        let reason = self
            .error_description
            .or_else(|| self.error.clone())
            .unwrap_or_else(|| "no reason given".to_string());
        match self.error.as_deref() {
            Some("authorization_pending") | Some("slow_down") => Outcome::KeepWaiting,
            Some("access_denied") | Some("expired_token") => Outcome::Declined(reason),
            _ => Outcome::Failed(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn authorizer(server: &MockServer) -> Authorizer {
        Authorizer::new("Iv1.test").with_endpoint(&server.uri())
    }

    async fn mount_device_code(server: &MockServer) {
        Mock::given(method("POST"))
            .and(path("/login/device/code"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "device_code": "dev-123",
                "user_code": "WDJB-MJHT",
                "verification_uri": "https://github.com/login/device",
                "interval": 0
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn the_user_is_told_where_to_go_and_what_to_enter() {
        let server = MockServer::start().await;
        mount_device_code(&server).await;
        let (prompt, _) = authorizer(&server).begin().await.unwrap();
        assert_eq!(prompt.user_code, "WDJB-MJHT");
        assert_eq!(prompt.verification_uri, "https://github.com/login/device");
    }

    #[tokio::test]
    async fn a_granted_authorization_carries_its_expiry() {
        let server = MockServer::start().await;
        mount_device_code(&server).await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "ghu_granted",
                "refresh_token": "ghr_renew",
                "expires_in": 28800
            })))
            .mount(&server)
            .await;
        let authorizer = authorizer(&server);
        let (_, pending) = authorizer.begin().await.unwrap();
        let granted = authorizer.wait(&pending).await.unwrap();
        assert_eq!(granted.token, "ghu_granted");
        assert_eq!(granted.refresh_token.as_deref(), Some("ghr_renew"));
        assert!(granted.expires_at.is_some());
        assert!(!granted.needs_renewal());
    }

    #[tokio::test]
    async fn waiting_continues_until_the_user_finishes() {
        let server = MockServer::start().await;
        mount_device_code(&server).await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": "authorization_pending"
            })))
            .up_to_n_times(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"access_token": "ghu_late"})),
            )
            .mount(&server)
            .await;
        let authorizer = authorizer(&server);
        let (_, pending) = authorizer.begin().await.unwrap();
        assert_eq!(authorizer.wait(&pending).await.unwrap().token, "ghu_late");
    }

    #[tokio::test]
    async fn a_declined_authorization_stops_waiting() {
        let server = MockServer::start().await;
        mount_device_code(&server).await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": "access_denied",
                "error_description": "the user declined"
            })))
            .mount(&server)
            .await;
        let authorizer = authorizer(&server);
        let (_, pending) = authorizer.begin().await.unwrap();
        let error = authorizer.wait(&pending).await.unwrap_err();
        assert!(matches!(error, AppError::NotGranted(_)), "{error}");
        assert!(error.to_string().contains("declined"), "{error}");
    }

    #[tokio::test]
    async fn an_abandoned_authorization_stops_waiting() {
        let server = MockServer::start().await;
        mount_device_code(&server).await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": "expired_token",
                "error_description": "this code has expired"
            })))
            .mount(&server)
            .await;
        let authorizer = authorizer(&server);
        let (_, pending) = authorizer.begin().await.unwrap();
        assert!(matches!(
            authorizer.wait(&pending).await.unwrap_err(),
            AppError::NotGranted(_)
        ));
    }

    #[tokio::test]
    async fn renewal_does_not_involve_the_user() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "ghu_renewed",
                "refresh_token": "ghr_next",
                "expires_in": 28800
            })))
            .mount(&server)
            .await;
        let stale = Authorization {
            token: "ghu_old".to_string(),
            refresh_token: Some("ghr_renew".to_string()),
            expires_at: Some(0),
        };
        assert!(stale.needs_renewal());
        let renewed = authorizer(&server).renew(&stale).await.unwrap();
        assert_eq!(renewed.token, "ghu_renewed");
        assert!(!renewed.needs_renewal());
    }

    #[tokio::test]
    async fn a_revoked_authorization_cannot_be_renewed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "error": "bad_refresh_token",
                "error_description": "the authorization was revoked"
            })))
            .mount(&server)
            .await;
        let revoked = Authorization {
            token: "ghu_old".to_string(),
            refresh_token: Some("ghr_revoked".to_string()),
            expires_at: Some(0),
        };
        let error = authorizer(&server).renew(&revoked).await.unwrap_err();
        assert!(matches!(error, AppError::NotGranted(_)), "{error}");
    }

    #[test]
    fn the_public_instance_uses_the_public_host() {
        assert_eq!(web_host(None, None), "https://github.com");
        assert_eq!(
            web_host(None, Some("https://api.github.com")),
            "https://github.com"
        );
    }

    #[test]
    fn an_enterprise_instance_authorizes_against_itself() {
        // Hardcoding the public host locks enterprise users out of
        // authorizing at all.
        assert_eq!(
            web_host(None, Some("https://ghe.example.com/api/v3")),
            "https://ghe.example.com"
        );
        assert_eq!(
            web_host(None, Some("https://ghe.example.com/api/v3/")),
            "https://ghe.example.com"
        );
    }

    #[test]
    fn a_stated_server_url_wins() {
        assert_eq!(
            web_host(
                Some("https://ghe.example.com/"),
                Some("https://api.github.com")
            ),
            "https://ghe.example.com"
        );
        // Empty is the same as unset.
        assert_eq!(
            web_host(Some("  "), Some("https://api.github.com")),
            "https://github.com"
        );
    }

    #[test]
    fn an_authorization_without_an_expiry_never_needs_renewal() {
        let forever = Authorization {
            token: "ghu".to_string(),
            refresh_token: None,
            expires_at: None,
        };
        assert!(!forever.needs_renewal());
    }

    #[test]
    fn one_about_to_expire_is_treated_as_expired() {
        // Renewing early beats failing halfway through publishing.
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let nearly = Authorization {
            token: "ghu".to_string(),
            refresh_token: None,
            expires_at: Some(now + 30),
        };
        assert!(nearly.needs_renewal());
    }

    #[test]
    fn a_kept_authorization_round_trips_and_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("token.json");
        let authorization = Authorization {
            token: "ghu_secret".to_string(),
            refresh_token: Some("ghr".to_string()),
            expires_at: Some(99),
        };
        authorization.write(&path).unwrap();
        let read = Authorization::read(&path).expect("round trips");
        assert_eq!(read.token, "ghu_secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "no other user may read it: {mode:o}");
        }
    }

    #[test]
    fn nothing_here_authenticates_as_the_application() {
        // The application can be registered without a private key, which is
        // what stops its owner from acting on every installation. That holds
        // only while no code path wants one.
        // Scan the implementation, not this list of words describing it.
        let implementation = |source: &'static str| -> &'static str {
            source.split("#[cfg(test)]").next().unwrap_or(source)
        };
        let auth = implementation(include_str!("app.rs"));
        let config = implementation(include_str!("config.rs"));
        for forbidden in ["private", "installations/", "app_id", "jwt"] {
            for (name, source) in [("app.rs", auth), ("config.rs", config)] {
                assert!(
                    !source.contains(forbidden),
                    "{name}: `{forbidden}` suggests a path that acts as the application itself"
                );
            }
        }
        // What the flow needs is the public identifier and nothing else.
        assert!(auth.contains("client_id"));
        assert!(config.contains("client_id"));
    }

    #[test]
    fn a_damaged_authorization_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(Authorization::read(&path).is_none());
        assert!(Authorization::read(&dir.path().join("missing.json")).is_none());
    }
}
