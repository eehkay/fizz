//! Noticing that a newer Fizz is out, and installing it.
//!
//! There are two ways Fizz gets onto a machine, and each updates its own
//! way:
//!
//! - A **release download** (the install script) knows the version tag it
//!   was built for. It asks GitHub for the latest release and, to update,
//!   downloads the binary for this platform and swaps itself for it.
//! - A **build from a clone** knows its commit and where the clone is. It
//!   asks the clone's remote for the newest commit and, to update, pulls
//!   and rebuilds.

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::AppEvent;

/// The commit this binary was built from; empty if built outside git.
pub fn built_commit() -> String {
    // Lets the check be exercised without making real commits.
    std::env::var("FIZZ_COMMIT_OVERRIDE").unwrap_or_else(|_| env!("FIZZ_COMMIT").to_string())
}

/// The release this binary was downloaded as, such as `v0.2.0`; empty for
/// a build from source.
pub fn built_release() -> String {
    std::env::var("FIZZ_RELEASE_OVERRIDE").unwrap_or_else(|_| env!("FIZZ_RELEASE").to_string())
}

pub fn short(commit: &str) -> &str {
    &commit[..commit.len().min(7)]
}

/// `owner/name` of the repository, from the package's own metadata.
fn repository() -> &'static str {
    env!("CARGO_PKG_REPOSITORY")
        .trim_end_matches('/')
        .trim_start_matches("https://github.com/")
}

/// The clone Fizz was built from, if it is still there.
fn source() -> Option<PathBuf> {
    let path = Path::new(env!("FIZZ_SOURCE"));
    path.join(".git").exists().then(|| path.to_path_buf())
}

fn git(source: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(source)
        // Never stop to ask for a password: a check must not hang.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -oBatchMode=yes")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

/// `v1.2.3` as numbers, for telling newer from older.
fn version_parts(tag: &str) -> Option<Vec<u64>> {
    tag.trim()
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect()
}

/// Whether release `candidate` is newer than release `current`.
fn is_newer(candidate: &str, current: &str) -> bool {
    match (version_parts(candidate), version_parts(current)) {
        (Some(a), Some(b)) => a > b,
        // Tags that are not plain versions: different is all we can say.
        _ => candidate != current,
    }
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        // GitHub's API refuses requests that do not say what they are.
        .user_agent("fizz-updater")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(600))
        .build()?)
}

/// The tag of the latest published release.
async fn latest_release() -> Result<String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repository());
    let reply: serde_json::Value = http()?
        .get(url)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .context("couldn't reach GitHub")?
        .error_for_status()
        .context("GitHub had no release to offer")?
        .json()
        .await?;
    reply
        .get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("GitHub's reply did not name a release"))
}

/// The newest commit on the clone's remote, if it is one we do not have.
async fn newer_commit() -> Result<Option<String>> {
    let built = built_commit();
    let Some(source) = source().filter(|_| !built.is_empty()) else {
        bail!("this copy of Fizz cannot tell where it came from, so it cannot check for updates");
    };
    let listing = tokio::time::timeout(
        Duration::from_secs(20),
        git(&source).args(["ls-remote", "origin", "HEAD"]).output(),
    )
    .await
    .context("GitHub took too long to answer")??;
    if !listing.status.success() {
        bail!("couldn't reach the repository");
    }
    let latest = String::from_utf8_lossy(&listing.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    if latest.is_empty() || latest == built {
        return Ok(None);
    }
    // If the server's commit is already part of what we were built from,
    // we are ahead of it, not behind.
    let ahead = git(&source)
        .args(["merge-base", "--is-ancestor", &latest, &built])
        .status()
        .await
        .is_ok_and(|status| status.success());
    Ok((!ahead).then_some(latest))
}

/// What is available that is newer than this copy, as something to show
/// (a version tag or a short commit). `Ok(None)` means up to date.
pub async fn newer_version() -> Result<Option<String>> {
    let release = built_release();
    if !release.is_empty() {
        let latest = latest_release().await?;
        return Ok(is_newer(&latest, &release).then_some(latest));
    }
    Ok(newer_commit().await?.map(|commit| short(&commit).to_string()))
}

/// Check now and then for as long as the app runs.
pub fn watch(tx: UnboundedSender<AppEvent>) {
    let settings = &crate::config::settings().updates;
    if !settings.check || (built_commit().is_empty() && built_release().is_empty()) {
        return;
    }
    let every = Duration::from_secs(settings.every_hours.clamp(1, 168) * 3600);
    tokio::spawn(async move {
        // Let the app finish starting before using the network for this.
        tokio::time::sleep(Duration::from_secs(8)).await;
        loop {
            if let Ok(found) = newer_version().await
                && tx.send(AppEvent::Update(found)).is_err()
            {
                return;
            }
            tokio::time::sleep(every).await;
        }
    });
}

fn run(program: &str, args: &[&str], dir: &Path) -> Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .current_dir(dir)
        .status()
        .with_context(|| format!("couldn't run {program}; is it installed?"))?;
    if !status.success() {
        bail!("{program} {} failed", args.first().unwrap_or(&""));
    }
    Ok(())
}

/// Replace the running program with the latest release for this platform.
async fn install_release(tag: &str) -> Result<()> {
    let target = env!("FIZZ_TARGET");
    let name = format!("fizz-{target}.tar.gz");
    let base = format!("https://github.com/{}/releases/download/{tag}", repository());
    let client = http()?;
    let fetch = |url: String| {
        let client = client.clone();
        async move {
            let response = client.get(&url).send().await?.error_for_status()?;
            Ok::<_, anyhow::Error>(response.bytes().await?.to_vec())
        }
    };
    println!("Downloading {name}…");
    let archive = fetch(format!("{base}/{name}"))
        .await
        .with_context(|| format!("there is no download for this platform ({target}) in {tag}"))?;
    let listed = fetch(format!("{base}/{name}.sha256")).await?;
    // The checksum file is "<hash>  <file name>".
    let expected = String::from_utf8_lossy(&listed)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    let actual: String = Sha256::digest(&archive)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if expected.is_empty() || expected != actual {
        bail!("the download did not match its checksum, so it was not installed");
    }

    let current = std::env::current_exe().context("couldn't find where Fizz is installed")?;
    let current = std::fs::canonicalize(&current).unwrap_or(current);
    let folder = current.parent().context("Fizz is installed somewhere odd")?;
    let work = folder.join(format!(".fizz-update-{}", std::process::id()));
    std::fs::create_dir_all(&work).with_context(|| {
        format!("can't write to {} (is it owned by someone else?)", folder.display())
    })?;
    let result = (|| {
        let packed = work.join(&name);
        std::fs::write(&packed, &archive)?;
        run("tar", &["-xzf", &name], &work)?;
        let fresh = work.join("fizz");
        if !fresh.is_file() {
            bail!("the download did not contain the program");
        }
        // Renaming over a running program is fine: the old one keeps going
        // until it exits, and the next start gets the new one.
        std::fs::rename(&fresh, &current)?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// `fizz update`: install the newest version, whichever way this copy
/// arrived. With `check_only`, just say whether there is one.
pub async fn update(check_only: bool) -> Result<()> {
    // Where this Fizz is, asked now: once it has been replaced the system
    // no longer gives a usable answer.
    let fizz = std::env::current_exe().ok();
    // The bar widget's files are part of Fizz, so they are renewed with it,
    // by the new version.
    let refresh_widget = move || {
        if crate::widget::installed()
            && let Some(fizz) = &fizz
        {
            let _ = std::process::Command::new(fizz).args(["widget", "refresh"]).status();
        }
    };
    let release = built_release();
    let built = built_commit();
    let installed = match (release.is_empty(), built.is_empty()) {
        (false, _) => release.clone(),
        (true, false) => short(&built).to_string(),
        (true, true) => "unknown".to_string(),
    };
    println!("Installed  {installed}");
    let Some(latest) = newer_version().await? else {
        println!("Fizz is up to date.");
        return Ok(());
    };
    println!("Available  {latest}");
    if check_only {
        println!("\nRun `fizz update` to install it.");
        return Ok(());
    }

    if !release.is_empty() {
        install_release(&latest).await?;
        refresh_widget();
        println!("\nDone. Quit Fizz and open it again to use {latest}.");
        return Ok(());
    }

    let source = source().context("the folder Fizz was built from has gone")?;
    println!("\nUpdating in {}\n", source.display());
    run("git", &["pull", "--ff-only"], &source).context(
        "couldn't pull the new version. If you have changed files in that folder, \
         set them aside (git stash) and try again",
    )?;
    println!("\nWhat's new:");
    let _ = run(
        "git",
        &["--no-pager", "log", "--format=  %s", &format!("{built}..HEAD")],
        &source,
    );
    println!("\nBuilding (this takes a minute)…\n");
    run("cargo", &["install", "--path", ".", "--locked"], &source)?;
    refresh_widget();
    println!("\nDone. Quit Fizz and open it again to use the new version.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_as_numbers() {
        assert!(is_newer("v0.2.0", "v0.1.9"));
        assert!(is_newer("v0.10.0", "v0.9.0"));
        assert!(is_newer("v1.0.0", "v0.99.99"));
        assert!(!is_newer("v0.1.0", "v0.1.0"));
        assert!(!is_newer("v0.1.0", "v0.2.0"));
        // Odd tags can only be compared for sameness.
        assert!(is_newer("nightly-2", "nightly-1"));
        assert!(!is_newer("nightly", "nightly"));
    }

    #[test]
    fn knows_its_repository() {
        assert_eq!(repository(), "eehkay/fizz");
    }
}
