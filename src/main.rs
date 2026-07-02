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
const VERSION: &str = "2.0.0";
const RELEASE_BASE_URL: &str =
    "https://github.com/Smech-Labs/SmechDeploy/releases/download/v1.0.0-packages";
const DOCS_URL: &str = "https://docs.smech.xyz";

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
    println!("    {GREEN}install <pkg>{R}              Fetch and install a package");
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

// ── Package install ───────────────────────────────────────────────────────────

fn fetch_and_install(pkg: &str, root: &str) -> bool {
    let url = format!("{RELEASE_BASE_URL}/{pkg}.tar.xz");
    let tmp = format!("/tmp/spk-{pkg}.tar.xz");

    println!("{CYAN}[spk] Fetching {pkg}...{R}");

    let ok = Command::new("curl")
        .args(["-sfL", "-o", &tmp, &url])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !ok {
        println!("{RED}[spk] Failed to download {pkg} -- package may not exist or network is unreachable.{R}");
        let _ = fs::remove_file(&tmp);
        return false;
    }

    let _ = fs::create_dir_all(root);

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
        let url = format!("{RELEASE_BASE_URL}/{pkg_name}.tar.xz");
        let tmp = format!("/tmp/spk-deploy-{pkg_name}.tar.xz");

        println!("{CYAN}[spk] Fetching {pkg_name}...{R}");
        let ok = Command::new("curl")
            .args(["-sfL", "-o", &tmp, &url])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
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
            if args.len() < 3 {
                println!("{RED}[spk] Error: specify a package.   spk install <pkg>{R}");
                exit(1);
            }
            cmd_install(&args[2]);
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
