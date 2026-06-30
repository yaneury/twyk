use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use dialoguer::Input;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[derive(Parser)]
#[command(name = "orc", about = "Orchestrator for the twyk picture frame")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    #[command(about = "Configure connection settings")]
    Setup,
    #[command(about = "Turn off the display")]
    Sleep,
    #[command(about = "Turn on the display and reboot")]
    Wake,
    #[command(about = "Sync memories to the picture frame")]
    Sync,
    #[command(about = "Build and deploy a release update")]
    Update,
    #[command(about = "Build and deploy a debug build")]
    Debug,
}

#[derive(Serialize, Deserialize)]
struct Config {
    user: String,
    host: String,
    memories: String,
}

impl Config {
    fn path() -> Result<PathBuf> {
        let base = dirs::config_dir().context("could not find config directory")?;
        Ok(base.join("twyk/digital-frame/orc.toml"))
    }

    fn load() -> Result<Self> {
        let path = Self::path()?;
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("config not found at {}, run `orc setup` first", path.display()))?;
        toml::from_str(&raw).context("failed to parse config")
    }

    fn save(&self) -> Result<()> {
        let path = Self::path()?;
        fs::create_dir_all(path.parent().unwrap()).context("failed to create config dir")?;
        let raw = toml::to_string_pretty(self).context("failed to serialize config")?;
        fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))?;
        println!("Config saved to {}", path.display());
        Ok(())
    }

    fn remote(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

fn setup() -> Result<()> {
    let user = Input::<String>::new()
        .with_prompt("SSH user")
        .interact_text()?;
    let host = Input::<String>::new()
        .with_prompt("SSH host")
        .interact_text()?;
    let memories = Input::<String>::new()
        .with_prompt("Memories directory (local path)")
        .interact_text()?;

    Config { user, host, memories }.save()
}

fn run(prog: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(prog)
        .args(args)
        .status()
        .with_context(|| format!("failed to spawn {prog}"))?;
    if !status.success() {
        bail!("{prog} exited with {status}");
    }
    Ok(())
}

fn ssh(config: &Config, cmd: &str) -> Result<()> {
    run("ssh", &[&config.remote(), cmd])
}

fn git_version() -> Result<String> {
    let out = Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .output()
        .context("failed to run git describe")?;
    if !out.status.success() {
        bail!("git describe failed");
    }
    let tag = String::from_utf8(out.stdout).context("git output not utf-8")?;
    Ok(tag.trim().trim_start_matches('v').to_string())
}

fn sync(config: &Config) -> Result<()> {
    let home = env::var("HOME").context("HOME not set")?;
    let staging = PathBuf::from(&home).join("tmp/twyk");
    let dest = format!(
        "{}@{}:/home/pi/.local/share/com.yaneury.twyk",
        config.user, config.host
    );

    fs::create_dir_all(&staging).context("failed to create staging dir")?;

    run(
        "rsync",
        &[
            "-avz",
            "--exclude=.DS_Store",
            &config.memories,
            &staging.to_string_lossy(),
        ],
    )?;

    for entry in fs::read_dir(&staging).context("failed to read staging dir")? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                let lower = name.to_lowercase();
                if lower != name {
                    fs::rename(&path, staging.join(&lower))?;
                }
            }
        }
    }

    for entry in fs::read_dir(&staging).context("failed to read staging dir")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("heic") {
            let out = path.with_extension("jpg");
            run(
                "heif-convert",
                &[&path.to_string_lossy(), &out.to_string_lossy()],
            )?;
        }
    }

    run(
        "rsync",
        &[
            "-avz",
            "--exclude=*.heic",
            &staging.to_string_lossy(),
            &dest,
        ],
    )?;

    Ok(())
}

fn deploy(config: &Config, version: &str, debug: bool) -> Result<()> {
    let profile = if debug { "debug" } else { "release" };
    let cwd = env::current_dir().context("failed to get cwd")?;
    let deb = format!(
        "{}/src-tauri/target/aarch64-unknown-linux-gnu/{}/bundle/deb/twyk_{}_arm64.deb",
        cwd.to_string_lossy(),
        profile,
        version
    );
    let remote = config.remote();

    let mut build_args = vec![
        "tauri",
        "build",
        "--target",
        "aarch64-unknown-linux-gnu",
        "--bundles",
        "deb",
    ];
    if debug {
        build_args.push("--debug");
    }

    let status = Command::new("cargo")
        .args(&build_args)
        .env("PKG_CONFIG_SYSROOT_DIR", "/usr/aarch64-linux-gnu/")
        .status()
        .context("failed to spawn cargo")?;
    if !status.success() {
        bail!("cargo tauri build failed");
    }

    run(
        "scp",
        &[&deb, &format!("{remote}:/home/pi/downloads/twyk.deb")],
    )?;
    ssh(config, "sudo dpkg -i /home/pi/downloads/twyk.deb")?;
    ssh(config, "rm /home/pi/downloads/twyk.deb")?;
    ssh(config, "sudo reboot")?;

    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Cmd::Setup = cli.command {
        return setup();
    }

    let config = Config::load()?;

    match cli.command {
        Cmd::Setup => unreachable!(),
        Cmd::Sleep => ssh(&config, "xset -d :0 dpms force off")?,
        Cmd::Wake => {
            ssh(&config, "xset -d :0 dpms force on")?;
            ssh(&config, "sudo reboot")?;
        }
        Cmd::Sync => sync(&config)?,
        Cmd::Update => deploy(&config, "0.0.6", false)?,
        Cmd::Debug => {
            let version = git_version()?;
            deploy(&config, &version, true)?;
        }
    }

    Ok(())
}
