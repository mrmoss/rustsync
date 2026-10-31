# Build helper for rustsync. Run `just` to list the recipes.
# Get just:  cargo install just  |  brew install just  |  pkg install just  |  apt install just
#
# Cross-building: set TARGET, e.g.  TARGET=x86_64-apple-darwin just package

version := `grep -m1 '^version' Cargo.toml | cut -d'"' -f2`
target  := env_var_or_default("TARGET", "")
flag    := if target == "" { "" } else { "--target " + target }
bin     := if target == "" { "target/release" } else { "target/" + target + "/release" }
cpu     := if target == "" { arch() } else { replace_regex(target, "-.*", "") }
name    := "rustsync-" + version + "-" + os() + "-" + cpu
prefix  := env_var_or_default("PREFIX", env_var("HOME") + "/.local")

# List the recipes
default:
    @just --list

# Build everything: the service, command line, window and tray icon
build:
    cargo build --release {{flag}}

# Build only the service and command line (for servers: no window libraries)
build-server:
    cargo build --release --no-default-features {{flag}}

# Run the tests
test:
    cargo test --release

# Install both programs (into ~/.local/bin; change with PREFIX=/usr/local just install)
install: build
    mkdir -p "{{prefix}}/bin"
    install -m 755 "{{bin}}/rustsync" "{{bin}}/rustsync-gui" "{{prefix}}/bin/"

# Install only the service and command line
install-server: build-server
    mkdir -p "{{prefix}}/bin"
    install -m 755 "{{bin}}/rustsync" "{{prefix}}/bin/"

# Make dist/rustsync-<version>-<os>-<cpu>.tar.gz with both programs and the docs
package: build (_pack "rustsync rustsync-gui")

# The same, without the window (what the BSD release uses)
package-server: build-server (_pack "rustsync")

# (used by package and package-server)
_pack programs:
    #!/bin/sh
    set -eu
    d="dist/{{name}}"
    rm -rf "$d"
    mkdir -p "$d"
    for p in {{programs}}; do cp "{{bin}}/$p" "$d/"; done
    cp README.md SECURITY.md "$d/"
    tar -C dist -czf "dist/{{name}}.tar.gz" "{{name}}"
    rm -rf "$d"
    echo "made dist/{{name}}.tar.gz"

# macOS only: make dist/rustsync.app (with the logo) and a zip of it to hand out
macos-app: build
    #!/bin/sh
    set -eu
    [ "$(uname)" = Darwin ] || { echo "macos-app only works on a Mac"; exit 1; }
    app=dist/rustsync.app
    icons=dist/rustsync.iconset
    rm -rf "$app" "$icons"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$icons"
    cp "{{bin}}/rustsync" "{{bin}}/rustsync-gui" "$app/Contents/MacOS/"
    for spec in "16 icon_16x16" "32 icon_16x16@2x" "32 icon_32x32" "64 icon_32x32@2x" \
                "128 icon_128x128" "256 icon_128x128@2x" "256 icon_256x256"; do
      set -- $spec
      sips -z "$1" "$1" assets/icon-256.png --out "$icons/$2.png" >/dev/null
    done
    iconutil -c icns "$icons" -o "$app/Contents/Resources/rustsync.icns"
    rm -rf "$icons"
    cat > "$app/Contents/Info.plist" <<PLIST
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0"><dict>
      <key>CFBundleName</key><string>rustsync</string>
      <key>CFBundleDisplayName</key><string>rustsync</string>
      <key>CFBundleIdentifier</key><string>com.rustsync.gui</string>
      <key>CFBundleExecutable</key><string>rustsync-gui</string>
      <key>CFBundleIconFile</key><string>rustsync</string>
      <key>CFBundlePackageType</key><string>APPL</string>
      <key>CFBundleShortVersionString</key><string>{{version}}</string>
      <key>CFBundleVersion</key><string>{{version}}</string>
      <key>NSHighResolutionCapable</key><true/>
    </dict></plist>
    PLIST
    # Not signed by a developer, but sign it "ad hoc" so macOS accepts the pieces as one app.
    codesign --force --deep --sign - "$app"
    ditto -c -k --keepParent "$app" "dist/rustsync-{{version}}-macos-{{cpu}}.app.zip"
    echo "made $app and dist/rustsync-{{version}}-macos-{{cpu}}.app.zip"

# Format the code
fmt:
    cargo fmt

# Remove build output and dist/
clean:
    cargo clean
    rm -rf dist
