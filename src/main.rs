use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write as IoWrite};
use std::net::{TcpListener, UdpSocket};
use std::path::Path;
use std::process::{Command, Stdio, exit};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ── ANSI ─────────────────────────────────────────────────────────────────────
const R: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const MAGENTA: &str = "\x1b[35m";

// ── Config ────────────────────────────────────────────────────────────────────
const VERSION: &str = "2.3.0";
// Fallback used only if /etc/spk-repo-conf.yaml is missing or fails to parse
// (e.g. a system that predates this file, or one where it was deleted).
const DEFAULT_RELEASE_URL: &str =
    "https://github.com/Smech-Labs/SmechDeploy/releases/download/v1.0.0-packages";
const REPO_CONF_PATH: &str = "/etc/spk-repo-conf.yaml";
const DOCS_URL: &str = "https://docs.smech.xyz";
// One text file per installed package -- see InstalledPkg below for format.
const INSTALLED_DB_DIR: &str = "/var/lib/spk/installed";

// ── Repo config ───────────────────────────────────────────────────────────────
// /etc/spk-repo-conf.yaml lets an operator point spk at one or more package
// repos without hardcoding a URL into the binary. Deliberately hand-parsed
// rather than pulling in a YAML crate -- spk is a zero-dependency binary by
// design (see Cargo.toml), and the schema this file actually needs is small
// and fixed enough that a general-purpose parser isn't worth that tradeoff.
//
// Priority: 1 is tried first, up to 99 for ordinary lower-priority mirrors.
// 100 is a distinct "insecure/untrusted" tier, not just "even lower
// priority" -- those repos are only ever used as a last resort after every
// other configured repo has failed, and only with a loud warning printed
// first, since silently falling back to an operator-marked-untrusted source
// is exactly the kind of thing CONTRIBUTING.md's security section warns
// against normalizing.
struct Repo {
    name: String,
    url: String,
    priority: u8,
}

fn default_repos() -> Vec<Repo> {
    vec![Repo {
        name: "default".to_string(),
        url: DEFAULT_RELEASE_URL.to_string(),
        priority: 1,
    }]
}

fn parse_repo_conf(text: &str) -> Vec<Repo> {
    let mut repos = Vec::new();
    let mut cur_name: Option<String> = None;
    let mut cur_url: Option<String> = None;
    let mut cur_priority: Option<u8> = None;
    let mut in_repos_list = false;

    fn flush(
        repos: &mut Vec<Repo>,
        name: &mut Option<String>,
        url: &mut Option<String>,
        priority: &mut Option<u8>,
    ) {
        if let (Some(n), Some(u)) = (name.take(), url.take()) {
            repos.push(Repo {
                name: n,
                url: u,
                priority: priority.take().unwrap_or(50),
            });
        } else {
            *priority = None;
        }
    }

    fn unquote(s: &str) -> String {
        let s = s.trim();
        let s = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s);
        let s = s.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).unwrap_or(s);
        s.to_string()
    }

    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("");
        if line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim_start();

        if !in_repos_list {
            if trimmed.starts_with("repos:") {
                in_repos_list = true;
            }
            continue;
        }

        if trimmed.starts_with("- ") || trimmed == "-" {
            flush(&mut repos, &mut cur_name, &mut cur_url, &mut cur_priority);
            let rest = trimmed.trim_start_matches('-').trim_start();
            if let Some((k, v)) = rest.split_once(':') {
                match k.trim() {
                    "name" => cur_name = Some(unquote(v)),
                    "url" => cur_url = Some(unquote(v)),
                    "priority" => cur_priority = v.trim().parse().ok(),
                    _ => {}
                }
            }
            continue;
        }

        if let Some((k, v)) = trimmed.split_once(':') {
            match k.trim() {
                "name" => cur_name = Some(unquote(v)),
                "url" => cur_url = Some(unquote(v)),
                "priority" => cur_priority = v.trim().parse().ok(),
                _ => {}
            }
        }
    }
    flush(&mut repos, &mut cur_name, &mut cur_url, &mut cur_priority);
    repos
}

fn load_repos() -> Vec<Repo> {
    match fs::read_to_string(REPO_CONF_PATH) {
        Ok(text) => {
            let mut repos = parse_repo_conf(&text);
            if repos.is_empty() {
                println!(
                    "{YELLOW}[spk] {REPO_CONF_PATH} has no usable repos, using built-in default.{R}"
                );
                repos = default_repos();
            }
            repos.sort_by_key(|r| r.priority);
            repos
        }
        Err(_) => default_repos(),
    }
}

/// Tries every configured repo in priority order (1 first), skipping the
/// insecure/untrusted tier (priority 100) unless nothing else worked.
/// Returns true and leaves the package at `dest_tmp` on the first success.
/// A repo's `url` can be `file:///abs/path` (see `local-package-repo`) as
/// well as http(s) -- curl handles both transparently, so this needed no
/// new fetch logic of its own.
fn fetch_package_ext(pkg: &str, ext: &str, dest_tmp: &str) -> bool {
    let repos = load_repos();
    let (trusted, untrusted): (Vec<_>, Vec<_>) = repos.iter().partition(|r| r.priority < 100);

    for repo in trusted.iter().chain(untrusted.iter()) {
        if repo.priority >= 100 {
            println!(
                "{RED}{BOLD}[spk] WARNING: falling back to untrusted repo '{}' ({}) -- \
                 every configured trusted repo failed.{R}",
                repo.name, repo.url
            );
        }
        let url = format!("{}/{pkg}.{ext}", repo.url.trim_end_matches('/'));
        let ok = Command::new("curl")
            .args(["-sfL", "-o", dest_tmp, &url])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return true;
        }
        let _ = fs::remove_file(dest_tmp);
    }
    false
}

/// Legacy name/signature preserved for cmd_deploy_to, which always wants
/// the raw .tar.xz bytes to stream verbatim over the SmechVisor deploy
/// wire protocol -- that path predates .spkg and isn't part of this
/// migration.
fn fetch_package(pkg: &str, dest_tmp: &str) -> bool {
    fetch_package_ext(pkg, "tar.xz", dest_tmp)
}

// Packages currently published in the release above. Used by system-upgrade to
// know what to re-fetch; install can fetch any package name (unknown ones just
// produce an HTTP 404 from curl).
//
// Packages are served from a GitHub Release rather than a REST file-content
// API. GitHub Releases serves raw files with no size limit; the GerritHub REST
// API was silently truncating large .tar.xz files.
const SMECHOS_PACKAGES: &[&str] = &[
    "base-system",
    "kernel-modules",
    "firmware",
    "bootloader-grub",
    "kde-frameworks",
    "plasma",
    "qt6",
    "mesa-graphics",
    "plasma-discover",
    "packagekit-spk",
];

const SMECHVISOR_PACKAGES: &[(&str, bool)] = &[
    ("smechvisor-daemon", false), // live-swappable, no reboot needed
    ("smechvisor-base", true),    // kernel/base: requires reboot
];

// Deploy shim wire protocol
const SHIM_UDP_PORT: u16 = 9191;
const SHIM_TCP_PORT: u16 = 9192;
const BROADCAST_MAGIC: &str = "SMECHVISOR_SHIM";

// ── Banner / help ─────────────────────────────────────────────────────────────

fn print_banner() {
    println!("{BOLD}{MAGENTA}========================================================================{R}");
    println!("{BOLD}{RED}     ███████╗██████╗ ██╗  ██╗    ███████╗ ██████╗ ██╗  ██╗{R}");
    println!("{BOLD}{RED}     ██╔════╝██╔══██╗██║ ██╔╝    ██╔════╝██╔═══██╗██║ ██╔╝{R}");
    println!("{BOLD}{RED}     ███████╗██████╔╝█████╔╝     ███████╗██║   ██║█████╔╝ {R}");
    println!("{BOLD}{RED}     ╚════██║██╔═══╝ ██╔═██╗     ╚════██║██║   ██║██╔═██╗ {R}");
    println!("{BOLD}{RED}     ███████║██║     ██║  ██╗    ███████║╚██████╔╝██║  ██╗{R}");
    println!("{BOLD}{CYAN}          SMECH SOVEREIGN PACKAGE KEEPER  v{VERSION}{R}");
    println!("{BOLD}{MAGENTA}========================================================================{R}");
}

fn print_help() {
    print_banner();
    println!("{BOLD}USAGE:{R}  spk <COMMAND> [args...]");
    println!();
    println!("{BOLD}PACKAGE MANAGEMENT{R}");
    println!("    {GREEN}install <pkg>{R}              Fetch and install a package from a repo");
    println!("    {GREEN}install --local-package <path>{R}  Install a local .spkg file (the only");
    println!("                                     way to install from disk -- a bare path");
    println!("                                     to 'install' is rejected on purpose)");
    println!("    {GREEN}local-package-repo <folder>{R}  Point 'install <name>' at a local folder");
    println!("                                     of .spkg files too (priority 1)");
    println!("    {GREEN}remove <pkg> [-y] [--force]{R}  Uninstall a package (checks reverse deps");
    println!("                                     and never deletes a file another");
    println!("                                     installed package also owns)");
    println!("    {GREEN}list{R}                       Show all installed packages");
    println!("    {GREEN}depends <pkg>{R}              Show a package's dependency tree");
    println!("    {GREEN}system-upgrade{R}             Re-fetch and reinstall all known packages");
    println!();
    println!("{BOLD}BUILD (native orchestration via spk-compile.py){R}");
    println!("    {GREEN}compile smechos{R}            Full SmechOS build");
    println!("    {GREEN}compile smechvisor{R}         Full SmechVisor build");
    println!("    {GREEN}compile smechos  --phase <p>{R}  Single SmechOS phase");
    println!("    {GREEN}compile smechvisor --phase <p>{R} Single SmechVisor phase");
    println!("    {GREEN}compile iso smechos{R}        SmechOS install ISO");
    println!("    {GREEN}compile iso smechvisor{R}     SmechVisor install ISO");
    println!("    {GREEN}compile iso shim{R}           SmechVisor deploy shim ISO");
    println!("    {GREEN}compile --list smechos{R}     List SmechOS build phases");
    println!("    {GREEN}compile --list smechvisor{R}  List SmechVisor build phases");
    println!();
    println!("{BOLD}SMECHVISOR NETWORK DEPLOY{R}");
    println!("    {GREEN}deploy-system-img-copy <code>{R}  Push SmechVisor to a shim node");
    println!("    {GREEN}receive-deploy{R}                 Receive a SmechVisor deploy (shim mode)");
    println!();
    println!("{BOLD}PACKAGEKIT BACKEND (called by PackageKit daemon -- not for manual use){R}");
    println!("    {GREEN}packagekit-backend{R}         Speak PackageKit script protocol on stdin/stdout");
    println!();
    println!("{BOLD}OTHER{R}");
    println!("    {GREEN}version{R}                    Print version");
    println!("    {GREEN}about{R}                      Workstation specs + credits");
    println!("    {GREEN}help{R}                       This help");
    println!();
    println!("  Docs: {CYAN}{DOCS_URL}{R}");
    println!();
}

fn print_about() {
    print_banner();
    println!("{BOLD}SPK -- Smech Sovereign Package Keeper{R}");
    println!("  Version:    {VERSION}");
    println!("  License:    MIT");
    println!("  Homepage:   {CYAN}{DOCS_URL}/smechos.html#spk{R}");
    println!("  Source:     {CYAN}https://github.com/Smech-Labs/spk{R}");
    println!();
    println!("{BOLD}Developed by{R}");
    println!("  Smech Labs -- https://labs.smech.xyz");
    println!("  Lead:  Smech");
    println!("  Co-dev: Gemini (Google DeepMind)");
    println!("  Co-dev: Claude (Anthropic)");
    println!("  First release: 2026");
    println!();
    println!("{BOLD}About{R}");
    println!("  Unified sovereign package keeper for SmechOS and SmechVisor.");
    println!("  Zero external crate dependencies -- pure Rust std.");
    println!("  PackageKit backend included: works with Plasma Discover and");
    println!("  any other PackageKit-aware frontend out of the box.");
    println!("  Build orchestration via spk-compile (Project SmechDeployV2).");
    println!();
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

fn target_root() -> &'static str {
    if Path::new("/mnt/smechos").exists() {
        "/mnt/smechos"
    } else {
        "/"
    }
}

fn is_smechvisor() -> bool {
    Path::new("/usr/bin/smechvisord").exists()
        || Path::new("/etc/smechvisor-release").exists()
}

// ── .spkg format ──────────────────────────────────────────────────────────────
//
// A .spkg is a plain (uncompressed) outer tar -- no point double-compressing
// what's already xz'd inside -- containing exactly two members, deb-style:
//
//   control.tar.xz   metadata (a `control` key:value file) + optional
//                     preinst/postinst/prerm/postrm executable scripts
//   data.tar.xz       payload, paths relative to the install root
//
// Kept implementable with zero new crates: the outer container and both
// inner members are handled with the same `tar` shell-out already used for
// legacy .tar.xz packages, and `control` is hand-parsed with the same
// "key: value, # comments stripped" scanner style already used for
// spk-repo-conf.yaml (see parse_repo_conf above).

struct ControlMeta {
    name: String,
    version: String,
    architecture: String,
    depends: String,
    description: String,
}

fn parse_control(text: &str) -> ControlMeta {
    let mut m = ControlMeta {
        name: String::new(),
        version: String::new(),
        architecture: String::new(),
        depends: String::new(),
        description: String::new(),
    };
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("");
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim().to_string();
            match k.trim() {
                "name" => m.name = v,
                "version" => m.version = v,
                "architecture" => m.architecture = v,
                "depends" => m.depends = v,
                "description" => m.description = v,
                _ => {}
            }
        }
    }
    m
}

fn run_hook_script(control_dir: &str, name: &str) {
    let path = format!("{control_dir}/{name}");
    if !Path::new(&path).exists() {
        return;
    }
    let _ = Command::new("chmod").args(["+x", &path]).status();
    match Command::new("sh").arg(&path).status() {
        Ok(s) if s.success() => {}
        _ => println!("{YELLOW}[spk] Warning: {name} script did not exit cleanly.{R}"),
    }
}

/// Install a .spkg file already sitting on local disk into `root`. Used by
/// both the repo-fetch path (fetch_and_install downloads to a temp file,
/// then calls this) and `spk install --local-package` (calls this
/// directly on the caller-given path, no fetch involved).
fn install_spkg_file(spkg_path: &str, root: &str) -> bool {
    install_spkg_file_inner(spkg_path, root, &mut Vec::new())
}

fn install_spkg_file_inner(spkg_path: &str, root: &str, resolving: &mut Vec<String>) -> bool {
    // Keyed by more than just the process ID: dependency resolution calls
    // this function recursively WITHIN the same process (installing a
    // package's dependency mid-install of that package), and a
    // process-ID-only path collided between the outer and inner call --
    // confirmed the hard way: the inner call's cleanup deleted the outer
    // call's still-in-use staging dir out from under it. A monotonic
    // per-call counter guarantees every nested call gets its own directory
    // regardless of how deep the recursion goes.
    static STAGE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let stage_id = STAGE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let stage = format!("/tmp/spk-spkg-stage-{}-{stage_id}", std::process::id());
    let _ = fs::remove_dir_all(&stage);
    if fs::create_dir_all(&stage).is_err() {
        println!("{RED}[spk] Failed to create staging dir {stage}.{R}");
        return false;
    }
    let outer_ok = Command::new("tar")
        .args(["-xf", spkg_path, "-C", &stage])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !outer_ok {
        println!("{RED}[spk] Failed to open {spkg_path} (not a valid .spkg?).{R}");
        let _ = fs::remove_dir_all(&stage);
        return false;
    }

    let control_tar = format!("{stage}/control.tar.xz");
    let data_tar = format!("{stage}/data.tar.xz");
    if !Path::new(&control_tar).exists() || !Path::new(&data_tar).exists() {
        println!("{RED}[spk] {spkg_path} is missing control.tar.xz or data.tar.xz -- not a valid .spkg.{R}");
        let _ = fs::remove_dir_all(&stage);
        return false;
    }

    let control_dir = format!("{stage}/control");
    let _ = fs::create_dir_all(&control_dir);
    let control_ok = Command::new("tar")
        .args(["-xf", &control_tar, "-C", &control_dir])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !control_ok {
        println!("{RED}[spk] Failed to extract control.tar.xz from {spkg_path}.{R}");
        let _ = fs::remove_dir_all(&stage);
        return false;
    }

    let control_text =
        fs::read_to_string(format!("{control_dir}/control")).unwrap_or_default();
    let meta = parse_control(&control_text);
    if meta.name.is_empty() {
        println!("{RED}[spk] {spkg_path}'s control file has no 'name' field -- refusing to install.{R}");
        let _ = fs::remove_dir_all(&stage);
        return false;
    }

    println!("{BOLD}[spk] {} {} ({}){R}", meta.name, meta.version, meta.architecture);
    if !meta.description.is_empty() {
        println!("  {}", meta.description);
    }
    if !meta.depends.is_empty() {
        println!("  {YELLOW}depends:{R} {}", meta.depends);
        if !resolve_dependencies(&meta.depends, root, resolving) {
            let _ = fs::remove_dir_all(&stage);
            return false;
        }
    }

    if is_installed(&meta.name) {
        println!("{YELLOW}[spk] {} is already installed -- reinstalling/upgrading over it.{R}", meta.name);
    }

    run_hook_script(&control_dir, "preinst");

    let _ = fs::create_dir_all(root);
    println!("{CYAN}[spk] Extracting {} into {root}...{R}", meta.name);
    let extract = format!("tar -xf '{data_tar}' -C '{root}'");
    let result = if is_root() {
        Command::new("sh").arg("-c").arg(&extract).status()
    } else {
        Command::new("sudo")
            .args(["-S", "sh", "-c", &extract])
            .stdin(Stdio::inherit())
            .status()
    };

    let ok = matches!(result, Ok(s) if s.success());
    if ok {
        run_hook_script(&control_dir, "postinst");
        // The archive's own member list is the precise, authoritative
        // record of what this package just put on disk -- read it back
        // from data.tar.xz rather than re-deriving it any other way, so
        // `remove` later knows exactly (and only) what to delete.
        let files: Vec<String> = Command::new("tar")
            .args(["-tf", &data_tar])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
            .lines()
            // GNU tar lists directory members with a trailing '/' -- these
            // are excluded outright, not just stripped of the slash:
            // directories (usr/, usr/lib/, ...) are near-universally
            // shared across packages, and `remove`'s file-deletion logic
            // does a plain `rm -f`, which errors on a directory anyway.
            // A package uninstall here never touches directories, only
            // the regular files/symlinks it actually owns.
            .filter(|l| !l.trim().is_empty() && *l != "." && !l.ends_with('/'))
            .map(|l| l.to_string())
            .collect();
        if !write_installed_record(&meta, &files) {
            println!(
                "{YELLOW}[spk] Warning: {} installed, but failed to write its record to \
                 {INSTALLED_DB_DIR} -- 'spk remove {}' won't know about it later.{R}",
                meta.name, meta.name
            );
        }
        // preinst/postinst already ran; persist prerm/postrm (if any) next
        // to the installed record so `remove` can run them too, since the
        // staging dir they live in is about to be deleted.
        for hook in ["prerm", "postrm"] {
            let src = format!("{control_dir}/{hook}");
            if Path::new(&src).exists() {
                if let Ok(contents) = fs::read_to_string(&src) {
                    write_root_file(&format!("{}.{hook}", installed_db_path(&meta.name)), &contents);
                }
            }
        }
        println!("{GREEN}[spk] {} installed.{R}", meta.name);
    } else {
        println!("{RED}[spk] Failed to extract data.tar.xz from {spkg_path}.{R}");
    }
    let _ = fs::remove_dir_all(&stage);
    ok
}

/// Write `content` to `path`, which may require root (installed-db records
/// live under /var/lib, repo-package-repo config under /etc). Shared by
/// every "write as root" call site instead of duplicating the sudo-tee
/// dance at each one.
fn write_root_file(path: &str, content: &str) -> bool {
    if is_root() {
        return fs::write(path, content).is_ok();
    }
    match Command::new("sudo")
        .args(["-S", "tee", path])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(content.as_bytes());
            }
            matches!(child.wait(), Ok(s) if s.success())
        }
        Err(_) => false,
    }
}

fn remove_root_file(path: &str) {
    if is_root() {
        let _ = fs::remove_file(path);
    } else {
        let _ = Command::new("sudo").args(["-S", "rm", "-f", path]).status();
    }
}

fn ensure_root_dir(path: &str) -> bool {
    if Path::new(path).is_dir() {
        return true;
    }
    if is_root() {
        fs::create_dir_all(path).is_ok()
    } else {
        matches!(
            Command::new("sudo").args(["-S", "mkdir", "-p", path]).status(),
            Ok(s) if s.success()
        )
    }
}

// ── Installed-package database ────────────────────────────────────────────────
//
// One text file per installed package at INSTALLED_DB_DIR/<name>:
//
//   version: 1.2.3
//   architecture: x86_64
//   depends: foo, bar (>= 2.0)
//   files:
//   usr/bin/thing
//   usr/lib/libthing.so
//
// This is the foundation everything else here needs: dependency
// resolution checks it to know what's already satisfied, `remove` reads
// it to know exactly which files it owns (and cross-references every
// OTHER package's file list before deleting anything, so a file two
// packages both ship is never removed out from under the other one).

struct InstalledPkg {
    name: String,
    version: String,
    depends: String,
    files: Vec<String>,
}

fn installed_db_path(name: &str) -> String {
    format!("{INSTALLED_DB_DIR}/{name}")
}

fn is_installed(name: &str) -> bool {
    Path::new(&installed_db_path(name)).exists()
}

fn list_installed_names() -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(INSTALLED_DB_DIR) {
        for e in entries.flatten() {
            if e.path().is_file() {
                if let Some(n) = e.file_name().to_str() {
                    names.push(n.to_string());
                }
            }
        }
    }
    names.sort();
    names
}

fn read_installed(name: &str) -> Option<InstalledPkg> {
    let text = fs::read_to_string(installed_db_path(name)).ok()?;
    let mut version = String::new();
    let mut depends = String::new();
    let mut files = Vec::new();
    let mut in_files = false;
    for line in text.lines() {
        if in_files {
            if !line.is_empty() {
                files.push(line.to_string());
            }
            continue;
        }
        if line.trim() == "files:" {
            in_files = true;
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            match k.trim() {
                "version" => version = v.trim().to_string(),
                "depends" => depends = v.trim().to_string(),
                _ => {}
            }
        }
    }
    Some(InstalledPkg {
        name: name.to_string(),
        version,
        depends,
        files,
    })
}

fn write_installed_record(meta: &ControlMeta, files: &[String]) -> bool {
    if !ensure_root_dir(INSTALLED_DB_DIR) {
        return false;
    }
    let mut out = String::new();
    out.push_str(&format!("version: {}\n", meta.version));
    out.push_str(&format!("architecture: {}\n", meta.architecture));
    out.push_str(&format!("depends: {}\n", meta.depends));
    out.push_str("files:\n");
    for f in files {
        out.push_str(f);
        out.push('\n');
    }
    write_root_file(&installed_db_path(&meta.name), &out)
}

/// Parse a `depends:` field into plain package names, stripping any
/// version-constraint annotation in parentheses (e.g. "qt6-base (>= 6.10.0)"
/// -> "qt6-base"). The constraint itself isn't enforced yet -- only
/// presence/absence of the dependency by name -- see README for why that's
/// an acceptable v1 scope (spk has no version-comparison logic anywhere
/// else either).
fn parse_depends(depends: &str) -> Vec<String> {
    depends
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.split('(').next().unwrap_or(s).trim().to_string())
        .collect()
}

/// Recursively install whatever `depends` names aren't already installed,
/// before the package that declared them. `resolving` is the current
/// dependency chain (for cycle detection, not just "already handled" --
/// an already-installed dependency is skipped entirely, never added
/// here).
fn resolve_dependencies(depends: &str, root: &str, resolving: &mut Vec<String>) -> bool {
    for dep in parse_depends(depends) {
        if is_installed(&dep) {
            continue;
        }
        if resolving.contains(&dep) {
            println!(
                "{RED}[spk] Dependency cycle detected involving '{dep}' -- refusing to continue.{R}"
            );
            return false;
        }
        println!("{CYAN}[spk] Resolving dependency: {dep}{R}");
        resolving.push(dep.clone());
        let ok = fetch_and_install_inner(&dep, root, resolving);
        resolving.pop();
        if !ok {
            println!("{RED}[spk] Failed to install dependency '{dep}'.{R}");
            return false;
        }
    }
    true
}

/// A bare `spk install` argument that looks like a filesystem path rather
/// than a repo package name -- used to reject that form outright instead
/// of silently auto-detecting it. See cmd_install's caller in main() for
/// the actual message; this only decides whether to trigger it.
fn looks_like_local_path(s: &str) -> bool {
    s.contains('/') || s.starts_with('.') || s.starts_with('~')
        || s.ends_with(".spkg") || s.ends_with(".tar.xz")
}

// ── Package install ───────────────────────────────────────────────────────────

fn fetch_and_install(pkg: &str, root: &str) -> bool {
    fetch_and_install_inner(pkg, root, &mut Vec::new())
}

fn fetch_and_install_inner(pkg: &str, root: &str, resolving: &mut Vec<String>) -> bool {
    // Prefer the new .spkg format.
    let spkg_tmp = format!("/tmp/spk-{pkg}.spkg");
    println!("{CYAN}[spk] Fetching {pkg}...{R}");
    if fetch_package_ext(pkg, "spkg", &spkg_tmp) {
        let ok = install_spkg_file_inner(&spkg_tmp, root, resolving);
        let _ = fs::remove_file(&spkg_tmp);
        return ok;
    }
    let _ = fs::remove_file(&spkg_tmp);

    // Legacy bare .tar.xz fallback for repos that haven't migrated to
    // .spkg yet -- kept deliberately during the transition rather than a
    // hard cutover, so an operator's existing repo doesn't break outright.
    let tmp = format!("/tmp/spk-{pkg}.tar.xz");
    println!("{YELLOW}[spk] No {pkg}.spkg found -- trying legacy {pkg}.tar.xz...{R}");
    if !fetch_package_ext(pkg, "tar.xz", &tmp) {
        println!("{RED}[spk] Failed to download {pkg} -- package may not exist on any configured repo, or network is unreachable.{R}");
        let _ = fs::remove_file(&tmp);
        return false;
    }

    let _ = fs::create_dir_all(root);

    // Grab the member list before extracting -- same reasoning as the
    // .spkg path: it's the precise record of what's about to land on
    // disk. Legacy bundles carry no control/depends metadata at all, so
    // this record is minimal (no version, no depends), but it's still
    // enough for `spk remove` to know what to delete later.
    let legacy_files: Vec<String> = Command::new("tar")
        .args(["-tf", &tmp])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
        .lines()
        // Directories excluded outright, not just slash-stripped -- see
        // the .spkg path's identical filter above for why.
        .filter(|l| !l.trim().is_empty() && *l != "." && !l.ends_with('/'))
        .map(|l| l.to_string())
        .collect();

    println!("{CYAN}[spk] Extracting {pkg} into {root}...{R}");
    let extract = format!("tar -xf '{tmp}' -C '{root}'");
    let result = if is_root() {
        Command::new("sh").arg("-c").arg(&extract).status()
    } else {
        Command::new("sudo")
            .args(["-S", "sh", "-c", &extract])
            .stdin(Stdio::inherit())
            .status()
    };
    let _ = fs::remove_file(&tmp);

    match result {
        Ok(s) if s.success() => {
            let legacy_meta = ControlMeta {
                name: pkg.to_string(),
                version: "unknown (legacy .tar.xz, no metadata)".to_string(),
                architecture: "x86_64".to_string(),
                depends: String::new(),
                description: String::new(),
            };
            write_installed_record(&legacy_meta, &legacy_files);
            println!("{GREEN}[spk] {pkg} installed.{R}");
            true
        }
        _ => {
            println!("{RED}[spk] Failed to extract {pkg}.{R}");
            false
        }
    }
}

fn cmd_install(pkg: &str) {
    println!("{BOLD}[spk] Installing: {pkg}{R}");
    if !fetch_and_install(pkg, target_root()) {
        println!("{RED}{BOLD}[spk] Installation failed for {pkg}.{R}");
        exit(1);
    }
    println!("{GREEN}{BOLD}[spk] {pkg} installed successfully.{R}");
}

fn cmd_list() {
    let names = list_installed_names();
    if names.is_empty() {
        println!(
            "{YELLOW}[spk] No packages installed (or {INSTALLED_DB_DIR} doesn't exist yet).{R}"
        );
        return;
    }
    println!("{BOLD}Installed packages ({}){R}", names.len());
    for name in &names {
        if let Some(p) = read_installed(name) {
            println!("  {GREEN}{}{R}  {}", p.name, p.version);
        }
    }
}

fn cmd_depends(name: &str) {
    fn print_tree(name: &str, depth: usize, seen: &mut std::collections::HashSet<String>) {
        let indent = "  ".repeat(depth);
        if seen.contains(name) {
            println!("{indent}{YELLOW}{name} (already shown above -- shared dependency){R}");
            return;
        }
        seen.insert(name.to_string());
        match read_installed(name) {
            Some(p) => {
                println!("{indent}{GREEN}{}{R} {}", p.name, p.version);
                for dep in parse_depends(&p.depends) {
                    print_tree(&dep, depth + 1, seen);
                }
            }
            None => println!("{indent}{RED}{name} (not installed){R}"),
        }
    }
    let mut seen = std::collections::HashSet::new();
    print_tree(name, 0, &mut seen);
}

/// Everything installed that declares `name` in its own `depends:` --
/// i.e. what breaks if `name` is removed. Used by cmd_remove's
/// reverse-dependency check.
fn find_dependents(name: &str) -> Vec<String> {
    list_installed_names()
        .into_iter()
        .filter(|other| other != name)
        .filter(|other| {
            read_installed(other)
                .map(|p| parse_depends(&p.depends).iter().any(|d| d == name))
                .unwrap_or(false)
        })
        .collect()
}

fn cmd_remove(name: &str, force: bool, yes: bool) {
    let pkg = match read_installed(name) {
        Some(p) => p,
        None => {
            println!(
                "{RED}[spk] '{name}' is not installed (no record at {}).{R}",
                installed_db_path(name)
            );
            exit(1);
        }
    };

    let dependents = find_dependents(name);
    if !dependents.is_empty() {
        if !force {
            println!(
                "{RED}[spk] Refusing to remove '{name}': still required by: {}{R}",
                dependents.join(", ")
            );
            println!("  Use --force to remove anyway (may break those packages).");
            exit(1);
        }
        println!(
            "{YELLOW}{BOLD}[spk] WARNING: '{name}' is still required by: {} -- removing anyway (--force).{R}",
            dependents.join(", ")
        );
    }

    // Shared-file safety: a file is only actually deleted if NO other
    // installed package's own record also lists it. This is the one
    // place a mistake is genuinely destructive (deleting a shared
    // library out from under an unrelated package), so it's checked
    // fresh against every other installed record here rather than
    // trusted from anywhere else.
    let mut other_owned: std::collections::HashSet<String> = std::collections::HashSet::new();
    for other in list_installed_names() {
        if other == name {
            continue;
        }
        if let Some(p) = read_installed(&other) {
            other_owned.extend(p.files);
        }
    }
    let to_delete: Vec<&String> = pkg.files.iter().filter(|f| !other_owned.contains(*f)).collect();
    let kept: Vec<&String> = pkg.files.iter().filter(|f| other_owned.contains(*f)).collect();

    println!("{BOLD}[spk] Removing: {} {}{R}", pkg.name, pkg.version);
    println!("  {} file(s) will be deleted.", to_delete.len());
    if !kept.is_empty() {
        println!(
            "  {YELLOW}{} file(s) kept -- still owned by another installed package.{R}",
            kept.len()
        );
    }

    if !yes {
        print!("  Proceed? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if answer.trim().to_lowercase() != "y" {
            println!("{YELLOW}[spk] Aborted -- nothing removed.{R}");
            exit(0);
        }
    }

    let prerm_path = format!("{}.prerm", installed_db_path(name));
    if Path::new(&prerm_path).exists() {
        let _ = Command::new("chmod").args(["+x", &prerm_path]).status();
        match Command::new("sh").arg(&prerm_path).status() {
            Ok(s) if s.success() => {}
            _ => println!("{YELLOW}[spk] Warning: prerm script did not exit cleanly.{R}"),
        }
    }

    let root = target_root();
    for f in &to_delete {
        let path = format!("{}/{}", root.trim_end_matches('/'), f);
        remove_root_file(&path);
    }

    let postrm_path = format!("{}.postrm", installed_db_path(name));
    if Path::new(&postrm_path).exists() {
        let _ = Command::new("chmod").args(["+x", &postrm_path]).status();
        match Command::new("sh").arg(&postrm_path).status() {
            Ok(s) if s.success() => {}
            _ => println!("{YELLOW}[spk] Warning: postrm script did not exit cleanly.{R}"),
        }
    }

    remove_root_file(&prerm_path);
    remove_root_file(&postrm_path);
    remove_root_file(&installed_db_path(name));
    println!("{GREEN}[spk] {} removed.{R}", pkg.name);
}

fn cmd_install_local(path: &str) {
    if !Path::new(path).exists() {
        println!("{RED}[spk] Error: '{path}' does not exist.{R}");
        exit(1);
    }
    if !install_spkg_file(path, target_root()) {
        println!("{RED}{BOLD}[spk] Local package installation failed.{R}");
        exit(1);
    }
}

/// Point `spk install <name>` (the ordinary, no-flag form) at a local
/// directory of .spkg files too, by adding it to spk-repo-conf.yaml as a
/// `file://` repo entry at the highest priority. Reuses fetch_package_ext
/// verbatim -- curl already speaks file:// -- so this needed no new fetch
/// code, just a config-writing subcommand.
fn cmd_local_package_repo(folder: &str) {
    let abs = match fs::canonicalize(folder) {
        Ok(p) => p,
        Err(_) => {
            println!("{RED}[spk] Error: '{folder}' does not exist or is not accessible.{R}");
            exit(1);
        }
    };
    if !abs.is_dir() {
        println!("{RED}[spk] Error: '{folder}' is not a directory.{R}");
        exit(1);
    }
    let url = format!("file://{}", abs.display());

    let mut repos = load_repos();
    // Replace any existing "local" entry rather than accumulating
    // duplicates -- only one local-package-repo is active at a time.
    repos.retain(|r| r.name != "local");
    repos.push(Repo {
        name: "local".to_string(),
        url,
        priority: 1,
    });
    repos.sort_by_key(|r| r.priority);

    let mut out = String::from("repos:\n");
    for r in &repos {
        out.push_str(&format!(
            "  - name: {}\n    url: {}\n    priority: {}\n",
            r.name, r.url, r.priority
        ));
    }

    let write_ok = if is_root() {
        fs::write(REPO_CONF_PATH, &out).is_ok()
    } else {
        match Command::new("sudo")
            .args(["-S", "tee", REPO_CONF_PATH])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(stdin) = child.stdin.as_mut() {
                    let _ = stdin.write_all(out.as_bytes());
                }
                matches!(child.wait(), Ok(s) if s.success())
            }
            Err(_) => false,
        }
    };

    if write_ok {
        println!("{GREEN}[spk] Local package repo set: {}{R}", abs.display());
        println!("  'spk install <name>' will check this folder first (priority 1).");
    } else {
        println!("{RED}[spk] Failed to write {REPO_CONF_PATH}.{R}");
        exit(1);
    }
}

fn cmd_system_upgrade() {
    println!("{BOLD}{MAGENTA}[spk] System upgrade starting...{R}");

    let mut failures: Vec<String> = Vec::new();

    if is_smechvisor() {
        println!("{CYAN}[spk] SmechVisor detected -- live OTA upgrade{R}");
        for (pkg, needs_reboot) in SMECHVISOR_PACKAGES {
            if *pkg == "smechvisor-daemon" {
                // Live swap: stop daemon, replace binary, restart
                let _ = Command::new("rc-service").args(["smechvisord", "stop"]).status();
            }
            if fetch_and_install(pkg, "/") {
                if *pkg == "smechvisor-daemon" {
                    let _ = Command::new("rc-service").args(["smechvisord", "start"]).status();
                    println!("{GREEN}[spk] smechvisord restarted live -- no reboot needed.{R}");
                } else if *needs_reboot {
                    println!("{YELLOW}[spk] {pkg} updated -- reboot to activate.{R}");
                }
            } else {
                if *pkg == "smechvisor-daemon" {
                    // Re-start old binary even on failure
                    let _ = Command::new("rc-service").args(["smechvisord", "start"]).status();
                }
                failures.push(pkg.to_string());
            }
        }
    } else {
        println!("{CYAN}[spk] SmechOS detected -- full system upgrade{R}");
        let root = target_root();
        for (i, pkg) in SMECHOS_PACKAGES.iter().enumerate() {
            println!("{BOLD}[{}/{}] Re-fetching {pkg}...{R}", i + 1, SMECHOS_PACKAGES.len());
            if !fetch_and_install(pkg, root) {
                failures.push(pkg.to_string());
            }
        }
    }

    if failures.is_empty() {
        println!("{GREEN}{BOLD}[spk] System upgrade complete. Sovereignty verified.{R}");
    } else {
        println!("{YELLOW}{BOLD}[spk] Upgrade complete with failures: {failures:?}{R}");
        exit(1);
    }
}

// ── Compile (build orchestration) ─────────────────────────────────────────────

fn find_spk_compile() -> Option<String> {
    let candidates = [
        "/usr/share/spk/spk-compile.py",
        "/opt/smechdeploy/spk-compile.py",
        "spk-compile.py",
    ];
    for c in &candidates {
        if Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    None
}

fn cmd_compile(args: &[String]) {
    let script = match find_spk_compile() {
        Some(s) => s,
        None => {
            println!("{RED}[spk] spk-compile.py not found.{R}");
            println!("  Install it to one of:");
            println!("    /usr/share/spk/spk-compile.py");
            println!("    /opt/smechdeploy/spk-compile.py");
            println!("  Or get spk-compile: https://github.com/Smech-Labs/spk-compile");
            exit(1);
        }
    };
    println!("{CYAN}[spk] Running build orchestrator: {script}{R}");
    let status = Command::new("python3")
        .arg(&script)
        .args(args)
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => exit(s.code().unwrap_or(1)),
        Err(e) => {
            println!("{RED}[spk] Failed to run spk-compile.py: {e}{R}");
            exit(1);
        }
    }
}

// ── Deploy (SmechVisor network deploy) ───────────────────────────────────────

fn gen_code() -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id() as u128;
    format!("{:07x}", (t ^ (pid << 17)) & 0x0FFF_FFFF)
}

/// Push SmechVisor packages to a shim node that is broadcasting `code`.
fn cmd_deploy_to(code: &str) {
    println!("{CYAN}[spk] Listening for shim broadcasting code '{code}' on UDP {SHIM_UDP_PORT}...{R}");
    let recv_sock = UdpSocket::bind(format!("0.0.0.0:{SHIM_UDP_PORT}"))
        .expect("[spk] UDP listen bind failed");
    recv_sock.set_read_timeout(Some(Duration::from_secs(120))).ok();

    let expected = format!("{BROADCAST_MAGIC}:{code}");
    let mut buf = [0u8; 256];

    let shim_ip = loop {
        match recv_sock.recv_from(&mut buf) {
            Ok((n, addr)) => {
                if String::from_utf8_lossy(&buf[..n]).trim() == expected {
                    println!("{GREEN}[spk] Shim found at {}{R}", addr.ip());
                    break addr.ip().to_string();
                }
            }
            Err(_) => {
                println!("{RED}[spk] Timed out. Is the shim ISO running on the target machine?{R}");
                exit(1);
            }
        }
    };

    let tcp_addr = format!("{shim_ip}:{SHIM_TCP_PORT}");
    println!("{CYAN}[spk] Connecting to shim TCP at {tcp_addr}...{R}");
    let mut stream = std::net::TcpStream::connect(&tcp_addr)
        .expect("[spk] TCP connect to shim failed");

    let packages = ["smechvisor-base", "smechvisor-daemon"];
    for pkg_name in &packages {
        let tmp = format!("/tmp/spk-deploy-{pkg_name}.tar.xz");

        println!("{CYAN}[spk] Fetching {pkg_name}...{R}");
        let ok = fetch_package(pkg_name, &tmp);
        if !ok {
            println!("{YELLOW}[spk] Warning: failed to fetch {pkg_name}, skipping.{R}");
            continue;
        }

        let data = fs::read(&tmp).expect("read package file");
        let _ = fs::remove_file(&tmp);

        let name_bytes = pkg_name.as_bytes();
        stream.write_all(&(name_bytes.len() as u32).to_be_bytes()).ok();
        stream.write_all(name_bytes).ok();
        stream.write_all(&(data.len() as u64).to_be_bytes()).ok();
        stream.write_all(&data).ok();
        stream.flush().ok();
        println!("{GREEN}[spk] Sent {pkg_name} ({} MB){R}", data.len() / 1_048_576);
    }
    // Terminator: 4-byte zero name length
    stream.write_all(&0u32.to_be_bytes()).ok();
    stream.flush().ok();
    println!("{GREEN}{BOLD}[spk] All packages sent. Shim will install and reboot.{R}");
}

/// Receive a SmechVisor deploy from a donor node (runs on the shim).
fn cmd_receive_deploy() {
    let code = gen_code();

    // Clear screen and show code banner
    print!("\x1b[2J\x1b[H");
    println!("{BOLD}{MAGENTA}");
    println!("  +------------------------------------------+");
    println!("  |      SMECHVISOR DEPLOY RECEIVER          |");
    println!("  +------------------------------------------+");
    println!("  |                                          |");
    println!("  |  Your deploy code:                       |");
    println!("  |                                          |");
    println!("  |    {YELLOW}{code}{MAGENTA}                        |");
    println!("  |                                          |");
    println!("  |  On the donor node run:                  |");
    println!("  |    spk deploy-system-img-copy {code}  |");
    println!("  |                                          |");
    println!("  +------------------------------------------+{R}");
    println!();

    // Broadcast UDP in background thread
    let code_for_broadcast = code.clone();
    std::thread::spawn(move || {
        if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
            sock.set_broadcast(true).ok();
            let msg = format!("{BROADCAST_MAGIC}:{code_for_broadcast}");
            loop {
                sock.send_to(msg.as_bytes(), format!("255.255.255.255:{SHIM_UDP_PORT}")).ok();
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    });

    // Listen for TCP from donor
    let listener = TcpListener::bind(format!("0.0.0.0:{SHIM_TCP_PORT}"))
        .expect("[spk] TCP bind failed");
    println!("{CYAN}[spk] Waiting for donor on TCP port {SHIM_TCP_PORT}...{R}");
    let (mut stream, addr) = listener.accept().expect("[spk] accept failed");
    println!("{GREEN}[spk] Donor connected from {addr}{R}");

    let target = "/mnt/target";
    let _ = fs::create_dir_all(target);

    loop {
        let mut name_len_buf = [0u8; 4];
        if stream.read_exact(&mut name_len_buf).is_err() {
            break;
        }
        let name_len = u32::from_be_bytes(name_len_buf);
        if name_len == 0 {
            break; // terminator
        }

        let mut name_buf = vec![0u8; name_len as usize];
        if stream.read_exact(&mut name_buf).is_err() {
            break;
        }
        let name = String::from_utf8_lossy(&name_buf).to_string();

        let mut data_len_buf = [0u8; 8];
        stream.read_exact(&mut data_len_buf).expect("read data_len");
        let data_len = u64::from_be_bytes(data_len_buf);

        println!("{CYAN}[spk] Receiving '{name}' ({} MB)...{R}", data_len / 1_048_576);

        // Stream directly into tar stdin to avoid buffering full package in RAM
        let mut child = Command::new("tar")
            .args(["-xJf", "-", "-C", target])
            .stdin(Stdio::piped())
            .spawn()
            .expect("tar spawn");

        let mut remaining = data_len;
        let mut buf = [0u8; 65536];
        if let Some(mut stdin) = child.stdin.take() {
            while remaining > 0 {
                let to_read = remaining.min(buf.len() as u64) as usize;
                match stream.read(&mut buf[..to_read]) {
                    Ok(0) => break,
                    Ok(n) => {
                        if stdin.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        remaining -= n as u64;
                    }
                    Err(_) => break,
                }
            }
        }
        child.wait().ok();
        println!("{GREEN}[spk] '{name}' installed into {target}.{R}");
    }

    println!("{GREEN}{BOLD}[spk] Receive complete. Rebooting...{R}");
    let _ = Command::new("sync").status();
    let _ = Command::new("reboot").arg("-f").spawn();
    std::thread::sleep(Duration::from_secs(5));
}

// ── PackageKit backend (script protocol) ─────────────────────────────────────
//
// PackageKit spawns this as `spk packagekit-backend` and communicates via
// stdin/stdout using a line-based protocol. Plasma Discover and any other
// PackageKit frontend can then use SPK to install/list packages.
//
// Response format per line:
//   package\t<status>\t<id>\t<summary>     (id = name;version;arch;repo)
//   progress\t<percent>
//   status\t<status_string>
//   error\t<errorcode>\t<message>
//   finished
//
fn cmd_packagekit_backend() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    macro_rules! pk_write {
        ($($arg:tt)*) => {
            let _ = writeln!(stdout, $($arg)*);
            let _ = stdout.flush();
        };
    }

    let reader = BufReader::new(stdin.lock());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let parts: Vec<&str> = line.trim().splitn(5, '\t').collect();
        if parts.is_empty() {
            continue;
        }

        // Build the active package list based on which OS we're running on
        let active_pkgs: Vec<&str> = if is_smechvisor() {
            SMECHVISOR_PACKAGES.iter().map(|(n, _)| *n).collect()
        } else {
            SMECHOS_PACKAGES.to_vec()
        };
        let repo_id = if is_smechvisor() { "smechvisor" } else { "smechos" };

        match parts[0] {
            "get-packages" => {
                pk_write!("status\tquery");
                for pkg in &active_pkgs {
                    pk_write!("package\tavailable\t{pkg};2.0.0;x86_64;{repo_id}\tSmech Labs package: {pkg}");
                }
                pk_write!("finished");
            }
            "resolve" => {
                let pkg = if parts.len() > 2 { parts[2] } else { "" };
                pk_write!("status\tquery");
                if active_pkgs.contains(&pkg) {
                    pk_write!("package\tavailable\t{pkg};2.0.0;x86_64;{repo_id}\tSmech Labs package: {pkg}");
                } else {
                    pk_write!("error\tpackage-not-found\tPackage '{pkg}' not found in SPK repos");
                }
                pk_write!("finished");
            }
            "install-packages" => {
                let pkg_id = if parts.len() > 2 { parts[2] } else { "" };
                let pkg_name = pkg_id.split(';').next().unwrap_or(pkg_id);
                pk_write!("status\tinstall");
                pk_write!("package\tinstalling\t{pkg_id}\tInstalling {pkg_name}...");
                pk_write!("progress\t10");
                if fetch_and_install(pkg_name, target_root()) {
                    pk_write!("progress\t100");
                    pk_write!("package\tinstalled\t{pkg_id}\t{pkg_name} installed");
                    pk_write!("finished");
                } else {
                    pk_write!("error\tpackage-install-failed\tFailed to install {pkg_name}");
                    pk_write!("finished");
                }
            }
            "remove-packages" => {
                pk_write!("error\tnot-supported\tSPK does not support package removal in this version");
                pk_write!("finished");
            }
            "update-packages" | "get-updates" => {
                pk_write!("status\tquery");
                for pkg in &active_pkgs {
                    pk_write!("package\tavailable\t{pkg};2.0.0;x86_64;{repo_id}\tUpdate available: {pkg}");
                }
                pk_write!("finished");
            }
            "refresh-cache" => {
                pk_write!("status\trefresh-cache");
                pk_write!("finished");
            }
            "get-repo-list" => {
                pk_write!("status\tquery");
                pk_write!("repo-detail\t{repo_id}\tSmech Labs Package Repository\ttrue");
                pk_write!("finished");
            }
            "quit" | "" => break,
            other => {
                pk_write!("error\tnot-supported\tUnknown command: {other}");
                pk_write!("finished");
            }
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_help();
        exit(0);
    }

    match args[1].as_str() {
        "help" | "--help" | "-h" => print_help(),

        "version" | "--version" | "-v" => println!("spk {VERSION}"),

        "about" => print_about(),

        "install" => {
            let rest = &args[2..];
            // --local-package <path>: the ONLY sanctioned way to install a
            // local .spkg file. No warning here on success -- the flag
            // itself already is the explicit "yes, I mean a local file"
            // signal, so there's nothing left to confirm.
            if let Some(pos) = rest.iter().position(|a| a == "--local-package") {
                match rest.get(pos + 1) {
                    Some(path) => cmd_install_local(path),
                    None => {
                        println!(
                            "{RED}[spk] Error: --local-package requires a path.   \
                             spk install --local-package /path/to/pkg.spkg{R}"
                        );
                        exit(1);
                    }
                }
                return;
            }

            if rest.is_empty() {
                println!("{RED}[spk] Error: specify a package.   spk install <pkg>{R}");
                exit(1);
            }
            let pkg = &rest[0];
            // A bare argument that looks like a filesystem path is a
            // mistake worth catching explicitly, not silently guessing at
            // -- "spk install <name>" only ever means "fetch from a
            // configured repo".
            if looks_like_local_path(pkg) {
                println!("{RED}[spk] '{pkg}' looks like a local file, not a repo package name.{R}");
                println!("  'spk install <name>' only fetches from configured repos.");
                println!("  To install a local .spkg file, use:");
                println!("    {BOLD}spk install --local-package {pkg}{R}");
                exit(1);
            }
            cmd_install(pkg);
        }

        "local-package-repo" => {
            if args.len() < 3 {
                println!(
                    "{RED}[spk] Error: specify a folder.   spk local-package-repo <folder>{R}"
                );
                exit(1);
            }
            cmd_local_package_repo(&args[2]);
        }

        "remove" | "uninstall" => {
            let rest = &args[2..];
            let force = rest.iter().any(|a| a == "--force");
            let yes = rest.iter().any(|a| a == "-y" || a == "--yes");
            match rest.iter().find(|a| !a.starts_with('-')) {
                Some(name) => cmd_remove(name, force, yes),
                None => {
                    println!("{RED}[spk] Error: specify a package.   spk remove <pkg>{R}");
                    exit(1);
                }
            }
        }

        "list" => cmd_list(),

        "depends" => {
            if args.len() < 3 {
                println!("{RED}[spk] Error: specify a package.   spk depends <pkg>{R}");
                exit(1);
            }
            cmd_depends(&args[2]);
        }

        "system-upgrade" => cmd_system_upgrade(),

        "compile" => {
            // Forward everything after "compile" to spk-compile.py
            cmd_compile(&args[2..].to_vec());
        }

        "deploy-system-img-copy" => {
            if args.len() < 3 {
                println!("{RED}[spk] Error: specify a code.   spk deploy-system-img-copy <code>{R}");
                exit(1);
            }
            cmd_deploy_to(&args[2]);
        }

        "receive-deploy" => cmd_receive_deploy(),

        "packagekit-backend" => cmd_packagekit_backend(),

        // ── Legacy aliases (v1.x compat) ─────────────────────────────────────
        "system-install" | "userland-install" => {
            println!("{YELLOW}[spk] '{} <pkg>' is now 'spk install <pkg>'{R}", args[1]);
            if args.len() < 3 {
                println!("{RED}[spk] Error: specify a package.{R}");
                exit(1);
            }
            cmd_install(&args[2]);
        }

        "entire-system-upgrade" => {
            println!("{YELLOW}[spk] 'entire-system-upgrade' is now 'spk system-upgrade'{R}");
            cmd_system_upgrade();
        }

        unknown => {
            println!("{RED}[spk] Unknown command: '{unknown}'{R}");
            println!("  Use '{BOLD}spk help{R}' to see valid commands.");
            exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_repos_with_priority_order() {
        let yaml = r#"
repos:
  - name: primary
    url: https://pkg.smech.xyz
    priority: 1
  - name: github-fallback
    url: https://github.com/Smech-Labs/SmechDeploy/releases/download/v1.0.0-packages
    priority: 50
  - name: sketchy-mirror
    url: http://example.com/mirror
    priority: 100
"#;
        let mut repos = parse_repo_conf(yaml);
        assert_eq!(repos.len(), 3);
        repos.sort_by_key(|r| r.priority);
        assert_eq!(repos[0].name, "primary");
        assert_eq!(repos[0].priority, 1);
        assert_eq!(repos[1].name, "github-fallback");
        assert_eq!(repos[1].priority, 50);
        assert_eq!(repos[2].name, "sketchy-mirror");
        assert_eq!(repos[2].priority, 100);
    }

    #[test]
    fn defaults_missing_priority_to_50() {
        let yaml = r#"
repos:
  - name: no-priority
    url: https://example.com
"#;
        let repos = parse_repo_conf(yaml);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].priority, 50);
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let yaml = r#"
# top comment
repos:
  # a comment inside the list too
  - name: one

    url: https://example.com/one
    priority: 5
"#;
        let repos = parse_repo_conf(yaml);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "one");
        assert_eq!(repos[0].url, "https://example.com/one");
        assert_eq!(repos[0].priority, 5);
    }

    #[test]
    fn handles_quoted_values() {
        let yaml = r#"
repos:
  - name: "quoted"
    url: 'https://example.com/single'
    priority: 3
"#;
        let repos = parse_repo_conf(yaml);
        assert_eq!(repos[0].name, "quoted");
        assert_eq!(repos[0].url, "https://example.com/single");
    }

    #[test]
    fn empty_input_yields_no_repos() {
        assert!(parse_repo_conf("").is_empty());
        assert!(parse_repo_conf("some: other\nyaml: entirely").is_empty());
    }

    #[test]
    fn single_repo_no_list_wrapper_needed() {
        // The user explicitly said "just a single repo" should be fine too --
        // confirm one-entry lists parse identically to multi-entry ones.
        let yaml = r#"
repos:
  - name: only
    url: https://pkg.smech.xyz
    priority: 1
"#;
        let repos = parse_repo_conf(yaml);
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].url, "https://pkg.smech.xyz");
    }

    #[test]
    fn parses_control_file() {
        let text = "name: kcoreaddons\nversion: 6.24.0\narchitecture: x86_64\ndepends: qt6-base\ndescription: KDE Frameworks - KCoreAddons\n";
        let m = parse_control(text);
        assert_eq!(m.name, "kcoreaddons");
        assert_eq!(m.version, "6.24.0");
        assert_eq!(m.architecture, "x86_64");
        assert_eq!(m.depends, "qt6-base");
        assert_eq!(m.description, "KDE Frameworks - KCoreAddons");
    }

    #[test]
    fn control_file_ignores_comments() {
        let text = "# a comment\nname: foo\n# depends: ignored\nversion: 1.0\n";
        let m = parse_control(text);
        assert_eq!(m.name, "foo");
        assert_eq!(m.version, "1.0");
        assert!(m.depends.is_empty());
    }

    #[test]
    fn detects_local_paths() {
        assert!(looks_like_local_path("./foo.spkg"));
        assert!(looks_like_local_path("/home/smech/foo.spkg"));
        assert!(looks_like_local_path("~/foo.spkg"));
        assert!(looks_like_local_path("foo.spkg"));
        assert!(looks_like_local_path("foo.tar.xz"));
        assert!(!looks_like_local_path("firefox"));
        assert!(!looks_like_local_path("kde-frameworks"));
    }
}
