use anyhow::{bail, Result};
use clap::{ArgAction, Parser, Subcommand};
use serde::Deserialize;
use std::{env, path::PathBuf};

/// AUR RPC base
const AUR_RPC: &str = "https://aur.archlinux.org/rpc/?v=5";

/// yaorust CLI
#[derive(Parser, Debug)]
#[command(version, author, about = "Fast minimal AUR + repo helper with root-safe build modes")]
struct Cli {
    /// Verbose output
    #[arg(short, long)]
    verbose: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Install from repos or AUR
    S {
        /// Force rebuild (delete pre-existing artifacts and pass -f -C to makepkg)
        #[arg(short = 'f', long = "force", action = ArgAction::SetTrue)]
        force: bool,

        /// Package(s)
        pkgs: Vec<String>,
    },

    /// Download PKGBUILD snapshot(s) from AUR into ./<pkg>/
    G {
        /// Package(s)
        pkgs: Vec<String>,
    },
}

/// Runtime configuration (from ENV with defaults)
#[derive(Debug, Clone)]
struct Config {
    /// Where makepkg will place built packages
    pkgdest: PathBuf,
    /// How to behave when running as root
    root_mode: RootMode,
    /// Auto-enable patched makepkg for root mode (off by default)
    auto_trust_root: bool,
    /// Build user for "user" mode fallback
    build_user: String,
    /// Snapshot cache dir for AUR tarballs
    snapshot_cache: PathBuf,
    /// Pacman binary name/path
    pacman: String,
    /// Sudo binary name/path
    sudo: String,
    /// Verbose flag from CLI
    verbose: bool,
}

#[derive(Debug, Clone, Copy)]
enum RootMode {
    Auto,
    Sandbox,
    User,
    TrustRoot,
}

impl Config {
    fn from_env(verbose: bool) -> Self {
        let pkgdest = env::var("PKGDEST").unwrap_or_else(|_| "/var/cache/makepkg".into());
        let snapshot_cache = env::var("YAORUST_SNAPSHOT_CACHE")
            .unwrap_or_else(|_| "/var/cache/yaorust/snapshots".into());

        let root_mode = match env::var("YAORUST_ROOT_MODE")
            .unwrap_or_else(|_| "auto".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "sandbox" => RootMode::Sandbox,
            "user" => RootMode::User,
            "trust-root" | "trustroot" => RootMode::TrustRoot,
            _ => RootMode::Auto,
        };

        let auto_trust_root = env::var("YAORUST_AUTO_TRUST_ROOT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        Self {
            pkgdest: PathBuf::from(pkgdest),
            root_mode,
            auto_trust_root,
            build_user: env::var("YAORUST_BUILD_USER").unwrap_or_else(|_| "nobody".into()),
            snapshot_cache: PathBuf::from(snapshot_cache),
            pacman: env::var("YAORUST_PACMAN").unwrap_or_else(|_| "pacman".into()),
            sudo: env::var("YAORUST_SUDO").unwrap_or_else(|_| "sudo".into()),
            verbose,
        }
    }
}

/// Simple AUR response types (we’ll expand later)
#[derive(Debug, Deserialize)]
struct AurInfoResponse {
    #[serde(rename = "type")]
    typ: String,
    resultcount: u32,
    results: Vec<AurPkg>,
}

#[derive(Debug, Deserialize)]
struct AurPkg {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Version")]
    version: String,
}

/// Repo/AUR classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PkgKind {
    Repo,
    Aur,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::from_env(cli.verbose);

    if cfg.verbose {
        eprintln!("==> config: {:?}", cfg);
    }

    match cli.cmd {
        Cmd::G { pkgs } => cmd_getpkgbuild(&cfg, pkgs),
        Cmd::S { force, pkgs } => cmd_sync(&cfg, pkgs, force),
    }
}

/* ---------- Command stubs (implement step by step) ---------- */

fn cmd_getpkgbuild(_cfg: &Config, pkgs: Vec<String>) -> Result<()> {
    if pkgs.is_empty() {
        bail!("no packages specified for -G");
    }
    // For now, just log what we would do.
    for p in pkgs {
        eprintln!("==> [G] would fetch PKGBUILD for {}", p);
        // Next steps (to implement):
        // 1) AUR RPC exists?
        // 2) Use snapshot cache dir
        // 3) Download if missing
        // 4) Extract into ./<pkg>/
    }
    Ok(())
}

fn cmd_sync(cfg: &Config, pkgs: Vec<String>, force: bool) -> Result<()> {
    if pkgs.is_empty() {
        bail!("no packages specified for -S");
    }

    for p in pkgs {
        let kind = classify_package(cfg, &p).unwrap_or_else(|_| PkgKind::Aur); // default to AUR on RPC success later
        match kind {
            PkgKind::Repo => {
                eprintln!("==> [repo] would install {}", p);
                // Next steps:
                // run pacman -S --needed
            }
            PkgKind::Aur => {
                eprintln!("==> [aur] would build {}", p);
                if force {
                    eprintln!("    (force rebuild enabled)");
                }
                // Next steps:
                // snapshot -> extract -> packagelist -> optional cleanup -> makepkg -> pacman -U
            }
        }
    }
    Ok(())
}

/* ---------- Helpers (stubs) ---------- */

/// Decide if a package is in repos or AUR.
/// MVP approach:
/// - Try `pacman -Si -- <name>`; if success -> Repo
/// - Else check AUR RPC exists -> Aur
fn classify_package(_cfg: &Config, _name: &str) -> Result<PkgKind> {
    // We’ll implement this in the next step.
    // For now, return Ok(PkgKind::Aur) so flows are visible.
    Ok(PkgKind::Aur)
}
