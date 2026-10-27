//! GitHub release updates for the macOS app.
//!
//! The app checks the repository's latest **stable** release through the
//! public GitHub API, compares semantic versions, downloads the release DMG,
//! verifies its published SHA-256, stages the `.app`, and hands the final
//! replacement to a detached helper that waits for this process to exit.
//!
//! Nothing here runs on the UI thread: the view calls the service's Tokio
//! runtime, and only the cheap script launch happens after staging.

use std::{
    fmt,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Public repository that publishes Worktable releases.
pub const REPO: &str = "MalinrRuwan/worktable";

/// GitHub's stateless API root. Tests inject a local base.
pub const API_BASE: &str = "https://api.github.com";

/// Human-facing release page when the app cannot replace itself.
pub const RELEASES_PAGE: &str = "https://github.com/MalinrRuwan/worktable/releases/latest";

/// Version compiled into this build.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is semantic")
}

/// A three-part release version. Pre-release suffixes are deliberately not
/// accepted: Worktable only offers stable builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let trimmed = trimmed.strip_prefix('v').unwrap_or(trimmed);
        // Stable only: no `-beta`/`+build` suffix, no extra components.
        if trimmed.is_empty() || !trimmed.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
            return None;
        }
        let mut parts = trimmed.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One release asset advertised by GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseAsset {
    pub name: String,
    pub url: String,
}

/// A stable release the app can offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub version: Version,
    pub tag: String,
    pub name: String,
    pub notes: String,
    pub page_url: String,
    pub dmg: ReleaseAsset,
    pub checksum: ReleaseAsset,
}

/// The outcome of an update check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateCheck {
    UpToDate { current: Version },
    Available(ReleaseInfo),
}

#[derive(Debug, Deserialize)]
struct LatestRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
}

/// Check the latest stable release. Network and HTTP failures return short
/// recovery copy suitable for the dialog; the raw detail goes to the gated log.
pub async fn check() -> Result<UpdateCheck, String> {
    check_with_base(API_BASE).await
}

pub async fn check_with_base(api_base: &str) -> Result<UpdateCheck, String> {
    let url = format!(
        "{}/repos/{REPO}/releases/latest",
        api_base.trim_end_matches('/')
    );
    let client = reqwest::Client::builder()
        .user_agent(format!("Worktable/{}", current_version()))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|error| {
            crate::logging::error(format!("failed to build the update HTTP client: {error}"));
            "Couldn't check for updates. Try again.".to_owned()
        })?;
    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|error| {
            crate::logging::error(format!("update check failed: {error}"));
            "Couldn't reach GitHub. Check the connection and try again.".to_owned()
        })?;
    let status = response.status();
    if status.as_u16() == 404 {
        return Err("No stable Worktable release is published yet.".to_owned());
    }
    if status.as_u16() == 403 || status.as_u16() == 429 {
        return Err("GitHub is limiting update checks. Try again in a few minutes.".to_owned());
    }
    if !status.is_success() {
        crate::logging::error(format!("update check returned {status}"));
        return Err("Couldn't check for updates. Try again.".to_owned());
    }
    let release: LatestRelease = response.json().await.map_err(|error| {
        crate::logging::error(format!("failed to parse the release response: {error}"));
        "GitHub returned an unexpected response. Try again.".to_owned()
    })?;
    release_check(release, current_version())
}

/// Pure release selection: stable tag, DMG asset, and its checksum asset.
fn release_check(release: LatestRelease, current: Version) -> Result<UpdateCheck, String> {
    if release.draft || release.prerelease {
        return Err("The newest release is still a pre-release.".to_owned());
    }
    let Some(version) = Version::parse(&release.tag_name) else {
        crate::logging::error(format!(
            "latest release tag is not a stable version: {}",
            release.tag_name
        ));
        return Err("The newest release has an unexpected version.".to_owned());
    };
    if version <= current {
        return Ok(UpdateCheck::UpToDate { current });
    }

    let dmg = release
        .assets
        .iter()
        .filter(|asset| asset.name.ends_with(".dmg") && !asset.name.ends_with(".dmg.sha256"))
        .max_by_key(|asset| asset.name.starts_with("Worktable-"))
        .map(|asset| ReleaseAsset {
            name: asset.name.clone(),
            url: asset.browser_download_url.clone(),
        })
        .filter(|asset| !asset.url.is_empty())
        .ok_or_else(|| "This release has no macOS installer.".to_owned())?;
    let checksum = release
        .assets
        .iter()
        .find(|asset| {
            asset.name == format!("{}.sha256", dmg.name) || asset.name.ends_with(".dmg.sha256")
        })
        .map(|asset| ReleaseAsset {
            name: asset.name.clone(),
            url: asset.browser_download_url.clone(),
        })
        .filter(|asset| !asset.url.is_empty())
        .ok_or_else(|| "This release can't be verified automatically.".to_owned())?;

    Ok(UpdateCheck::Available(ReleaseInfo {
        version,
        tag: release.tag_name,
        name: if release.name.is_empty() {
            format!("Worktable {version}")
        } else {
            release.name
        },
        notes: release.body,
        page_url: if release.html_url.is_empty() {
            RELEASES_PAGE.to_owned()
        } else {
            release.html_url
        },
        dmg,
        checksum,
    }))
}

/// A downloaded, checksum-verified app staged outside the running bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedUpdate {
    pub version: Version,
    pub staged_app: PathBuf,
}

/// Download the release DMG and its checksum, verify the digest, then mount
/// and copy the `.app` into a staging directory that survives app exit.
pub async fn download_and_stage(release: ReleaseInfo) -> Result<PreparedUpdate, String> {
    let dir = staging_root(&release.version);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|error| {
            crate::logging::error(format!(
                "failed to clear the update staging directory: {error}"
            ));
            "Couldn't prepare the update. Try again.".to_owned()
        })?;
    }
    std::fs::create_dir_all(&dir).map_err(|error| {
        crate::logging::error(format!(
            "failed to create the update staging directory: {error}"
        ));
        "Couldn't prepare the update. Try again.".to_owned()
    })?;

    let client = reqwest::Client::builder()
        .user_agent(format!("Worktable/{}", current_version()))
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|error| {
            crate::logging::error(format!("failed to build the download client: {error}"));
            "Couldn't download the update. Try again.".to_owned()
        })?;
    let dmg_bytes = download(&client, &release.dmg.url, "installer").await?;
    let checksum_text = download(&client, &release.checksum.url, "checksum").await?;
    let checksum_text = String::from_utf8_lossy(&checksum_text);
    let expected = parse_checksum(&checksum_text, &release.dmg.name)
        .ok_or_else(|| "The update checksum is malformed.".to_owned())?;
    let actual = sha256_hex(&dmg_bytes);
    if actual != expected {
        crate::logging::error(format!(
            "update checksum mismatch: expected {expected}, got {actual}"
        ));
        return Err("The update download was incomplete. Try again.".to_owned());
    }

    let dmg_path = dir.join(&release.dmg.name);
    std::fs::write(&dmg_path, &dmg_bytes).map_err(|error| {
        crate::logging::error(format!("failed to save the update DMG: {error}"));
        "Couldn't save the update. Try again.".to_owned()
    })?;

    let version = release.version;
    let dmg_for_stage = dmg_path.clone();
    let dir_for_stage = dir.clone();
    tokio::task::spawn_blocking(move || stage_dmg(&dmg_for_stage, &dir_for_stage, version))
        .await
        .map_err(|error| {
            crate::logging::error(format!("update staging task failed: {error}"));
            "Couldn't prepare the update. Try again.".to_owned()
        })?
}

fn staging_root(version: &Version) -> PathBuf {
    std::env::temp_dir().join(format!("worktable-update-{version}"))
}

async fn download(client: &reqwest::Client, url: &str, what: &str) -> Result<Vec<u8>, String> {
    let response = client.get(url).send().await.map_err(|error| {
        crate::logging::error(format!("failed to download the update {what}: {error}"));
        "Couldn't download the update. Check the connection and try again.".to_owned()
    })?;
    if !response.status().is_success() {
        crate::logging::error(format!(
            "update {what} download returned {}",
            response.status()
        ));
        return Err("Couldn't download the update. Try again.".to_owned());
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| {
            crate::logging::error(format!("failed to read the update {what}: {error}"));
            "The update download was interrupted. Try again.".to_owned()
        })
}

/// Mount the DMG, copy out `Worktable.app`, and verify its version and
/// signature before it is allowed near `/Applications`.
fn stage_dmg(dmg: &Path, dir: &Path, version: Version) -> Result<PreparedUpdate, String> {
    let mount = dir.join("mount");
    std::fs::create_dir_all(&mount).map_err(|error| {
        crate::logging::error(format!("failed to create the update mount point: {error}"));
        "Couldn't prepare the update. Try again.".to_owned()
    })?;
    let attached = Command::new("/usr/bin/hdiutil")
        .arg("attach")
        .arg("-nobrowse")
        .arg("-readonly")
        .arg("-mountpoint")
        .arg(&mount)
        .arg(dmg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            crate::logging::error(format!("failed to run hdiutil: {error}"));
            "Couldn't open the update image. Try again.".to_owned()
        })?;
    if !attached.status.success() {
        crate::logging::error(format!(
            "hdiutil attach failed: {}",
            String::from_utf8_lossy(&attached.stderr)
        ));
        return Err("Couldn't open the update image. Try again.".to_owned());
    }

    let source = mount.join("Worktable.app");
    let staged = dir.join("Worktable.app");
    let copy = if source.is_dir() {
        Command::new("/usr/bin/ditto")
            .arg(&source)
            .arg(&staged)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
    } else {
        detach(&mount);
        return Err("The update image doesn't contain Worktable.".to_owned());
    };
    detach(&mount);
    let copy = copy.map_err(|error| {
        crate::logging::error(format!("failed to copy the staged app: {error}"));
        "Couldn't prepare the update. Try again.".to_owned()
    })?;
    if !copy.status.success() {
        crate::logging::error(format!(
            "ditto failed: {}",
            String::from_utf8_lossy(&copy.stderr)
        ));
        return Err("Couldn't prepare the update. Try again.".to_owned());
    }

    let staged_version = bundle_version(&staged);
    if staged_version.as_deref() != Some(version.to_string().as_str()) {
        crate::logging::error(format!(
            "staged app version {staged_version:?} does not match release {version}"
        ));
        return Err("The update image doesn't match its release. Try again.".to_owned());
    }
    let verify = Command::new("/usr/bin/codesign")
        .arg("--verify")
        .arg("--deep")
        .arg("--strict")
        .arg(&staged)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            crate::logging::error(format!("failed to run codesign: {error}"));
            "Couldn't verify the update. Try again.".to_owned()
        })?;
    if !verify.status.success() {
        crate::logging::error(format!(
            "staged app signature failed verification: {}",
            String::from_utf8_lossy(&verify.stderr)
        ));
        return Err("The update signature couldn't be verified.".to_owned());
    }

    Ok(PreparedUpdate {
        version,
        staged_app: staged,
    })
}

fn detach(mount: &Path) {
    let _ = Command::new("/usr/bin/hdiutil")
        .arg("detach")
        .arg(mount)
        .arg("-quiet")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn bundle_version(app: &Path) -> Option<String> {
    let plist = app.join("Contents/Info.plist");
    let output = Command::new("/usr/bin/plutil")
        .arg("-extract")
        .arg("CFBundleShortVersionString")
        .arg("raw")
        .arg("-o")
        .arg("-")
        .arg(plist)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The `.app` bundle containing `executable`, if any. Only a real bundle can
/// be replaced safely; a debug binary simply gets the release-page path.
pub fn bundle_from_executable(executable: &Path) -> Option<PathBuf> {
    let contents = executable.parent()?.parent()?;
    let bundle = contents.parent()?;
    (bundle.extension()?.to_str()? == "app"
        && bundle.join("Contents/MacOS").is_dir()
        && bundle.join("Contents/Info.plist").is_file())
    .then(|| bundle.to_path_buf())
}

pub fn running_bundle() -> Option<PathBuf> {
    bundle_from_executable(&std::env::current_exe().ok()?)
}

/// Whether the running copy can replace itself without an administrator.
pub fn can_self_install() -> bool {
    let Some(bundle) = running_bundle() else {
        return false;
    };
    let Some(parent) = bundle.parent() else {
        return false;
    };
    let probe = parent.join(format!(".worktable-update-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Hand the staged app to a detached helper and return. The caller quits; the
/// helper waits for this process to disappear, swaps the bundle atomically,
/// and reopens the app.
pub fn launch_installer(prepared: &PreparedUpdate) -> Result<(), String> {
    let bundle = running_bundle().ok_or_else(|| {
        "Worktable isn't running from an app bundle. Open the release page to install it."
            .to_owned()
    })?;
    if !can_self_install() {
        return Err(
            "Worktable can't replace itself in this location. Open the release page to install it."
                .to_owned(),
        );
    }
    let script = staging_root(&prepared.version).join("install-update.sh");
    let script_body = r#"#!/bin/sh
set -eu
bundle="$1"
staged="$2"
pid="$3"
while kill -0 "$pid" 2>/dev/null; do
  sleep 0.2
done
backup="${bundle}.old.$$"
mv "$bundle" "$backup"
if /usr/bin/ditto "$staged" "$bundle"; then
  rm -rf "$backup"
  /usr/bin/open "$bundle"
else
  rm -rf "$bundle"
  mv "$backup" "$bundle"
  exit 1
fi
"#;
    let mut file = std::fs::File::create(&script).map_err(|error| {
        crate::logging::error(format!("failed to write the update helper: {error}"));
        "Couldn't prepare the update. Try again.".to_owned()
    })?;
    file.write_all(script_body.as_bytes()).map_err(|error| {
        crate::logging::error(format!("failed to write the update helper: {error}"));
        "Couldn't prepare the update. Try again.".to_owned()
    })?;
    drop(file);

    Command::new("/bin/sh")
        .arg(&script)
        .arg(&bundle)
        .arg(&prepared.staged_app)
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            crate::logging::error(format!("failed to launch the update helper: {error}"));
            "Couldn't start the update. Try again.".to_owned()
        })?;
    Ok(())
}

/// Parse the first 64-hex digest in a `shasum -a 256` file. When the file
/// names its file, accept only this DMG's line.
fn parse_checksum(text: &str, dmg_name: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        if digest.len() != 64 || !digest.chars().all(|ch| ch.is_ascii_hexdigit()) {
            continue;
        }
        let named = line.contains(dmg_name);
        if parts.next().is_none() || named {
            return Some(digest.to_ascii_lowercase());
        }
    }
    None
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_owned(),
            browser_download_url: format!("https://example.test/{name}"),
        }
    }

    fn release(tag: &str) -> LatestRelease {
        LatestRelease {
            tag_name: tag.to_owned(),
            name: format!("Worktable {tag}"),
            body: "Notes".to_owned(),
            html_url: "https://github.com/MalinrRuwan/worktable/releases/tag/x".to_owned(),
            draft: false,
            prerelease: false,
            assets: vec![
                asset("Worktable-1.6.0.dmg"),
                asset("Worktable-1.6.0.dmg.sha256"),
            ],
        }
    }

    #[test]
    fn versions_parse_and_order_semantically() {
        let current = Version::parse("1.5.0").unwrap();
        assert_eq!(Version::parse("v1.5.1").unwrap().to_string(), "1.5.1");
        assert!(Version::parse("1.5.1-beta").is_none());
        assert!(Version::parse("1.5").is_none());
        assert!(Version::parse("1.5.1.2").is_none());
        assert!(Version::parse("").is_none());
        let next = Version {
            major: 1,
            minor: 6,
            patch: 0,
        };
        assert!(next > current);
        assert!(current < next);
    }

    #[test]
    fn stable_release_selects_the_dmg_and_checksum() {
        let current = Version::parse("1.5.0").unwrap();
        let check = release_check(release("v1.6.0"), current).unwrap();
        let UpdateCheck::Available(info) = check else {
            panic!("a newer release must be offered");
        };
        assert_eq!(info.version.to_string(), "1.6.0");
        assert_eq!(info.dmg.name, "Worktable-1.6.0.dmg");
        assert_eq!(info.checksum.name, "Worktable-1.6.0.dmg.sha256");
    }

    #[test]
    fn same_or_newer_current_version_is_up_to_date() {
        let current = Version::parse("1.6.0").unwrap();
        assert_eq!(
            release_check(release("v1.6.0"), current).unwrap(),
            UpdateCheck::UpToDate { current }
        );
        assert_eq!(
            release_check(release("v1.5.9"), current).unwrap(),
            UpdateCheck::UpToDate { current }
        );
    }

    #[test]
    fn prerelease_and_missing_assets_are_not_offered() {
        let current = Version::parse("1.5.0").unwrap();
        let mut prerelease = release("v1.6.0");
        prerelease.prerelease = true;
        assert!(release_check(prerelease, current).is_err());

        let mut no_checksum = release("v1.6.0");
        no_checksum
            .assets
            .retain(|asset| !asset.name.ends_with(".sha256"));
        assert!(release_check(no_checksum, current).is_err());

        let mut no_dmg = release("v1.6.0");
        no_dmg.assets.clear();
        assert!(release_check(no_dmg, current).is_err());
    }

    #[test]
    fn checksum_parser_accepts_shasum_output_and_rejects_malformed_lines() {
        let digest = sha256_hex(b"abc");
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let text = format!("{digest}  Worktable-1.6.0.dmg\n");
        assert_eq!(
            parse_checksum(&text, "Worktable-1.6.0.dmg"),
            Some(digest.clone())
        );
        assert_eq!(parse_checksum("not-a-digest  file\n", "file"), None);
        let other = format!("{}  Other.dmg\n", "0".repeat(64));
        assert_eq!(parse_checksum(&other, "Worktable-1.6.0.dmg"), None);
    }

    #[test]
    fn only_real_app_bundles_are_considered_installable() {
        let root = std::env::temp_dir().join(format!("worktable-bundle-{}", std::process::id()));
        let bundle = root.join("Worktable.app");
        std::fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
        std::fs::write(bundle.join("Contents/Info.plist"), "plist").unwrap();
        let executable = bundle.join("Contents/MacOS/Worktable");
        assert_eq!(bundle_from_executable(&executable), Some(bundle.clone()));
        assert!(
            bundle_from_executable(&root.join("target/debug/worktable-app")).is_none(),
            "a debug binary is not an app bundle"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
