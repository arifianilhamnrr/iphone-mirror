APP_NAME := iPhone Mirror.app
APP := dist/$(APP_NAME)
TARGET := target/release/iphone-mirror
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

icon:
	python3 scripts/generate_icon.py

bundle: build
	@echo "Bundling $(APP)..."
	rm -rf "$(APP)"
	mkdir -p "$(APP)/Contents/MacOS" "$(APP)/Contents/Resources"
	cp Info.plist "$(APP)/Contents/Info.plist"
	cp "$(ICON)" "$(APP)/Contents/Resources/AppIcon.icns"
	cp "$(TARGET)" "$(APP)/Contents/MacOS/iphone-mirror"
	chmod +x "$(APP)/Contents/MacOS/iphone-mirror"
	plutil -lint "$(APP)/Contents/Info.plist"
	codesign --force --deep --sign - "$(APP)"
	@echo "Build complete: $(APP)"

verify: bundle
	codesign --verify --deep --strict --verbose=2 "$(APP)"
	@test "$$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' "$(APP)/Contents/Info.plist")" = "iphone-mirror"
	@test -x "$(APP)/Contents/MacOS/iphone-mirror"

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

checksums:
	cd dist && shasum -a 256 "$$(basename "$(ZIP)")" "$$(basename "$(DMG)")" > SHA256SUMS.txt

clean:
	cargo clean
	rm -rf dist build
