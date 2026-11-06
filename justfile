# Dev workflows for live-mcp. Run `just` to list them.

# Pass recipe arguments through as "$@", so quoted arguments with spaces survive.
set positional-arguments

ear_bundle := "target/bundled/LiveMCP Ear.vst3"
bin := "./target/release/live-mcp"

# List the recipes
default:
    @just --list --unsorted

# Build the live-mcp binary (release), with the LiveMCP Ear plugin inside it
build: bundle-ear
    LIVE_MCP_EAR_BUNDLE="{{ ear_bundle }}" cargo build --release

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

# Build the LiveMCP Ear plugin into target/bundled/
bundle-ear:
    cargo xtask bundle live-mcp-ear --release

# Build, then install the Remote Script and the Ear plugin (e.g. `just install --user-library "/path"`); then restart Live
install *args: build
    {{ bin }} install "$@"

# Run the live-mcp CLI against the running Live, e.g. `just live status`
live *args: build
    {{ bin }} "$@"
