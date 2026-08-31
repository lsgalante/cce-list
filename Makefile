.PHONY: build install run clean

build:
	cargo build --release

# Binaries, helper scripts and user units are enumerated by ccebuild from
# cargo metadata, so extra [[bin]] targets are picked up without being named
# here — hand-listing them is what left crates shipping incomplete for weeks.
install: build
	@command -v ccebuild >/dev/null || { echo "ccebuild not installed — run: make -C ../cce-compositor install"; exit 1; }
	ccebuild install --no-build cce-list

run:
	cargo run

clean:
	cargo clean
