//! GitHub Stars fetcher.

use anyhow::Context as _;
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct GithubRepo {
    pub name: String,
    pub stars: u64,
    pub html_url: String,
}

#[derive(Debug, Deserialize)]
struct RepoResponse {
    name: String,
    #[serde(default)]
    stargazers_count: u64,
    #[serde(default)]
    html_url: String,
}

/// Fetch all public repos for `username` and return total stars + per-repo breakdown.
///
/// - Uses `https://api.github.com/users/{username}/repos?per_page=100&page=N`
/// - Handles pagination via `Link` header (`rel="next"`) and via `len < 100` heuristic.
/// - Sends `User-Agent: Worktable/0.1.0` (required by GitHub API).
/// - If `token` is `Some`, sends `Authorization: Bearer <token>` (raises anon limit 60/h → 5000/h).
/// - Returns `(total_stars, repos_sorted_by_stars_desc)`.
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
            "https://api.github.com/users/{}/repos?per_page=100&page={}&type=owner&sort=updated",
            username, page
        );

        let mut req = client
            .get(&url)
            .header("User-Agent", "Worktable/0.1.0")
            .header("Accept", "application/vnd.github+json")
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
            anyhow::bail!("GitHub user '{}' not found (404). {}", username, body);
        }
        if status.as_u16() == 401 {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub authentication failed (401). Check GITHUB_TOKEN. {}", body);
        }
        if status.as_u16() == 403 {
            let body = resp.text().await.unwrap_or_default();
            // 403 is most often rate limiting when anon (60/h). Surface rate-limit headers if present.
            let remaining = headers
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("?");
            let reset = headers
                .get("x-ratelimit-reset")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("?");
            anyhow::bail!(
                "GitHub API forbidden (403). Rate limit remaining={} reset={}. Body: {} — try setting GITHUB_TOKEN for 5000/h.",
                remaining,
                reset,
                body
            );
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("GitHub API error {}: {}", status, body);
        }

        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let repos: Vec<RepoResponse> = resp
            .json::<Vec<RepoResponse>>()
            .await
            .context("failed to parse GitHub response")?;

        let repos_len = repos.len();
        for r in repos {
            all_repos.push(GithubRepo {
                name: r.name,
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
            if let Some(rem) = remaining.as_deref().and_then(|s| s.parse::<i32>().ok()) {
                if rem <= 1 {
                    anyhow::bail!(
                        "GitHub rate limit nearly exhausted (remaining={}). Set GITHUB_TOKEN to increase limit.",
                        rem
                    );
                }
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

    if all_repos.is_empty() {
        // Could be a valid user with 0 public repos — not an error, just total 0.
        // Caller will show "0 stars" and empty list.
    }

    Ok((total, all_repos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_response_deser() {
        let json = r#"[{"name":"foo","stargazers_count":42,"html_url":"https://github.com/u/foo"}]"#;
        let repos: Vec<RepoResponse> = serde_json::from_str(json).unwrap();
        assert_eq!(repos[0].name, "foo");
        assert_eq!(repos[0].stargazers_count, 42);
    }
}
