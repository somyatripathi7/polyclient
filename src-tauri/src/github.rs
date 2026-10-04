//! GitHub release helpers for installing plugins from prebuilt assets.
//!
//! Pure, network-isolated logic (asset selection, checksum parsing, target
//! triple) is unit-tested without touching the network; the two `async`
//! functions are thin `reqwest` wrappers.
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// A plugin release channel. `Nightly` is the rolling `plugins-latest`
/// prerelease (version not in the tag); `Stable` is an immutable
/// `v<semver>` release (version is in the tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Nightly,
    Stable,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Nightly => "nightly",
            Channel::Stable => "stable",
        }
    }

    /// Parse a channel string; anything that isn't `stable` is treated as
    /// `nightly` (the default for local/dev builds).
    pub fn parse(s: &str) -> Channel {
        if s.eq_ignore_ascii_case("stable") {
            Channel::Stable
        } else {
            Channel::Nightly
        }
    }
}

/// The Rust target triple this host was built for, used to pick the matching
/// release asset. `None` on platforms we don't have a mapping for.
pub fn target_triple() -> Option<&'static str> {
    Some(match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("x86_64", "windows") => "x86_64-pc-windows-msvc",
        ("aarch64", "windows") => "aarch64-pc-windows-msvc",
        _ => return None,
    })
}

/// A GitHub release (only the fields we use).
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    /// ISO-8601 UTC timestamp (e.g. `2026-06-01T12:00:00Z`). `None` for drafts.
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// A downloadable file attached to a release.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
    /// ISO-8601 UTC timestamp; GitHub refreshes this when the asset binary is
    /// re-uploaded. The rolling `plugins-latest` release keeps a fixed release
    /// `published_at`, so this is what drives nightly update detection.
    #[serde(default)]
    pub updated_at: Option<String>,
}

fn is_aux(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".sha256") || n.ends_with(".json") || n.ends_with(".txt") || n.ends_with(".sig")
}

/// Pick the single binary asset whose name contains the target `triple` and
/// optionally matches a specific `plugin_id`. Checksum/manifest/signature files
/// are ignored. Ambiguous or missing matches produce a descriptive error listing
/// the available asset names.
pub fn select_binary_asset<'a>(
    release: &'a Release,
    triple: &str,
    plugin_id: Option<&str>,
) -> Result<&'a Asset, String> {
    let matches: Vec<&Asset> = release
        .assets
        .iter()
        .filter(|a| {
            if is_aux(&a.name) || !a.name.contains(triple) {
                return false;
            }
            if let Some(id) = plugin_id {
                extract_plugin_id(&a.name).as_deref() == Some(id)
            } else {
                true
            }
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => {
            let filter_desc = match plugin_id {
                Some(id) => format!("target '{triple}' and plugin_id '{id}'"),
                None => format!("target '{triple}'"),
            };
            Err(format!(
                "no asset matching {filter_desc} in release {}. assets: [{}]",
                release.tag_name,
                asset_names(release)
            ))
        }
        many => {
            let filter_desc = match plugin_id {
                Some(id) => format!("target '{triple}' and plugin_id '{id}'"),
                None => format!("target '{triple}'"),
            };
            Err(format!(
                "{} assets match {filter_desc}; expected exactly one: [{}]",
                many.len(),
                many.iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        }
    }
}

/// Tag for the rolling plugins prerelease, as published by `publish-plugins.yml`.
pub const PLUGINS_LATEST_TAG: &str = "plugins-latest";

/// A plugin release selected for a given platform + channel, with the metadata
/// the installer needs to decide install/update status without downloading.
#[derive(Debug, Clone)]
pub struct PluginRelease<'a> {
    /// Plugin id, extracted from the binary asset name.
    pub id: String,
    pub tag: &'a str,
    pub channel: Channel,
    /// Stable version (from the `v<semver>` tag); `None` for nightly, whose
    /// version is only known after `--describe`.
    pub version: Option<String>,
    pub published_at: Option<&'a str>,
    /// The selected binary asset's `updated_at` (refreshes on re-upload).
    /// Drives nightly update detection, where the release `published_at` is
    /// frozen because `plugins-latest` is updated in place.
    pub asset_updated_at: Option<&'a str>,
    pub asset: &'a Asset,
}

/// Determine if a release tag is a plugins release tag, and extract the version.
/// `plugins-latest` -> nightly (no version); `plugins-v<maj.min.patch>` -> stable.
/// Returns `None` for the app's own release tags (anything not starting with `plugins-`).
pub fn is_plugins_release(tag: &str) -> Option<(Channel, Option<String>)> {
    if tag == PLUGINS_LATEST_TAG {
        return Some((Channel::Nightly, None));
    }
    if let Some(version) = tag.strip_prefix("plugins-v") {
        if parse_semver(version).is_some() {
            return Some((Channel::Stable, Some(version.to_string())));
        }
    }
    None
}

/// Extract the plugin id from a binary asset name.
/// Expects format like `polyclient-plugin-postgres-aarch64-apple-darwin`.
fn extract_plugin_id(asset_name: &str) -> Option<String> {
    // Asset name format: polyclient-plugin-<id>-<target-triple>
    if let Some(rest) = asset_name.strip_prefix("polyclient-plugin-") {
        // Remove .exe suffix on Windows
        let rest = rest.strip_suffix(".exe").unwrap_or(rest);
        // Find the last occurrence of a known target triple to split id from target
        for target in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
        ] {
            if let Some(idx) = rest.rfind(target) {
                let id = &rest[..idx];
                // Strip the trailing hyphen separating id from the target triple.
                if let Some(id) = id.strip_suffix('-') {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

/// Parse a strict `major.minor.patch` version into a comparable tuple.
pub fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.split('.');
    let maj = parts.next()?.parse().ok()?;
    let min = parts.next()?.parse().ok()?;
    let pat = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((maj, min, pat))
}

/// From all `releases`, pick the plugins release for `channel` that has binary
/// assets for `triple`. For `Stable`, only the highest-semver release is kept.
/// For `Nightly`, the `plugins-latest` release is returned. Extracts individual
/// plugin ids from asset names. Pure — no network.
pub fn select_plugin_releases<'a>(
    releases: &'a [Release],
    triple: &str,
    channel: Channel,
) -> Vec<PluginRelease<'a>> {
    let mut result: Vec<PluginRelease<'a>> = Vec::new();
    let mut best_stable_version: Option<(u64, u64, u64)> = None;
    let mut best_stable_plugins: Vec<PluginRelease<'a>> = Vec::new();

    for r in releases {
        let Some((ch, version)) = is_plugins_release(&r.tag_name) else {
            continue;
        };
        if ch != channel {
            continue;
        }

        // For stable, keep only the highest semver; for nightly, use the latest.
        match channel {
            Channel::Stable => {
                let new_version = version.as_deref().and_then(parse_semver);
                let should_keep = match best_stable_version {
                    Some(existing) => new_version > Some(existing),
                    None => new_version.is_some(),
                };
                if !should_keep {
                    continue;
                }
                // This is a better stable release; reset and collect its plugins.
                best_stable_version = new_version;
                best_stable_plugins.clear();
            }
            Channel::Nightly => {} // keep all nightly matches
        }

        // Extract each plugin from the assets matching this triple.
        for asset in &r.assets {
            if is_aux(&asset.name) || !asset.name.contains(triple) {
                continue;
            }
            if let Some(id) = extract_plugin_id(&asset.name) {
                let pr = PluginRelease {
                    id,
                    tag: &r.tag_name,
                    channel: ch,
                    version: version.clone(),
                    published_at: r.published_at.as_deref(),
                    asset_updated_at: asset.updated_at.as_deref(),
                    asset,
                };
                match channel {
                    Channel::Nightly => result.push(pr),
                    Channel::Stable => best_stable_plugins.push(pr),
                }
            }
        }
    }

    result.extend(best_stable_plugins);
    result
}

fn asset_names(release: &Release) -> String {
    release
        .assets
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Find the checksum asset that covers `binary_name`: either a per-asset
/// `<binary_name>.sha256`, or an aggregate `SHA256SUMS` / `checksums.txt`.
pub fn find_checksum<'a>(release: &'a Release, binary_name: &str) -> Option<&'a Asset> {
    let per_asset = format!("{binary_name}.sha256").to_ascii_lowercase();
    release.assets.iter().find(|a| {
        let n = a.name.to_ascii_lowercase();
        n == per_asset || n == "sha256sums" || n == "sha256sums.txt" || n == "checksums.txt"
    })
}

/// Asset name of the aggregate plugin-metadata file published by
/// `publish-plugins.yml` (a map of plugin id -> [`PluginMeta`]).
pub const PLUGIN_INFO_ASSET: &str = "plugin_info.json";

/// Human-facing metadata for one plugin, as published in `plugin_info.json`.
/// Generated from each binary's `--describe` so the installer can show a name,
/// description, and version BEFORE downloading or executing anything. All fields
/// are optional so a partial/older file still parses.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PluginMeta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// Find the published `plugin_info.json` asset in `release`, if present
/// (case-insensitive match on the asset name).
pub fn find_plugin_info(release: &Release) -> Option<&Asset> {
    release
        .assets
        .iter()
        .find(|a| a.name.eq_ignore_ascii_case(PLUGIN_INFO_ASSET))
}

/// Parse the aggregate `plugin_info.json` (a JSON object mapping plugin id ->
/// metadata). Best-effort: returns an empty map on any parse error so listing
/// still works when the file is missing or malformed.
pub fn parse_plugin_info(text: &str) -> std::collections::HashMap<String, PluginMeta> {
    serde_json::from_str(text).unwrap_or_default()
}

/// Extract the hex SHA-256 for `binary_name` from checksum-file `text`.
///
/// Handles both the aggregate `<hash>  <name>` format (coreutils `sha256sum`,
/// where `name` may carry a `*` binary-mode marker) and a bare-hash file
/// (a lone `<hash>`, as produced by `shasum -a256 file | cut`).
pub fn parse_sha256sums(text: &str, binary_name: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    // Bare single-hash file.
    if let [only] = lines.as_slice() {
        let tok = only.split_whitespace().next().unwrap_or("");
        if is_hex_sha256(tok) {
            return Some(tok.to_ascii_lowercase());
        }
    }
    for line in lines {
        let mut parts = line.split_whitespace();
        let hash = parts.next().unwrap_or("");
        let name = parts.next().unwrap_or("").trim_start_matches('*');
        if is_hex_sha256(hash) && name == binary_name {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

fn is_hex_sha256(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// A `reqwest` client configured for the GitHub API (required User-Agent,
/// optional `GITHUB_TOKEN` auth to lift the unauthenticated rate limit).
fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent("polyclient-plugin-installer")
        .build()
        .map_err(|e| e.to_string())
}

fn auth(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    match std::env::var("GITHUB_TOKEN") {
        Ok(t) if !t.is_empty() => req.bearer_auth(t),
        _ => req,
    }
}

/// Normalise and validate a `owner/name` repo string (tolerating a pasted
/// `https://github.com/owner/name` URL and surrounding slashes).
fn normalize_repo(repo: &str) -> Result<String, String> {
    let repo = repo
        .trim()
        .trim_start_matches("https://github.com/")
        .trim_matches('/');
    if repo.split('/').filter(|s| !s.is_empty()).count() != 2 {
        return Err(format!("expected repo as 'owner/name', got '{repo}'"));
    }
    Ok(repo.to_string())
}

/// Fetch a release by `tag`, or the latest release when `tag` is `None`.
/// `repo` is `owner/name`.
pub async fn fetch_release(repo: &str, tag: Option<&str>) -> Result<Release, String> {
    let repo = normalize_repo(repo)?;
    let url = match tag {
        Some(t) => format!("https://api.github.com/repos/{repo}/releases/tags/{t}"),
        None => format!("https://api.github.com/repos/{repo}/releases/latest"),
    };
    let resp = auth(client()?.get(&url))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GitHub returned {} for {url}", resp.status()));
    }
    resp.json::<Release>()
        .await
        .map_err(|e| format!("failed to parse release: {e}"))
}

/// Fetch all releases for `repo` (`owner/name`), most-recent first.
pub async fn fetch_releases(repo: &str) -> Result<Vec<Release>, String> {
    let repo = normalize_repo(repo)?;
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=100");
    let resp = auth(client()?.get(&url))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GitHub returned {} for {url}", resp.status()));
    }
    resp.json::<Vec<Release>>()
        .await
        .map_err(|e| format!("failed to parse releases: {e}"))
}

/// Download an asset's bytes.
pub async fn download_bytes(url: &str) -> Result<Vec<u8>, String> {
    let resp = auth(client()?.get(url))
        .send()
        .await
        .map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download returned {} for {url}", resp.status()));
    }
    Ok(resp
        .bytes()
        .await
        .map_err(|e| format!("download read failed: {e}"))?
        .to_vec())
}

/// Download an asset's body as UTF-8 text (for checksum files).
pub async fn download_text(url: &str) -> Result<String, String> {
    let bytes = download_bytes(url).await?;
    String::from_utf8(bytes).map_err(|e| format!("checksum file not UTF-8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release_with(names: &[&str]) -> Release {
        Release {
            tag_name: "v1.0.0".into(),
            published_at: Some("2026-01-01T00:00:00Z".into()),
            assets: names
                .iter()
                .map(|n| Asset {
                    name: (*n).into(),
                    browser_download_url: format!("https://example/{n}"),
                    size: 10,
                    ..Default::default()
                })
                .collect(),
        }
    }

    #[test]
    fn target_triple_is_known_on_this_host() {
        assert!(target_triple().is_some());
    }

    #[test]
    fn selects_unique_binary_ignoring_aux_files() {
        let r = release_with(&[
            "polyclient-plugin-mysql-aarch64-apple-darwin",
            "polyclient-plugin-mysql-aarch64-apple-darwin.sha256",
            "polyclient-plugin-mysql-x86_64-apple-darwin",
            "SHA256SUMS",
        ]);
        let a = select_binary_asset(&r, "aarch64-apple-darwin", None).unwrap();
        assert_eq!(a.name, "polyclient-plugin-mysql-aarch64-apple-darwin");
    }

    #[test]
    fn errors_when_no_asset_matches() {
        let r = release_with(&["polyclient-plugin-mysql-x86_64-apple-darwin"]);
        let e = select_binary_asset(&r, "aarch64-unknown-linux-gnu", None).unwrap_err();
        assert!(e.contains("no asset matching"), "{e}");
    }

    #[test]
    fn errors_when_ambiguous() {
        let r = release_with(&["plugin-linux-extra", "other-plugin-linux"]);
        let e = select_binary_asset(&r, "linux", None).unwrap_err();
        assert!(e.contains("expected exactly one"), "{e}");
    }

    #[test]
    fn filters_by_plugin_id_to_disambiguate() {
        let r = release_with(&[
            "polyclient-plugin-postgres-aarch64-apple-darwin",
            "polyclient-plugin-mongodb-aarch64-apple-darwin",
            "polyclient-plugin-rabbitmq-aarch64-apple-darwin",
        ]);
        // Without plugin_id: ambiguous
        assert!(select_binary_asset(&r, "aarch64-apple-darwin", None).is_err());
        // With plugin_id: selects the right one
        let a = select_binary_asset(&r, "aarch64-apple-darwin", Some("postgres")).unwrap();
        assert_eq!(a.name, "polyclient-plugin-postgres-aarch64-apple-darwin");
    }

    #[test]
    fn finds_aggregate_and_per_asset_checksums() {
        let bin = "polyclient-plugin-mysql-aarch64-apple-darwin";
        let agg = release_with(&[bin, "SHA256SUMS"]);
        assert_eq!(find_checksum(&agg, bin).unwrap().name, "SHA256SUMS");
        let per = release_with(&[bin, &format!("{bin}.sha256")]);
        assert_eq!(
            find_checksum(&per, bin).unwrap().name,
            format!("{bin}.sha256")
        );
    }

    #[test]
    fn parses_aggregate_sha256sums() {
        let hash = "a".repeat(64);
        let text = format!(
            "{hash}  polyclient-plugin-mysql-aarch64-apple-darwin\n{}  other\n",
            "b".repeat(64)
        );
        let got = parse_sha256sums(&text, "polyclient-plugin-mysql-aarch64-apple-darwin").unwrap();
        assert_eq!(got, hash);
    }

    #[test]
    fn parses_binary_mode_marker_and_bare_hash() {
        let hash = "c".repeat(64);
        let text = format!("{hash} *the-binary\n");
        assert_eq!(parse_sha256sums(&text, "the-binary").unwrap(), hash);
        let bare = format!("{}\n", "d".repeat(64));
        assert_eq!(parse_sha256sums(&bare, "anything").unwrap(), "d".repeat(64));
    }

    #[test]
    fn parses_is_plugins_release() {
        let (ch, ver) = is_plugins_release("plugins-latest").unwrap();
        assert_eq!((ch, ver), (Channel::Nightly, None));

        let (ch, ver) = is_plugins_release("plugins-v0.2.0").unwrap();
        assert_eq!(ch, Channel::Stable);
        assert_eq!(ver.as_deref(), Some("0.2.0"));

        // The app's own tags are not plugin releases.
        assert!(is_plugins_release("latest").is_none());
        assert!(is_plugins_release("v0.2.0").is_none());
        // Non-semver after plugins-v is rejected.
        assert!(is_plugins_release("plugins-vnightly").is_none());
    }

    #[test]
    fn extracts_plugin_id_from_asset_name() {
        // Standard format with target triple
        let id = extract_plugin_id("polyclient-plugin-postgres-aarch64-apple-darwin");
        assert_eq!(id, Some("postgres".to_string()));

        // With .exe suffix on Windows
        let id = extract_plugin_id("polyclient-plugin-mongodb-x86_64-pc-windows-msvc.exe");
        assert_eq!(id, Some("mongodb".to_string()));

        // Hyphenated plugin id
        let id = extract_plugin_id("polyclient-plugin-my-plugin-aarch64-apple-darwin");
        assert_eq!(id, Some("my-plugin".to_string()));

        // Invalid formats
        assert_eq!(extract_plugin_id("SHA256SUMS"), None);
        assert_eq!(extract_plugin_id("random-file"), None);
    }

    #[test]
    fn selects_nightly_plugin_releases_with_matching_assets() {
        let triple = "aarch64-apple-darwin";
        let releases = vec![
            // App release: not a plugins release tag -> skipped.
            Release {
                tag_name: "latest".into(),
                published_at: Some("2026-01-01T00:00:00Z".into()),
                assets: vec![Asset {
                    name: format!("polyclient-{triple}.dmg"),
                    browser_download_url: "https://example/app".into(),
                    size: 1,
                    ..Default::default()
                }],
            },
            // Plugins release with all three plugins -> each returned separately.
            Release {
                tag_name: "plugins-latest".into(),
                published_at: Some("2026-02-01T00:00:00Z".into()),
                assets: vec![
                    Asset {
                        name: format!("polyclient-plugin-postgres-{triple}"),
                        browser_download_url: "https://example/pg".into(),
                        size: 2,
                        updated_at: Some("2026-03-01T00:00:00Z".into()),
                    },
                    Asset {
                        name: format!("polyclient-plugin-mongodb-{triple}"),
                        browser_download_url: "https://example/mongo".into(),
                        size: 3,
                        ..Default::default()
                    },
                    Asset {
                        name: format!("polyclient-plugin-rabbitmq-{triple}"),
                        browser_download_url: "https://example/rabbit".into(),
                        size: 4,
                        ..Default::default()
                    },
                    Asset {
                        name: "SHA256SUMS".into(),
                        browser_download_url: "https://example/SHA256SUMS".into(),
                        size: 1,
                        ..Default::default()
                    },
                ],
            },
            // No asset for this triple -> skipped.
            Release {
                tag_name: "plugins-latest".into(),
                published_at: Some("2026-02-01T00:00:00Z".into()),
                assets: vec![Asset {
                    name: "polyclient-plugin-postgres-x86_64-pc-windows-msvc.exe".into(),
                    browser_download_url: "https://example/pg-win".into(),
                    size: 3,
                    ..Default::default()
                }],
            },
        ];
        let got = select_plugin_releases(&releases, triple, Channel::Nightly);
        assert_eq!(got.len(), 3);
        let ids: Vec<_> = got.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"postgres"));
        assert!(ids.contains(&"mongodb"));
        assert!(ids.contains(&"rabbitmq"));
        assert!(got.iter().all(|p| p.tag == "plugins-latest"));
        // The asset's updated_at is threaded through for nightly staleness.
        let pg = got.iter().find(|p| p.id == "postgres").unwrap();
        assert_eq!(pg.asset_updated_at, Some("2026-03-01T00:00:00Z"));
    }

    #[test]
    fn selects_highest_stable_version() {
        let triple = "aarch64-apple-darwin";
        let mk = |tag: &str| Release {
            tag_name: tag.into(),
            published_at: Some("2026-01-01T00:00:00Z".into()),
            assets: vec![
                Asset {
                    name: format!("polyclient-plugin-postgres-{triple}"),
                    browser_download_url: "https://example/pg".into(),
                    size: 2,
                    ..Default::default()
                },
                Asset {
                    name: format!("polyclient-plugin-mongodb-{triple}"),
                    browser_download_url: "https://example/mongo".into(),
                    size: 3,
                    ..Default::default()
                },
            ],
        };
        let releases = vec![
            mk("plugins-v0.1.0"),
            mk("plugins-v0.2.0"),
            mk("plugins-v0.1.9"),
        ];
        let got = select_plugin_releases(&releases, triple, Channel::Stable);
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|p| p.version.as_deref() == Some("0.2.0")));
        assert!(got.iter().all(|p| p.tag == "plugins-v0.2.0"));
        // Nightly selection ignores stable releases.
        assert!(select_plugin_releases(&releases, triple, Channel::Nightly).is_empty());
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // SHA-256("abc")
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn finds_plugin_info_asset_case_insensitively() {
        let r = release_with(&[
            "polyclient-plugin-postgres-aarch64-apple-darwin",
            "Plugin_Info.json",
        ]);
        assert_eq!(find_plugin_info(&r).unwrap().name, "Plugin_Info.json");
        let none = release_with(&["polyclient-plugin-postgres-aarch64-apple-darwin"]);
        assert!(find_plugin_info(&none).is_none());
    }

    #[test]
    fn parses_plugin_info_map() {
        let text = r#"{
            "postgres": {"name": "PostgreSQL", "description": "Connect to Postgres", "version": "0.2.41"},
            "mongodb":  {"name": "MongoDB"}
        }"#;
        let map = parse_plugin_info(text);
        assert_eq!(map["postgres"].name.as_deref(), Some("PostgreSQL"));
        assert_eq!(
            map["postgres"].description.as_deref(),
            Some("Connect to Postgres")
        );
        assert_eq!(map["postgres"].version.as_deref(), Some("0.2.41"));
        // Missing fields are None, not an error.
        assert_eq!(map["mongodb"].name.as_deref(), Some("MongoDB"));
        assert_eq!(map["mongodb"].description, None);
    }

    #[test]
    fn parse_plugin_info_is_empty_on_garbage() {
        assert!(parse_plugin_info("not json").is_empty());
        assert!(parse_plugin_info("[1, 2, 3]").is_empty());
        assert!(parse_plugin_info("").is_empty());
    }
}
