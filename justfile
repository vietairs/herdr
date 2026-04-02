# herdr task runner

# Run unit tests
test:
    cargo test
    python3 -m unittest scripts.test_changelog

# Check formatting + run unit tests
check:
    cargo fmt --check
    cargo test
    python3 -m unittest scripts.test_changelog

# Run the full local test suite
test-all: check

# Build release binary
build:
    cargo build --release

# Finalize changelog, bump version, commit, tag, push, trigger release build (usage: just release 0.1.1)
release version:
    @if [ -n "$(git status --porcelain)" ]; then \
        echo "error: commit your changes first"; \
        exit 1; \
    fi
    @if git rev-parse "v{{version}}" >/dev/null 2>&1; then \
        echo "error: tag v{{version}} already exists"; \
        exit 1; \
    fi
    python3 scripts/changelog.py prepare --version {{version}}
    sed -i.bak 's/^version = ".*"/version = "{{version}}"/' Cargo.toml && rm -f Cargo.toml.bak
    cargo test --quiet
    python3 -m unittest scripts.test_changelog
    git add CHANGELOG.md Cargo.toml Cargo.lock
    git diff --cached --quiet || git commit -m "release: v{{version}}"
    git tag -a v{{version}} -m "v{{version}}"
    git push --follow-tags
    @echo "v{{version}} released — GitHub Actions building binaries"

# Print default config
default-config:
    cargo run --release -- --default-config
