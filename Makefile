APP_NAME := iPhone Mirror.app
APP := dist/$(APP_NAME)
TARGET := target/release/iphone-mirror
ICON := resources/AppIcon.icns
ZIP := dist/iPhone-Mirror-macOS.zip

.PHONY: all build icon bundle package verify clean

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

package: verify
	rm -f "$(ZIP)"
	ditto -c -k --sequesterRsrc --keepParent "$(APP)" "$(ZIP)"
	@echo "Package complete: $(ZIP)"

clean:
	cargo clean
	rm -rf dist build
