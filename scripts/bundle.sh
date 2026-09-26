#!/bin/sh
# Builds Grove.app into target/release/.
#
#   scripts/bundle.sh
#   cp -R target/release/Grove.app /Applications/
#   ln -sf /Applications/Grove.app/Contents/MacOS/grove /usr/local/bin/grove   # the CLI
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

cargo build --release -p grove-app

APP="target/release/Grove.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/grove "$APP/Contents/MacOS/grove"
# The layered Liquid Glass icon (macOS 26+) compiles to Assets.car, plus a
# Grove.icns fallback for older systems. Needs Xcode 26+.
if xcrun --find actool >/dev/null 2>&1; then
  xcrun actool crates/grove-app/assets/Grove.icon \
    --compile "$APP/Contents/Resources" \
    --output-format human-readable-text --errors \
    --output-partial-info-plist "$ROOT/target/release/grove-icon.plist" \
    --app-icon Grove --include-all-app-icons \
    --enable-on-demand-resources NO --development-region en \
    --target-device mac --minimum-deployment-target 13.0 --platform macosx >/dev/null
else
  echo "warning: Xcode's actool not found; Grove.app will have no icon" >&2
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Grove</string>
  <key>CFBundleDisplayName</key><string>Grove</string>
  <key>CFBundleIdentifier</key><string>dev.grove.Grove</string>
  <key>CFBundleExecutable</key><string>grove</string>
  <key>CFBundleIconFile</key><string>Grove</string>
  <key>CFBundleIconName</key><string>Grove</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

# Ad-hoc signature so Gatekeeper lets a local build run.
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || true
echo "Built $APP"
