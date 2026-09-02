# Maintainer: zbndev <https://github.com/zbndev>
#
# Builds Tidemark from *this working tree* — not from a release tarball and not from a git
# URL. That is the whole point of it: `makepkg -sif` after finishing a step installs exactly
# what is on disk, so the result can be looked at in a real installation, with the systemd
# user unit and the real XDG paths, rather than through `cargo run`.
#
# There is deliberately no pkgver(): makepkg rewrites the `pkgver=` line of the PKGBUILD it
# runs, and this one is tracked in git. The version comes from the workspace manifest and is
# bumped there.
#
# There is deliberately no check() either. The test suite talks to the session bus and to
# the Secret Service; running it from a packaging build would write test keys into the
# user's keyring for no benefit, and it has already run in the development loop.

pkgname=tidemark
pkgver=0.3.0
pkgrel=1
pkgdesc='Track AI provider quota limits: how much of each rate-limit window is burned, when it resets, and whether the current pace reaches it'
arch=('x86_64')
url='https://github.com/zbndev/tidemark'
license=('MIT')
# rustls and oo7's native crypto keep OpenSSL and libsecret out; SQLite is the system
# library rather than a vendored copy, on purpose (CONTEXT.md § API floor).
depends=('gtk4' 'libadwaita' 'sqlite' 'dbus')
makedepends=('cargo' 'cmake' 'clang')
install=tidemark.install
source=()
# !lto is not a preference, it is a link requirement. rustls pulls aws-lc-sys, whose C and
# assembly are compiled by the cc crate with makepkg's CFLAGS; with makepkg's lto option
# those objects come out as GCC LTO bitcode, and rust-lld then fails the final link with
# hundreds of undefined aws_lc_* symbols. Our own release profile already does thin LTO
# across the Rust side, which is where the win is anyway.
options=(!debug !lto)

build() {
    cd "$startdir"
    # Kept out of the developer's own target/ directory: makepkg exports its own RUSTFLAGS,
    # and sharing one target directory with plain `cargo build` makes the two invalidate
    # each other's cache on every switch.
    export CARGO_TARGET_DIR="$startdir/target/makepkg"
    cargo build --release --locked --workspace
}

package() {
    cd "$startdir"
    local bin="target/makepkg/release"

    install -Dm755 "$bin/tidemark" "$pkgdir/usr/bin/tidemark"
    install -Dm755 "$bin/tidemarkd" "$pkgdir/usr/bin/tidemarkd"

    # The unit's ExecStart is /usr/bin/tidemarkd, which is where the line above puts it.
    install -Dm644 data/tidemarkd.service "$pkgdir/usr/lib/systemd/user/tidemarkd.service"

    # Tidemark's own full-colour icon. Native panel sizes matter: some SNI hosts display a
    # small fallback pixmap at integer scale, which makes even simple art look pixelated.
    # These are high-quality reductions of the owner's 1024px source; the small variants
    # also trim only transparent margin so the mark remains legible in a 16px panel. The
    # source itself is a 512px icon at 2x scale, a directory declared by hicolor rather
    # than an unindexed 1024x1024 directory GTK would never select.
    local size
    for size in 16 22 24 32 48 64 128 256 512; do
        install -Dm644 \
            "data/icons/hicolor/${size}x${size}/apps/io.github.zbndev.Tidemark.png" \
            "$pkgdir/usr/share/icons/hicolor/${size}x${size}/apps/io.github.zbndev.Tidemark.png"
    done
    install -Dm644 data/icons/hicolor/512x512@2/apps/io.github.zbndev.Tidemark.png \
        "$pkgdir/usr/share/icons/hicolor/512x512@2/apps/io.github.zbndev.Tidemark.png"

    install -Dm644 data/applications/io.github.zbndev.Tidemark.desktop \
        "$pkgdir/usr/share/applications/io.github.zbndev.Tidemark.desktop"
    install -Dm644 data/autostart/io.github.zbndev.Tidemark.desktop \
        "$pkgdir/etc/xdg/autostart/io.github.zbndev.Tidemark.desktop"
    install -Dm644 data/metainfo/io.github.zbndev.Tidemark.metainfo.xml \
        "$pkgdir/usr/share/metainfo/io.github.zbndev.Tidemark.metainfo.xml"
    install -Dm644 data/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service \
        "$pkgdir/usr/share/dbus-1/services/io.github.zbndev.Tidemark.Daemon.service"
    install -Dm755 data/restart-user-daemon \
        "$pkgdir/usr/lib/tidemark/restart-user-daemon"

    # The provider marks. They are recoloured by the theme only because GTK finds them
    # through the icon theme as symbolic icons, which is what putting them in hicolor buys;
    # loaded any other way they would keep the colours in the file.
    #
    # These five files are their owners' trademarks and are *not* under this package's
    # licence, so TRADEMARKS.md is installed next to LICENSE where a reader checking what
    # the licence covers will see it. A rebuild that drops both of these lines is a
    # supported configuration: a card with no mark is a state the interface already has.
    install -Dm644 -t "$pkgdir/usr/share/icons/hicolor/symbolic/apps" \
        data/icons/hicolor/symbolic/apps/tidemark-*-symbolic.svg
    install -Dm644 docs/TRADEMARKS.md "$pkgdir/usr/share/licenses/$pkgname/TRADEMARKS.md"

    install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
    install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
}
