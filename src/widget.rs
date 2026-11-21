//! `fizz widget`: the Omarchy bar widget, carried inside the binary.
//!
//! The widget's files live in `contrib/omarchy-widget` and are built into
//! Fizz, so every kind of install has them and they always match the
//! `fizz bridge` they talk to. Installing writes them into the Omarchy
//! shell's plugin folder, along with the path of this Fizz for the widget
//! to start, and asks Omarchy to put the widget in the bar.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Command;

pub const ID: &str = "eehkay.fizz";

const FILES: [(&str, &str); 3] = [
    ("manifest.json", include_str!("../contrib/omarchy-widget/manifest.json")),
    ("Service.qml", include_str!("../contrib/omarchy-widget/Service.qml")),
    ("Widget.qml", include_str!("../contrib/omarchy-widget/Widget.qml")),
];

const HELP: &str = "\
The Fizz widget for the Omarchy bar: an unread count, your recent
conversations, and a reply box.

  fizz widget install         Put it in the bar (again after an update)
  fizz widget install --demo  The same, showing made-up conversations
  fizz widget remove          Take it out again";

/// Where the Omarchy shell looks for plugins, if Omarchy is here at all.
fn plugins_dir() -> Result<PathBuf> {
    let omarchy = dirs::config_dir()
        .context("no config folder on this system")?
        .join("omarchy");
    if !omarchy.is_dir() {
        bail!(
            "the widget is for the Omarchy bar, and {} does not exist",
            omarchy.display()
        );
    }
    Ok(omarchy.join("plugins"))
}

/// The line the widget reads to learn which Fizz to start.
fn config_js(fizz: &Path) -> String {
    format!(
        ".pragma library\n\n// Written by `fizz widget install`: the Fizz this widget starts.\nvar fizz = {}\n",
        serde_json::Value::String(fizz.to_string_lossy().into_owned())
    )
}

fn write_files(dir: &Path, fizz: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("couldn't create {}", dir.display()))?;
    let config = config_js(fizz);
    let files = FILES.iter().copied().chain([("Config.js", config.as_str())]);
    for (name, text) in files {
        // Write then rename: the shell reloads plugins as their files change.
        let tmp = dir.join(format!(".{name}.tmp"));
        std::fs::write(&tmp, text).with_context(|| format!("couldn't write {}", tmp.display()))?;
        std::fs::rename(&tmp, dir.join(name))?;
    }
    Ok(())
}

/// Run an `omarchy` command, quietly. False when it is missing or failed.
fn omarchy(args: &[&str]) -> bool {
    Command::new("omarchy")
        .args(args)
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Whether the widget's files are installed here.
pub fn installed() -> bool {
    plugins_dir().is_ok_and(|dir| dir.join(ID).join("manifest.json").exists())
}

/// Whether Omarchy has the widget switched on.
fn in_bar() -> bool {
    Command::new("omarchy")
        .args(["plugin", "list", "--json"])
        .output()
        .ok()
        .and_then(|out| serde_json::from_slice::<serde_json::Value>(&out.stdout).ok())
        .and_then(|list| {
            let list = list.as_array()?;
            Some(list.iter().any(|p| p["id"] == ID && p["enabled"] == true))
        })
        .unwrap_or(false)
}

/// Bring installed files up to date with this Fizz, and change nothing else.
/// `fizz update` runs this through the new version once it is in place.
fn refresh() -> Result<()> {
    let dir = plugins_dir()?.join(ID);
    if dir.is_symlink() || !dir.join("manifest.json").exists() {
        return Ok(());
    }
    write_files(&dir, &std::env::current_exe()?)
}

fn install(demo: bool) -> Result<()> {
    let dir = plugins_dir()?.join(ID);
    let fizz = std::env::current_exe().context("couldn't tell where this Fizz is")?;
    if dir.is_symlink() {
        // Someone working on the widget has linked their copy in.
        println!("{} is a link; its files are left as they are.", dir.display());
    } else {
        write_files(&dir, &fizz)?;
        println!("Widget files written to {}", dir.display());
        println!("It will start {}", fizz.display());
    }
    // `put` adds it only where it is not in the bar already, so one that has
    // been moved stays where it was put.
    let placed = in_bar();
    if !placed {
        // The shell notices new plugins by itself, but not at once.
        let _ = Command::new("omarchy-shell").args(["shell", "rescanPlugins"]).output();
        if !omarchy(&["bar", "put", ID]) {
            println!("\nCouldn't add it to the bar. Try: omarchy plugin enable {ID}");
            return Ok(());
        }
    }
    if demo || placed {
        // Without --demo, an earlier demo install goes back to real messages.
        let value = if demo { "true" } else { "false" };
        omarchy(&["bar", "set", ID, "demo", value, "--json"]);
    }
    println!(
        "\nThe widget is in the bar{}. Click it for your conversations; right-click opens Fizz.",
        if demo { ", showing made-up conversations" } else { "" }
    );
    if demo {
        println!("For your own messages: fizz widget install");
    }
    if placed {
        // The shell keeps running the version of a widget it first loaded.
        println!("If it was already running, load this version with: omarchy restart shell");
    }
    Ok(())
}

fn remove() -> Result<()> {
    let dir = plugins_dir()?.join(ID);
    omarchy(&["plugin", "disable", ID]);
    if dir.is_symlink() {
        std::fs::remove_file(&dir)?;
    } else if dir.join("manifest.json").exists() {
        // Only ever a folder of ours: check before deleting it.
        let manifest = std::fs::read_to_string(dir.join("manifest.json")).unwrap_or_default();
        let ours = serde_json::from_str::<serde_json::Value>(&manifest)
            .is_ok_and(|m| m.get("id").and_then(|id| id.as_str()) == Some(ID));
        if !ours {
            bail!("{} is not the Fizz widget; left alone", dir.display());
        }
        std::fs::remove_dir_all(&dir)?;
    } else {
        println!("The widget was not installed.");
        return Ok(());
    }
    println!("The widget has been removed.");
    Ok(())
}

pub fn command(args: &[String]) -> Result<()> {
    let has = |word: &str| args.iter().any(|a| a == word);
    if has("install") {
        install(has("--demo"))
    } else if has("remove") {
        remove()
    } else if has("refresh") {
        refresh()
    } else {
        println!("{HELP}");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_names_the_files_that_ship() {
        let manifest: serde_json::Value = serde_json::from_str(FILES[0].1).unwrap();
        assert_eq!(manifest["id"], ID);
        for entry in manifest["entryPoints"].as_object().unwrap().values() {
            let name = entry.as_str().unwrap();
            assert!(FILES.iter().any(|(file, _)| *file == name), "{name} is not shipped");
        }
    }

    #[test]
    fn the_path_to_fizz_survives_awkward_characters() {
        let js = config_js(Path::new("/home/it's \"me\"/bin/fizz"));
        assert!(js.contains(r#"var fizz = "/home/it's \"me\"/bin/fizz""#));
    }

    #[test]
    fn installing_writes_every_file() {
        let dir = std::env::temp_dir().join(format!("fizz-widget-test-{}", std::process::id()));
        write_files(&dir, Path::new("/usr/bin/fizz")).unwrap();
        for name in ["manifest.json", "Service.qml", "Widget.qml", "Config.js"] {
            assert!(dir.join(name).exists(), "{name} missing");
        }
        assert!(std::fs::read_dir(&dir).unwrap().count() == 4, "nothing left behind");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
