use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use self_update::backends::github::ReleaseList;
use semver::Version;

const OWNER: &str = match option_env!("NET_COMBINER_UPDATE_OWNER") {
    Some(value) => value,
    None => "ivLis-Studio",
};
const REPO: &str = match option_env!("NET_COMBINER_UPDATE_REPO") {
    Some(value) => value,
    None => "net-combiner",
};

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub available: bool,
}

pub fn check() -> Result<UpdateInfo> {
    let releases = ReleaseList::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .build()?
        .fetch()
        .context("failed to fetch GitHub releases")?;

    let release = releases
        .into_iter()
        .next()
        .context("no GitHub releases found")?;
    let current_version = env!("CARGO_PKG_VERSION").to_owned();
    let latest_version = normalize_version(&release.version);
    let available = version_is_newer(&current_version, &latest_version);

    Ok(UpdateInfo {
        current_version,
        latest_version,
        available,
    })
}

pub fn install_latest() -> Result<String> {
    let releases = ReleaseList::configure()
        .repo_owner(OWNER)
        .repo_name(REPO)
        .build()?
        .fetch()
        .context("failed to fetch GitHub releases")?;
    let release = releases
        .into_iter()
        .next()
        .context("no GitHub releases found")?;

    let current_version = env!("CARGO_PKG_VERSION");
    if !version_is_newer(current_version, &release.version) {
        return Ok(format!("UpToDate({current_version})"));
    }

    let target = self_update::get_target();
    let asset = release
        .asset_for(target, Some("portable"))
        .with_context(|| format!("no portable asset found for target `{target}`"))?;

    let tmp_dir = self_update::TempDir::new().context("failed to create update temp dir")?;
    let archive_path = tmp_dir.path().join(&asset.name);
    let mut archive = fs::File::create(&archive_path).context("failed to create update archive")?;
    self_update::Download::from_url(&asset.download_url)
        .set_header(reqwest::header::ACCEPT, "application/octet-stream".parse()?)
        .download_to(&mut archive)
        .context("failed to download update archive")?;

    self_update::Extract::from_source(&archive_path)
        .extract_into(tmp_dir.path())
        .context("failed to extract update archive")?;

    let new_exe = tmp_dir.path().join(binary_path_in_archive());
    if !new_exe.exists() {
        anyhow::bail!(
            "update archive did not contain {}",
            binary_path_in_archive().display()
        );
    }

    let install_dir = std::env::current_exe()
        .context("failed to resolve current executable")?
        .parent()
        .map(Path::to_path_buf)
        .context("current executable has no parent directory")?;
    let archive_dir = new_exe
        .parent()
        .map(Path::to_path_buf)
        .context("archive binary has no parent directory")?;
    let sidecars = copy_sidecars(&archive_dir, &install_dir);

    self_update::self_replace::self_replace(new_exe).context("failed to replace executable")?;

    match sidecars {
        Ok(count) if count > 0 => Ok(format!(
            "Updated({}); sidecars updated: {count}",
            release.version
        )),
        Ok(_) => Ok(format!("Updated({})", release.version)),
        Err(error) => Ok(format!(
            "Updated({}); sidecar update skipped: {error}",
            release.version
        )),
    }
}

pub fn repo_label() -> String {
    format!("{OWNER}/{REPO}")
}

fn normalize_version(value: &str) -> String {
    value.trim().trim_start_matches('v').to_owned()
}

fn version_is_newer(current: &str, latest: &str) -> bool {
    let current = Version::parse(&normalize_version(current));
    let latest = Version::parse(&normalize_version(latest));
    match (current, latest) {
        (Ok(current), Ok(latest)) => latest > current,
        _ => false,
    }
}

fn binary_path_in_archive() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from("net-combiner.exe")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("net-combiner").join("net-combiner")
    }
}

fn copy_sidecars(source_dir: &Path, install_dir: &Path) -> Result<usize> {
    let mut copied = 0;
    for name in sidecar_names() {
        let source = source_dir.join(name);
        if !source.exists() {
            continue;
        }
        let dest = install_dir.join(name);
        fs::copy(&source, &dest).with_context(|| {
            format!("failed to copy {} to {}", source.display(), dest.display())
        })?;
        set_executable_if_needed(&dest)?;
        copied += 1;
    }
    Ok(copied)
}

#[cfg(windows)]
fn sidecar_names() -> &'static [&'static str] {
    &["tun2proxy-bin.exe", "tun2proxy.exe", "wintun.dll"]
}

#[cfg(not(windows))]
fn sidecar_names() -> &'static [&'static str] {
    &["tun2proxy-bin", "tun2proxy"]
}

#[cfg(unix)]
fn set_executable_if_needed(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_executable_if_needed(_path: &Path) -> Result<()> {
    Ok(())
}
