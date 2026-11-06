# Dev workflows for live-mcp. Run `just` to list them.

# Pass recipe arguments through as "$@", so quoted arguments with spaces survive.
set positional-arguments

vst3_dir := home_directory() / "Library/Audio/Plug-Ins/VST3"
ear_bundle := "LiveMCP Ear.vst3"
bin := "./target/release/live-mcp"

# List the recipes
default:
    @just --list --unsorted

# Build the live-mcp binary (release)
build:
    cargo build --release

# Run the tests (the end-to-end tests need python3 on PATH); extra args go to cargo test
test *args:
    cargo test --workspace "$@"

# Format the code
fmt:
    cargo fmt --all

# Run clippy, with warnings as errors
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Check that the Remote Script compiles (Live runs it on its own Python)
check-script:
    python3 -c "import ast, sys; ast.parse(open(sys.argv[1]).read(), sys.argv[1])" remote_script/LiveMCP/__init__.py

# Everything to run before committing: formatting, lints, script syntax, tests
check: check-script lint test
    cargo fmt --all --check

# Build and install the Remote Script into Live (e.g. `just install-script --user-library "/path"`), then restart Live
install-script *args: build
    {{ bin }} install "$@"
    @echo "Quit and restart Live to load the new script; 'just live status' then reports script_current: true."

# Build the LiveMCP Ear plugin into target/bundled/
bundle-ear:
    cargo xtask bundle live-mcp-ear --release

# Build the Ear and replace the installed copy in the VST3 folder, then restart Live
install-ear: bundle-ear
    mkdir -p "{{ vst3_dir }}"
    rm -rf "{{ vst3_dir }}/{{ ear_bundle }}"
    cp -R "target/bundled/{{ ear_bundle }}" "{{ vst3_dir }}/"
    @echo "Installed {{ vst3_dir }}/{{ ear_bundle }}. Restart Live: it keeps the old plugin binary loaded."

# Install both the Remote Script and the Ear plugin
install: install-script install-ear

# Run the live-mcp CLI against the running Live, e.g. `just live status`
live *args: build
    {{ bin }} "$@"
