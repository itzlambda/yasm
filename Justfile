# Run the complete quality gate required before sharing changes.
lint: workspace-deps fmt-check clippy test marketplace-disabled

# Member crates must inherit every dependency from [workspace.dependencies].
# Root Cargo.toml is the only place that may pin versions or paths.
workspace-deps:
    #!/usr/bin/env sh
    set -eu
    fail=0
    for file in $(find crates -name Cargo.toml | sort); do
        section=
        lineno=0
        while IFS= read -r line || [ -n "$line" ]; do
            lineno=$((lineno + 1))
            stripped=$(printf '%s\n' "$line" | sed 's/#.*//')
            trimmed=$(printf '%s\n' "$stripped" | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')
            [ -z "$trimmed" ] && continue
            case "$trimmed" in
                \[*\])
                    section=$(printf '%s\n' "$trimmed" | sed 's/^\[//;s/\]$//')
                    continue
                    ;;
            esac
            case "$section" in
                *dependencies*)
                    if ! printf '%s\n' "$trimmed" | grep -Eq 'workspace[[:space:]]*=[[:space:]]*true'; then
                        printf '%s:%s: inherit this dependency from the workspace: %s\n' "$file" "$lineno" "$trimmed" >&2
                        fail=1
                    fi
                    ;;
            esac
        done < "$file"
    done
    if [ "$fail" -ne 0 ]; then
        printf 'error: crate Cargo.toml files must use workspace = true for every dependency\n' >&2
        exit 1
    fi

# Check formatting without modifying files.
fmt-check:
    cargo fmt --all --check

# Treat every Clippy warning in every workspace target and feature as an error.
clippy:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run the full workspace test suite.
test:
    cargo test --workspace --all-features

# Marketplace command wiring and its optional dependency must disappear together.
marketplace-disabled:
    cargo test -p yasm-cli --no-default-features
