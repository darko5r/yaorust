use anyhow::{bail, Result};
use clap::{ArgAction, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use serde::Deserialize;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use which::which;

const AUR_RPC: &str = "https://aur.archlinux.org/rpc/?v=5";

/// Top-level CLI
#[derive(Parser, Debug)]
#[command(name = "yaorust", version, about = "Fast minimal AUR + repo helper")]
struct Cli {
    /// Verbose logging
    #[arg(short, long, action = ArgAction::SetTrue)]
    verbose: bool,

    #[command(subcommand)]
    cmd: CommandKind,
}

#[derive(Subcommand, Debug)]
enum CommandKind {
    /// Install from repos or AUR
    S {
        /// Force rebuild/overwrite
        #[arg(short = 'f', long, action = ArgAction::SetTrue)]
        force: bool,

        /// Packages to install
        pkgs: Vec<String>,
    },
    /// Download PKGBUILD(s) to ./<pkg>/
    G {
        /// Packages to fetch
        pkgs: Vec<String>,
    },
}

/// Root-mode behavior (placeholder for coming features)
#[derive(Clone, Copy, Debug)]
enum RootMode {
    Auto,
    Sandbox,
    User,
    TrustRoot,
}

impl RootMode {
    fn from_env() -> Self {
        match env::var("YAORUST_ROOT_MODE")
            .unwrap_or_else(|_| "auto".into())
            .as_str()
        {
            "sandbox" => RootMode::Sandbox,
            "user" => RootMode::User,
            "trust-root" => RootMode::TrustRoot,
            _ => RootMode::Auto,
        }
    }
}

/// Runtime configuration collected from ENV (and defaults)
#[derive(Clone, Debug)]
struct Config {
    /// Where makepkg will place built packages
    pkgdest: PathBuf,
    /// How to behave when running as root
    root_mode: RootMode,
    /// Auto-enable patched makepkg for root mode (later)
    auto_trust_root: bool,
    /// Build user for "user" mode fallback (later)
    build_user: String,
    /// Snapshot cache dir for AUR tarballs
    snapshot_cache: PathBuf,
    /// Pacman binary name/path
    pacman: String,
    /// Sudo binary name/path
    sudo: String,
    /// Whether to print extra logs
    verbose: bool,
}

impl Config {
    fn load(verbose: bool) -> Result<Self> {
        let pkgdest = env::var("PKGDEST")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/var/cache/makepkg"));

        let snapshot_cache = env::var("YAORUST_SNAPSHOT_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/var/cache/yaorust/snapshots"));

        let pacman = env::var("YAORUST_PACMAN").unwrap_or_else(|_| "pacman".to_string());
        let sudo = env::var("YAORUST_SUDO").unwrap_or_else(|_| "sudo".to_string());

        let build_user = env::var("YAORUST_BUILD_USER").unwrap_or_else(|_| "nobody".to_string());
        let auto_trust_root = env::var("YAORUST_AUTO_TRUST_ROOT")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        Ok(Self {
            pkgdest,
            root_mode: RootMode::from_env(),
            auto_trust_root,
            build_user,
            snapshot_cache,
            pacman,
            sudo,
            verbose,
        })
    }
}

/* ---------------------- AUR RPC models ---------------------- */

#[derive(Deserialize, Debug)]
struct AurInfoResponse {
    #[serde(rename = "type")]
    _ty: String, // underscore: we deserialize but don't use directly
    _version: i32,
    resultcount: i32,
    results: Option<Vec<AurPkg>>,
}

#[derive(Deserialize, Debug)]
struct AurPkg {
    #[serde(rename = "Name")]
    name: String,
    // other fields not needed now
}

/* ---------------------- Entry ---------------------- */

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(cli.verbose)?;

    // Ensure required external tools are present
    ensure_tools(&cfg)?;

    // Create caches/dirs up-front
    fs::create_dir_all(&cfg.pkgdest)?;
    fs::create_dir_all(&cfg.snapshot_cache)?;

    if cfg.verbose {
        eprintln!(
            "==> config: PKGDEST={}, snapshot_cache={}, pacman={}, sudo={}, root_mode={:?}, auto_trust_root={}, build_user={}, euid={}",
            cfg.pkgdest.display(),
            cfg.snapshot_cache.display(),
            cfg.pacman, cfg.sudo, cfg.root_mode, cfg.auto_trust_root, cfg.build_user, nix_like_geteuid()
        );
    }

    match cli.cmd {
        CommandKind::G { pkgs } => cmd_getpkgbuild(&cfg, pkgs),
        CommandKind::S { pkgs, force } => cmd_sync(&cfg, pkgs, force),
    }
}

/* ---------------------- Commands ---------------------- */

fn cmd_getpkgbuild(cfg: &Config, pkgs: Vec<String>) -> Result<()> {
    if pkgs.is_empty() {
        bail!("no packages specified for -G");
    }

    let client = http_client()?;

    for p in pkgs {
        if !aur_exists(&client, &p)? {
            bail!("{p} not found in AUR");
        }
        let tgz = download_snapshot(&client, cfg, &p)?;
        let tmp = TempDir::new()?;
        extract_tgz(&tgz, tmp.path())?;
        let src = tmp.path().join(&p);
        let dst = Path::new(&p);
        if dst.exists() {
            fs::remove_dir_all(dst)?;
        }
        fs::rename(&src, &p)?;
        eprintln!("==> PKGBUILD for {p} saved to ./{}", p);
    }
    Ok(())
}

fn cmd_sync(cfg: &Config, pkgs: Vec<String>, force: bool) -> Result<()> {
    if pkgs.is_empty() {
        bail!("no packages specified for -S");
    }
    let client = http_client()?;

    for p in pkgs {
        match classify_pkg(cfg, &client, &p)? {
            PkgKind::Repo => {
                eprintln!("==> [repo] installing {p}");
                pacman_install_repo(cfg, &[p])?;
            }
            PkgKind::Aur => {
                eprintln!("==> [aur] building {p}");
                aur_build_install(cfg, &client, &p, force)?;
            }
        }
    }
    Ok(())
}

/* ---------------------- Classify ---------------------- */

#[derive(Debug, Clone, Copy)]
enum PkgKind {
    Repo,
    Aur,
}

fn classify_pkg(cfg: &Config, client: &Client, name: &str) -> Result<PkgKind> {
    if pacman_si_ok(&cfg.pacman, name) {
        return Ok(PkgKind::Repo);
    }
    if aur_exists(client, name)? {
        return Ok(PkgKind::Aur);
    }
    bail!("{name} not found in repos or AUR");
}

/* ---------------------- Repo path ---------------------- */

fn pacman_si_ok(pacman: &str, name: &str) -> bool {
    Command::new(pacman)
        .arg("-Si")
        .arg("--")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn pacman_install_repo(cfg: &Config, pkgs: &[String]) -> Result<()> {
    let mut cmd = Command::new(&cfg.pacman);
    cmd.arg("-S").arg("--needed").args(pkgs);

    if !is_root() {
        cmd = with_sudo(cfg, cmd);
    }

    run_command_printing(&mut cmd, cfg.verbose)
}

/* ---------------------- AUR path ---------------------- */

fn http_client() -> Result<Client> {
    let client = Client::builder()
        .user_agent("yaorust/0.1 (+https://github.com/darko5r/yaorust)")
        .build()?;
    Ok(client)
}

fn aur_exists(client: &Client, name: &str) -> Result<bool> {
    let url = format!("{AUR_RPC}&type=info&arg[]={}", name);
    let resp = client.get(url).send()?;
    if !resp.status().is_success() {
        bail!("AUR RPC returned {}", resp.status());
    }
    let info: AurInfoResponse = resp.json()?;
    Ok(info.resultcount > 0
        && info
            .results
            .as_ref()
            .map_or(false, |v| v.iter().any(|x| x.name == name)))
}

fn download_snapshot(client: &Client, cfg: &Config, name: &str) -> Result<PathBuf> {
    let url = format!("https://aur.archlinux.org/cgit/aur.git/snapshot/{name}.tar.gz");
    let out = cfg.snapshot_cache.join(format!("{name}.tar.gz"));

    if out.exists() {
        if cfg.verbose {
            eprintln!("==> Using cached snapshot {}", out.display());
        }
        return Ok(out);
    }

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner} downloading {msg}")?.tick_chars("/|\\- "),
    );
    pb.set_message(name.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(80));

    let mut resp = client.get(&url).send()?;
    if !resp.status().is_success() {
        pb.finish_and_clear();
        bail!("download failed for {name}: {}", resp.status());
    }
    let mut tmp = tempfile::NamedTempFile::new_in(&cfg.snapshot_cache)?;
    io::copy(&mut resp, &mut tmp)?;
    tmp.persist(&out)?;
    pb.finish_and_clear();
    Ok(out)
}

fn extract_tgz(tgz_path: &Path, dest_dir: &Path) -> Result<()> {
    let status = Command::new(which("bsdtar")?)
        .arg("-xzf")
        .arg(tgz_path)
        .arg("-C")
        .arg(dest_dir)
        .status()?;
    if !status.success() {
        bail!("bsdtar failed to extract {}", tgz_path.display());
    }
    Ok(())
}

fn aur_build_install(cfg: &Config, client: &Client, name: &str, force: bool) -> Result<()> {
    // 1) Fetch & extract
    let tgz = download_snapshot(client, cfg, name)?;
    let tmp = TempDir::new()?;
    extract_tgz(&tgz, tmp.path())?;
    let build_dir = tmp.path().join(name);
    if !build_dir.is_dir() {
        bail!("unexpected snapshot layout for {name}");
    }

    // 2) Resolve exact outputs (makepkg --packagelist with PKGDEST)
    let targets = packagelist(&build_dir, &cfg.pkgdest)?;
    if targets.is_empty() {
        bail!("packagelist is empty for {name}");
    }

    // 3) Force handling
    if force {
        for t in &targets {
            let file = Path::new(t);
            if file.exists() {
                if cfg.verbose {
                    eprintln!("==> removing {}", file.display());
                }
                let _ = fs::remove_file(file);
            }
            // also remove possible artifacts in CWD of build_dir (same basename)
            if let Some(base) = Path::new(t).file_name() {
                let local = build_dir.join(base);
                if local.exists() {
                    let _ = fs::remove_file(local);
                }
            }
        }
    }

    // 4) Build (M1: run as current EUID; sandbox comes next milestone)
    let mut mk = Command::new(which("makepkg")?);
    mk.current_dir(&build_dir)
        .env("PKGDEST", &cfg.pkgdest)
        .arg("--clean")
        .arg("--cleanbuild")
        .arg("--syncdeps")
        .arg("--needed")
        .arg("--noconfirm")
        .arg("--log")
        .arg("--config")
        .arg("/etc/makepkg.conf");

    if force {
        mk.arg("-f").arg("-C");
    }

    eprintln!("==> Building {name} (makepkg)…");
    run_command_printing(&mut mk, cfg.verbose)?;

    // 5) Ensure artifacts exist (some PKGBUILDs might drop in CWD → move to PKGDEST)
    for t in &targets {
        let target = Path::new(t);
        if !target.exists() {
            if let Some(base) = target.file_name() {
                let local = build_dir.join(base);
                if local.exists() {
                    fs::rename(&local, &target)?;
                }
            }
        }
    }

    // 6) Install
    let mut pac = Command::new(&cfg.pacman);
    pac.arg("-U").arg("--noconfirm").args(&targets);

    if !is_root() {
        pac = with_sudo(cfg, pac);
    }

    eprintln!("==> Installing {}", name);
    run_command_printing(&mut pac, cfg.verbose)
}

fn packagelist(build_dir: &Path, pkgdest: &Path) -> Result<Vec<String>> {
    let output = Command::new(which("makepkg")?)
        .current_dir(build_dir)
        .env("PKGDEST", pkgdest)
        .arg("--packagelist")
        .output()?;
    if !output.status.success() {
        bail!("makepkg --packagelist failed");
    }
    let s = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    for line in s.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

/* ---------------------- Utilities ---------------------- */

fn ensure_tools(cfg: &Config) -> Result<()> {
    for bin in ["bsdtar", "makepkg", &cfg.pacman] {
        let p = which(bin)?;
        if cfg.verbose {
            eprintln!("==> using {bin} at {}", p.display());
        }
    }
    Ok(())
}

fn is_root() -> bool {
    nix_like_geteuid() == 0
}

#[cfg(target_family = "unix")]
fn nix_like_geteuid() -> u32 {
    // small dependency-free geteuid
    unsafe { libc::geteuid() }
}

#[cfg(not(target_family = "unix"))]
fn nix_like_geteuid() -> u32 {
    1 // not root on non-unix targets
}

fn with_sudo(cfg: &Config, cmd: Command) -> Command {
    // Build a new `sudo ...` command that runs the original program + args
    let prog = cmd.get_program().to_os_string();
    let args: Vec<_> = cmd.get_args().map(|s| s.to_os_string()).collect();

    let mut sc = Command::new(&cfg.sudo);
    sc.arg(prog);
    sc.args(args);
    sc
}

fn run_command_printing(cmd: &mut Command, verbose: bool) -> Result<()> {
    if verbose {
        eprintln!("$ {}", pretty_cmd(cmd));
    }
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;

    // simple tee: forward both stdout/stderr
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let t1 = std::thread::spawn(move || io::copy(&mut out, &mut io::stdout()).ok());
    let t2 = std::thread::spawn(move || io::copy(&mut err, &mut io::stderr()).ok());

    let status = child.wait()?;
    let _ = t1.join();
    let _ = t2.join();

    // make sure we actually use `Write` by flushing explicitly
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();

    if !status.success() {
        bail!("command failed with status {status}");
    }
    Ok(())
}

fn pretty_cmd(cmd: &Command) -> String {
    let prog = cmd.get_program().to_string_lossy().to_string();
    let args = cmd
        .get_args()
        .map(|a| shell_escape(a))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{prog} {args}")
}

fn shell_escape<S: AsRef<OsStr>>(s: S) -> String {
    let s = s.as_ref().to_string_lossy();
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./=:".contains(c))
    {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}
