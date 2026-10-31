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
      <key>NSCameraUsageDescription</key><string>rustsync uses the camera to read the QR code that pairs two devices.</string>
    </dict></plist>
    PLIST
    # Not signed by a developer, but sign it "ad hoc" so macOS accepts the pieces as one app.
    codesign --force --deep --sign - "$app"
    ditto -c -k --keepParent "$app" "dist/rustsync-{{version}}-macos-{{cpu}}.app.zip"
    echo "made $app and dist/rustsync-{{version}}-macos-{{cpu}}.app.zip"

# Android: build the Rust core for phones (needs the Android NDK and `cargo install cargo-ndk`)
android-core:
    rustup target add aarch64-linux-android x86_64-linux-android
    cargo ndk -t arm64-v8a -t x86_64 -o android/app/src/main/jniLibs build --release --lib --no-default-features

# Android: make dist/rustsync-<version>-android.apk (needs JDK 17, the Android SDK and `gradle`)
android-apk: android-core
    #!/bin/sh
    set -eu
    cd android
    gradle assembleRelease -PappVersion={{version}}
    cd ..
    mkdir -p dist
    cp android/app/build/outputs/apk/release/app-release.apk "dist/rustsync-{{version}}-android.apk"
    echo "made dist/rustsync-{{version}}-android.apk"

# Android: make a signing key, so that new versions of the app install over old ones
android-keystore:
    #!/bin/sh
    set -eu
    [ ! -e rustsync.jks ] || { echo "rustsync.jks already exists"; exit 1; }
    keytool -genkeypair -v -keystore rustsync.jks -alias rustsync -keyalg RSA -keysize 4096 -validity 10000
    echo
    echo "Made rustsync.jks. KEEP IT SAFE (it is not in git): whoever has it can sign updates."
    echo "For GitHub builds, add these repository secrets (Settings > Secrets and variables > Actions):"
    echo "  ANDROID_KEYSTORE_BASE64   the output of:  base64 < rustsync.jks | tr -d '\\n'"
    echo "  ANDROID_KEYSTORE_PASSWORD the keystore password you just typed"
    echo "  ANDROID_KEY_ALIAS         rustsync"
    echo "  ANDROID_KEY_PASSWORD      the key password (the same, if you pressed enter)"
    echo "For a local build:  ANDROID_KEYSTORE_FILE=\$PWD/rustsync.jks ANDROID_KEYSTORE_PASSWORD=... ANDROID_KEY_ALIAS=rustsync ANDROID_KEY_PASSWORD=... just android-apk"

# Format the code
fmt:
    cargo fmt

# Remove build output and dist/
clean:
    cargo clean
    rm -rf dist
