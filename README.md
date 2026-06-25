# spk -- SmechOS Sovereign Package Keeper

`spk` is SmechOS's own package manager. It fetches pre-built `.tar.xz`
packages from a GitHub Release and extracts them into the target root --
no Gentoo/Portage, no Flatpak, nothing borrowed from another distro's
package format.

## Commands

```
spk system-install <pkg>     Fetch and install a package onto the target system partition
spk userland-install <pkg>   Fetch and install a userland package
spk entire-system-upgrade    Re-fetch and reinstall every known SmechOS package
spk about                    Show version/credits
spk help                     Show usage
```

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
| `curl` | Downloading package `.tar.xz` files from GitHub Releases |
| `tar` | Extracting downloaded packages into the target root |
| `sudo` | Privilege escalation when not already running as root (for writing into the target root) |

No Gentoo Portage, no Flatpak, no GerritHub REST API calls -- those were
all removed. See "Architecture history" below for why.

## Where packages actually come from

Packages are fetched from a GitHub Release on
[Smech-Labs/SmechDeploy](https://github.com/Smech-Labs/SmechDeploy/releases/tag/v1.0.0-packages):

```
https://github.com/Smech-Labs/SmechDeploy/releases/download/v1.0.0-packages/<package>.tar.xz
```

Currently published: `kernel-modules`, `firmware`, `bootloader-grub`,
`kde-frameworks`, `plasma`, `qt6`, `mesa-graphics`, `calamares-installer`.

**`base-system` is not currently published.** It's corrupted in the
`spk-repo-gun` git history itself (its `xz` stream ends prematurely --
confirmed with `xz -t`), independent of any hosting issue. It needs to be
rebuilt from source before it can be republished. `spk system-install
base-system` will attempt the fetch and fail clearly rather than silently
pretend the package doesn't exist.

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
