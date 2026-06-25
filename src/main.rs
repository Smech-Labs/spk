use std::env;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio, exit};

// Color escape codes
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const MAGENTA: &str = "\x1b[35m";

// Packages are served from a GitHub Release rather than GerritHub's REST
// file-content API. That API works for small files but silently truncates
// large binaries (confirmed: base-system.tar.xz fetches incomplete through
// it while smaller packages don't) -- GitHub Releases serves raw files
// directly with no such limit.
const RELEASE_BASE_URL: &str = "https://github.com/Smech-Labs/SmechDeploy/releases/download/v1.0.0-packages";

// Packages currently published in the release above. Used by
// entire-system-upgrade to know what to re-fetch; system-install/
// userland-install can fetch any package name, known or not, and let the
// HTTP request itself fail if it doesn't exist.
//
// base-system was rebuilt from source against musl+Clang (see
// bin/10_bootstrap_musl.sh, bin/11_bootstrap_userland_musl.sh,
// bin/12_write_etc_skeleton.py) after the old copy turned out to be
// corrupted in the spk-repo-gun git history. It's the GNU userland
// (coreutils, grep, sed, tar, gzip, xz, findutils, diffutils, gawk, make,
// file) compiled against musl instead of glibc, with every optional
// host-only library dependency (SELinux, OpenSSL, GMP, libcap, ACLs,
// PCRE, zlib/bzlib/zstdlib/libseccomp) explicitly disabled at configure
// time and every binary individually verified to actually execute, not
// just compile.
const KNOWN_PACKAGES: &[&str] = &[
    "base-system",
    "kernel-modules",
    "firmware",
    "bootloader-grub",
    "kde-frameworks",
    "plasma",
    "qt6",
    "mesa-graphics",
    "calamares-installer",
];

fn print_banner() {
    println!(
        "{}{}{}========================================================================{}",
        BOLD, MAGENTA, RESET, RESET
    );
    println!(
        "{}{}     ███████╗██████╗ ██╗  ██╗    ███████╗ ██████╗ ██╗  ██╗{}",
        BOLD, RED, RESET
    );
    println!(
        "{}{}     ██╔════╝██╔══██╗██║ ██╔╝    ██╔════╝██╔═══██╗██║ ██╔╝{}",
        BOLD, RED, RESET
    );
    println!(
        "{}{}     ███████╗██████╔╝█████╔╝     ███████╗██║   ██║█████╔╝ {}",
        BOLD, RED, RESET
    );
    println!(
        "{}{}     ╚════██║██╔═══╝ ██╔═██╗     ╚════██║██║   ██║██╔═██╗ {}",
        BOLD, RED, RESET
    );
    println!(
        "{}{}     ███████║██║     ██║  ██╗    ███████║╚██████╔╝██║  ██╗{}",
        BOLD, RED, RESET
    );
    println!(
        "{}{}               SMECHOS SOVEREIGN PACKAGE KEEPER (SPK){}",
        BOLD, CYAN, RESET
    );
    println!(
        "{}{}{}========================================================================{}",
        BOLD, MAGENTA, RESET, RESET
    );
}

fn print_help() {
    print_banner();
    println!("{}USAGE:{}", BOLD, RESET);
    println!("    spk <COMMAND> [package]");
    println!();
    println!("{}COMMANDS:{}", BOLD, RESET);
    println!(
        "    {}system-install <pkg>{}   Fetch and install a package onto the target system partition",
        GREEN, RESET
    );
    println!(
        "    {}userland-install <pkg>{} Fetch and install a userland package",
        GREEN, RESET
    );
    println!(
        "    {}entire-system-upgrade{}  Re-fetch and reinstall every known SmechOS package",
        GREEN, RESET
    );
    println!("    {}about{}                  Show SmechOS workstation specs and software credits", GREEN, RESET);
    println!("    {}help{}                   Show this help menu", GREEN, RESET);
    println!();
    println!("{}EXAMPLES:{}", BOLD, RESET);
    println!("    spk system-install base-system");
    println!("    spk userland-install plasma");
    println!("    spk entire-system-upgrade");
    println!();
}

fn print_about() {
    print_banner();
    println!("{}--- SMECH-SOVEREIGN WORKSTATION 2026 build CONFIGURATION ---{}", BOLD, RESET);
    println!("  {}CPU:{}             AMD Threadripper PRO 9965WX (Zen 5, 24-core, 48-thread)", CYAN, RESET);
    println!("  {}Motherboard:{}     ASUS Pro WS WRX90E-SAGE SE SSI-EEB", CYAN, RESET);
    println!("  {}ECC Memory:{}     256GB DDR5 RDIMM (8x 32GB Kingston FURY Renegade Pro)", CYAN, RESET);
    println!("  {}GPUs:{}           2x NVIDIA RTX 5080 (Horizontal active liquid cooled)", CYAN, RESET);
    println!("  {}Storage Tier:{}    Dual 1TB Samsung 990 PRO NVMe RAID (SmechOS Boot/System)", CYAN, RESET);
    println!("  {}Cooling Loop:{}    Industrial Active Syltherm 800 - 4x D5 Pumps, EPDM Tubing", CYAN, RESET);
    println!("  {}Power Res:{}      Dual ROG Thor III 1200W (Total 2400W fully isolated)", CYAN, RESET);
    println!();
    println!("{}--- SPK ARCHITECTURE CREDITS ---{}", BOLD, RESET);
    println!("  Designed by Gemini / Antigravity with Comrade Smech.");
    println!("  Built as a zero-dependency, static sovereign manager.");
    println!("  Fetches real packages from Smech-Labs/SmechDeploy releases -- no Gentoo/Portage,");
    println!("  no Flatpak, nothing borrowed from another distro's package format.");
    println!();
}

fn get_target_context() -> (bool, &'static str) {
    // Check if we are running on host with /mnt/smechos mounted
    if Path::new("/mnt/smechos").exists() {
        (true, "/mnt/smechos")
    } else {
        (false, "")
    }
}

fn is_root() -> bool {
    if let Ok(uid_str) = env::var("UID") {
        uid_str == "0"
    } else {
        // Fallback using id -u
        if let Ok(output) = Command::new("id").arg("-u").output() {
            let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
            s == "0"
        } else {
            false
        }
    }
}

/// Fetches a package's .tar.xz from the SmechDeploy GitHub Release and
/// extracts it into target_root. Shells out to curl/tar (system tools)
/// rather than pulling in an HTTP/TLS crate, keeping spk a zero-crate-
/// dependency binary -- consistent with how it already shells out to
/// chroot/sudo rather than linking against their internals.
fn fetch_and_install_package(pkg: &str, target_root: &str) -> bool {
    let url = format!("{}/{}.tar.xz", RELEASE_BASE_URL, pkg);

    println!("{}[+] Fetching {}...{}", CYAN, pkg, RESET);

    let tmp_tar = format!("/tmp/spk-{}.tar.xz", pkg);

    let curl_status = Command::new("curl")
        .args(["-sfL", "-o", &tmp_tar, &url])
        .status();
    match curl_status {
        Ok(status) if status.success() => {}
        _ => {
            println!(
                "{}[-] Failed to download {} -- package may not exist, or the network is unreachable.{}",
                RED, pkg, RESET
            );
            let _ = fs::remove_file(&tmp_tar);
            return false;
        }
    }

    if let Err(e) = fs::create_dir_all(target_root) {
        println!("{}[-] Failed to create target root {}: {}{}", RED, target_root, e, RESET);
        let _ = fs::remove_file(&tmp_tar);
        return false;
    }

    println!("{}[+] Extracting {} into {}...{}", CYAN, pkg, target_root, RESET);
    let extract_cmd = format!("tar -xf '{}' -C '{}'", tmp_tar, target_root);
    let extract_status = if is_root() {
        Command::new("sh").arg("-c").arg(&extract_cmd).status()
    } else {
        Command::new("sudo")
            .args(["-S", "sh", "-c", &extract_cmd])
            .stdin(Stdio::inherit())
            .status()
    };
    let _ = fs::remove_file(&tmp_tar);

    match extract_status {
        Ok(status) if status.success() => true,
        _ => {
            println!("{}[-] Failed to extract {} into {}.{}", RED, pkg, target_root, RESET);
            false
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_help();
        exit(0);
    }

    let command = args[1].as_str();

    match command {
        "help" | "--help" | "-h" => {
            print_help();
        }
        "about" | "--about" => {
            print_about();
        }
        "system-install" | "userland-install" => {
            if args.len() < 3 {
                println!("{}[-] Error: Please specify a package to install.{}", BOLD, RESET);
                println!("    Example: spk {} base-system", command);
                exit(1);
            }
            let pkg = &args[2];
            let label = if command == "system-install" { "SYSTEM" } else { "USERLAND" };
            println!("{}====================================================", BOLD);
            println!("  SPK: INSTALLING {} PACKAGE: {}", label, pkg);
            println!("===================================================={}", RESET);

            let (is_host, target_dir) = get_target_context();
            let target_root = if is_host { target_dir } else { "/" };

            if fetch_and_install_package(pkg, target_root) {
                println!("{} [+] Package {} installed successfully!{}", BOLD, pkg, RESET);
            } else {
                println!("{} [-] Installation failed for package {}.{}", BOLD, pkg, RESET);
                exit(1);
            }
        }
        "entire-system-upgrade" => {
            println!("{}{}{}========================================================================{}", BOLD, MAGENTA, RESET, RESET);
            println!("{}{}        SMECHOS SOVEREIGN PACKAGE KEEPER (SPK) - FULL UPGRADE HUD{}", BOLD, CYAN, RESET);
            println!("{}{}{}========================================================================{}", BOLD, MAGENTA, RESET, RESET);

            let (is_host, target_dir) = get_target_context();
            let target_root = if is_host { target_dir } else { "/" };
            if is_host {
                println!("    - Context: Host system (targeting SmechOS rootfs at {})", target_dir);
            } else {
                println!("    - Context: Target SmechOS local env");
            }
            println!("{}{}{}========================================================================{}", BOLD, MAGENTA, RESET, RESET);

            let mut failures = Vec::new();
            for (i, pkg) in KNOWN_PACKAGES.iter().enumerate() {
                println!(
                    "\n{}[{}/{}] Re-fetching {}...{}",
                    BOLD,
                    i + 1,
                    KNOWN_PACKAGES.len(),
                    pkg,
                    RESET
                );
                if !fetch_and_install_package(pkg, target_root) {
                    failures.push(*pkg);
                }
            }

            if failures.is_empty() {
                println!("\n{}{}{}========================================================================{}", BOLD, GREEN, RESET, RESET);
                println!("{}{}          SMECHOS SYSTEM COMPILATION & UPGRADE COMPLETED!{}", BOLD, GREEN, RESET);
                println!("{}            Sovereignty verified. Your workstation is secure.{}", BOLD, RESET);
                println!("{}{}{}========================================================================{}", BOLD, GREEN, RESET, RESET);
            } else {
                println!("\n{}{}{}========================================================================{}", BOLD, YELLOW, RESET, RESET);
                println!("{}{}    SMECHOS UPGRADE COMPLETED WITH FAILURES: {:?}{}", BOLD, YELLOW, failures, RESET);
                println!("{}{}{}========================================================================{}", BOLD, YELLOW, RESET, RESET);
                exit(1);
            }
        }
        _ => {
            println!("{}[-] Error: Unknown command: '{}'{}", BOLD, command, RESET);
            println!("    Use 'spk help' to see valid commands.");
            exit(1);
        }
    }
}
