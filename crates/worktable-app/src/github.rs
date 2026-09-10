//! GitHub Stars fetcher.

use anyhow::Context as _;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct GithubRepo {
    pub name: String,
    pub stars: u64,
    pub html_url: String,
}

/// A repository the user starred, with the moment they starred it.
#[derive(Debug, Clone)]
pub struct StarredRepo {
    pub full_name: String,
    pub description: Option<String>,
    pub html_url: String,
    /// Unix epoch milliseconds of GitHub's `starred_at`.
    pub starred_at_ms: i64,
}

#[derive(Debug, Deserialize)]
struct StarredResponse {
    #[serde(default)]
    starred_at: String,
    repo: RepoResponse,
}

/// Log the raw response body for diagnostics while keeping it out of the
/// user-facing error copy (`worktable_view` displays these errors directly).
fn log_api_body(context: &str, body: &str) {
    let body = body.trim();
    if !body.is_empty() {
        eprintln!("Worktable: {context}: {body}");
    }
}

#[derive(Debug, Deserialize)]
struct RepoResponse {
    name: String,
    #[serde(default)]
    full_name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    stargazers_count: u64,
    #[serde(default)]
    html_url: String,
}

/// Fetch the repositories `username` **starred** and return their total stars
/// plus a per-repo breakdown.
///
/// - Uses `https://api.github.com/users/{username}/starred?per_page=100&page=N`
///   with `application/vnd.github.star+json` so `starred_at` comes along.
/// - Handles pagination via `Link` header (`rel="next"`) and via `len < 100` heuristic.
/// - Sends `User-Agent: Worktable/0.1.0` (required by GitHub API).
/// - If `token` is `Some`, sends `Authorization: Bearer <token>` (raises anon limit 60/h → 5000/h).
/// - Returns `(total_stars, starred_repos_sorted_by_stars_desc)`.
pub async fn fetch_github_stars(
    username: &str,
    token: Option<String>,
) -> anyhow::Result<(u64, Vec<GithubRepo>)> {
    let username = username.trim();
    if username.is_empty() {
        anyhow::bail!("GitHub username is empty");
    }
    if username.contains('/') || username.contains(' ') {
        anyhow::bail!("Invalid GitHub username: '{}'", username);
    }

    let client = reqwest::Client::builder()
        .user_agent("Worktable/0.1.0")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("failed to build HTTP client")?;

    let mut all_repos: Vec<GithubRepo> = Vec::new();
    let mut page: u32 = 1;

    loop {
        let url = format!(
            "https://api.github.com/users/{}/starred?per_page=100&page={}",
            username, page
        );

        let mut req = client
            .get(&url)
            .header("User-Agent", "Worktable/0.1.0")
            .header("Accept", "application/vnd.github.star+json")
            .header("X-GitHub-Api-Version", "2022-11-28");

        if let Some(ref t) = token {
            let t = t.trim();
            if !t.is_empty() {
                // GitHub accepts both "Bearer" and "token" prefixes. "Bearer" is canonical for fine-grained PATs.
                req = req.header("Authorization", format!("Bearer {t}"));
            }
        }

        let resp = req.send().await.context("failed to send GitHub request")?;
        let status = resp.status();
        let headers = resp.headers().clone();

        if status.as_u16() == 404 {
            let body = resp.text().await.unwrap_or_default();
            log_api_body("GitHub user lookup returned 404", &body);
            anyhow::bail!("GitHub user '{username}' not found.");
        }
        if status.as_u16() == 401 {
            let body = resp.text().await.unwrap_or_default();
            log_api_body("GitHub authentication returned 401", &body);
            anyhow::bail!("GitHub authentication failed. Check GITHUB_TOKEN.");
        }
        if status.as_u16() == 403 {
            let body = resp.text().await.unwrap_or_default();
            // 403 is most often rate limiting when anonymous (60/h). Log the
            // rate-limit headers, but keep user copy actionable.
            let remaining = headers
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("?");
            let reset = headers
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("?");
            log_api_body(
                &format!("GitHub API returned 403 (remaining={remaining} reset={reset})"),
                &body,
            );
            anyhow::bail!("GitHub API rate limit reached. Set GITHUB_TOKEN to raise it.");
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            log_api_body(&format!("GitHub API returned {status}"), &body);
            anyhow::bail!("GitHub API error {status}.");
        }

        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let repos: Vec<StarredResponse> = resp
            .json::<Vec<StarredResponse>>()
            .await
            .context("failed to parse GitHub response")?;

        let repos_len = repos.len();
        for item in repos {
            let r = item.repo;
            all_repos.push(GithubRepo {
                name: if r.full_name.is_empty() {
                    r.name
                } else {
                    r.full_name
                },
                stars: r.stargazers_count,
                html_url: r.html_url,
            });
        }

        // Pagination: prefer Link header if present, else fall back to len < 100.
        let has_next = headers
            .get("link")
            .and_then(|v| v.to_str().ok())
            .map(|link| link.contains(r#"rel="next""#))
            .unwrap_or(false);

        if has_next {
            page += 1;
            if page > 20 {
                // Safety cap: 20 * 100 = 2000 repos, far beyond typical users.
                break;
            }
            // Small delay to be nice to the API (not required but polite).
            // We don't sleep here to keep fetch snappy; rely on rate limits.
            continue;
        }

        if repos_len < 100 {
            break;
        }
        // No Link header but got exactly 100: try next page heuristically.
        // If user has exactly N*100 repos this may do one extra empty fetch,
        // which will then break on 0.
        if repos_len == 100 {
            // Check remaining header to avoid hammering when rate limited.
            if let Some(rem) = remaining.as_deref().and_then(|s| s.parse::<i32>().ok())
                && rem <= 1
            {
                anyhow::bail!("GitHub rate limit nearly exhausted. Set GITHUB_TOKEN to raise it.");
            }
            page += 1;
            if page > 20 {
                break;
            }
            continue;
        }
        break;
    }

    // Sort descending by stars for nicer display, then by name.
    all_repos.sort_by(|a, b| b.stars.cmp(&a.stars).then_with(|| a.name.cmp(&b.name)));

    let total: u64 = all_repos.iter().map(|r| r.stars).sum();

    Ok((total, all_repos))
}

/// Fetch the starred repositories with an injectable API root, so the
/// parsing/pagination path can be exercised against a local mock server.
pub async fn fetch_starred_repos_with_base(
    username: &str,
    token: Option<String>,
    api_base: &str,
) -> anyhow::Result<Vec<StarredRepo>> {
    let username = username.trim();
    if username.is_empty() {
        anyhow::bail!("GitHub username is empty");
    }

    let client = reqwest::Client::builder()
        .user_agent("Worktable/0.1.0")
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("failed to build HTTP client")?;

    let mut all: Vec<StarredRepo> = Vec::new();
    let mut page: u32 = 1;
    loop {
        let url = format!(
            "{}/users/{}/starred?per_page=100&page={}",
            api_base.trim_end_matches('/'),
            username,
            page
        );
        let mut req = client
            .get(&url)
            .header("User-Agent", "Worktable/0.1.0")
            // The star+json media type wraps each repo with its `starred_at`.
            .header("Accept", "application/vnd.github.star+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(ref t) = token
            && !t.trim().is_empty()
        {
            req = req.header("Authorization", format!("Bearer {}", t.trim()));
        }

        let resp = req.send().await.context("failed to send GitHub request")?;
        let status = resp.status();
        let headers = resp.headers().clone();
        if status.as_u16() == 404 {
            anyhow::bail!("GitHub user '{username}' not found.");
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            log_api_body(&format!("GitHub starred API returned {status}"), &body);
            anyhow::bail!("GitHub API error {status}.");
        }

        let items: Vec<StarredResponse> = resp
            .json()
            .await
            .context("failed to parse GitHub starred response")?;
        let count = items.len();
        for item in items {
            let full_name = if item.repo.full_name.is_empty() {
                item.repo.name.clone()
            } else {
                item.repo.full_name.clone()
            };
            all.push(StarredRepo {
                full_name,
                description: item.repo.description,
                html_url: item.repo.html_url,
                starred_at_ms: crate::format::iso8601_to_epoch_ms(&item.starred_at)
                    .unwrap_or_default(),
            });
        }

        let has_next = headers
            .get("link")
            .and_then(|v| v.to_str().ok())
            .map(|link| link.contains(r#"rel="next""#))
            .unwrap_or(false);
        if !has_next || count < 100 || page >= 20 {
            break;
        }
        page += 1;
    }

    // Newest stars first.
    all.sort_by_key(|repo| std::cmp::Reverse(repo.starred_at_ms));
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starred_response_deser_keeps_full_name_and_stars() {
        let json = r#"[{"starred_at":"2025-01-02T03:04:05Z","repo":{"name":"gpui","full_name":"zed-industries/gpui","description":null,"stargazers_count":1234,"html_url":"https://github.com/zed-industries/gpui"}}]"#;
        let items: Vec<StarredResponse> = serde_json::from_str(json).unwrap();
        assert_eq!(items[0].repo.full_name, "zed-industries/gpui");
        assert_eq!(items[0].repo.stargazers_count, 1234);
        assert!(!items[0].starred_at.is_empty());
    }

    #[test]
    fn repo_response_deser() {
        let json =
            r#"[{"name":"foo","stargazers_count":42,"html_url":"https://github.com/u/foo"}]"#;
        let repos: Vec<RepoResponse> = serde_json::from_str(json).unwrap();
        assert_eq!(repos[0].name, "foo");
        assert_eq!(repos[0].stargazers_count, 42);
    }
}
