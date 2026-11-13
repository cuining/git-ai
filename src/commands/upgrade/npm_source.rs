use base64::Engine;
use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha512};
use std::{fs, io::Read};
use url::Url;

const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
pub(super) const STAGED_BINARY: &str = "binary";

pub(super) fn package_prefix() -> Option<String> {
    #[cfg(any(test, debug_assertions, feature = "test-support"))]
    if let Ok(value) = std::env::var("GIT_AI_NPM_UPDATE_PACKAGE_URL") {
        return Some(value);
    }
    option_env!("GIT_AI_NPM_UPDATE_PACKAGE_URL").map(str::to_owned)
}

fn platform() -> Result<&'static str, String> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("darwin-arm64"),
        ("macos", "x86_64") => Ok("darwin-x64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        ("linux", "x86_64") => Ok("linux-x64"),
        ("windows", "aarch64") => Ok("win32-arm64"),
        ("windows", "x86_64") => Ok("win32-x64"),
        _ => Err("No npm update package for this platform".into()),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct Package {
    name: String,
    pub(super) version: semver::Version,
    os: Vec<String>,
    cpu: Vec<String>,
    dist: Distribution,
}

#[derive(Debug, Clone, Deserialize)]
struct Distribution {
    tarball: Url,
    integrity: String,
}

fn get(url: &Url, seconds: u64, limit: u64) -> Result<Vec<u8>, String> {
    let mut response = crate::http::build_agent(Some(seconds))
        .get(url.as_str())
        .config()
        .max_redirects(0)
        .build()
        .header("Accept", "application/json, application/octet-stream")
        .call()
        .map_err(|err| format!("npm update request failed: {err}"))?;
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
        .map_err(|err| format!("Could not read npm update response: {err}"))
}

pub(super) fn latest(prefix: &str) -> Result<Package, String> {
    let platform = platform()?;
    let (os, cpu) = platform.split_once('-').expect("platform is OS-CPU");
    let (_, prefix_name) = prefix.rsplit_once('/').ok_or("Missing npm package name")?;
    let expected_name = prefix
        .rsplit_once('/')
        .and_then(|(parent, _)| parent.rsplit('/').next())
        .filter(|scope| scope.starts_with('@'))
        .map_or_else(
            || format!("{prefix_name}-{platform}"),
            |scope| format!("{scope}/{prefix_name}-{platform}"),
        );
    let endpoint = Url::parse(&format!("{prefix}-{platform}/latest"))
        .map_err(|err| format!("Invalid npm update URL: {err}"))?;
    let local_test = cfg!(any(test, debug_assertions, feature = "test-support"))
        && matches!(endpoint.host_str(), Some("127.0.0.1" | "[::1]"))
        && endpoint.scheme() == "http";
    if !(endpoint.scheme() == "https" || local_test)
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err("npm update source must be an HTTPS package URL".into());
    }
    let mut package: Package = serde_json::from_slice(&get(&endpoint, 10, MAX_METADATA_BYTES)?)
        .map_err(|err| format!("Invalid npm latest response: {err}"))?;
    if package.name != expected_name || package.os != [os] || package.cpu != [cpu] {
        return Err("npm latest returned an unexpected package or platform".into());
    }
    let tarball = &mut package.dist.tarball;
    // Artifactory can advertise HTTP URLs for artifacts it also serves over HTTPS.
    if tarball.scheme() == "http" && endpoint.scheme() == "https" {
        tarball
            .set_scheme("https")
            .map_err(|()| "Invalid tarball scheme")?;
    }
    if tarball.origin() != endpoint.origin()
        || !tarball.username().is_empty()
        || tarball.password().is_some()
    {
        return Err("npm tarball is outside the update source".into());
    }
    Ok(package)
}

fn verify_sha512(bytes: &[u8], integrity: &str) -> Result<(), String> {
    let actual = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes));
    if integrity.strip_prefix("sha512-") == Some(actual.as_str()) {
        Ok(())
    } else {
        Err("npm tarball SHA-512 integrity mismatch".into())
    }
}

pub(super) fn stage(package: &Package) -> Result<tempfile::TempDir, String> {
    let bytes = get(&package.dist.tarball, 120, MAX_ARCHIVE_BYTES)?;
    verify_sha512(&bytes, &package.dist.integrity)?;
    let root = crate::config::git_ai_dir_path()
        .ok_or("Cannot locate git-ai home")?
        .join("tmp");
    fs::create_dir_all(&root).map_err(|err| err.to_string())?;
    let stage = tempfile::Builder::new()
        .prefix("npm-upgrade-")
        .tempdir_in(root)
        .map_err(|err| err.to_string())?;
    let expected = if cfg!(windows) {
        "package/bin/git-ai.exe"
    } else {
        "package/bin/git-ai"
    };
    let mut archive = tar::Archive::new(
        GzDecoder::new(bytes.as_slice()).take(MAX_BINARY_BYTES + MAX_METADATA_BYTES),
    );
    let entries = archive.entries().map_err(|err| err.to_string())?;
    for entry in entries {
        let mut entry = entry.map_err(|err| err.to_string())?;
        if entry.path_bytes().as_ref() != expected.as_bytes() {
            continue;
        }
        if !entry.header().entry_type().is_file() || !(1..=MAX_BINARY_BYTES).contains(&entry.size())
        {
            return Err("Invalid git-ai binary in npm tarball".into());
        }
        let mut file =
            fs::File::create(stage.path().join(STAGED_BINARY)).map_err(|err| err.to_string())?;
        let size = std::io::copy(&mut entry, &mut file).map_err(|err| err.to_string())?;
        if size != entry.size() {
            return Err("Truncated git-ai binary in npm tarball".into());
        }
        return Ok(stage);
    }
    Err(format!("npm tarball does not contain {expected}"))
}
