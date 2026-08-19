default:
    @just --list

# Bump the version, tag, run the release hooks (see cog.toml), then package.
release type='auto': && package
    cog bump --{{type}}

# Build the Arch packages in a clean chroot and upload the current version.
package:
    #!/usr/bin/env bash
    set -euo pipefail
    pkgver=$(sed -n 's/^pkgver=//p' packaging/arch/PKGBUILD)
    sw1nn-makepkg-chroot -C packaging/arch
    sw1nn-pkg-ctl upload packaging/arch/*-"$pkgver"-*.pkg.tar.zst
