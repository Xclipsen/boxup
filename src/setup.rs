use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, ensure};
use tokio::process::Command;

use crate::Config;

pub async fn run_wizard() -> Result<()> {
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "setup requires an interactive terminal"
    );
    println!("Boxup setup\n");
    println!("This wizard reconnects an existing backup. It never initializes or deletes one.");
    println!("Use the exact recovered profile and credentials from the previous installation.\n");
    confirm("Is the previous backup writer disabled")?;
    confirm("Was the SSH host key verified independently")?;
    confirm("Were database, container, VM, and filesystem-boundary requirements reviewed")?;

    let profile_source = regular_file_input("Recovered Boxup profile")?;
    let config = Config::load(&profile_source)?;
    ensure!(
        !config.notifications.enabled,
        "the guided setup does not install Discord webhooks; disable that option in the recovered profile"
    );
    let host = config.host.id.clone();
    let system_profile = Path::new("/etc/boxup").join(format!("{host}.toml"));
    config.validate_system_profile(&system_profile)?;
    let passphrase_source = regular_file_input("Borg passphrase file")?;
    let ssh_key_source = regular_file_input("Routine SSH private key")?;
    let known_hosts_source = regular_file_input("Verified known_hosts file")?;
    let browse_user = prompt(
        "Local user",
        std::env::var("USER")
            .ok()
            .as_deref()
            .filter(|value| !value.is_empty()),
    )?;

    println!("\nProfile: {host}");
    println!("Repository: {}", config.repository.location);
    println!("Protected roots:");
    for source in &config.backup.sources {
        println!("  {}", source.display());
    }
    println!("\nNo repository will be contacted during installation.");
    println!("Automatic backups and background indexing remain off.");
    confirm("Install this recovered profile now")?;

    let maintenance = config
        .repository
        .maintenance_ssh_key
        .as_ref()
        .map(|_| regular_file_input("Maintenance SSH private key"))
        .transpose()?;
    let mut command = if nix::unistd::Uid::effective().is_root() {
        Command::new("/usr/lib/boxup/setup-profile")
    } else {
        let mut command = Command::new("/usr/bin/pkexec");
        command.arg("/usr/lib/boxup/setup-profile");
        command
    };
    let status = command
        .arg(&host)
        .arg(&profile_source)
        .arg(&passphrase_source)
        .arg(&ssh_key_source)
        .arg(&known_hosts_source)
        .arg(maintenance.as_deref().unwrap_or_else(|| Path::new("-")))
        .arg(&browse_user)
        .arg("reconnect")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("failed to start the Boxup setup helper")?;
    ensure!(status.success(), "Boxup setup did not complete");

    println!("\nProfile installed. The next operation must be read-only validation:");
    println!("  boxup snapshots --live");
    println!("Confirm the expected archive names and dates before running any backup.");
    println!("Then refresh the index, restore a safe copy, and only afterwards enable automation.");
    Ok(())
}

fn prompt(label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(default) => print!("{label} [{default}]: "),
        None => print!("{label}: "),
    }
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();
    let value = if input.is_empty() {
        default.unwrap_or_default()
    } else {
        input
    };
    ensure!(!value.is_empty(), "{label} is required");
    ensure!(
        !value.chars().any(char::is_control),
        "{label} contains control characters"
    );
    Ok(value.to_owned())
}

fn regular_file_input(label: &str) -> Result<PathBuf> {
    let value = PathBuf::from(prompt(label, None)?);
    ensure!(value.is_absolute(), "{label} must be an absolute path");
    let metadata = fs::symlink_metadata(&value)
        .with_context(|| format!("{label} is unavailable: {}", value.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "{label} must be a regular non-symlink file"
    );
    Ok(value)
}

fn confirm(question: &str) -> Result<()> {
    let answer = prompt(&format!("{question}? (y/N)"), Some("N"))?;
    ensure!(
        matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"),
        "setup cancelled"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_file_input_contract_rejects_relative_paths() {
        let path = PathBuf::from("relative-profile.toml");
        assert!(!path.is_absolute());
    }
}
