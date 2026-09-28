# Packaging

## Arch Linux (`ferropipe-git`)

`PKGBUILD` builds the current `HEAD` of Ferropipe from GitHub. Because the crate
has path dependencies on two sibling repositories (`ferropipe-rdp`, which in turn
depends on `rdpeudp`), the PKGBUILD clones all three next to each other under
`src/` so that `../ferropipe-rdp` and `../rdpeudp` resolve.

```bash
cd packaging
makepkg -si          # build and install (pulls libssh2, openssl, smbclient)
```

`makepkg` needs `base-devel` plus the `makedepends` (`cargo`, `git`). The version
is derived from the commit count and short hash (`rNNN.hhhhhhh`), so rebuilding
after new commits bumps it automatically; `makepkg -si` again to upgrade.

Optional dependencies:

- `remmina` for RDP sessions (the native RDP client needs nothing extra).
- `alacritty` as the default terminal for "Open terminal" on SSH hosts (any
  terminal in `$TERMINAL`, or kitty/wezterm/konsole/…, works too).

What gets installed:

| File | Destination |
| --- | --- |
| `target/release/ferropipe` | `/usr/bin/ferropipe` |
| `assets/ferropipe.desktop` | `/usr/share/applications/ferropipe.desktop` |
| `assets/ferropipe.svg` | `/usr/share/icons/hicolor/scalable/apps/ferropipe.svg` |
| `LICENSE` | `/usr/share/licenses/ferropipe-git/LICENSE` |
| `README.md` | `/usr/share/doc/ferropipe-git/README.md` |

Lint the PKGBUILD with `namcap PKGBUILD` (and `namcap *.pkg.tar.zst` after a
build) when changing it.

## User-local install (any distro)

`../install.sh` remains the alternative for a per-user install without a package
manager: it builds a release binary and installs it, the desktop entry and the
icon under `~/.local` (override with `PREFIX=...`). Both repositories must be
checked out as siblings of `ferropipe/` first:

```bash
git clone https://github.com/nicatmhmmdv/rdpeudp.git
git clone https://github.com/nicatmhmmdv/ferropipe-rdp.git
git clone https://github.com/nicatmhmmdv/ferropipe.git
cd ferropipe && ./install.sh
```
