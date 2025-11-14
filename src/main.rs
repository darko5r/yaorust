use anyhow::{bail, Result};
use clap::{ArgAction, CommandFactory, Parser};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use serde::Deserialize;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use which::which;

const AUR_RPC: &str = "https://aur.archlinux.org/rpc/?v=5";

/* ---------------------- CLI: pacman/yaourt style ---------------------- */

#[derive(Parser, Debug)]
#[command(
    name = "yao",
    version,
    about = "Fast minimal AUR + repo helper (yaourt-style flags)"
)]
struct Cli {
    /// Sync/install (repo or AUR), like pacman -S
    #[arg(short = 'S', action = ArgAction::SetTrue, conflicts_with = "getpkgbuild")]
    sync: bool,

    /// Get PKGBUILD snapshot(s) into ./<pkg>/, like yaourt -G
    #[arg(short = 'G', action = ArgAction::SetTrue, conflicts_with = "sync")]
    getpkgbuild: bool,

    /// Force rebuild/overwrite existing packages (passed to makepkg)
    #[arg(short = 'f', long, action = ArgAction::SetTrue)]
    force: bool,

    /// Verbose logging (print executed commands & config)
    #[arg(short = 'v', long, action = ArgAction::SetTrue)]
    verbose: bool,

    /// Package names (for -S or -G)
    pkgs: Vec<String>,
}

/* ---------------------- Root-mode enum (future use) ---------------------- */

#[derive(Clone, Copy, Debug)]
enum RootMode {
    Auto,
    Sandbox,
    User,
    TrustRoot,
}

impl RootMode {
    fn from_env() -> Self {
        match env::var("YAORUST_ROOT_MODE").unwrap_or_else(|_| "auto".into()).as_str() {
            "sandbox" => RootMode::Sandbox,
            "user" => RootMode::User,
            "trust-root" => RootMode::TrustRoot,
            _ => RootMode::Auto,
        }
    }
}

/* ---------------------- Runtime config ---------------------- */

#[derive(Clone, Debug)]
struct Config {
    pkgdest: PathBuf,
    root_mode: RootMode,
    auto_trust_root: bool,
    build_user: String,
    snapshot_cache: PathBuf,
    pacman: String,
    sudo: String,
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
    ty: String,
    #[serde(default)]
    version: Option<i32>,
    resultcount: i32,
    results: Option<Vec<AurPkg>>,
}

#[derive(Deserialize, Debug)]
struct AurPkg {
    #[serde(rename = "Name")]
    name: String,
    // (We ignore the rest for now)
}

/* ---------------------- Entry ---------------------- */

fn main() -> Result<()> {
    let cli = Cli::parse();

    // If neither -S nor -G were provided, show help (pacman-like UX)
    if !cli.sync && !cli.getpkgbuild {
        Cli::command().print_help()?;
        eprintln!();
        return Ok(());
    }

    let cfg = Config::load(cli.verbose)?;
    ensure_tools(&cfg)?;
    fs::create_dir_all(&cfg.pkgdest)?;
    fs::create_dir_all(&cfg.snapshot_cache)?;

    if cfg.verbose {
        eprintln!(
            "==> config: PKGDEST={}, snapshot_cache={}, pacman={}, sudo={}, root_mode={:?}, auto_trust_root={}, build_user={}, euid={}",
            cfg.pkgdest.display(),
            cfg.snapshot_cache.display(),
            cfg.pacman,
            cfg.sudo,
            cfg.root_mode,
            cfg.auto_trust_root,
            cfg.build_user,
            nix_like_geteuid()
        );
    }

    if cli.getpkgbuild {
        cmd_getpkgbuild(&cfg, cli.pkgs)
    } else {
        cmd_sync(&cfg, cli.pkgs, cli.force)
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

    // 1) Build a plan: (name, kind)
    let mut plan: Vec<(String, PkgKind)> = Vec::new();
    for name in pkgs {
        let kind = classify_pkg(cfg, &client, &name)?;
        plan.push((name, kind));
    }

    // 2) Show summary
    eprintln!(":: Packages to process:");
    for (name, kind) in &plan {
        let k = match kind {
            PkgKind::Repo => "repo",
            PkgKind::Aur => "AUR",
        };
        eprintln!("   {name} ({k})");
    }

    // 3) Ask for confirmation
    if !ask_yes_no(":: Proceed with installation?") {
        eprintln!(":: Aborted by user.");
        return Ok(());
    }

    // 4) Execute plan
    for (name, kind) in plan {
        match kind {
            PkgKind::Repo => {
                eprintln!("==> [repo] installing {name}");
                pacman_install_repo(cfg, &[name])?;
            }
            PkgKind::Aur => {
                eprintln!("==> [aur] building {name}");
                aur_build_install(cfg, &client, &name, force)?;
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
        .user_agent("yao/0.1 (+https://github.com/darko5r/yaorust)")
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
    Ok(info.resultcount > 0 && info.results.as_ref().map_or(false, |v| v.iter().any(|x| x.name == name)))
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
    pb.set_style(ProgressStyle::with_template("{spinner} downloading {msg}")?.tick_chars("/|\\- "));
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

    // 2) Resolve outputs (respects PKGDEST)
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
            let local = build_dir.join(Path::new(t).file_name().unwrap());
            if local.exists() {
                let _ = fs::remove_file(local);
            }
        }
    }

    // 4) Build
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

    // 5) Ensure artifacts exist (move from CWD if needed)
   for t in &targets {
    let target = Path::new(t);
    if !target.exists() {
        let local = build_dir.join(target.file_name().unwrap());
        if local.exists() {
            fs::rename(&local, &target)?;
        }
    }
}

// NEW: filter to only existing files
let mut install_targets = Vec::new();
for t in &targets {
    let p = Path::new(t);
    if p.exists() {
        install_targets.push(t.clone());
    } else if cfg.verbose {
        eprintln!(
            "==> warning: expected package {} not found, skipping",
            p.display()
        );
    }
}

if install_targets.is_empty() {
    bail!("no packages produced for {name}");
}

        // 6) Filter to actually existing artifacts (debug packages may be skipped)
    let existing: Vec<String> = targets
        .iter()
        .filter(|t| Path::new(t).exists())
        .cloned()
        .collect();

    if existing.is_empty() {
        bail!("no built package artifacts found for {name} in {}", cfg.pkgdest.display());
    }

    if cfg.verbose {
        eprintln!("==> install targets:");
        for t in &existing {
            eprintln!("    {}", t);
        }
    }

    // 7) Install
   let mut pac = Command::new(&cfg.pacman);

// no --noconfirm → pacman will prompt like normal
pac.arg("-U").args(&install_targets);

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

/* ---------------------- yes/no prompt ---------------------- */

fn ask_yes_no(prompt: &str) -> bool {
    use std::io::{self, Write};

    let mut input = String::new();
    loop {
        eprint!("{prompt} [Y/n] ");
        let _ = io::stderr().flush();

        input.clear();
        if io::stdin().read_line(&mut input).is_err() {
            // On IO error, be conservative: treat as "no"
            return false;
        }

        let t = input.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("y") || t.eq_ignore_ascii_case("yes") {
            return true;
        }
        if t.eq_ignore_ascii_case("n") || t.eq_ignore_ascii_case("no") {
            return false;
        }

        eprintln!("Please answer y or n.");
    }
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
    unsafe { libc::geteuid() }
}

#[cfg(not(target_family = "unix"))]
fn nix_like_geteuid() -> u32 {
    1
}

fn with_sudo(cfg: &Config, cmd: Command) -> Command {
    // Rebuild cmd as: sudo <prog> <args...>
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

    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let t1 = std::thread::spawn(move || io::copy(&mut out, &mut io::stdout()).ok());
    let t2 = std::thread::spawn(move || io::copy(&mut err, &mut io::stderr()).ok());

    let status = child.wait()?;
    let _ = t1.join();
    let _ = t2.join();

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
