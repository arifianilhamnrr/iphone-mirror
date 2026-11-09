APP_NAME := iPhone Mirror.app
APP := dist/$(APP_NAME)
HELPER := $(APP)/Contents/Helpers/iPhone Mirror Stream.app
TARGET := target/release/iphone-mirror
LAUNCHER := build/iphone-mirror-launcher
ICON := resources/AppIcon.icns
VERSION := $(shell /usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' Info.plist)
ARCH := $(shell uname -m)
ZIP := dist/iPhone-Mirror-v$(VERSION)-macOS-$(ARCH).zip
DMG := dist/iPhone-Mirror-v$(VERSION)-macOS-$(ARCH).dmg
DMG_ROOT := build/dmg-root

.PHONY: all build icon bundle package installer checksums verify clean

all: build

build:
	cargo build --release --locked
	mkdir -p build
	xcrun swiftc -O -parse-as-library launcher/main.swift -o "$(LAUNCHER)"

icon:
	python3 scripts/generate_icon.py

bundle: build
	@echo "Bundling $(APP)..."
	rm -rf "$(APP)"
	mkdir -p "$(APP)/Contents/MacOS" "$(APP)/Contents/Resources" "$(HELPER)/Contents/MacOS"
	cp Info.plist "$(APP)/Contents/Info.plist"
	cp "$(ICON)" "$(APP)/Contents/Resources/AppIcon.icns"
	cp "$(LAUNCHER)" "$(APP)/Contents/MacOS/iphone-mirror-launcher"
	cp "$(TARGET)" "$(HELPER)/Contents/MacOS/iphone-mirror-backend"
	python3 -c 'import plistlib; plistlib.dump({"CFBundleExecutable":"iphone-mirror-backend","CFBundleIdentifier":"id.web.projectar.iphone-mirror.stream","CFBundleName":"iPhone Mirror Stream","CFBundlePackageType":"APPL","CFBundleShortVersionString":"$(VERSION)","CFBundleVersion":"1","LSMinimumSystemVersion":"15.0","LSUIElement":True,"NSHighResolutionCapable":True}, open("$(HELPER)/Contents/Info.plist", "wb"))'
	chmod +x "$(APP)/Contents/MacOS/iphone-mirror-launcher" "$(HELPER)/Contents/MacOS/iphone-mirror-backend"
	plutil -lint "$(APP)/Contents/Info.plist"
	codesign --force --deep --sign - "$(APP)"
	@echo "Build complete: $(APP)"

verify: bundle
	codesign --verify --deep --strict --verbose=2 "$(APP)"
	@test "$$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$(APP)/Contents/Info.plist")" = "iphone-mirror-launcher"
	@test -x "$(APP)/Contents/MacOS/iphone-mirror-launcher"
	@test -x "$(HELPER)/Contents/MacOS/iphone-mirror-backend"
	@test "$$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$(HELPER)/Contents/Info.plist")" = "id.web.projectar.iphone-mirror.stream"

package: verify installer
	rm -f "$(ZIP)"
	ditto -c -k --sequesterRsrc --keepParent "$(APP)" "$(ZIP)"
	$(MAKE) checksums
	@echo "Packages complete: $(ZIP) and $(DMG)"

installer: verify
	rm -rf "$(DMG_ROOT)" "$(DMG)"
	mkdir -p "$(DMG_ROOT)"
	cp -R "$(APP)" "$(DMG_ROOT)/$(APP_NAME)"
	ln -s /Applications "$(DMG_ROOT)/Applications"
	hdiutil create -volname "iPhone Mirror $(VERSION)" -srcfolder "$(DMG_ROOT)" -ov -format UDZO "$(DMG)"
	rm -rf "$(DMG_ROOT)"

checksums:
	cd dist && shasum -a 256 "$$(basename "$(ZIP)")" "$$(basename "$(DMG)")" > SHA256SUMS.txt

clean:
	cargo clean
	rm -rf dist build
