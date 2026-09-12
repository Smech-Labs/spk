# spk -- Smech Sovereign Package Keeper

`spk` is SmechOS's own package manager. It fetches pre-built packages
from one or more configured repos and extracts them into the target root
-- no Gentoo/Portage, no Flatpak, nothing borrowed from another distro's
package format.

## Commands

```
spk install <pkg>                    Fetch and install a package from a configured repo
spk install --local-package <path>   Install a local .spkg file (the ONLY way to install
                                      from disk -- a bare path passed to 'install' is
                                      rejected on purpose, see ".spkg format" below)
spk local-package-repo <folder>      Point 'install <name>' at a local folder of .spkg
                                      files too, at priority 1 (highest)
spk system-upgrade                   Re-fetch and reinstall every known SmechOS package
spk compile ...                      Forward to spk-compile.py (build orchestration)
spk about                            Show version/credits
spk help                             Show usage
```

`system-install`/`userland-install`/`entire-system-upgrade` still work as
deprecated aliases for `install`/`system-upgrade` (v1.x compat), printing
a one-line notice pointing at the current name.

## .spkg format

A `.spkg` is a plain (uncompressed) outer `tar` containing exactly two
members, deb-style:

```
foo-1.2.3.spkg
├── control.tar.xz   metadata: a `control` key:value file, plus optional
│                     preinst/postinst/prerm/postrm executable scripts
└── data.tar.xz       payload -- paths relative to the install root
```

`control` file schema (same hand-parsed `key: value` style as
`spk-repo-conf.yaml` below -- `spk` has zero crate dependencies, so
there's no YAML/JSON parser to reach for):

```
name: kcoreaddons
version: 6.24.0
architecture: x86_64
depends: qt6-base
description: KDE Frameworks - KCoreAddons module
```

`spk install <name>` requests `<name>.spkg` from each configured repo in
priority order, falling back to the legacy bare `<name>.tar.xz` only if
no repo has a `.spkg` for it -- existing repos don't break outright
during the migration off the old bundle format.

### Why `--local-package` is required, not auto-detected

Most package managers auto-detect a local file (`apt install ./foo.deb`,
`pacman -U ./foo.pkg.tar.zst`) by checking whether the argument looks
like a path. `spk` deliberately does the opposite: `spk install
./foo.spkg` is rejected outright with a message pointing at
`--local-package`, so "where did this package actually come from" is
always explicit in scripts and shell history, never inferred. The flag
itself is the complete signal -- a correct `--local-package` invocation
installs with no further warning.

### `local-package-repo`

`spk local-package-repo <folder>` writes that folder into
`/etc/spk-repo-conf.yaml` as a `file://` repo entry at priority 1 (checked
first). This needed no new fetch code at all: `curl` already speaks
`file://` URLs natively, so it reuses the exact same repo-iteration path
as any `https://` repo.

## Build requirements

- Rust + Cargo (any recent stable toolchain)
- **Zero crate dependencies** -- `Cargo.toml` has an empty `[dependencies]`
  section by design. `spk` is a small, fully static-logic binary; it shells
  out to system tools for everything external rather than linking against
  HTTP/TLS/archive crates.

```sh
cargo build --release
# binary at target/release/spk
```

## Runtime requirements

`spk` is a thin orchestrator around a handful of system tools that must be
present in `$PATH` on whatever machine runs it:

| Tool | Used for |
|---|---|
| `curl` | Fetching `.spkg`/`.tar.xz` packages from any configured repo (`https://` or `file://`) |
| `tar` | Extracting `.spkg`'s outer container + its `control.tar.xz`/`data.tar.xz` members, and legacy `.tar.xz` packages |
| `sudo` | Privilege escalation when not already running as root (for writing into the target root, or `/etc/spk-repo-conf.yaml`) |

No Gentoo Portage, no Flatpak, no GerritHub REST API calls -- those were
all removed. See "Architecture history" below for why.

## Where packages actually come from

Repos are read from `/etc/spk-repo-conf.yaml` (see `parse_repo_conf` /
`load_repos` in `src/main.rs`), tried in priority order (1 first; 100 is
a distinct untrusted tier, only ever used as a last resort with a loud
warning). If that file is missing or unparseable, `spk` falls back to a
single built-in default pointing at a GitHub Release:

```
repos:
  - name: smech-pkg
    url: https://pkg.smech.xyz
    priority: 1
  - name: github-releases
    url: https://github.com/Smech-Labs/SmechDeploy/releases/download/v1.0.0-packages
    priority: 10
```

Currently published (see `SMECHOS_PACKAGES` in `src/main.rs`, used by
`system-upgrade` to know what to re-fetch -- `install` can fetch any name,
unknown ones just 404): `base-system`, `kernel-modules`, `firmware`,
`bootloader-grub`, `kde-frameworks`, `plasma`, `qt6`, `mesa-graphics`,
`plasma-discover`, `packagekit-spk`. These are category-level bundles
still being migrated to individual per-component `.spkg` packages --
see `spk-compile`'s `phase_bundle_spkg_packages` for the KF6/Plasma/Qt6
modules that already build as separate packages.

`base-system` was rebuilt from source against musl+Clang after the
original copy turned out to be corrupted in the `spk-repo-gun` git history
itself (its `xz` stream ended prematurely). See
[Smech-Labs/SmechDeploy](https://github.com/Smech-Labs/SmechDeploy)'s
`bin/10_bootstrap_musl.sh`, `bin/11_bootstrap_userland_musl.sh`, and
`bin/12_write_etc_skeleton.py` for the actual build process -- every
binary in the rebuilt package was individually verified to execute
correctly (not just compile), with every optional host-only library
dependency (SELinux, OpenSSL, GMP, libcap, ACLs, PCRE, zlib/bzlib/
zstdlib/libseccomp) explicitly disabled at configure time.

## Architecture history

`spk` originally shelled out to `emerge` (Gentoo Portage) and `flatpak`
for `system-install`/`userland-install`/`entire-system-upgrade`, and fetched
package metadata from a GerritHub-hosted repo
([spk-repo-gun](https://review.gerrithub.io/admin/repos/Smech-Labs/spk-repo-gun))
via its REST file-content API. Both of these were removed:

- **No Gentoo/Portage, no Flatpak**: SmechOS is an independent distribution
  with its own build system, not a Gentoo derivative -- `spk` shelling out
  to `emerge` was a leftover from an earlier direction that no longer
  matches how SmechOS is actually built (see
  [Smech-Labs/SmechDeploy](https://github.com/Smech-Labs/SmechDeploy)'s
  `bin/MUSL_BOOTSTRAP_PLAN.md` for the from-source musl+Clang userland
  bootstrap that replaced the old host-copy approach).
- **No GerritHub REST API for package downloads**: GerritHub's
  `/files/{path}/content` endpoint works fine for small files but silently
  *truncates* large binary downloads -- confirmed directly: `base-system.tar.xz`
  fetched through the REST API came back incomplete every time, while the
  exact same file fetched via a plain `git clone` of the repo was the
  correct size (just separately corrupted at the source, an unrelated
  problem). The REST API is built for reviewing source diffs, not serving
  large binaries reliably. GitHub Releases has no such limit.
