//! Origin providers: where a mirror's truth lives and how we reach it.
//!
//! `GithubApp` is the production integration — RS256 app JWT → installation
//! access token → authenticated fetch URL — with every endpoint base
//! configurable so the hermetic fakes serve it in tests. `Generic` mirrors
//! any URL git can fetch (public https, file://, or URLs with embedded
//! credentials). GitLab is deliberately just another shape of this trait
//! (webhook format + token exchange); only GitHub ships in v1.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use stratum_control::registry::Repo;

/// A push-shaped event parsed from a verified webhook delivery.
#[derive(Debug, Clone)]
pub struct WebhookEvent {
    /// Origin identity, e.g. "acme/widget" for GitHub.
    pub full_name: String,
}

pub trait OriginProvider: Send + Sync {
    /// Verify a webhook delivery's signature and parse the event.
    fn verify_webhook(&self, signature: Option<&str>, body: &[u8]) -> Result<WebhookEvent, String>;

    /// Verify a delivery's signature and nothing else — for the events
    /// that are not a push and carry no `repository` to parse.
    fn verify_signature(&self, signature: Option<&str>, body: &[u8]) -> Result<(), String>;

    /// A URL `git fetch` can use right now (credentials embedded when the
    /// origin requires them; short-lived tokens re-minted as needed).
    fn fetch_url(&self, repo: &Repo) -> Result<String, String>;

    /// Whether the URL [`fetch_url`](Self::fetch_url) builds could carry
    /// a push, as far as this side can tell without asking.
    ///
    /// Default `true`: a generic origin's URL is whatever the person
    /// pasted, credentials and all, and only the origin can say whether
    /// it writes. The GitHub App knows more — see its answer.
    fn can_push(&self, _repo: &Repo) -> bool {
        true
    }

    /// What the origin says about a repository, if it says anything.
    ///
    /// Default `Ok(None)`, and that is the honest answer for a provider
    /// with no metadata API: a bare git URL over HTTPS can be fetched
    /// and cannot be interviewed. Returning a zero here instead would
    /// let every generic mirror publish "0 upstream" as though it had
    /// asked and been told.
    ///
    /// `Err` is for "we asked and it went wrong", which callers treat as
    /// "we do not know" — never as zero.
    fn repo_meta(&self, _origin_url: &str) -> Result<Option<RemoteRepo>, String> {
        Ok(None)
    }
}

/// Mirrors any git-fetchable URL. Webhooks are verified against the
/// server-wide webhook secret (generic webhook: JSON `{"full_name": …}`).
pub struct Generic {
    pub webhook_secret: String,
}

impl OriginProvider for Generic {
    fn verify_signature(&self, signature: Option<&str>, body: &[u8]) -> Result<(), String> {
        verify_hmac_sig(&self.webhook_secret, signature, body)
    }

    fn verify_webhook(&self, signature: Option<&str>, body: &[u8]) -> Result<WebhookEvent, String> {
        self.verify_signature(signature, body)?;
        let v: serde_json::Value =
            serde_json::from_slice(body).map_err(|e| format!("webhook body: {e}"))?;
        let full_name = v["full_name"]
            .as_str()
            .or_else(|| v["repository"]["full_name"].as_str())
            .ok_or("webhook body has no full_name")?;
        Ok(WebhookEvent {
            full_name: full_name.to_string(),
        })
    }

    fn fetch_url(&self, repo: &Repo) -> Result<String, String> {
        repo.origin_url
            .clone()
            .ok_or_else(|| "mirror has no origin_url".into())
    }
}

/// Why the person behind an install redirect could not be matched to
/// an installation. The two are answered differently: a refusal is
/// final for this trip, an unanswered GitHub is worth trying again.
#[derive(Debug)]
pub enum UserAuthError {
    /// GitHub answered, and said the code is not one it issued — spent,
    /// forged, or minted for another App.
    Refused(String),
    /// GitHub did not answer, or answered something that is not an
    /// answer. Nothing is known about the person either way.
    Unanswered(String),
}

/// GitHub App integration: private repos stay private (M5) because every
/// fetch uses a short-lived installation token minted from the app key.
pub struct GithubApp {
    pub app_id: String,
    pub private_key_pem: String,
    /// API base, default `https://api.github.com`.
    pub api_base: String,
    /// Git base, default `https://github.com`.
    pub git_base: String,
    pub webhook_secret: String,
    /// The App's OAuth client, when the App requests user authorization
    /// during installation. Set, the install callback can prove that the
    /// person arriving with an `installation_id` controls it, by asking
    /// GitHub what that person's own token may see; unset, the callback
    /// has only the `state` to go on, which proves the org and not the
    /// installation — acceptable on a single-tenant deployment, a
    /// cross-tenant read on a public one.
    pub user_auth: Option<UserAuth>,
    /// installation id -> (token, expiry). Tokens live ~1h; refresh with
    /// 5 min of slack.
    cache: Mutex<std::collections::HashMap<String, (String, u64)>>,
}

/// The OAuth half of a GitHub App: its client id and secret, and where
/// the token exchange lives (`https://github.com`, not the API host).
#[derive(Clone)]
pub struct UserAuth {
    pub client_id: String,
    pub client_secret: String,
    pub oauth_base: String,
}

/// Who a user token belongs to, as GitHub answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubIdentity {
    /// GitHub's numeric account id, as a string. The half that never
    /// changes, and the only half an account may be keyed on.
    pub id: String,
    /// Their current login. Display and a default namespace name, never
    /// identity — it is renameable and, once released, re-claimable.
    pub login: String,
    /// Their display name, when they have set one.
    pub name: Option<String>,
    /// Their primary address, **and only when GitHub says it is
    /// verified**. See [`GithubApp::verified_primary_email`].
    pub email: Option<String>,
}

/// Who `GET /user` says this is, from its body alone.
///
/// Split from the fetch so the *recorded* body from
/// `scripts/manual-github-signin.sh fixtures` can be fed to exactly the
/// code the product runs — a parser that can only be exercised through
/// an HTTP call is a parser that gets tested against our own fake and
/// nothing else, which is how the `Retry-After` bug survived.
pub(crate) fn identity_from(body: &str) -> Result<GithubIdentity, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("user: {e}"))?;
    // The numeric id, not the login: see `identities` for why.
    let id = v["id"].as_i64().ok_or("user has no id")?.to_string();
    let login = v["login"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or("user has no login")?
        .to_string();
    let name = v["name"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(GithubIdentity {
        id,
        login,
        name,
        email: None,
    })
}

/// The one address a `GET /user/emails` body proves: primary **and**
/// verified, and storable.
///
/// Every other shape is `None`, which the caller reads as "no proved
/// address" — so a provider that changed this body could only ever stop
/// sign-in working, never let an unproved address through.
pub(crate) fn verified_primary_from(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.as_array()?
        .iter()
        .find(|e| e["primary"] == true && e["verified"] == true)
        .and_then(|e| e["email"].as_str())
        .map(stratum_control::users::normalize_email)
        .filter(|e| stratum_control::users::valid_email(e))
}

impl GithubApp {
    pub fn new(
        app_id: String,
        private_key_pem: String,
        api_base: String,
        git_base: String,
        webhook_secret: String,
    ) -> GithubApp {
        GithubApp {
            app_id,
            private_key_pem,
            api_base,
            git_base,
            webhook_secret,
            user_auth: None,
            cache: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Turn a `code` GitHub appended to the install redirect into the
    /// installations that person may reach.
    ///
    /// Two calls, both GitHub's own: the code becomes a user token at the
    /// OAuth endpoint, and `GET /user/installations` under that token
    /// lists what the person behind the browser is allowed to see. An
    /// installation not in that list is not theirs, whatever the URL
    /// says. `auth` is this App's own OAuth client, `self.user_auth`,
    /// passed in so a caller cannot ask without having checked it is
    /// configured.
    pub fn installations_of_user(
        &self,
        auth: &UserAuth,
        code: &str,
    ) -> Result<Vec<String>, UserAuthError> {
        let token = self.exchange_code(auth, code)?;
        self.installations_for_token(&token)
    }

    /// Trade a user-authorization `code` for that person's token.
    ///
    /// Split out from [`installations_of_user`](Self::installations_of_user)
    /// because signing in with GitHub needs the same first step and a
    /// different second one: the install callback goes on to ask what
    /// the person may reach, and the sign-in callback goes on to ask
    /// who they are. Doing the exchange twice would not work even if it
    /// were tidy — a code is spent by the first call, and GitHub
    /// refuses the second.
    pub fn exchange_code(&self, auth: &UserAuth, code: &str) -> Result<String, UserAuthError> {
        use UserAuthError::Unanswered;
        let url = format!("{}/login/oauth/access_token", auth.oauth_base);
        let resp = ureq::post(&url)
            .set("Accept", "application/json")
            .timeout(Duration::from_secs(10))
            .send_form(&[
                ("client_id", auth.client_id.as_str()),
                ("client_secret", auth.client_secret.as_str()),
                ("code", code),
            ])
            .map_err(|e| Unanswered(format!("POST {url}: {e}")))?;
        let text = resp
            .into_string()
            .map_err(|e| Unanswered(format!("read {url}: {e}")))?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| Unanswered(format!("token exchange: {e}")))?;
        // GitHub answers a bad or spent code with 200 and an `error`
        // field, not a status — a client that checked only the status
        // would carry an empty token into the next call and read a
        // refusal there instead of here.
        if let Some(err) = v["error"].as_str() {
            return Err(UserAuthError::Refused(format!(
                "token exchange refused: {err}"
            )));
        }
        let token = v["access_token"]
            .as_str()
            .ok_or_else(|| Unanswered("token exchange answered no access_token".into()))?;
        Ok(token.to_string())
    }

    /// What the person behind a user token may reach.
    fn installations_for_token(&self, token: &str) -> Result<Vec<String>, UserAuthError> {
        use UserAuthError::Unanswered;
        let url = format!("{}/user/installations?per_page=100", self.api_base);
        let text = self
            .get_with(&url, &format!("Bearer {token}"))
            .map_err(Unanswered)?;
        let v: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| Unanswered(format!("user installations: {e}")))?;
        let items = v["installations"]
            .as_array()
            .ok_or_else(|| Unanswered("user installations is not a list".into()))?;
        Ok(items
            .iter()
            .filter_map(|i| i["id"].as_i64().map(|n| n.to_string()))
            .collect())
    }

    /// Who the person behind a user token *is*, as GitHub tells it.
    ///
    /// Two calls again, and the second one is the whole security
    /// argument for skipping our own email confirmation: `GET /user`
    /// says which account this is, and `GET /user/emails` says which of
    /// their addresses GitHub has itself proved.
    pub fn user_identity(&self, token: &str) -> Result<GithubIdentity, UserAuthError> {
        use UserAuthError::Unanswered;
        let auth = format!("Bearer {token}");
        let url = format!("{}/user", self.api_base);
        let text = self.get_with(&url, &auth).map_err(Unanswered)?;
        let mut ident = identity_from(&text).map_err(Unanswered)?;
        ident.email = self.verified_primary_email(&auth);
        Ok(ident)
    }

    /// The one address GitHub reports as both primary *and* verified.
    ///
    /// Never `GET /user`'s own `email` field. That is whatever the
    /// person chose to publish, GitHub does not check it, and it is
    /// empty for anybody who keeps it private — so trusting it would
    /// let a stranger take somebody else's account here by typing their
    /// address into a GitHub profile. `verified` is the only flag that
    /// means the provider did the work we are declining to repeat.
    ///
    /// A refusal is not an error. An App that was never granted the
    /// `Email addresses` permission is answered 403, and a person may
    /// decline it at the authorization screen; both land here as
    /// `None`, which the caller treats exactly like "no proved
    /// address" — say so, and offer the password path.
    ///
    /// An address we could not store is `None` for the same reason
    /// rather than a failure further in. The caller's next move is
    /// `users::create`, which refuses an address that is not one — so
    /// without this check a provider answering something malformed
    /// would surface to a person as "something went wrong" instead of
    /// the accurate "GitHub has no confirmed address for you", and the
    /// only way out of the first sentence is to guess.
    fn verified_primary_email(&self, auth: &str) -> Option<String> {
        let url = format!("{}/user/emails?per_page=100", self.api_base);
        verified_primary_from(&self.get_with(&url, auth).ok()?)
    }

    fn app_jwt(&self) -> Result<String, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs();
        let header = b64url(br#"{"alg":"RS256","typ":"JWT"}"#);
        let payload = b64url(
            serde_json::json!({
                // 60s clock-drift allowance, 9 min lifetime (GitHub max 10).
                "iat": now - 60,
                "exp": now + 540,
                "iss": self.app_id,
            })
            .to_string()
            .as_bytes(),
        );
        let signing_input = format!("{header}.{payload}");
        let sig = rs256_sign(&self.private_key_pem, signing_input.as_bytes())?;
        Ok(format!("{signing_input}.{}", b64url(&sig)))
    }

    fn installation_token(&self, installation: &str) -> Result<String, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs();
        if let Some((tok, exp)) = self.cache.lock().unwrap().get(installation) {
            if *exp > now + 300 {
                return Ok(tok.clone());
            }
        }
        let jwt = self.app_jwt()?;
        let url = format!(
            "{}/app/installations/{installation}/access_tokens",
            self.api_base
        );
        let resp = ureq::post(&url)
            .set("Authorization", &format!("Bearer {jwt}"))
            .set("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(10))
            .send_string("{}")
            .map_err(|e| format!("installation token exchange: {e}"))?;
        let text = resp
            .into_string()
            .map_err(|e| format!("token response: {e}"))?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("token response: {e}"))?;
        let token = v["token"]
            .as_str()
            .ok_or("token response missing token")?
            .to_string();
        // GitHub tokens last ~1h; cache conservatively.
        self.cache
            .lock()
            .unwrap()
            .insert(installation.to_string(), (token.clone(), now + 3300));
        Ok(token)
    }

    /// One page of an installation's readable repositories.
    ///
    /// The installation already knows exactly which repositories it may
    /// read — that is what the person chose when they installed the App.
    /// Nothing asked it until now, which is why mirroring a private repo
    /// meant typing an id and a `owner/name` and hoping they matched.
    ///
    /// Paged, and bounded: `per_page` is capped at GitHub's 100 and the
    /// caller decides how far to walk. An unbounded walk over an
    /// installation with ten thousand repositories is a request that
    /// never returns.
    pub fn installation_repos(
        &self,
        installation: &str,
        page: u32,
        per_page: u32,
    ) -> Result<Vec<RemoteRepo>, String> {
        let token = self.installation_token(installation)?;
        let per_page = per_page.clamp(1, 100);
        let url = format!(
            "{}/installation/repositories?per_page={per_page}&page={}",
            self.api_base,
            page.max(1)
        );
        let text = self.get_with(&url, &format!("token {token}"))?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("repository list: {e}"))?;
        let items = v["repositories"]
            .as_array()
            .ok_or("repository list has no repositories")?;
        Ok(items.iter().map(RemoteRepo::from_json).collect())
    }

    /// One page of a repository's issues, **newest first, all states**.
    ///
    /// `state=all` is not a default anybody should rely on being right:
    /// GitHub's default is `open`, and an import that takes it loses
    /// every closed issue — which is most of a project's institutional
    /// memory and exactly the part a migration is for. It is spelled out
    /// here so no caller has to remember.
    ///
    /// Pull requests come back from this endpoint too, marked by a
    /// `pull_request` object. Deciding what to do with them is the
    /// caller's; knowing they are there is not optional.
    pub fn issues_page(
        &self,
        installation: &str,
        full_name: &str,
        per_page: u32,
        page: u32,
    ) -> Result<Page, String> {
        let token = self.installation_token(installation)?;
        let url = format!(
            "{}/repos/{full_name}/issues?state=all&per_page={}&page={}&sort=created&direction=asc",
            self.api_base,
            per_page.clamp(1, 100),
            page.max(1),
        );
        self.get_page(&url, &format!("token {token}"))
    }

    /// Follow a `Link` to the next page, whatever it points at.
    ///
    /// The URL comes from GitHub rather than from us, so it is checked
    /// against the API base we were configured with before it is
    /// dialled. A redirect target that walked us off to another host
    /// would be handed an installation token, and a token is the one
    /// thing that must never leave the host it was minted for.
    pub fn next_page(&self, installation: &str, url: &str) -> Result<Page, String> {
        if !url.starts_with(&self.api_base) {
            return Err(format!(
                "refusing to follow {url}: not on the configured API host"
            ));
        }
        let token = self.installation_token(installation)?;
        self.get_page(url, &format!("token {token}"))
    }

    /// Comments on one issue, oldest first — the order they were said in,
    /// which is the only order a conversation reads in.
    pub fn comments_page(
        &self,
        installation: &str,
        full_name: &str,
        number: i64,
        per_page: u32,
        page: u32,
    ) -> Result<Page, String> {
        let token = self.installation_token(installation)?;
        let url = format!(
            "{}/repos/{full_name}/issues/{number}/comments?per_page={}&page={}",
            self.api_base,
            per_page.clamp(1, 100),
            page.max(1),
        );
        self.get_page(&url, &format!("token {token}"))
    }

    /// A repository's labels.
    pub fn labels_page(
        &self,
        installation: &str,
        full_name: &str,
        per_page: u32,
        page: u32,
    ) -> Result<Page, String> {
        let token = self.installation_token(installation)?;
        let url = format!(
            "{}/repos/{full_name}/labels?per_page={}&page={}",
            self.api_base,
            per_page.clamp(1, 100),
            page.max(1),
        );
        self.get_page(&url, &format!("token {token}"))
    }

    /// A repository's milestones, both states.
    pub fn milestones_page(
        &self,
        installation: &str,
        full_name: &str,
        per_page: u32,
        page: u32,
    ) -> Result<Page, String> {
        let token = self.installation_token(installation)?;
        let url = format!(
            "{}/repos/{full_name}/milestones?state=all&per_page={}&page={}",
            self.api_base,
            per_page.clamp(1, 100),
            page.max(1),
        );
        self.get_page(&url, &format!("token {token}"))
    }

    /// One page of a repository's GitHub Actions runs, newest first.
    ///
    /// Stratum never runs anybody's code; this reads the verdicts of a
    /// provider that did. Which is why a refusal here has to survive as
    /// a refusal all the way to the screen — the only thing we have is
    /// what GitHub tells us, and "nothing" is an answer we are not
    /// entitled to invent.
    ///
    /// `per_page` is GitHub's maximum, deliberately not a parameter: a
    /// Checks tab wants the most recent runs and the pages after the
    /// first exist to be walked, not chosen between.
    pub fn actions_runs(
        &self,
        installation: &str,
        full_name: &str,
        page: u32,
    ) -> Result<ActionsRuns, String> {
        let token = self.installation_token(installation)?;
        let url = format!(
            "{}/repos/{full_name}/actions/runs?per_page=100&page={}",
            self.api_base,
            page.max(1),
        );
        let got = self.get_page(&url, &format!("token {token}"))?;
        actions_from_page(got)
    }

    /// The next page of runs, following the URL GitHub named.
    ///
    /// Goes through [`GithubApp::next_page`] rather than reaching for
    /// `get_page` directly, so the host check applies here too: the URL
    /// came from the response, and dialling it means handing an
    /// installation token to whatever it names.
    pub fn actions_runs_next(&self, installation: &str, url: &str) -> Result<ActionsRuns, String> {
        let got = self.next_page(installation, url)?;
        actions_from_page(got)
    }

    /// Every installation of this App, as the App itself sees them.
    ///
    /// Used to learn the *account* an installation belongs to, so a
    /// person choosing between two of them sees `acme` and `ada` rather
    /// than two nine-digit numbers.
    pub fn installations(&self) -> Result<Vec<(String, String)>, String> {
        let jwt = self.app_jwt()?;
        let url = format!("{}/app/installations?per_page=100", self.api_base);
        let text = self.get_with(&url, &format!("Bearer {jwt}"))?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("installation list: {e}"))?;
        let items = v.as_array().ok_or("installation list is not a list")?;
        Ok(items
            .iter()
            .filter_map(|i| {
                let id = i["id"].as_i64().map(|n| n.to_string())?;
                let account = i["account"]["login"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                Some((id, account))
            })
            .collect())
    }

    /// One installation, as the App sees it: whose it is, whether that
    /// is a person or an organisation, and what it may do. `None` is
    /// GitHub's 404 — uninstalled since we last heard.
    ///
    /// The two permissions the runner feature rests on are read here
    /// rather than discovered by failing: a settings page has to be able
    /// to say "approve this on GitHub" *before* the first job is refused
    /// for want of it.
    pub fn installation(&self, id: &str) -> Result<Option<InstallationDetail>, String> {
        let jwt = self.app_jwt()?;
        let url = format!("{}/app/installations/{id}", self.api_base);
        match self.send("GET", &url, &format!("Bearer {jwt}"), None)? {
            Page::Data { body, .. } => {
                let v: serde_json::Value =
                    serde_json::from_str(&body).map_err(|e| format!("installation {id}: {e}"))?;
                let perm = |k: &str| v["permissions"][k].as_str() == Some("write");
                Ok(Some(InstallationDetail {
                    id: v["id"]
                        .as_i64()
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| id.to_string()),
                    account: v["account"]["login"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    target_type: v["target_type"].as_str().unwrap_or_default().to_string(),
                    administration_write: perm("administration"),
                    actions_write: perm("actions"),
                    contents_write: perm("contents"),
                    events: v["events"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|e| e.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    suspended: !v["suspended_at"].is_null(),
                }))
            }
            Page::Refused { status: 404, .. } => Ok(None),
            Page::RateLimited { retry_after_secs } => Err(format!(
                "GitHub is rate limiting the App; try again in {retry_after_secs}s"
            )),
            Page::Refused { status, body } => Err(format!(
                "GitHub refused the installation read ({status}): {body}"
            )),
        }
    }

    /// Where a person approves the permissions an installation is
    /// missing — the installation's own settings page on GitHub, which
    /// lives in a different place for a person and for an organisation.
    pub fn approve_url(&self, detail: &InstallationDetail) -> String {
        let base = self.git_base.trim_end_matches('/');
        if detail.target_type == "Organization" {
            format!(
                "{base}/organizations/{}/settings/installations/{}",
                detail.account, detail.id
            )
        } else {
            format!("{base}/settings/installations/{}", detail.id)
        }
    }

    /// One request of any method, classified the way [`get_page`]
    /// classifies a page: data, wait, or refused. The runner calls all
    /// go through here so that the one decision `rate_limit_wait` makes
    /// — a spent budget is a wait and not a permission problem — is made
    /// once, for every call that could meet it.
    fn send(
        &self,
        method: &str,
        url: &str,
        authorization: &str,
        body: Option<&str>,
    ) -> Result<Page, String> {
        let req = ureq::request(method, url)
            .set("Authorization", authorization)
            .set("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(10));
        let resp = match body {
            Some(b) => req.set("Content-Type", "application/json").send_string(b),
            None => req.call(),
        };
        match resp {
            Ok(r) => {
                let body = r.into_string().map_err(|e| format!("read {url}: {e}"))?;
                Ok(Page::Data { body, next: None })
            }
            Err(ureq::Error::Status(status, r)) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if let Some(secs) = rate_limit_wait(
                    r.header("Retry-After"),
                    r.header("x-ratelimit-remaining"),
                    r.header("x-ratelimit-reset"),
                    now,
                ) {
                    return Ok(Page::RateLimited {
                        retry_after_secs: secs,
                    });
                }
                let body = r.into_string().unwrap_or_default();
                Ok(Page::Refused { status, body })
            }
            Err(e) => Err(format!("{method} {url}: {e}")),
        }
    }

    /// One page of a paged GitHub collection.
    ///
    /// Three outcomes, kept apart on purpose, because an importer must
    /// do something different with each and collapsing any two of them
    /// is a bug that looks like success:
    ///
    /// * **data**, with the next page's URL if there is one;
    /// * **rate limited**, which means wait exactly as long as GitHub
    ///   said and ask again — the work is not lost;
    /// * **refused**, which means stop and say why. An installation
    ///   without `issues: read` answers 403 here, and reporting that as
    ///   "no issues" would be indistinguishable from a project that
    ///   never had any.
    ///
    /// The next page comes from the `Link` header and nowhere else.
    /// Incrementing a page counter works right up until the last page is
    /// exactly full, and then silently stops one page early.
    fn get_page(&self, url: &str, authorization: &str) -> Result<Page, String> {
        let resp = ureq::get(url)
            .set("Authorization", authorization)
            .set("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(10))
            .call();
        let resp = match resp {
            Ok(r) => r,
            // GitHub answers a rate limit with 403 **and** 429 depending
            // on which limit was hit, so the status alone never tells a
            // refusal-to-wait from a refusal-to-proceed. The headers do —
            // see [`rate_limit_wait`], which is where the whole decision
            // lives so that it can be tested without a socket.
            Err(ureq::Error::Status(status, r)) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if let Some(secs) = rate_limit_wait(
                    r.header("Retry-After"),
                    r.header("x-ratelimit-remaining"),
                    r.header("x-ratelimit-reset"),
                    now,
                ) {
                    return Ok(Page::RateLimited {
                        retry_after_secs: secs,
                    });
                }
                let body = r.into_string().unwrap_or_default();
                return Ok(Page::Refused { status, body });
            }
            Err(e) => return Err(format!("GET {url}: {e}")),
        };
        let next = resp.header("Link").and_then(next_link);
        let body = resp.into_string().map_err(|e| format!("read {url}: {e}"))?;
        Ok(Page::Data { body, next })
    }

    fn get_with(&self, url: &str, authorization: &str) -> Result<String, String> {
        ureq::get(url)
            .set("Authorization", authorization)
            .set("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(10))
            .call()
            .map_err(|e| format!("GET {url}: {e}"))?
            .into_string()
            .map_err(|e| format!("read {url}: {e}"))
    }
}

/// One installation as `GET /app/installations/{id}` describes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InstallationDetail {
    pub id: String,
    pub account: String,
    /// `Organization` or `User`.
    pub target_type: String,
    /// `administration: write` — what registering a just-in-time
    /// runner on a repository needs.
    pub administration_write: bool,
    /// `actions: write` — what cancelling a run we refused needs.
    pub actions_write: bool,
    /// `contents: write` — what forwarding a push to the origin needs.
    /// Installations made while the App asked only for `read` lack it
    /// until their owner approves the change.
    pub contents_write: bool,
    pub events: Vec<String>,
    pub suspended: bool,
}

/// What one page request came back as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Page {
    Data {
        body: String,
        /// The next page's URL, from `Link`. `None` means this was the
        /// last page — which is the only reliable way to know.
        next: Option<String>,
    },
    /// Wait this long and ask again. The work is not lost.
    RateLimited { retry_after_secs: u64 },
    /// Stop, and say why. Never to be read as "there was nothing there".
    Refused { status: u16, body: String },
}

/// The longest we will ever wait on a refusal before asking again.
///
/// A primary budget resets on the hour, so an hour is the true worst
/// case; anything longer is a clock disagreeing with us rather than a
/// budget, and waiting on it would strand the poller.
const MAX_RATE_LIMIT_WAIT: u64 = 3600;

/// How long to wait on a refused request, or `None` if waiting is not
/// the answer to it.
///
/// **This is the difference between "come back later" and "you are not
/// allowed", and GitHub spells it two different ways.**
///
/// * A **secondary** limit — too much, too fast — carries `Retry-After`,
///   a duration in seconds.
/// * A **primary** limit — the hourly budget spent — carries
///   `x-ratelimit-remaining: 0` and `x-ratelimit-reset`, an *absolute
///   unix timestamp*, and **no `Retry-After` at all**.
///
/// Reading only `Retry-After` classified every primary exhaustion as a
/// refusal, which on the Actions endpoint became "this installation
/// cannot read Actions". The person is then told to grant a permission
/// they already granted, the button does nothing, and an hour later CI
/// starts working for reasons nobody can attribute. Nothing anywhere
/// recorded that a budget had been spent. It survived review because the
/// fixture always attached `Retry-After`, so the test that names this
/// exact failure could not fail.
///
/// `remaining: 0` is the signal, not the status: a permission denial
/// carries a *nonzero* remaining, and a response with no rate headers at
/// all is not a rate limit either. Both stay refusals.
///
/// The reset is a timestamp and not a duration, which is the easy way to
/// misread it — treating it as one would sleep for fifty-five years.
/// `now_secs` is a parameter rather than a call to the clock so the
/// arithmetic is testable at a fixed instant.
fn rate_limit_wait(
    retry_after: Option<&str>,
    remaining: Option<&str>,
    reset: Option<&str>,
    now_secs: u64,
) -> Option<u64> {
    // The secondary limit, and the plain-duration case.
    if let Some(after) = retry_after.and_then(|v| v.trim().parse::<u64>().ok()) {
        return Some(after.clamp(1, MAX_RATE_LIMIT_WAIT));
    }
    // The primary limit. Only a budget of exactly zero is one.
    if remaining.and_then(|v| v.trim().parse::<u64>().ok()) != Some(0) {
        return None;
    }
    // A reset already past, or one we cannot read at all, still means
    // the budget is spent: both wait rather than falling through to a
    // refusal, which would report a spent budget as a permission problem
    // all over again. An unreadable reset waits a minute — long enough
    // not to spin, short enough that a header GitHub changes the shape
    // of costs a poll rather than an hour.
    let wait = match reset.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(at) => at.saturating_sub(now_secs),
        None => 60,
    };
    Some(wait.clamp(1, MAX_RATE_LIMIT_WAIT))
}

/// The `rel="next"` URL out of a `Link` header.
///
/// GitHub's shape is `<url>; rel="next", <url>; rel="last"`. Parsed
/// rather than pattern-matched on position, because the relations do not
/// always arrive in the same order and `last` is absent on the last page
/// while `next` is absent on it too.
pub fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        if !part.contains("rel=\"next\"") {
            return None;
        }
        let start = part.find('<')? + 1;
        let end = part.find('>')?;
        Some(part[start..end].to_string())
    })
}

/// One `Page` from the Actions endpoint, read as runs.
///
/// The 403 arm is why this is not just a `parse`. By the time a refusal
/// reaches here, [`GithubApp::get_page`] has already taken both rate
/// limits away — the secondary one by its `Retry-After`, the primary one
/// by `x-ratelimit-remaining: 0` — so a 403 arriving here is one with a
/// budget still on it, or with no rate headers at all. On this endpoint
/// that means one thing: the installation lacks `actions: read`.
///
/// That reasoning is load-bearing and was wrong once. While `get_page`
/// classified on `Retry-After` alone, every primary budget exhaustion
/// arrived here and was reported to a maintainer as a missing
/// permission, next to a button that re-granted what they already had.
/// If the classification above ever narrows again, this arm becomes a
/// lie again — the two are one decision written in two places.
fn actions_from_page(page: Page) -> Result<ActionsRuns, String> {
    match page {
        Page::Data { body, next } => Ok(ActionsRuns::Data {
            runs: parse_actions_runs(&body)?,
            next,
        }),
        Page::RateLimited { retry_after_secs } => Ok(ActionsRuns::RateLimited { retry_after_secs }),
        Page::Refused { status: 403, .. } => Err(ACTIONS_READ_DENIED.to_string()),
        Page::Refused { status, body } => Err(format!(
            "GitHub refused the Actions read ({status}): {body}"
        )),
    }
}

/// What the Checks tab is told when it asks GitHub Actions for a
/// repository's verdicts and is not allowed to have them.
///
/// A distinguishable outcome, and deliberately not an empty list. "This
/// project runs no CI" and "this installation may not read the CI this
/// project runs" are different facts about the world, and a Checks tab
/// showing nothing for both tells a person the first one when the truth
/// is the second — sending them to look for a problem in their workflows
/// when the problem is a permission on the App.
///
/// Matched with [`is_actions_read_denied`] rather than by substring at
/// the call site: the wording here is a message to a person and will be
/// rewritten, and a caller matching on a fragment of it silently stops
/// recognising the case the day somebody improves the sentence.
pub const ACTIONS_READ_DENIED: &str =
    "this GitHub App installation cannot read Actions on this repository — it needs the \
     `actions: read` permission. Reported as a refusal rather than an empty list, because \
     a project with no CI and a project whose CI we may not see look identical otherwise";

/// Whether an error from [`GithubApp::actions_runs`] is the missing
/// `actions: read` permission.
pub fn is_actions_read_denied(err: &str) -> bool {
    err == ACTIONS_READ_DENIED
}

/// One page of workflow runs, or a reason to come back later.
///
/// Same three-way split as [`Page`] and for the same reason, minus the
/// refusal: a refusal here is an `Err`, because unlike a paged import
/// there is no partial work to preserve — the Checks tab either has this
/// repository's verdicts or has to say why it does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionsRuns {
    Data {
        runs: Vec<ActionsRun>,
        /// The next page's URL, from `Link`, and from nowhere else.
        next: Option<String>,
    },
    /// Wait this long and ask again. The verdicts are still there.
    RateLimited { retry_after_secs: u64 },
}

/// One GitHub Actions workflow run, reduced to what a Checks tab shows.
///
/// Everything optional here is optional on the wire. `actor` is null for
/// a deleted account, `head_branch` is null for a run that was not on
/// one, and `run_started_at` is simply absent on runs old enough to
/// predate the field — so each is an `Option` rather than a default, and
/// the timestamps most of all. A missing time defaulted to zero renders
/// as January 1970, which is not "unknown", it is a confident wrong
/// answer sitting at the top of a list sorted by date.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ActionsRun {
    /// GitHub's own run id, kept as a string.
    ///
    /// It is a 64-bit integer on the wire and it is an *identifier*: we
    /// never do arithmetic on it, we only store it and hand it back, and
    /// a JSON number that large loses its low bits the moment it passes
    /// through a browser. A string cannot.
    pub id: String,
    /// The workflow's name. Absent when the workflow file has no `name:`.
    pub name: Option<String>,
    /// The per-repository counter a person actually sees in the Actions
    /// UI. Not the id, and not unique across repositories.
    pub run_number: i64,
    pub head_sha: String,
    pub head_branch: Option<String>,
    pub event: String,
    /// GitHub's own two fields, kept verbatim beside our single mapped
    /// state. Storing only the mapping would mean an unrecognised
    /// conclusion is unrecoverable after the fact — nobody could tell,
    /// from the row, whether a `queued` was really queued or was a
    /// verdict we had not learned to read yet.
    pub status: String,
    pub conclusion: Option<String>,
    pub html_url: String,
    /// `actor.login`, if there is still an account behind it.
    pub actor: Option<String>,
    /// Epoch milliseconds, or `None` — never zero.
    pub run_started_at: Option<i64>,
    pub updated_at: Option<i64>,
}

impl ActionsRun {
    /// This run's state in our vocabulary. See [`run_state`].
    pub fn state(&self) -> &'static str {
        run_state(&self.status, self.conclusion.as_deref())
    }

    fn from_json(v: &serde_json::Value) -> ActionsRun {
        ActionsRun {
            id: v["id"].as_i64().map(|n| n.to_string()).unwrap_or_default(),
            name: v["name"].as_str().map(str::to_string),
            run_number: v["run_number"].as_i64().unwrap_or_default(),
            head_sha: v["head_sha"].as_str().unwrap_or_default().to_string(),
            head_branch: v["head_branch"].as_str().map(str::to_string),
            event: v["event"].as_str().unwrap_or_default().to_string(),
            status: v["status"].as_str().unwrap_or_default().to_string(),
            conclusion: v["conclusion"].as_str().map(str::to_string),
            html_url: v["html_url"].as_str().unwrap_or_default().to_string(),
            // `actor` is null for a deleted account, and indexing a null
            // with `["login"]` is `Value::Null` rather than a panic — so
            // this reads correctly for both a missing key and a null.
            actor: v["actor"]["login"].as_str().map(str::to_string),
            run_started_at: rfc3339_millis(v["run_started_at"].as_str()),
            updated_at: rfc3339_millis(v["updated_at"].as_str()),
        }
    }
}

/// The `workflow_runs` array out of an Actions response.
///
/// The runs live under a `workflow_runs` key beside a `total_count`, not
/// at the top level the way issues do. A missing key is an error rather
/// than an empty list: the one thing this must never do is turn a
/// response it did not understand into "no CI here".
pub fn parse_actions_runs(body: &str) -> Result<Vec<ActionsRun>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("workflow runs: {e}"))?;
    let items = v["workflow_runs"]
        .as_array()
        .ok_or("workflow run response has no workflow_runs")?;
    Ok(items.iter().map(ActionsRun::from_json).collect())
}

/// GitHub's `status` + `conclusion` collapsed onto our single state.
///
/// Our vocabulary is exactly `queued | running | passing | failing |
/// cancelled | skipped`, and the mapping is total: **anything
/// unrecognised becomes `queued`**, never a panic and never `passing`.
///
/// Both halves of that matter. A panic would let GitHub take a poller
/// down by shipping a new conclusion, which it has done repeatedly —
/// `skipped`, `stale` and `action_required` all arrived after this
/// endpoint did. And defaulting to `passing` would render an unknown
/// verdict as a green tick, which is the one wrong answer that gets
/// acted on: a person merges on it. `queued` reads as "we do not know
/// yet", which is precisely true of a state we have never seen.
pub fn run_state(status: &str, conclusion: Option<&str>) -> &'static str {
    match status {
        "in_progress" => "running",
        "completed" => match conclusion {
            Some("success") => "passing",
            Some("failure") | Some("timed_out") | Some("startup_failure") => "failing",
            Some("cancelled") => "cancelled",
            Some("skipped") | Some("neutral") => "skipped",
            _ => "queued",
        },
        _ => "queued",
    }
}

/// An RFC3339 timestamp as epoch milliseconds, or `None`.
///
/// `None` for absent and `None` for unparseable, both on purpose: the
/// caller's field is an `Option` and the alternative — a zero — is a
/// date, and a wrong one. Every component is range-checked, so a
/// malformed month cannot walk the civil-days arithmetic off into a
/// plausible-looking day in another year.
pub(crate) fn rfc3339_millis(s: Option<&str>) -> Option<i64> {
    let s = s?;
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    // Digits only. `parse::<i64>` accepts a leading `-`, so a string like
    // `-024-01-01T00:00:00Z` would otherwise read as a year of -24 and
    // produce a confident timestamp out of nonsense.
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let days = stratum_control::contribs::days_from_civil(y as i32, mo as u32, d as u32) as i64;
    let local = ((days * 86_400) + h * 3600 + mi * 60 + sec) * 1000;
    Some(local - utc_offset_millis(&s[19..])?)
}

/// The zone on the end of an RFC3339 timestamp, in milliseconds to
/// subtract to reach UTC.
///
/// Everything before this read positions 0..19 and threw the rest away,
/// so `2024-05-01T09:00:00+02:00` was stored as 09:00 UTC — two hours
/// wrong, silently, in a field that sorts a list. GitHub always sends
/// `Z`, so it was not live; it becomes live the moment a second
/// provider's timestamps reach this function, and by then the wrong
/// values are already in the database. The test that covered it asserted
/// `+00:00`, which is the single offset where the bug cannot show.
///
/// A bare timestamp with no zone at all is read as UTC. RFC3339 requires
/// an offset, so this is a tolerance rather than a rule — but it is the
/// tolerance that was already in effect, and reading such a stamp as UTC
/// is at least what it says. Anything else in that position is `None`:
/// a suffix we do not understand may well be a zone, and guessing at a
/// zone is how a timestamp ends up confidently wrong.
fn utc_offset_millis(tail: &str) -> Option<i64> {
    // Fractional seconds first — `.123456` — which carry no zone
    // information and are deliberately discarded rather than rounded.
    let tail = tail.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let b = tail.as_bytes();
    let sign = match b.first() {
        None => return Some(0),
        Some(b'Z' | b'z') if b.len() == 1 => return Some(0),
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return None,
    };
    // `+HH:MM` and `+HHMM` are both RFC3339, and both are written.
    let digits: String = tail[1..].chars().filter(|c| *c != ':').collect();
    if digits.len() != 4 || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let hours: i64 = digits[0..2].parse().ok()?;
    let mins: i64 = digits[2..4].parse().ok()?;
    if hours > 23 || mins > 59 {
        return None;
    }
    Some(sign * (hours * 3600 + mins * 60) * 1000)
}

/// A repository as GitHub describes it, reduced to what a picker shows
/// and what mirror creation needs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RemoteRepo {
    pub full_name: String,
    pub private: bool,
    pub default_branch: Option<String>,
    pub description: Option<String>,
    /// Bytes, as GitHub estimates them — it reports kilobytes, and this
    /// is that number scaled, so it is a hint for a person and not a
    /// measurement anything should depend on.
    pub size: Option<u64>,
    /// What the upstream says its star count is.
    ///
    /// `None` means the origin did not tell us — a field absent from the
    /// response, or a provider with no metadata API at all. It is
    /// deliberately not `Some(0)`: "nobody has starred it there" and "we
    /// never found out" are different facts, and collapsing them is how
    /// a mirrored project ends up displaying a confident zero it has no
    /// basis for.
    pub stars: Option<u32>,
}

impl RemoteRepo {
    fn from_json(v: &serde_json::Value) -> RemoteRepo {
        RemoteRepo {
            full_name: v["full_name"].as_str().unwrap_or_default().to_string(),
            private: v["private"].as_bool().unwrap_or(true),
            default_branch: v["default_branch"].as_str().map(str::to_string),
            description: v["description"].as_str().map(str::to_string),
            size: v["size"].as_u64().map(|kb| kb * 1024),
            // Absent field stays absent. `as_u64` on a missing key is
            // `None`, which is exactly the distinction we want, so this
            // deliberately has no `unwrap_or(0)` on it.
            stars: v["stargazers_count"]
                .as_u64()
                .map(|n| n.min(u32::MAX as u64) as u32),
        }
    }
}

impl GithubApp {
    /// `owner/name` out of an origin URL, or `None` if this is not one
    /// of ours. Tolerates a `.git` suffix and a trailing slash, because
    /// both are things people paste.
    fn full_name_of(&self, origin_url: &str) -> Option<String> {
        // Two shapes reach here and both are real. Mirror creation
        // accepts a bare `owner/name` shorthand as well as a full URL —
        // the e2e suites use the shorthand throughout — and a parser
        // that only understood URLs would return `None` for the common
        // case and quietly import no count at all. That is the worst
        // kind of failure this feature can have: silent, and it looks
        // exactly like an origin that has no stars.
        let rest = match origin_url.strip_prefix(&format!("{}/", self.git_base)) {
            Some(rest) => rest,
            None if !origin_url.contains("://") => origin_url,
            None => return None,
        };
        let rest = rest.trim_end_matches('/').trim_end_matches(".git");
        let mut parts = rest.split('/');
        let owner = parts.next().filter(|s| !s.is_empty())?;
        let name = parts.next().filter(|s| !s.is_empty())?;
        // Exactly two segments. Anything deeper is a URL into a
        // repository, not the repository itself, and guessing at it
        // would send a request nobody asked for.
        if parts.next().is_some() {
            return None;
        }
        Some(format!("{owner}/{name}"))
    }
}

impl OriginProvider for GithubApp {
    fn repo_meta(&self, origin_url: &str) -> Result<Option<RemoteRepo>, String> {
        let Some(full_name) = self.full_name_of(origin_url) else {
            return Ok(None);
        };
        let url = format!("{}/repos/{full_name}", self.api_base);
        // Unauthenticated: this is only ever asked about an origin
        // somebody is mirroring, and a public repository answers
        // anybody. A private one answers 404, which lands in `Err` and
        // is read as "we do not know" — the correct outcome, and one we
        // should not spend an installation token to reach.
        let text = ureq::get(&url)
            .set("Accept", "application/vnd.github+json")
            .timeout(Duration::from_secs(10))
            .call()
            .map_err(|e| format!("GET {url}: {e}"))?
            .into_string()
            .map_err(|e| format!("read {url}: {e}"))?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("repository metadata: {e}"))?;
        Ok(Some(RemoteRepo::from_json(&v)))
    }

    fn verify_signature(&self, signature: Option<&str>, body: &[u8]) -> Result<(), String> {
        verify_hmac_sig(&self.webhook_secret, signature, body)
    }

    fn verify_webhook(&self, signature: Option<&str>, body: &[u8]) -> Result<WebhookEvent, String> {
        self.verify_signature(signature, body)?;
        let v: serde_json::Value =
            serde_json::from_slice(body).map_err(|e| format!("webhook body: {e}"))?;
        let full_name = v["repository"]["full_name"]
            .as_str()
            .ok_or("webhook body has no repository.full_name")?;
        Ok(WebhookEvent {
            full_name: full_name.to_string(),
        })
    }

    fn fetch_url(&self, repo: &Repo) -> Result<String, String> {
        // Before anything else, and specifically before a token is
        // minted: the credential this builds is only safe to build if
        // the host it will be sent to is the host that issued it. See
        // [`github_hosts_paired`].
        github_hosts_paired(&self.api_base, &self.git_base)?;
        let full_name = repo
            .origin_url
            .as_deref()
            .ok_or("mirror has no origin identity")?;
        // file:// bases (hermetic tests) need no credentials.
        if self.git_base.starts_with("file://") {
            return Ok(format!("{}/{full_name}.git", self.git_base));
        }
        // No installation is the public path: the person pasted a URL
        // and installed nothing, and the origin answers anybody. It is
        // fetched as itself, with no credential to leak. A private origin
        // recorded this way fails the fetch with git's own "not found",
        // which the sync error carries back to the repository page.
        let Some(inst) = repo.origin_installation.clone() else {
            let base = self.git_base.trim_end_matches('/');
            return Ok(format!("{base}/{full_name}.git"));
        };
        let token = self.installation_token(&inst)?;
        let base = self
            .git_base
            .split_once("://")
            .map(|(scheme, rest)| (scheme.to_string(), rest.to_string()))
            .ok_or("bad git base")?;
        Ok(format!(
            "{}://x-access-token:{token}@{}/{full_name}.git",
            base.0, base.1
        ))
    }

    /// A `file://` base needs no credential and takes any push; a real
    /// host takes one only under an installation token. The public,
    /// pasted-URL mirror is the case this says no to — it fetches as a
    /// stranger, and GitHub does not let strangers push.
    fn can_push(&self, repo: &Repo) -> bool {
        self.git_base.starts_with("file://") || repo.origin_installation.is_some()
    }
}

/// GitHub's real API host, and the git host that belongs with it.
///
/// Named rather than inlined because the whole of [`github_hosts_paired`]
/// is the claim that these two go together and that neither goes with
/// anything else.
const GITHUB_API: &str = "https://api.github.com";
const GITHUB_GIT: &str = "https://github.com";

/// Refuse an API base and a git base that are not the same world.
///
/// A mirror's installation token is minted by the **API** host and then
/// embedded in a URL dialled against the **git** host. Those are two
/// separate configuration values, so nothing but this stops them naming
/// two different companies — and when they do, the fetch takes a
/// credential issued by one host and sends it to another. That is a
/// credential leak with a plausible cover story, in whichever direction
/// it happens:
///
/// * API elsewhere, git on real github.com — what the e2e suite has been
///   doing. Every `POST /v1/orgs/:org/mirrors` in `import_e2e` minted a
///   token from the hermetic fake and then opened a real socket to
///   github.com carrying it, which is why the suite prints `remote:
///   Invalid username or token` on a machine with no GitHub credentials
///   at all. The tests pass, because nothing asserts the sync succeeds,
///   so it stayed invisible for as long as nobody read the output.
/// * API on real github.com, git elsewhere — the same mistake with a
///   *real* token, which is strictly worse and which nothing else here
///   would have caught either.
///
/// The rule is "both real, or neither", which admits exactly the three
/// configurations that make sense: production, GitHub Enterprise (both
/// pointed at the corporate host), and a hermetic test (both pointed at
/// the fakes). It needs no new environment variable — the two that exist
/// simply have to agree — and it is checked on the path that builds the
/// URL rather than at startup, because a knob nobody sets is a knob that
/// silently reverts.
///
/// Trailing slashes are tolerated on both, because both are things
/// people paste into a deployment config.
pub fn github_hosts_paired(api_base: &str, git_base: &str) -> Result<(), String> {
    let api_is_github = api_base.trim_end_matches('/') == GITHUB_API;
    let git_is_github = git_base.trim_end_matches('/') == GITHUB_GIT;
    if api_is_github == git_is_github {
        return Ok(());
    }
    Err(format!(
        "refusing to fetch: STRATUM_GITHUB_API_BASE ({api_base}) and \
         STRATUM_GITHUB_GIT_BASE ({git_base}) are different hosts, so this fetch \
         would send an installation token minted by one of them to the other. \
         Point both at {GITHUB_API}/{GITHUB_GIT}, both at your GitHub Enterprise \
         host, or both at the test fakes"
    ))
}

/// `sha256=<hex hmac>` verification (GitHub's X-Hub-Signature-256 shape).
fn verify_hmac_sig(secret: &str, signature: Option<&str>, body: &[u8]) -> Result<(), String> {
    let sig = signature.ok_or("missing webhook signature")?;
    let hex = sig
        .strip_prefix("sha256=")
        .ok_or("signature must be sha256=<hex>")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).map_err(|e| e.to_string())?;
    mac.update(body);
    let want = mac.finalize().into_bytes();
    let want_hex = stratum_store::pack::hex(&want);
    if !constant_time_eq(want_hex.as_bytes(), hex.as_bytes()) {
        return Err("webhook signature mismatch".into());
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn b64url(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// PKCS#1 v1.5 RSA-SHA256 signature. Accepts PKCS#1 ("BEGIN RSA PRIVATE
/// KEY", what GitHub issues) and PKCS#8 PEM keys.
fn rs256_sign(pem: &str, data: &[u8]) -> Result<Vec<u8>, String> {
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::Pkcs1v15Sign;
    let key = rsa::RsaPrivateKey::from_pkcs1_pem(pem)
        .or_else(|_| rsa::RsaPrivateKey::from_pkcs8_pem(pem))
        .map_err(|e| format!("app private key: {e}"))?;
    let digest = Sha256::digest(data);
    key.sign(Pkcs1v15Sign::new::<Sha256>(), &digest)
        .map_err(|e| format!("rs256 sign: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> GithubApp {
        GithubApp::new(
            "1".into(),
            String::new(),
            "https://api.example".into(),
            "https://github.com".into(),
            String::new(),
        )
    }

    /// Which origins name a repository we can ask about, and which do
    /// not.
    ///
    /// Pure string work, and every branch of it decides whether an
    /// imported star count appears at all — so a wrong `None` here is
    /// silent, and looks exactly like an upstream with no stars. The
    /// coverage gate found three of these arms unexercised: the full-URL
    /// form, a URL belonging to somebody else, and a path with more
    /// segments than a repository has.
    #[test]
    fn full_name_of_reads_both_shapes_and_refuses_the_rest() {
        let gh = app();

        // The shorthand mirror creation accepts, and the shape every
        // e2e uses.
        assert_eq!(
            gh.full_name_of("acme/widget"),
            Some("acme/widget".to_string())
        );
        // The full URL under our own git base. This arm had no test,
        // and it is the one a person pasting from a browser produces.
        assert_eq!(
            gh.full_name_of("https://github.com/acme/widget"),
            Some("acme/widget".to_string())
        );
        // Both tolerated endings, because both are things people paste.
        assert_eq!(
            gh.full_name_of("https://github.com/acme/widget.git"),
            Some("acme/widget".to_string())
        );
        assert_eq!(
            gh.full_name_of("https://github.com/acme/widget/"),
            Some("acme/widget".to_string())
        );

        // A URL somewhere else entirely. Refused rather than guessed
        // at: asking api.github.com about a repository hosted on
        // another forge would send a request nobody asked for and
        // attribute somebody else's count to it.
        assert_eq!(gh.full_name_of("https://gitlab.com/acme/widget"), None);
        assert_eq!(gh.full_name_of("file:///srv/origins/acme/widget"), None);

        // Deeper than a repository: a URL *into* one.
        assert_eq!(
            gh.full_name_of("https://github.com/acme/widget/tree/main"),
            None
        );
        assert_eq!(gh.full_name_of("acme/widget/extra"), None);

        // Not enough to name one.
        assert_eq!(gh.full_name_of("acme"), None);
        assert_eq!(gh.full_name_of(""), None);
        assert_eq!(gh.full_name_of("acme/"), None);
        assert_eq!(gh.full_name_of("/widget"), None);
    }

    /// An origin we cannot name is answered "we do not know", never
    /// "zero".
    ///
    /// The distinction is the whole of the imported-count feature: a
    /// mirrored project showing a confident `0 on GitHub` beside its
    /// own honest count is the lie the two-field shape exists to
    /// prevent. This arm returns before any network call, so it is
    /// testable without one.
    #[test]
    fn repo_meta_says_nothing_about_an_origin_it_cannot_name() {
        let gh = app();
        assert_eq!(gh.repo_meta("https://gitlab.com/acme/widget"), Ok(None));
        assert_eq!(gh.repo_meta("acme"), Ok(None));
    }

    #[test]
    fn b64url_matches_known_vectors() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn github_app_jwt_token_exchange_and_cache() {
        let gh = stratum_testkit::fake_github::spawn();
        let app = GithubApp::new(
            "12345".into(),
            stratum_testkit::fake_github::TEST_APP_KEY_PEM.into(),
            gh.base_url.clone(),
            "https://github.example".into(),
            "secret".into(),
        );
        let repo = stratum_control::registry::Repo {
            description: None,
            homepage: None,
            id: "r1".into(),
            org_id: "o1".into(),
            name: "private".into(),
            kind: stratum_control::registry::RepoKind::Mirror,
            public: false,
            default_branch: "main".into(),
            origin_url: Some("acme/private".into()),
            origin_provider: Some("github".into()),
            origin_installation: Some("777".into()),
            last_sync_at: None,
            last_synced_commit: None,
            sync_error: None,
            created_at: 0,
        };
        let url = app.fetch_url(&repo).unwrap();
        assert_eq!(
            url,
            "https://x-access-token:ghs_fake_1@github.example/acme/private.git"
        );
        // Cached: a second fetch_url mints no new token.
        let url2 = app.fetch_url(&repo).unwrap();
        assert_eq!(url, url2);
        assert_eq!(
            gh.tokens_issued.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    /// Every arm of the status/conclusion mapping, including the one
    /// GitHub has not invented yet.
    ///
    /// Exhaustive on purpose. The two fields are a product, not a union,
    /// and every wrong pairing is a plausible-looking mistake: reading
    /// `completed` without its conclusion calls a failure green, reading
    /// the conclusion without the status calls an in-flight run finished.
    ///
    /// The last block is the one that matters. An unrecognised
    /// conclusion must become `queued` — "we do not know yet" — and must
    /// never become `passing`, because `passing` is the state a person
    /// acts on. GitHub has added conclusions three times (`skipped`,
    /// `stale`, `action_required`); the day it adds a fourth, this
    /// mapping degrades and the poller keeps running.
    #[test]
    fn run_state_maps_every_github_verdict_and_degrades_the_rest() {
        // In flight.
        assert_eq!(run_state("in_progress", None), "running");
        // A conclusion on an in-progress run is not a thing GitHub
        // sends, and if it started, the run is still running.
        assert_eq!(run_state("in_progress", Some("success")), "running");

        // Finished, in every conclusion we know.
        assert_eq!(run_state("completed", Some("success")), "passing");
        assert_eq!(run_state("completed", Some("failure")), "failing");
        assert_eq!(run_state("completed", Some("timed_out")), "failing");
        assert_eq!(run_state("completed", Some("startup_failure")), "failing");
        assert_eq!(run_state("completed", Some("cancelled")), "cancelled");
        assert_eq!(run_state("completed", Some("skipped")), "skipped");
        assert_eq!(run_state("completed", Some("neutral")), "skipped");

        // Not started.
        for s in ["queued", "waiting", "requested", "pending"] {
            assert_eq!(run_state(s, None), "queued", "{s}");
        }

        // Unknown, in both fields. `queued`, never `passing`.
        for (status, conclusion) in [
            ("completed", Some("action_required")),
            ("completed", Some("stale")),
            ("completed", Some("something_github_ships_in_2027")),
            // `completed` with no conclusion at all is a real transient
            // state on GitHub, seen between the run finishing and the
            // conclusion being written.
            ("completed", None),
            ("a status we have never seen", None),
            ("", None),
        ] {
            let got = run_state(status, conclusion);
            assert_eq!(got, "queued", "{status:?}/{conclusion:?}");
            assert_ne!(
                got, "passing",
                "an unrecognised verdict rendered as a green tick, which is \
                 the one wrong answer a person merges on"
            );
        }
    }

    /// The two rate limits, and the refusals that are not rate limits,
    /// decided without a socket.
    ///
    /// `now` is fixed, so the reset arithmetic is a value and not a race.
    /// The case that matters most is the last block: a 403 with a budget
    /// still on it is a permission problem, and must not become an
    /// eternal retry — the mirror image of the bug this fixes.
    #[test]
    fn rate_limit_wait_reads_both_of_githubs_limits_and_neither_of_its_refusals() {
        const NOW: u64 = 1_800_000_000;

        // Secondary: a duration, in `Retry-After`.
        assert_eq!(rate_limit_wait(Some("30"), None, None, NOW), Some(30));
        assert_eq!(rate_limit_wait(Some(" 30 "), None, None, NOW), Some(30));
        // Clamped at both ends: never a spin, never a wait past the hour
        // a primary budget takes to return.
        assert_eq!(rate_limit_wait(Some("0"), None, None, NOW), Some(1));
        assert_eq!(rate_limit_wait(Some("99999"), None, None, NOW), Some(3600));

        // Primary: a zero budget and an **absolute** reset. Read as a
        // duration this would be a wait of fifty-five years.
        assert_eq!(
            rate_limit_wait(None, Some("0"), Some(&(NOW + 300).to_string()), NOW),
            Some(300)
        );
        // A reset already past, and one we cannot read: the budget is
        // still spent, so both wait rather than falling through to a
        // refusal and being reported as a missing permission.
        assert_eq!(
            rate_limit_wait(None, Some("0"), Some(&(NOW - 50).to_string()), NOW),
            Some(1)
        );
        assert_eq!(rate_limit_wait(None, Some("0"), None, NOW), Some(60));
        assert_eq!(
            rate_limit_wait(None, Some("0"), Some("not a timestamp"), NOW),
            Some(60)
        );
        // A reset absurdly far out is a clock disagreeing with us, not a
        // budget, and waiting on it would strand the poller.
        assert_eq!(
            rate_limit_wait(None, Some("0"), Some("4000000000"), NOW),
            Some(3600)
        );

        // Not rate limits. A permission denial carries a *nonzero*
        // budget, and GitHub sends these headers on it — so keying on
        // their presence rather than on the zero would retry a refusal
        // that will never once succeed.
        assert_eq!(rate_limit_wait(None, Some("4999"), None, NOW), None);
        assert_eq!(
            rate_limit_wait(None, Some("4999"), Some(&(NOW + 300).to_string()), NOW),
            None
        );
        // No rate headers at all is not a rate limit either.
        assert_eq!(rate_limit_wait(None, None, None, NOW), None);
        // And a remaining we cannot parse is not a zero.
        assert_eq!(rate_limit_wait(None, Some(""), None, NOW), None);
        assert_eq!(rate_limit_wait(None, Some("none"), None, NOW), None);
    }

    /// `Link` is the only place the next page is named, so what this
    /// declines matters as much as what it returns: a walk that reads a
    /// `prev` or a `last` as `next` either loops or goes backwards, and
    /// one that returns `None` too eagerly stops a page short with no
    /// error anywhere.
    ///
    /// Untested until now, and the skip arm — a part of the header that
    /// is not `next` — had never executed, because every header the
    /// suite produced named `next` in its **first** part.
    #[test]
    fn next_link_takes_only_the_next_relation() {
        // GitHub's own shape, `next` first.
        assert_eq!(
            next_link(
                "<https://api.github.com/x?page=2>; rel=\"next\", \
                 <https://api.github.com/x?page=9>; rel=\"last\""
            )
            .as_deref(),
            Some("https://api.github.com/x?page=2")
        );
        // `next` **not** first, which is the arm that was never run: the
        // parts before it have to be skipped rather than the first one
        // taken. Reading `prev` as `next` walks a paging loop backwards
        // for ever.
        assert_eq!(
            next_link(
                "<https://api.github.com/x?page=1>; rel=\"prev\", \
                 <https://api.github.com/x?page=1>; rel=\"first\", \
                 <https://api.github.com/x?page=3>; rel=\"next\""
            )
            .as_deref(),
            Some("https://api.github.com/x?page=3")
        );
        // The last page: `next` is absent, and inventing one is how a
        // walk never terminates.
        assert_eq!(
            next_link(
                "<https://api.github.com/x?page=1>; rel=\"first\", \
                 <https://api.github.com/x?page=9>; rel=\"last\""
            ),
            None
        );
        assert_eq!(next_link(""), None);
        // A `next` relation with no URL brackets is not a URL, and half
        // of one is worse than none.
        assert_eq!(next_link("rel=\"next\""), None);
    }

    /// Timestamps: parsed, or `None`. Never zero, which is a date.
    #[test]
    fn rfc3339_millis_parses_or_declines_but_never_invents_1970() {
        assert_eq!(rfc3339_millis(Some("1970-01-01T00:00:00Z")), Some(0));
        assert_eq!(
            rfc3339_millis(Some("2024-05-01T09:00:00Z")),
            Some(1_714_554_000_000)
        );
        // Fractional seconds and an offset suffix are both shapes GitHub
        // sends; neither is allowed to defeat the parse.
        assert_eq!(
            rfc3339_millis(Some("2024-05-01T09:00:00.123Z")),
            Some(1_714_554_000_000)
        );
        assert_eq!(
            rfc3339_millis(Some("2024-05-01T09:00:00+00:00")),
            Some(1_714_554_000_000)
        );

        // **Offsets are applied.** Everything above this line passes
        // against a parser that reads the first 19 characters and throws
        // the zone away — `+00:00` is the one offset where that bug
        // cannot show, and it was the only one asserted.
        assert_eq!(
            rfc3339_millis(Some("2024-05-01T11:00:00+02:00")),
            Some(1_714_554_000_000),
            "an offset ahead of UTC was not subtracted"
        );
        assert_eq!(
            rfc3339_millis(Some("2024-05-01T04:00:00-05:00")),
            Some(1_714_554_000_000),
            "an offset behind UTC was not added"
        );
        // Both spellings RFC3339 allows, and an offset with minutes in
        // it — the half-hour zones are where a wrong sign or a dropped
        // `:30` is easiest to miss.
        assert_eq!(
            rfc3339_millis(Some("2024-05-01T11:00:00+0200")),
            Some(1_714_554_000_000)
        );
        assert_eq!(
            rfc3339_millis(Some("2024-05-01T14:30:00+05:30")),
            Some(1_714_554_000_000)
        );
        // Fraction and offset together, which is the shape that has to
        // survive both pieces of the parse at once.
        assert_eq!(
            rfc3339_millis(Some("2024-05-01T11:00:00.500+02:00")),
            Some(1_714_554_000_000)
        );
        // **No zone at all**, which is the tolerance the doc comment
        // states and the one case none of the above reaches: every
        // string here carries a `Z` or an offset, so the arm that reads
        // a bare local timestamp as UTC had never run. RFC3339 requires
        // an offset, so this is a tolerance rather than a rule — but a
        // provider that omits one is real, and reading it as UTC is at
        // least what it says.
        assert_eq!(
            rfc3339_millis(Some("2024-05-01T09:00:00")),
            Some(1_714_554_000_000)
        );
        // And the same with fractional seconds, which are stripped
        // before the zone is looked for — so this arrives at that arm
        // with an empty tail rather than a `.500`.
        assert_eq!(
            rfc3339_millis(Some("2024-05-01T09:00:00.500")),
            Some(1_714_554_000_000)
        );

        // A zone we cannot read is `None`, not a guess: a suffix that
        // might be an offset, read as UTC, is a confidently wrong time.
        assert_eq!(rfc3339_millis(Some("2024-05-01T09:00:00+2")), None);
        assert_eq!(rfc3339_millis(Some("2024-05-01T09:00:00+99:00")), None);
        assert_eq!(rfc3339_millis(Some("2024-05-01T09:00:00 CEST")), None);
        assert_eq!(rfc3339_millis(Some("2024-05-01T09:00:00ZZ")), None);

        // Absent, truncated, and garbage — all `None`, and specifically
        // not `Some(0)`, which would render as January 1970 and sort to
        // the top or bottom of every list of runs.
        for bad in [
            None,
            Some(""),
            Some("2024-05-01"),
            Some("not a timestamp at all"),
            // Digits in the right places, nonsense in the values.
            Some("2024-13-01T09:00:00Z"),
            Some("2024-05-32T09:00:00Z"),
            Some("2024-05-01T24:00:00Z"),
            Some("2024-05-01T09:60:00Z"),
            // A signed year: `parse::<i64>` would take this happily and
            // return a real-looking millisecond count for it.
            Some("-024-05-01T09:00:00Z"),
            Some("20x4-05-01T09:00:00Z"),
        ] {
            assert_eq!(rfc3339_millis(bad), None, "{bad:?}");
        }
    }

    /// A response whose shape we do not recognise is an error, not an
    /// empty list — the failure mode this whole slice exists to avoid.
    #[test]
    fn a_response_without_workflow_runs_is_an_error_and_not_no_ci() {
        assert!(parse_actions_runs("{\"total_count\":0}").is_err());
        assert!(parse_actions_runs("[]").is_err());
        assert!(parse_actions_runs("not json").is_err());
        // An empty list, however, is genuinely an empty list.
        assert_eq!(
            parse_actions_runs("{\"total_count\":0,\"workflow_runs\":[]}"),
            Ok(vec![])
        );
    }

    /// The read client against the hermetic fake, walked to the end.
    ///
    /// Paging is followed out of `Link` rather than counted, because
    /// counting works right up until the last page is exactly full — and
    /// `actions_runs_next` goes through the host guard, so this also
    /// proves the guard accepts the fake's own URLs rather than refusing
    /// the client's only way forward.
    #[test]
    fn actions_runs_reads_every_page_and_every_optional_field() {
        let gh = stratum_testkit::fake_github::spawn();
        let app = GithubApp::new(
            "12345".into(),
            stratum_testkit::fake_github::TEST_APP_KEY_PEM.into(),
            gh.base_url.clone(),
            "https://github.example".into(),
            "secret".into(),
        );

        let mut all: Vec<ActionsRun> = Vec::new();
        let mut got = app.actions_runs("777", "many/widget", 1).unwrap();
        let mut pages = 0;
        loop {
            pages += 1;
            assert!(pages <= 10, "the walk did not terminate");
            let ActionsRuns::Data { runs, next } = got else {
                panic!("the fake rate limited a walk it was not asked to");
            };
            all.extend(runs);
            match next {
                Some(url) => got = app.actions_runs_next("777", &url).unwrap(),
                None => break,
            }
        }
        assert_eq!(pages, 3, "250 runs at 100 a page is three pages");
        assert_eq!(all.len(), 250);

        // Ids and run numbers are different numbers, and neither is the
        // other. Confusing them is silent both ways.
        let first = &all[0];
        assert_eq!(first.run_number, 250);
        assert_ne!(first.id, first.run_number.to_string());
        assert!(first.id.starts_with("900"), "{}", first.id);
        assert_eq!(first.event, "push");
        assert_eq!(first.name.as_deref(), Some("CI"));
        assert!(first.html_url.contains("/actions/runs/"), "{first:?}");
        assert_eq!(first.head_sha.len(), 40);

        // Every optional field is actually absent somewhere in the walk,
        // or this test proves nothing about them.
        assert!(
            all.iter().any(|r| r.actor.is_none()),
            "no run had a deleted actor, so unwrapping one would pass here"
        );
        assert!(all.iter().any(|r| r.head_branch.is_none()));
        assert!(
            all.iter().any(|r| r.run_started_at.is_none()),
            "no run was missing its start time"
        );
        // And where a timestamp *is* present it is a real one, not the
        // zero a lenient parser would leave behind.
        assert!(all
            .iter()
            .filter_map(|r| r.updated_at)
            .all(|ms| ms > 1_600_000_000_000));

        // Every state our vocabulary has, produced by the fixture's own
        // status/conclusion pairs — including the unmapped conclusion,
        // which arrives as `queued`.
        let states: std::collections::BTreeSet<&str> = all.iter().map(|r| r.state()).collect();
        for want in [
            "queued",
            "running",
            "passing",
            "failing",
            "cancelled",
            "skipped",
        ] {
            assert!(states.contains(want), "no run was {want}: {states:?}");
        }
        assert!(
            all.iter()
                .any(|r| r.conclusion.as_deref() == Some("action_required")),
            "the fixture stopped serving an unmapped conclusion, so nothing \
             here proves one degrades rather than panics"
        );
    }

    /// A repository with no CI and a repository we may not read the CI of
    /// are told apart, which is the whole reason this endpoint has an
    /// error of its own.
    #[test]
    fn a_missing_actions_permission_is_distinguishable_from_a_repo_with_no_ci() {
        let gh = stratum_testkit::fake_github::spawn();
        let app = GithubApp::new(
            "12345".into(),
            stratum_testkit::fake_github::TEST_APP_KEY_PEM.into(),
            gh.base_url.clone(),
            "https://github.example".into(),
            "secret".into(),
        );

        // Note the fixture sends rate-limit headers on this refusal, with
        // a budget still on it — as GitHub does. Keying on their presence
        // rather than on a remaining of zero would turn this permission
        // denial into an eternal, silent retry.
        let err = app
            .actions_runs("777", "noperm/widget", 1)
            .expect_err("a 403 must not read as an empty list");
        assert!(
            is_actions_read_denied(&err),
            "the caller cannot tell this from any other failure: {err}"
        );
        assert!(err.contains("actions: read"), "{err}");

        let empty = app.actions_runs("777", "empty/widget", 1).unwrap();
        assert_eq!(
            empty,
            ActionsRuns::Data {
                runs: vec![],
                next: None
            }
        );
    }

    /// The host guard still applies to the runs walk. An installation
    /// token must never leave the host it was minted for, and the next
    /// page's URL is a string GitHub chose.
    #[test]
    fn a_next_page_off_the_configured_host_is_refused_for_runs_too() {
        let app = app();
        let err = app
            .actions_runs_next("777", "https://evil.example/repos/a/b/actions/runs?page=2")
            .expect_err("followed a URL off the API host");
        assert!(err.contains("refusing to follow"), "{err}");
        // And it is not mistaken for the permission failure.
        assert!(!is_actions_read_denied(&err), "{err}");
    }

    /// A spent budget is a rate limit, and specifically not the missing
    /// permission — over a real socket, from a real 403.
    ///
    /// Both are 403s on this endpoint and `Retry-After` is the only thing
    /// between them. Getting it wrong is not cosmetic in either
    /// direction: reading a rate limit as a permission failure tells a
    /// person to grant something they already granted and stops the
    /// poller for good, and reading a permission failure as a rate limit
    /// makes it retry a request that will never once succeed.
    #[test]
    fn a_spent_budget_is_a_rate_limit_and_not_the_permission_refusal() {
        let gh = stratum_testkit::fake_github::spawn();
        let app = GithubApp::new(
            "12345".into(),
            stratum_testkit::fake_github::TEST_APP_KEY_PEM.into(),
            gh.base_url.clone(),
            "https://github.example".into(),
            "secret".into(),
        );

        // The **secondary** limit, which carries `Retry-After`.
        match app.actions_runs("777", "ratelimited/widget", 1) {
            Ok(ActionsRuns::RateLimited { retry_after_secs }) => {
                assert!(retry_after_secs > 0, "a wait of zero is a spin");
            }
            other => panic!("a rate limit did not survive the read: {other:?}"),
        }

        // The **primary** limit — the hourly budget spent — which carries
        // no `Retry-After` at all, only a zero remaining and an absolute
        // reset. This is the half that was untestable: the classifier
        // read `Retry-After` alone, so every primary exhaustion arrived
        // as `ACTIONS_READ_DENIED` and this test passed anyway, because
        // the fixture could only produce the other half.
        match app.actions_runs("777", "budgetspent/widget", 1) {
            Ok(ActionsRuns::RateLimited { retry_after_secs }) => {
                assert!(retry_after_secs > 0, "a wait of zero is a spin");
                assert!(
                    retry_after_secs <= 3600,
                    "the reset was read as a duration rather than a \
                     timestamp: {retry_after_secs}s"
                );
            }
            Err(e) if is_actions_read_denied(&e) => panic!(
                "a spent budget was reported to a maintainer as a missing \
                 permission, next to a button that re-grants what they \
                 already have"
            ),
            other => panic!("a primary rate limit did not survive the read: {other:?}"),
        }

        // And the wait is honoured by coming back, not by giving up: the
        // work was never lost. (The fixture alternates, so the next call
        // is the recovery a poller would make after sleeping.)
        let ActionsRuns::Data { runs, .. } =
            app.actions_runs("777", "ratelimited/widget", 1).unwrap()
        else {
            panic!("the retry was refused as well, so nothing here proves recovery");
        };
        assert!(!runs.is_empty());
    }

    /// The other two arms of the page reader, which no live fixture
    /// produces: a rate limit passes through as itself, and a refusal
    /// that is not a 403 keeps its status and body.
    #[test]
    fn a_rate_limit_survives_and_a_non_403_refusal_says_what_it_was() {
        assert_eq!(
            actions_from_page(Page::RateLimited {
                retry_after_secs: 12
            }),
            Ok(ActionsRuns::RateLimited {
                retry_after_secs: 12
            })
        );
        let err = actions_from_page(Page::Refused {
            status: 500,
            body: "upstream is unwell".into(),
        })
        .expect_err("a 500 is not runs");
        assert!(err.contains("500"), "{err}");
        assert!(err.contains("upstream is unwell"), "{err}");
        assert!(
            !is_actions_read_denied(&err),
            "a server error was reported to a person as a permission problem: {err}"
        );
    }

    /// Which pairs of hosts may be fetched between, exhaustively.
    ///
    /// Pure string work with no network in it at all, which is the point:
    /// the decision that stops a token crossing hosts must be testable
    /// without dialling either of them, or it gets tested once by hand
    /// and never again.
    #[test]
    fn github_hosts_must_both_be_real_or_neither() {
        // Production.
        assert_eq!(
            github_hosts_paired("https://api.github.com", "https://github.com"),
            Ok(())
        );
        // The same, as somebody pastes it into a deployment config.
        assert_eq!(
            github_hosts_paired("https://api.github.com/", "https://github.com/"),
            Ok(())
        );
        // GitHub Enterprise: not github.com, but coherently not.
        assert_eq!(
            github_hosts_paired("https://ghe.corp/api/v3", "https://ghe.corp"),
            Ok(())
        );
        // A hermetic test: fake API, file:// origins on disk.
        assert_eq!(
            github_hosts_paired("http://127.0.0.1:54321", "file:///tmp/origins"),
            Ok(())
        );

        // The e2e suite's actual configuration: the API pointed at the
        // fake and the git side left on its default. A fake's token,
        // sent to github.com.
        let err = github_hosts_paired("http://127.0.0.1:54321", "https://github.com")
            .expect_err("a fake token would have been sent to github.com");
        assert!(err.contains("STRATUM_GITHUB_GIT_BASE"), "{err}");
        assert!(err.contains("installation token"), "{err}");

        // And the reverse, which is the same mistake with a real token
        // and is strictly worse.
        assert!(github_hosts_paired("https://api.github.com", "https://evil.example").is_err());
        assert!(github_hosts_paired("https://api.github.com", "file:///tmp/origins").is_err());
    }

    /// **Nothing dials out, and nothing is minted to dial with.**
    ///
    /// This is the assertion the finding actually needs. `tokens_issued`
    /// is the fake's own count of installation tokens it has handed over,
    /// so a zero here is a positive statement rather than an absence of
    /// evidence: the refusal happened *before* the credential existed, so
    /// there was never anything to send to github.com even if the fetch
    /// had gone on to run.
    ///
    /// Checking the ordering matters as much as checking the refusal. A
    /// guard placed after the token exchange would still fail the fetch,
    /// still make the suite look fixed, and still have minted a
    /// credential for a host it then refused to talk to.
    #[test]
    fn a_mismatched_git_base_mints_no_token_and_opens_no_socket() {
        let gh = stratum_testkit::fake_github::spawn();
        let app = GithubApp::new(
            "12345".into(),
            stratum_testkit::fake_github::TEST_APP_KEY_PEM.into(),
            // Exactly what `import_e2e` configures: the API at the fake,
            // the git base left at its default.
            gh.base_url.clone(),
            "https://github.com".into(),
            "secret".into(),
        );
        let repo = stratum_control::registry::Repo {
            description: None,
            homepage: None,
            id: "r1".into(),
            org_id: "o1".into(),
            name: "widget".into(),
            kind: stratum_control::registry::RepoKind::Mirror,
            public: false,
            default_branch: "main".into(),
            origin_url: Some("acme/widget".into()),
            origin_provider: Some("github".into()),
            origin_installation: Some("777".into()),
            last_sync_at: None,
            last_synced_commit: None,
            sync_error: None,
            created_at: 0,
        };

        let err = app
            .fetch_url(&repo)
            .expect_err("built a github.com URL from a fake's installation");
        assert!(err.contains("refusing to fetch"), "{err}");
        assert_eq!(
            gh.tokens_issued.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a token was minted before the hosts were checked, so a \
             credential existed for a host we then refused to talk to"
        );
    }

    /// The dashboard's lead path is "paste a public URL, mirror it now,
    /// install nothing". The server accepted exactly that — provider
    /// `github`, no installation — probed the origin, created the repo,
    /// and then refused every sync with "mirror has no installation id".
    /// The hermetic suite could not see it: a `file://` git base returns
    /// before the installation check, so only real github.com reached
    /// it. A public origin with no credential is fetched as itself.
    #[test]
    fn a_public_origin_with_no_installation_is_fetched_without_a_credential() {
        let gh = stratum_testkit::fake_github::spawn();
        let app = GithubApp::new(
            "12345".into(),
            stratum_testkit::fake_github::TEST_APP_KEY_PEM.into(),
            gh.base_url.clone(),
            "https://github.example".into(),
            "secret".into(),
        );
        let repo = stratum_control::registry::Repo {
            description: None,
            homepage: None,
            id: "r1".into(),
            org_id: "o1".into(),
            name: "hello-world".into(),
            kind: stratum_control::registry::RepoKind::Mirror,
            public: true,
            default_branch: "main".into(),
            origin_url: Some("octocat/Hello-World".into()),
            origin_provider: Some("github".into()),
            origin_installation: None,
            last_sync_at: None,
            last_synced_commit: None,
            sync_error: None,
            created_at: 0,
        };
        let url = app.fetch_url(&repo).unwrap();
        assert_eq!(url, "https://github.example/octocat/Hello-World.git");
        assert_eq!(
            gh.tokens_issued.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no installation, so nothing to mint a token from"
        );
    }

    #[test]
    fn webhook_hmac_verification() {
        let secret = "hunter2";
        let body = br#"{"repository":{"full_name":"a/b"}}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let sig = format!(
            "sha256={}",
            stratum_store::pack::hex(&mac.finalize().into_bytes())
        );
        let g = Generic {
            webhook_secret: secret.into(),
        };
        let ev = g.verify_webhook(Some(&sig), body).unwrap();
        assert_eq!(ev.full_name, "a/b");
        assert!(g.verify_webhook(Some("sha256=deadbeef"), body).is_err());
        assert!(g.verify_webhook(None, body).is_err());
    }
}

/// The App's own calls, against the fake — which encodes what we believe
/// GitHub answers.
#[cfg(test)]
mod app_tests {
    use super::*;

    fn app() -> (stratum_testkit::fake_github::FakeGithub, GithubApp) {
        let gh = stratum_testkit::fake_github::spawn();
        let app = GithubApp::new(
            "12345".into(),
            stratum_testkit::fake_github::TEST_APP_KEY_PEM.into(),
            gh.base_url.clone(),
            "https://github.example".into(),
            "secret".into(),
        );
        (gh, app)
    }

    /// An installation's permissions are read as facts, not discovered
    /// by failing: an organisation and a person that hold both, one
    /// that predates the feature and holds neither, and one that is
    /// gone.
    #[test]
    fn an_installation_says_what_it_may_do_and_whose_it_is() {
        let (_gh, app) = app();
        let org = app.installation("4001").unwrap().unwrap();
        assert_eq!(
            (org.account.as_str(), org.target_type.as_str()),
            ("acme-inc", "Organization")
        );
        assert!(org.administration_write && org.actions_write && org.contents_write);
        assert!(org.events.contains(&"workflow_job".to_string()));
        assert!(!org.suspended);
        let person = app.installation("4002").unwrap().unwrap();
        assert_eq!(
            (person.account.as_str(), person.target_type.as_str()),
            ("ada", "User")
        );
        assert!(person.administration_write && person.actions_write);
        let old = app.installation("4003").unwrap().unwrap();
        assert!(!old.administration_write && !old.actions_write && !old.contents_write);
        // Approved once, and not since: pushes are the permission it
        // lacks, and it must not read as lacking the others too.
        let prepush = app.installation("4007").unwrap().unwrap();
        assert!(prepush.administration_write && prepush.actions_write);
        assert!(!prepush.contents_write);
        assert!(!old.events.contains(&"workflow_job".to_string()));
        assert!(app.installation("4999").unwrap().is_none());
        // The two refusals that are not "gone": a read the App is not
        // allowed, and a rate limit — each an error naming which.
        let refused = app.installation("4004").unwrap_err();
        assert!(
            refused.contains("refused") && refused.contains("403"),
            "{refused}"
        );
        let limited = app.installation("4005").unwrap_err();
        assert!(
            limited.contains("rate limiting") && limited.contains("7s"),
            "{limited}"
        );
        // Where a person approves what is missing: the organisation's
        // settings for an organisation, the person's own for a person.
        assert_eq!(
            app.approve_url(&org),
            "https://github.example/organizations/acme-inc/settings/installations/4001"
        );
        assert_eq!(
            app.approve_url(&person),
            "https://github.example/settings/installations/4002"
        );
    }

    /// The recorded wire, parsed by the code the product runs.
    ///
    /// These bodies came off real github.com on 2026-09-13 via
    /// `scripts/manual-github-signin.sh fixtures`, with addresses and
    /// the login scrubbed and the *shape* kept. The whole GitHub
    /// sign-up rests on one field in them — `verified`, on the `primary`
    /// entry — and until this ran, that field was something we had
    /// asserted about GitHub rather than seen.
    ///
    /// The fake in `stratum-testkit` answers the same two routes. If it
    /// and the recording ever disagree about the shape, this goes red
    /// and the suite stops believing a thing GitHub does not do.
    #[test]
    fn github_signin_fixtures_parse_like_the_fake() {
        const USER: &str =
            include_str!("../../../stratum-testkit/fixtures/github-signin/user.json");
        const EMAILS: &str =
            include_str!("../../../stratum-testkit/fixtures/github-signin/user-emails.json");

        // BELIEF 4, as recorded: a numeric id, and a login beside it.
        let ident = identity_from(USER).expect("the recorded user body parses");
        assert_eq!(ident.id, "6254949");
        assert_eq!(ident.login, "octocat");

        // BELIEFS 1 and 2: a flat array, `primary` and `verified` as
        // separate booleans, exactly one primary — and the rule the
        // sign-in actually applies finds an address in it.
        assert_eq!(
            verified_primary_from(EMAILS),
            Some("person0@example.com".to_string())
        );

        // The recording caught GitHub answering `"email": null` on
        // `GET /user` for an account that *does* have a proved primary.
        // That is the case this module refuses to read that field for:
        // an implementation trusting it would have had nothing here.
        let raw: serde_json::Value = serde_json::from_str(USER).unwrap();
        assert!(raw["email"].is_null(), "the recorded public email is null");

        // And the fake answers the same shapes. `fake_user`'s verified
        // person is what a sign-in e2e drives, so the two must agree
        // about which fields carry the answer.
        let fake_emails = "[{\"email\":\"ada@example.com\",\"primary\":true,\"verified\":true,\
                            \"visibility\":\"private\"},\
                           {\"email\":\"ada@users.noreply.github.com\",\"primary\":false,\
                            \"verified\":true,\"visibility\":null}]";
        assert_eq!(
            verified_primary_from(fake_emails),
            Some("ada@example.com".to_string())
        );
        let fake_user = "{\"id\":501,\"login\":\"ada\",\"name\":\"Person ada\",\"type\":\"User\"}";
        let fake_ident = identity_from(fake_user).expect("the fake's user body parses");
        assert_eq!(fake_ident.id, "501");
        assert_eq!(fake_ident.login, "ada");
    }
}
