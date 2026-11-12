use base64::Engine;
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha512};
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use url::Url;

const REGISTRY: &str = "https://npm.intra.xiaojukeji.com";
const TAG: &str = "latest";
const MAX_METADATA: u64 = 8 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 32 * 1024 * 1024;
const MAX_BINARY: u64 = 64 * 1024 * 1024;
const MAX_SMALL_FILE: u64 = 512 * 1024;

pub(super) fn enabled() -> bool {
    option_env!("OSS_BUILD") == Some("0")
}

pub(super) fn install_home() -> Result<PathBuf, String> {
    let root =
        crate::config::git_ai_dir_path().ok_or("Cannot determine the installation directory")?;
    if !root.join(Platform::current()?.filenames().0).is_file() {
        return Err("No git-ai installation found in the user bin directory".into());
    }
    Ok(root)
}

#[cfg(not(windows))]
pub(super) fn installed_executable() -> Result<PathBuf, String> {
    Ok(install_home()?.join(Platform::current()?.filenames().0))
}

struct Platform {
    name: &'static str,
    os: &'static str,
    cpu: &'static str,
}

impl Platform {
    fn current() -> Result<Self, String> {
        Self::for_target(std::env::consts::OS, std::env::consts::ARCH)
    }

    #[cfg(any(windows, test))]
    fn for_wsl(arch: &str) -> Result<Self, String> {
        let arch = match arch.trim() {
            "arm64" => "aarch64",
            arch => arch,
        };
        Self::for_target("linux", arch)
            .map_err(|_| format!("No DCC npm package for WSL architecture {arch}"))
    }

    fn for_target(os: &str, arch: &str) -> Result<Self, String> {
        let (name, os, cpu) = match (os, arch) {
            ("macos", "aarch64") => ("@dcc/git-ai-darwin-arm64", "darwin", "arm64"),
            ("macos", "x86_64") => ("@dcc/git-ai-darwin-x64", "darwin", "x64"),
            ("linux", "aarch64") => ("@dcc/git-ai-linux-arm64-musl", "linux", "arm64"),
            ("linux", "x86_64") => ("@dcc/git-ai-linux-x64-musl", "linux", "x64"),
            ("windows", "x86_64") => ("@dcc/git-ai-win32-x64", "win32", "x64"),
            _ => return Err("No DCC npm update package for this platform".into()),
        };
        Ok(Self { name, os, cpu })
    }

    fn filenames(&self) -> (&'static str, &'static str) {
        if self.os == "win32" {
            ("bin/git-ai.exe", "install.ps1")
        } else {
            ("bin/git-ai", "install.sh")
        }
    }

    fn verify(&self, manifest: &Manifest, version: &str) -> Result<(), String> {
        if manifest.name != self.name
            || manifest.version != version
            || manifest.os != [self.os]
            || manifest.cpu != [self.cpu]
            || (self.os == "linux" && manifest.libc.as_deref().is_none_or(|libc| libc != ["musl"]))
        {
            return Err("npm update package identity or platform mismatch".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    name: String,
    version: String,
    os: Vec<String>,
    cpu: Vec<String>,
    libc: Option<Vec<String>>,
    dcc: DccMetadata,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DccMetadata {
    binary_sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Dist {
    tarball: String,
    integrity: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Release {
    #[serde(flatten)]
    manifest: Manifest,
    dist: Dist,
}

impl Release {
    pub(super) fn version(&self) -> &str {
        &self.manifest.version
    }
}

fn trusted_tarball(url: &str) -> Result<Url, String> {
    let mut url = Url::parse(url).map_err(|error| format!("Invalid npm tarball URL: {error}"))?;
    if !matches!(
        url.host_str(),
        Some("npm.intra.xiaojukeji.com" | "artifactory.intra.xiaojukeji.com")
    ) || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err("Untrusted npm tarball URL".into());
    }
    // Artifactory advertises HTTP for this endpoint even when it is served over TLS.
    if url.scheme() == "http" {
        url.set_scheme("https")
            .map_err(|_| "Invalid npm URL scheme")?;
    }
    if url.scheme() != "https" {
        return Err("npm update downloads require HTTPS".into());
    }
    Ok(url)
}

fn get(url: &str, timeout: u64, limit: u64) -> Result<Vec<u8>, String> {
    // Package downloads deliberately do not use the Git AI API send/auth path.
    // Callers supply only the fixed registry origin or a validated tarball URL.
    let mut response = crate::http::build_agent_with_redirects(Some(timeout), Some(0))
        .get(url)
        .call()
        .map_err(|error| format!("npm update request failed: {error}"))?;
    if response.status() != 200 {
        return Err(format!(
            "npm update request returned HTTP {}",
            response.status()
        ));
    }
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|error| format!("Cannot read npm update response: {error}"))
}

fn release_from_json(platform: &Platform, data: &[u8]) -> Result<Release, String> {
    let mut data: serde_json::Value =
        serde_json::from_slice(data).map_err(|error| format!("Invalid npm metadata: {error}"))?;
    let version = data["dist-tags"][TAG]
        .as_str()
        .ok_or("npm update package has no latest tag")?
        .to_string();
    let record = data
        .get_mut("versions")
        .and_then(|versions| versions.get_mut(&version))
        .map(serde_json::Value::take)
        .ok_or("npm latest tag has no matching version metadata")?;
    semver::Version::parse(&version).map_err(|_| "Invalid npm update version")?;
    let release: Release =
        serde_json::from_value(record).map_err(|error| format!("Invalid npm release: {error}"))?;
    platform.verify(&release.manifest, &version)?;
    Ok(release)
}

pub(super) fn fetch() -> Result<Release, String> {
    fetch_from_registry(REGISTRY, &Platform::current()?)
}

fn fetch_from_registry(registry: &str, platform: &Platform) -> Result<Release, String> {
    let url = format!("{registry}/{}", platform.name);
    release_from_json(platform, &get(&url, 10, MAX_METADATA)?)
}

#[cfg(any(windows, test))]
fn fetch_version_from_registry(
    registry: &str,
    platform: &Platform,
    version: &str,
) -> Result<Release, String> {
    semver::Version::parse(version).map_err(|_| "Invalid npm package version")?;
    let url = format!("{registry}/{}/{version}", platform.name);
    let release: Release = serde_json::from_slice(&get(&url, 10, MAX_METADATA)?)
        .map_err(|error| format!("Invalid npm release: {error}"))?;
    platform.verify(&release.manifest, version)?;
    Ok(release)
}

#[cfg(any(windows, test))]
pub(crate) fn fetch_wsl_package(arch: &str, version: &str) -> Result<(Vec<u8>, String), String> {
    let platform = Platform::for_wsl(arch)?;
    let release = fetch_version_from_registry(REGISTRY, &platform, version)?;
    let archive = download_archive(&release)?;
    verify_archive(&platform, &release, &archive)
}

pub(super) struct Staged {
    pub directory: tempfile::TempDir,
    filename: &'static str,
    pub script: String,
}

impl Staged {
    pub(super) fn binary(&self) -> PathBuf {
        self.directory.path().join(self.filename)
    }
}

pub(super) fn stage(release: &Release) -> Result<Staged, String> {
    let root = install_home()?;
    let data = download_archive(release)?;
    let directory = root.join("tmp");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Cannot create staging parent: {error}"))?;
    stage_archive(&Platform::current()?, release, &data, &directory)
}

fn download_archive(release: &Release) -> Result<Vec<u8>, String> {
    let url = trusted_tarball(&release.dist.tarball)?;
    get(url.as_str(), 60, MAX_ARCHIVE)
}

fn stage_archive(
    platform: &Platform,
    release: &Release,
    data: &[u8],
    parent: &std::path::Path,
) -> Result<Staged, String> {
    let (binary, script) = verify_archive(platform, release, data)?;
    let directory = tempfile::Builder::new()
        .prefix("npm-upgrade-")
        .tempdir_in(parent)
        .map_err(|error| format!("Cannot create upgrade staging directory: {error}"))?;
    let staged = Staged {
        directory,
        filename: platform.filenames().0.rsplit('/').next().unwrap(),
        script,
    };
    fs::write(staged.binary(), binary)
        .map_err(|error| format!("Cannot stage upgrade binary: {error}"))?;
    Ok(staged)
}

fn verify_archive(
    platform: &Platform,
    release: &Release,
    data: &[u8],
) -> Result<(Vec<u8>, String), String> {
    let integrity = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(data));
    if !release
        .dist
        .integrity
        .split_whitespace()
        .any(|token| token.strip_prefix("sha512-") == Some(integrity.as_str()))
    {
        return Err("npm update archive integrity mismatch".into());
    }

    let (binary_name, installer) = platform.filenames();
    let expected = [
        "package/package.json".to_string(),
        format!("package/{binary_name}"),
        format!("package/{installer}"),
        "package/LICENSE".to_string(),
        "package/THIRD-PARTY-NOTICES".to_string(),
    ];
    let mut files = std::collections::HashMap::new();
    let mut archive = tar::Archive::new(GzDecoder::new(data));
    for entry in archive
        .entries()
        .map_err(|error| error.to_string())?
        .raw(true)
    {
        let mut entry = entry.map_err(|error| error.to_string())?;
        let name = entry.path_bytes();
        let name = std::str::from_utf8(&name)
            .map_err(|_| "Invalid npm archive path")?
            .to_string();
        let limit = if name == expected[1] {
            MAX_BINARY
        } else {
            MAX_SMALL_FILE
        };
        if !entry.header().entry_type().is_file()
            || !expected.contains(&name)
            || files.contains_key(&name)
            || entry.size() == 0
            || entry.size() > limit
        {
            return Err(format!("Unexpected npm update archive entry: {name}"));
        }
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        files.insert(name, bytes);
    }
    if files.len() != expected.len() {
        return Err("npm update archive is missing required files".into());
    }

    let manifest: Manifest = serde_json::from_slice(&files[&expected[0]])
        .map_err(|error| format!("Invalid npm archive manifest: {error}"))?;
    platform.verify(&manifest, release.version())?;
    let script = String::from_utf8(files.remove(&expected[2]).unwrap())
        .map_err(|_| "Invalid npm installer encoding")?;
    let binary = files.remove(&expected[1]).unwrap();
    super::verify_sha256(&binary, &manifest.dcc.binary_sha256)?;
    super::verify_sha256(&binary, &release.manifest.dcc.binary_sha256)?;
    Ok((binary, script))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::GzEncoder};
    use serial_test::serial;
    use sha2::Sha256;

    fn platform() -> Platform {
        Platform {
            name: "@dcc/git-ai-darwin-arm64",
            os: "darwin",
            cpu: "arm64",
        }
    }

    fn package_metadata() -> serde_json::Value {
        serde_json::json!({
            "name": "@dcc/git-ai-darwin-arm64",
            "version": "1.7.7-dcc.10",
            "os": ["darwin"],
            "cpu": ["arm64"],
            "dcc": {"binarySha256": format!("{:x}", Sha256::digest(b"native-binary"))},
            "dist": {
                "tarball": "http://artifactory.intra.xiaojukeji.com/npm/fixture.tgz",
                "integrity": "unset"
            }
        })
    }

    #[test]
    fn chooses_latest_when_the_dcc_tag_points_to_another_release() {
        let metadata = package_metadata();
        let mut previous = metadata.clone();
        previous["version"] = "1.7.7-dcc.9".into();
        let mut document = serde_json::json!({
            "dist-tags": {"latest": "1.7.7-dcc.10", "dcc": "1.7.7-dcc.9"},
            "versions": {"1.7.7-dcc.10": metadata, "1.7.7-dcc.9": previous}
        });
        let release =
            release_from_json(&platform(), &serde_json::to_vec(&document).unwrap()).unwrap();
        assert_eq!(release.version(), "1.7.7-dcc.10");
        document["versions"]["1.7.7-dcc.10"]["version"] = "1.7.7-dcc.9".into();
        assert!(release_from_json(&platform(), &serde_json::to_vec(&document).unwrap()).is_err());
    }

    #[test]
    fn revalidates_latest_after_a_release_is_withdrawn() {
        let mut registry = mockito::Server::new();
        let platform = platform();
        let withdrawn = package_metadata();
        let mut current = withdrawn.clone();
        current["version"] = "1.7.7-dcc.9".into();
        let metadata = serde_json::json!({
            "dist-tags": {"latest": "1.7.7-dcc.9"},
            "versions": {"1.7.7-dcc.9": current, "1.7.7-dcc.10": withdrawn},
        });
        let packument = registry
            .mock("GET", format!("/{}", platform.name).as_str())
            .with_body(metadata.to_string())
            .create();
        let exact_version = registry
            .mock("GET", format!("/{}/1.7.7-dcc.10", platform.name).as_str())
            .with_body(withdrawn.to_string())
            .expect(0)
            .create();

        let release = fetch_from_registry(&registry.url(), &platform).unwrap();
        assert_eq!(release.version(), "1.7.7-dcc.9");
        packument.assert();
        exact_version.assert();
    }

    #[test]
    fn wsl_uses_the_matching_linux_architecture_and_exact_version() {
        let mut registry = mockito::Server::new();
        let platform = Platform::for_wsl("x86_64\r\n").unwrap();
        let mut metadata = package_metadata();
        metadata["name"] = "@dcc/git-ai-linux-x64-musl".into();
        metadata["os"] = serde_json::json!(["linux"]);
        metadata["cpu"] = serde_json::json!(["x64"]);
        metadata["libc"] = serde_json::json!(["musl"]);
        let exact_version = registry
            .mock("GET", "/@dcc/git-ai-linux-x64-musl/1.7.7-dcc.10")
            .with_body(metadata.to_string())
            .create();
        let wrong_version = registry
            .mock("GET", "/@dcc/git-ai-linux-x64-musl/1.7.7-dcc.9")
            .with_body(metadata.to_string())
            .create();

        assert_eq!(
            fetch_version_from_registry(&registry.url(), &platform, "1.7.7-dcc.10")
                .unwrap()
                .version(),
            "1.7.7-dcc.10"
        );
        assert!(fetch_version_from_registry(&registry.url(), &platform, "1.7.7-dcc.9").is_err());
        exact_version.assert();
        wrong_version.assert();
        for arch in ["aarch64", "arm64"] {
            assert_eq!(
                Platform::for_wsl(arch).unwrap().name,
                "@dcc/git-ai-linux-arm64-musl"
            );
        }
        assert!(fetch_wsl_package("i686", "1.7.7-dcc.10").is_err());
        assert!(fetch_wsl_package("x86_64", "latest").is_err());
    }

    #[test]
    fn update_decision_keeps_the_entire_npm_prerelease() {
        use super::super::{ChannelRelease, ReleaseDownload, UpgradeAction, determine_action};

        let release: Release = serde_json::from_value(package_metadata()).unwrap();
        let version = release.version().to_string();
        let release = ChannelRelease {
            tag: version.clone(),
            semver: version,
            download: ReleaseDownload::Npm(Box::new(release)),
        };
        assert_eq!(
            determine_action(false, &release, "1.7.7-dcc.9.999"),
            UpgradeAction::UpgradeAvailable
        );
        assert_eq!(
            determine_action(false, &release, "1.7.7-dcc.10"),
            UpgradeAction::AlreadyLatest
        );
    }

    #[test]
    fn promotes_the_known_artifactory_tarball_to_https() {
        assert_eq!(
            trusted_tarball("http://artifactory.intra.xiaojukeji.com/npm/fixture.tgz")
                .unwrap()
                .as_str(),
            "https://artifactory.intra.xiaojukeji.com/npm/fixture.tgz"
        );
        for url in [
            "https://artifactory.intra.xiaojukeji.com.attacker.test/a",
            "https://artifactory.intra.xiaojukeji.com:8443/a",
            "ftp://artifactory.intra.xiaojukeji.com/a",
        ] {
            assert!(trusted_tarball(url).is_err(), "{url}");
        }
    }

    #[test]
    fn stages_only_the_verified_binary_and_installer() {
        let metadata = package_metadata();
        let manifest = serde_json::to_vec(&metadata).unwrap();
        let parent = tempfile::tempdir().unwrap();
        let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::fast()));
        for (path, bytes) in [
            ("package/package.json", manifest.as_slice()),
            ("package/bin/git-ai", b"native-binary".as_slice()),
            ("package/install.sh", b"#!/bin/bash\nexit 0\n".as_slice()),
            ("package/LICENSE", b"license".as_slice()),
            ("package/THIRD-PARTY-NOTICES", b"notices".as_slice()),
        ] {
            let mut header = tar::Header::new_ustar();
            header.set_mode(0o644);
            header.set_size(bytes.len() as u64);
            tar.append_data(&mut header, path, bytes).unwrap();
        }
        let archive = tar.into_inner().unwrap().finish().unwrap();
        let mut release: Release = serde_json::from_value(metadata).unwrap();
        assert!(stage_archive(&platform(), &release, &archive, parent.path()).is_err());
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
        release.dist.integrity = format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&archive))
        );
        let staged = stage_archive(&platform(), &release, &archive, parent.path()).unwrap();
        assert_eq!(fs::read(staged.binary()).unwrap(), b"native-binary");
        assert_eq!(staged.script, "#!/bin/bash\nexit 0\n");
        drop(staged);
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
    }

    #[test]
    #[serial]
    fn package_http_works_with_remote_api_disabled_and_never_follows_redirects() {
        use mockito::Matcher;

        let mut server = mockito::Server::new();
        let metadata = server
            .mock("GET", "/metadata")
            .match_header("Authorization", Matcher::Missing)
            .match_header("X-Api-Key", Matcher::Missing)
            .with_body("metadata")
            .create();
        let redirect = server
            .mock("GET", "/redirect")
            .with_status(302)
            .with_header("Location", &format!("{}/external", server.url()))
            .create();
        let external = server.mock("GET", "/external").expect(0).create();

        // The source is loopback only in this transport test. Production callers
        // construct a fixed registry URL or validate the tarball before using get.
        unsafe {
            std::env::set_var(
                "GIT_AI_TEST_CONFIG_PATCH",
                r#"{"remote_api_enabled":false}"#,
            )
        };
        let gated = crate::http::ensure_remote_api_enabled();
        let body = get(&format!("{}/metadata", server.url()), 3, 128);
        let redirected = get(&format!("{}/redirect", server.url()), 3, 128);
        unsafe { std::env::remove_var("GIT_AI_TEST_CONFIG_PATCH") };

        assert!(gated.is_err());
        assert_eq!(body.unwrap(), b"metadata");
        assert!(redirected.is_err());
        metadata.assert();
        redirect.assert();
        external.assert();
    }
}
