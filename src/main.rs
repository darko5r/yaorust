/// todo next PKGBUILD view after closing
use anyhow::{bail, Result};
use clap::{ArgAction, Parser};
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

/// yaourt-style front-end: `yao -S foo`, `yao -G foo`
#[derive(Parser, Debug)]
#[command(
    name = "yao",
    version,
    about = "Fast minimal AUR + repo helper (yaourt-style flags)"
)]
struct Cli {
    /// Sync/install (repo or AUR), like pacman -S
    #[arg(short = 'S', action = ArgAction::SetTrue)]
    sync: bool,

    /// Get PKGBUILD snapshot(s) into ./<pkg>/, like yaourt -G
    #[arg(short = 'G', action = ArgAction::SetTrue)]
    get: bool,

    /// Force rebuild/overwrite (passed to makepkg)
    #[arg(short = 'f', long, action = ArgAction::SetTrue)]
    force: bool,

    /// Verbose logging (print executed commands & config)
    #[arg(short, long, action = ArgAction::SetTrue)]
    verbose: bool,

    /// Package names (for -S or -G)
    pkgs: Vec<String>,
}

/// Root-mode behavior (future hook for sandbox/user mapping)
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

/// Runtime config from env + defaults
#[derive(Clone, Debug)]
struct Config {
    /// Where makepkg will place built packages
    pkgdest: PathBuf,
    /// How to behave when running as root (future)
    root_mode: RootMode,
    /// Auto-enable patched makepkg for root mode (future)
    auto_trust_root: bool,
    /// Build user for "user" mode (future)
    build_user: String,
    /// Snapshot cache dir for AUR tarballs
    snapshot_cache: PathBuf,
    /// Pacman binary name/path
    pacman: String,
    /// Sudo binary name/path
    sudo: String,
    /// Verbose logging
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

        let build_user =
            env::var("YAORUST_BUILD_USER").unwrap_or_else(|_| "nobody".to_string());
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
    // other fields not needed yet
}

/* ---------------------- Package kind ---------------------- */

#[derive(Debug, Clone, Copy)]
enum PkgKind {
    Repo,
    Aur,
}

/* ---------------------- Entry ---------------------- */

fn main() -> Result<()> {
    let cli = Cli::parse();

    if !cli.sync && !cli.get {
        bail!("you must specify either -S (sync) or -G (get PKGBUILD)");
    }

    let cfg = Config::load(cli.verbose)?;

    // Ensure required external tools
    ensure_tools(&cfg)?;

    // Create caches/dirs up-front
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

    if cli.get {
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

    let mut repo_pkgs: Vec<String> = Vec::new();
    let mut aur_pkgs: Vec<String> = Vec::new();

    // Also record installed status so we can print a warning like pacman
    struct PlanItem {
        name: String,
        kind: PkgKind,
        installed: bool,
    }

    let mut plan: Vec<PlanItem> = Vec::new();

    for p in &pkgs {
        let kind = classify_pkg(cfg, &client, p)?;
        let installed = pacman_is_installed(&cfg.pacman, p);

        match kind {
            PkgKind::Repo => repo_pkgs.push(p.clone()),
            PkgKind::Aur => aur_pkgs.push(p.clone()),
        }

        plan.push(PlanItem {
            name: p.clone(),
            kind,
            installed,
        });
    }

    if repo_pkgs.is_empty() && aur_pkgs.is_empty() {
        bail!("no packages found in repos or AUR");
    }

    // PURE REPO: delegate fully to pacman -S
    if aur_pkgs.is_empty() {
        eprintln!("==> [repo] delegating to pacman -S");
        return pacman_install_repo(cfg, &repo_pkgs);
    }

    // AUR present (maybe mixed with repo): show a simple plan, including
    // "warning: foo is up to date -- reinstalling" when already installed.
    eprintln!(":: Packages to process:");
    for item in &plan {
        let source = match item.kind {
            PkgKind::Repo => "repo",
            PkgKind::Aur => "AUR",
        };
        eprintln!("   {} ({})", item.name, source);
        if item.installed {
            eprintln!(
                "      warning: {} is up to date -- reinstalling",
                item.name
            );
        }
    }

    if !prompt_yes_no(":: Proceed with installation? [Y/n] ")? {
        eprintln!(":: Aborted by user.");
        return Ok(());
    }

    // 1) Handle repo pkgs first via pacman -S (full pacman output + prompt)
    if !repo_pkgs.is_empty() {
        pacman_install_repo(cfg, &repo_pkgs)?;
    }

    // 2) Then handle AUR packages one by one
    for p in aur_pkgs {
        eprintln!("==> [aur] building {p}");
        aur_build_install(cfg, &client, &p, force)?;
    }

    Ok(())
}

/* ---------------------- Classify ---------------------- */

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

fn pacman_is_installed(pacman: &str, name: &str) -> bool {
    Command::new(pacman)
        .arg("-Qi")
        .arg("--")
        .arg(name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Call pacman -S for repo packages, let pacman show all info + its own [Y/n] prompt.
fn pacman_install_repo(cfg: &Config, pkgs: &[String]) -> Result<()> {
    let mut cmd = Command::new(&cfg.pacman);
    cmd.arg("-S")
        // no --needed here; behave like plain pacman (allow reinstall)
        .args(pkgs);

    if !is_root() {
        cmd = with_sudo(cfg, cmd);
    }

    // treat "n" -> exit code 1 as "Aborted by user."
    run_command_printing_abort_ok(&mut cmd, cfg.verbose)
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
        ProgressStyle::with_template("{spinner} downloading {msg}")?
            .tick_chars("/|\\- "),
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

    // 1.5) Optional PKGBUILD review/edit
    let pkgbuild = build_dir.join("PKGBUILD");
    if pkgbuild.is_file() {
        if prompt_yes_no(":: View PKGBUILD? [Y/n] ")? {
            let editor = choose_editor()?;
            eprintln!("==> Opening PKGBUILD with {}", editor);
            let status = Command::new(&editor).arg(&pkgbuild).status()?;
            if !status.success() {
                eprintln!(":: Aborted by user (editor).");
                return Ok(());
            }
        }
    } else if cfg.verbose {
        eprintln!("==> No PKGBUILD found in {}", build_dir.display());
    }

    // 2) Resolve exact outputs (makepkg --packagelist with PKGDEST)
    let targets = packagelist(&build_dir, &cfg.pkgdest)?;
    if targets.is_empty() {
        bail!("packagelist is empty for {name}");
    }

    // 3) Force handling (remove previous artifacts when -f)
    if force {
        for t in &targets {
            let file = Path::new(t);
            if file.exists() {
                if cfg.verbose {
                    eprintln!("==> removing {}", file.display());
                }
                let _ = fs::remove_file(file);
            }
            let local = build_dir.join(
                Path::new(t)
                    .file_name()
                    .expect("package filename should exist"),
            );
            if local.exists() {
                let _ = fs::remove_file(local);
            }
        }
    }

    // If all target files already exist and NOT forcing, skip rebuild
    let all_exist = targets.iter().all(|t| Path::new(t).exists());
    if !force && all_exist {
        if cfg.verbose {
            eprintln!(
                "==> Using existing package file(s) for {name}, skipping rebuild"
            );
        }
    } else {
        // 4) Build with makepkg (as current EUID; root-safe modes come later)
        let mut mk = Command::new(which("makepkg")?);
        mk.current_dir(&build_dir)
            .env("PKGDEST", &cfg.pkgdest)
            .arg("--clean")
            .arg("--cleanbuild")
            .arg("--syncdeps")
            .arg("--needed")
            .arg("--log")
            .arg("--config")
            .arg("/etc/makepkg.conf");

        if force {
            mk.arg("-f").arg("-C");
        }

        eprintln!("==> Building {name} (makepkg)...");
        run_command_printing(&mut mk, cfg.verbose)?;

        // 5) Ensure artifacts exist (some PKGBUILDs might drop in CWD → move to PKGDEST)
        for t in &targets {
            let target = Path::new(t);
            if !target.exists() {
                let local = build_dir.join(
                    target
                        .file_name()
                        .expect("package filename should exist"),
                );
                if local.exists() {
                    fs::rename(&local, &target)?;
                }
            }
        }
    }

    // 6) Install via pacman -U (no --noconfirm: let pacman show details + prompt)
    let mut pac = Command::new(&cfg.pacman);
    pac.arg("-U").args(&targets);

    if !is_root() {
        pac = with_sudo(cfg, pac);
    }

    eprintln!("==> Installing {}", name);
    // use the same "Aborted by user" logic here when user presses 'n'
    run_command_printing_abort_ok(&mut pac, cfg.verbose)
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
    unsafe { libc::geteuid() }
}

#[cfg(not(target_family = "unix"))]
fn nix_like_geteuid() -> u32 {
    1
}

/// Simple [Y/n] prompt on stdin.
fn prompt_yes_no(prompt: &str) -> Result<bool> {
    let mut stdout = io::stdout();
    write!(stdout, "{prompt}")?;
    stdout.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_lowercase();
    if answer.is_empty() || answer == "y" || answer == "yes" {
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Pick editor for PKGBUILD:
/// YAORUST_EDITOR > VISUAL > EDITOR > interactive with default "nano".
fn choose_editor() -> Result<String> {
    if let Ok(e) = env::var("YAORUST_EDITOR") {
        if !e.trim().is_empty() {
            return Ok(e);
        }
    }
    if let Ok(e) = env::var("VISUAL") {
        if !e.trim().is_empty() {
            return Ok(e);
        }
    }
    if let Ok(e) = env::var("EDITOR") {
        if !e.trim().is_empty() {
            return Ok(e);
        }
    }

    let default = "nano";
    let mut stdout = io::stdout();
    write!(stdout, ":: Editor to use for PKGBUILD [{}]: ", default)?;
    stdout.flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let choice = input.trim();
    let ed = if choice.is_empty() {
        default.to_string()
    } else {
        choice.to_string()
    };
    Ok(ed)
}

fn with_sudo(cfg: &Config, cmd: Command) -> Command {
    let prog = cmd.get_program().to_os_string();
    let args: Vec<_> = cmd.get_args().map(|s| s.to_os_string()).collect();

    let mut sc = Command::new(&cfg.sudo);
    sc.arg(prog);
    sc.args(args);
    sc
}

/// Generic runner: any non-zero status is treated as an error.
fn run_command_printing(cmd: &mut Command, verbose: bool) -> Result<()> {
    if verbose {
        eprintln!("$ {}", pretty_cmd(cmd));
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();

    let t1 = std::thread::spawn(move || {
        io::copy(&mut out, &mut stdout).ok();
    });
    let t2 = std::thread::spawn(move || {
        io::copy(&mut err, &mut stderr).ok();
    });

    let status = child.wait()?;
    let _ = t1.join();
    let _ = t2.join();

    if !status.success() {
        bail!("command failed with status {status}");
    }
    Ok(())
}

/// Variant used for pacman calls: exit code 1 is treated as "Aborted by user."
fn run_command_printing_abort_ok(cmd: &mut Command, verbose: bool) -> Result<()> {
    if verbose {
        eprintln!("$ {}", pretty_cmd(cmd));
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();

    let t1 = std::thread::spawn(move || {
        io::copy(&mut out, &mut stdout).ok();
    });
    let t2 = std::thread::spawn(move || {
        io::copy(&mut err, &mut stderr).ok();
    });

    let status = child.wait()?;
    let _ = t1.join();
    let _ = t2.join();

    if !status.success() {
        if let Some(1) = status.code() {
            eprintln!(":: Aborted by user.");
            return Ok(());
        }
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
    if s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./=:".contains(c))
    {
        s.into_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}
