//! Noticing that a newer Fizz has been pushed, and installing it.
//!
//! Fizz is installed by cloning its repository and building it, so the
//! clone is the update channel: the build records the commit and the
//! clone's location, and checks go through that clone with whatever git
//! login it already uses. That works for a private repository with no
//! tokens of our own.

use anyhow::{Context, Result, bail};
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

pub fn short(commit: &str) -> &str {
    &commit[..commit.len().min(7)]
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

/// The newest commit on the server, if it is one we do not have. `Ok(None)`
/// means up to date (or ahead, on a machine with unpushed work).
pub async fn newer_commit() -> Result<Option<String>> {
    let built = built_commit();
    let Some(source) = source().filter(|_| !built.is_empty()) else {
        bail!("this copy of Fizz was not built from a git clone, so it cannot check for updates");
    };
    let listing = tokio::time::timeout(
        Duration::from_secs(20),
        git(&source).args(["ls-remote", "origin", "HEAD"]).output(),
    )
    .await
    .context("GitHub took too long to answer")??;
    if !listing.status.success() {
        bail!("couldn't reach the repository (are you signed in to GitHub?)");
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

/// Check now and then for as long as the app runs.
pub fn watch(tx: UnboundedSender<AppEvent>) {
    let settings = &crate::config::settings().updates;
    if !settings.check || built_commit().is_empty() {
        return;
    }
    let every = Duration::from_secs(settings.every_hours.clamp(1, 168) * 3600);
    tokio::spawn(async move {
        // Let the app finish starting before using the network for this.
        tokio::time::sleep(Duration::from_secs(8)).await;
        loop {
            if let Ok(found) = newer_commit().await
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

/// `fizz update`: fetch the newest code, rebuild and reinstall.
pub async fn update(check_only: bool) -> Result<()> {
    let built = built_commit();
    println!("Installed  {}", if built.is_empty() { "unknown" } else { short(&built) });
    let latest = newer_commit().await?;
    let Some(latest) = latest else {
        println!("Fizz is up to date.");
        return Ok(());
    };
    println!("Available  {}", short(&latest));
    if check_only {
        println!("\nRun `fizz update` to install it.");
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
    println!("\nDone. Quit Fizz and open it again to use the new version.");
    Ok(())
}
