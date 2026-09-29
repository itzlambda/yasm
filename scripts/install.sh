#!/bin/sh
# Install yasm from GitHub Releases.
# Keep this script short and auditable.
#
#   curl --proto '=https' --tlsv1.2 -fsSL \
#     https://raw.githubusercontent.com/itzlambda/yasm/main/scripts/install.sh | sh
#
# Later upgrades use `yasm self-upgrade`.

set -eu

repo="itzlambda/yasm"
yasm_install="${YASM_INSTALL:-$HOME/.yasm}"
assume_yes=0
no_modify_path=0
requested_version=""

usage() {
    cat <<'EOF'
Install yasm from GitHub Releases.

Usage:
  install.sh [options]

Options:
  -y, --yes           Add yasm to PATH without prompting
  --no-modify-path    Install the binary without changing shell startup files
  --version <version> Install this release (for example 0.0.1 or v0.0.1)
  -h, --help          Show this help

The binary is installed to $YASM_INSTALL/bin/yasm (default: ~/.yasm/bin).
Upgrade an existing install with `yasm self-upgrade`.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        -y|--yes)
            assume_yes=1
            shift
            ;;
        --no-modify-path)
            no_modify_path=1
            shift
            ;;
        --version)
            if [ "$#" -lt 2 ]; then
                echo "missing value for --version" >&2
                exit 1
            fi
            requested_version=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

say() {
    printf '%s\n' "$1"
}

err() {
    printf '%s\n' "$1" >&2
}

download() {
    url=$1
    dest=$2
    if command -v curl >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error "$url" -o "$dest" || return 1
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$dest" "$url" || return 1
    else
        err "curl or wget is required"
        return 1
    fi
}

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os:$arch" in
        Linux:x86_64) printf '%s\n' "x86_64-unknown-linux-musl" ;;
        Linux:aarch64) printf '%s\n' "aarch64-unknown-linux-musl" ;;
        Darwin:x86_64) printf '%s\n' "x86_64-apple-darwin" ;;
        Darwin:arm64) printf '%s\n' "aarch64-apple-darwin" ;;
        *)
            err "unsupported platform: $os $arch"
            err "supported platforms: Linux (x86_64, aarch64) and macOS (x86_64, arm64)"
            exit 1
            ;;
    esac
}

release_base() {
    if [ -z "$requested_version" ]; then
        printf '%s\n' "https://github.com/$repo/releases/latest/download"
        return
    fi
    case "$requested_version" in
        v*) tag=$requested_version ;;
        *) tag="v$requested_version" ;;
    esac
    printf '%s\n' "https://github.com/$repo/releases/download/$tag"
}

file_sha256() {
    file=$1
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | awk '{ print $1 }'
    else
        err "sha256sum or shasum is required to verify the download"
        exit 1
    fi
}

profile_file() {
    case "${SHELL:-}" in
        */zsh) printf '%s\n' "$HOME/.zshrc" ;;
        */fish) printf '%s\n' "$HOME/.config/fish/config.fish" ;;
        */bash)
            if [ -f "$HOME/.bashrc" ]; then
                printf '%s\n' "$HOME/.bashrc"
            elif [ -f "$HOME/.bash_profile" ]; then
                printf '%s\n' "$HOME/.bash_profile"
            else
                printf '%s\n' "$HOME/.profile"
            fi
            ;;
        *) printf '%s\n' "$HOME/.profile" ;;
    esac
}

write_env_files() {
    bin_dir=$1
    if [ "$yasm_install" = "$HOME/.yasm" ]; then
        cat > "$yasm_install/env" <<'EOF'
#!/bin/sh
# yasm shell setup. This file is sourced by bash and zsh.
case ":${PATH}:" in
    *:"$HOME/.yasm/bin":*)
        ;;
    *)
        export PATH="$HOME/.yasm/bin:${PATH}"
        ;;
esac
EOF
        cat > "$yasm_install/env.fish" <<'EOF'
# yasm shell setup. This file is sourced by fish.
if not contains "$HOME/.yasm/bin" $PATH
    set -gx PATH "$HOME/.yasm/bin" $PATH
end
EOF
        return
    fi

    cat > "$yasm_install/env" <<EOF
#!/bin/sh
# yasm shell setup. This file is sourced by bash and zsh.
case ":\${PATH}:" in
    *:"$bin_dir":*)
        ;;
    *)
        export PATH="$bin_dir:\${PATH}"
        ;;
esac
EOF
    cat > "$yasm_install/env.fish" <<EOF
# yasm shell setup. This file is sourced by fish.
if not contains "$bin_dir" \$PATH
    set -gx PATH "$bin_dir" \$PATH
end
EOF
}

source_line_for() {
    profile=$1
    case "$profile" in
        *.fish)
            if [ "$yasm_install" = "$HOME/.yasm" ]; then
                printf '%s\n' 'source "$HOME/.yasm/env.fish"'
            else
                printf '%s\n' "source \"$yasm_install/env.fish\""
            fi
            ;;
        *)
            if [ "$yasm_install" = "$HOME/.yasm" ]; then
                printf '%s\n' '. "$HOME/.yasm/env"'
            else
                printf '%s\n' ". \"$yasm_install/env\""
            fi
            ;;
    esac
}

path_contains_bin() {
    bin_dir=$1
    case ":${PATH:-}:" in
        *":$bin_dir:"*) return 0 ;;
        *) return 1 ;;
    esac
}

confirm_path_change() {
    profile=$1
    if [ "$no_modify_path" -eq 1 ]; then
        return 1
    fi
    if [ "$assume_yes" -eq 1 ]; then
        return 0
    fi
    if [ ! -r /dev/tty ] || [ ! -w /dev/tty ]; then
        return 1
    fi
    printf 'Add %s/bin to PATH in %s? [Y/n] ' "$yasm_install" "$profile" >/dev/tty
    if ! read -r answer </dev/tty; then
        return 1
    fi
    case "$answer" in
        n|N|no|NO) return 1 ;;
        *) return 0 ;;
    esac
}

configure_path() {
    bin_dir=$1
    profile=$(profile_file)
    line=$(source_line_for "$profile")

    if path_contains_bin "$bin_dir"; then
        say "PATH already includes $bin_dir"
        return
    fi

    if ! confirm_path_change "$profile"; then
        say "Add yasm to PATH by adding this line to your shell startup file:"
        say "  $line"
        return
    fi

    mkdir -p "$(dirname "$profile")"
    if [ -f "$profile" ] && grep -qF "$line" "$profile"; then
        say "PATH entry already present in $profile"
        return
    fi
    printf '\n%s\n' "$line" >>"$profile"
    say "Added yasm to PATH in $profile"
    say "Restart your shell or run: $line"
}

main() {
    if [ "$(id -u)" -eq 0 ]; then
        say "warning: installing as root; the binary will belong to root"
    fi

    target=$(detect_target)
    asset="yasm-$target"
    base=$(release_base)
    bin_dir="$yasm_install/bin"
    exe="$bin_dir/yasm"

    if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
        err "curl or wget is required"
        exit 1
    fi
    if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
        err "sha256sum or shasum is required to verify the download"
        exit 1
    fi

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    say "Downloading $asset"
    if ! download "$base/$asset" "$tmp/$asset"; then
        err "cannot download $base/$asset"
        err "Check https://github.com/$repo/releases for a published yasm release."
        exit 1
    fi
    if ! download "$base/SHA256SUMS" "$tmp/SHA256SUMS"; then
        err "cannot download checksums from $base/SHA256SUMS"
        exit 1
    fi

    expected=$(awk -v asset="$asset" '$2 == asset { print $1; exit }' "$tmp/SHA256SUMS")
    if [ -z "$expected" ]; then
        err "SHA256SUMS has no entry for $asset"
        exit 1
    fi
    actual=$(file_sha256 "$tmp/$asset")
    expected=$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')
    actual=$(printf '%s' "$actual" | tr '[:upper:]' '[:lower:]')
    if [ "$expected" != "$actual" ]; then
        err "checksum mismatch for $asset"
        err "expected $expected"
        err "got      $actual"
        exit 1
    fi

    mkdir -p "$bin_dir"
    cp "$tmp/$asset" "$bin_dir/yasm.new"
    chmod 755 "$bin_dir/yasm.new"
    mv -f "$bin_dir/yasm.new" "$exe"
    if command -v xattr >/dev/null 2>&1; then
        xattr -d com.apple.quarantine "$exe" 2>/dev/null || true
    fi

    write_env_files "$bin_dir"

    if ! "$exe" --version; then
        err "installed binary failed to run: $exe"
        if [ "$(uname -s)" = "Darwin" ]; then
            err "macOS Gatekeeper may be blocking this unsigned binary. Open it from Finder, then run it again."
        fi
        exit 1
    fi

    configure_path "$bin_dir"

    if ! command -v git >/dev/null 2>&1; then
        say "git is not on PATH. Yasm needs git to fetch GitHub skill sources."
    fi

    say "Upgrade later with: yasm self-upgrade"
}

main
