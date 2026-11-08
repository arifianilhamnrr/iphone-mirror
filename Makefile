APP_NAME = iPhone\ Mirror.app
TARGET = target/release/iphone-mirror

all: build

build:
	cargo build --release

bundle: build
	@echo "Bundling $(APP_NAME) for macOS..."
	mkdir -p "$(APP_NAME)/Contents/MacOS"
	mkdir -p "$(APP_NAME)/Contents/Resources"
	cp Info.plist "$(APP_NAME)/Contents/"
	cp $(TARGET) "$(APP_NAME)/Contents/MacOS/"
	chmod +x "$(APP_NAME)/Contents/MacOS/iphone-mirror"
	@echo "Build complete: $(APP_NAME)"

clean:
	cargo clean
	rm -rf "$(APP_NAME)"
