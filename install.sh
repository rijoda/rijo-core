#!/usr/bin/env bash
# Rijo Code CLI installer — by Rijoda Labs.
#
# Defaults to building from source in this checkout and installing the binary
# into ~/.local/bin (no sudo, no system directories touched).
#
#   ./install.sh                     # build + install
#   ./install.sh --prefix ~/bin      # other target directory
#   ./install.sh --binary v0.1.0     # download a prebuilt release asset instead
#   ./install.sh --check             # only verify the prerequisites
#   ./install.sh --skip-models       # do not pre-download the embedding model
#   ./install.sh --dry-run           # show what would happen
#   ./install.sh --uninstall         # remove the installed binary
#
# Rijo Code keeps its state in ~/.rijo (config, sessions, skills, memory). Nothing
# in this script writes outside $PREFIX and (optionally) ~/.rijo.

set -euo pipefail

REPO="rijoda/rijo-code"
REPO_URL="https://github.com/${REPO}"
PREFIX="${RIJO_PREFIX:-$HOME/.local/bin}"
MODE="source"
DRY=0
CHECK=0
UNINSTALL=0
VERSION_TAG=""

die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
info() { printf '\033[36m::\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarn:\033[0m %s\n' "$*" >&2; }

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)    PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
        --prefix=*)  PREFIX="${1#*=}"; shift ;;
        --binary)    MODE="binary"; VERSION_TAG="${2:-latest}"; shift 2 ;;
        --binary=*)  MODE="binary"; VERSION_TAG="${1#*=}"; shift ;;
        --check)     CHECK=1; shift ;;
        --dry-run)   DRY=1; shift ;;
        --skip-models) SKIP_MODELS=1; shift ;;
        --uninstall) UNINSTALL=1; shift ;;
        -h|--help)   usage; exit 0 ;;
        *)           usage; die "unknown option: $1" ;;
    esac
done

# ── uninstall ────────────────────────────────────────────────────────────────
if [ "$UNINSTALL" = "1" ]; then
    if [ -x "$PREFIX/rijo" ]; then
        info "removing $PREFIX/rijo"
        [ "$DRY" = "1" ] || rm -f "$PREFIX/rijo"
    else
        info "nothing installed at $PREFIX/rijo"
    fi
    info "your data in ~/.rijo is untouched (config, sessions, skills, memory)"
    exit 0
fi

# ── prerequisites ────────────────────────────────────────────────────────────
case "$(uname -s)" in
    Linux) ;;
    Darwin) warn "macOS is untested; the tool layer is Linux-first" ;;
    *) die "unsupported OS: $(uname -s)" ;;
esac

need_rust=0
if [ "$MODE" = "source" ]; then
    if command -v cargo >/dev/null 2>&1; then
        RUSTC_V="$(rustc --version 2>/dev/null || true)"
        if [ -n "$RUSTC_V" ]; then
            info "toolchain: $RUSTC_V"
            minor="$(printf '%s' "$RUSTC_V" | sed -n 's/^rustc 1\.\([0-9]*\).*/\1/p')"
            if [ -n "$minor" ] && [ "$minor" -lt 75 ]; then
                warn "Rust 1.$minor found; the pinned dependencies want 1.75 or newer"
            fi
        else
            warn "rustc not on PATH (cargo is) — continuing"
        fi
    else
        need_rust=1
    fi
fi

if [ "$MODE" = "binary" ]; then
    command -v curl >/dev/null 2>&1 || die "curl is required"
    command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1 \
        || warn "no sha256sum/shasum found — checksum verification will be skipped"
fi

if [ "$need_rust" = "1" ]; then
    cat >&2 <<'EOF'
error: cargo not found.

Rijo Code is written in Rust. Install the toolchain (user-local, no sudo):

    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
    source "$HOME/.cargo/env"

Then run this script again.
EOF
    exit 1
fi

if [ "$CHECK" = "1" ]; then
    info "prefix: $PREFIX"
    info "mode:   $MODE"
    [ "$MODE" = "source" ] && info "source: $(cd "$(dirname "$0")" && pwd)"
    info "checks passed"
    exit 0
fi

# ── install ──────────────────────────────────────────────────────────────────
mkdir -p "$PREFIX" 2>/dev/null || die "cannot create $PREFIX"

if [ "$MODE" = "source" ]; then
    SRC="$(cd "$(dirname "$0")" && pwd)"
    [ -f "$SRC/Cargo.toml" ] || die "no Cargo.toml in $SRC — run install.sh from the repo"
    info "building release binary (this takes a few minutes; first build downloads deps and the ONNX runtime)"
    if [ "$DRY" = "1" ]; then
        info "dry-run: would run 'cargo build --release' in $SRC"
        info "dry-run: would install $SRC/target/release/rijo -> $PREFIX/rijo"
        info "dry-run: would download the embedding model once into ~/.rijo/models (~130 MB)"
        exit 0
    fi
    ( cd "$SRC" && cargo build --release )
    BIN="$SRC/target/release/rijo"
else
    ARCH="$(uname -m)"
    ASSET="rijo-${ARCH}-linux.tar.gz"
    if [ "$VERSION_TAG" = "latest" ]; then
        URL_BASE="$REPO_URL/releases/latest/download"
    else
        URL_BASE="$REPO_URL/releases/download/$VERSION_TAG"
    fi
    # Mirror/test hook: install from your own release directory without having
    # the assets on GitHub.
    URL_BASE="${RIJO_RELEASE_URL_BASE:-$URL_BASE}"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    info "downloading $URL_BASE/$ASSET"
    [ "$DRY" = "1" ] && { info "dry-run: would download $URL_BASE/$ASSET"; exit 0; }
    curl -fsSL "$URL_BASE/$ASSET" -o "$TMP/rijo.tar.gz" \
        || die "download failed — is there a release asset named $ASSET?"
    if curl -fsSL "$URL_BASE/$ASSET.sha256" -o "$TMP/rijo.sha256" 2>/dev/null; then
        # The checksum file names the asset, the local copy is named differently —
        # so compare the hashes instead of using `sha256sum -c`.
        EXPECTED="$(awk '{print $1}' "$TMP/rijo.sha256")"
        if command -v sha256sum >/dev/null 2>&1; then
            ACTUAL="$(sha256sum "$TMP/rijo.tar.gz" | awk '{print $1}')"
        else
            ACTUAL="$(shasum -a 256 "$TMP/rijo.tar.gz" | awk '{print $1}')"
        fi
        [ -n "$EXPECTED" ] || die "checksum file is empty"
        [ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch — refusing to install"
        info "checksum verified"
    else
        warn "no checksum file published for this release"
    fi
    tar -xzf "$TMP/rijo.tar.gz" -C "$TMP"
    BIN="$(find "$TMP" -maxdepth 2 -type f -name rijo | head -1)"
    [ -n "$BIN" ] || die "archive did not contain a 'rijo' binary"
fi

[ -x "$BIN" ] || die "built binary not found at $BIN"
install -m 755 "$BIN" "$PREFIX/rijo"
info "installed: $PREFIX/rijo ($(du -h "$PREFIX/rijo" | cut -f1))"

"$PREFIX/rijo" --version >/dev/null 2>&1 && info "smoke test: $("$PREFIX/rijo" --version)" \
    || warn "binary did not answer --version"

# ── model cache (once, not on every start) ───────────────────────────────────
# The embedding model + tokenizer are ~130 MB. Downloading them once here means
# no later start has to, and no project directory gets its own copy.
if [ "${SKIP_MODELS:-0}" != "1" ]; then
    echo ":: fetching the embedding model (once, ~130 MB) ..."
    # An installer must not be interactive: --download-models no longer triggers
    # onboarding, but the trust prompt is separate, so both are pre-set for the
    # call. Without this a fresh machine waited on stdin.
    if RIJO_SKIP_ONBOARD=1 RIJO_SKIP_TRUST=1 "$PREFIX/rijo" --download-models 2>&1 | sed 's/^/   /'; then
        :
    else
        echo "   !! model download failed - Rijo Code will retry on first use"
    fi
fi

# ── next steps ───────────────────────────────────────────────────────────────
case ":$PATH:" in
    *":$PREFIX:"*) ;;
    *) warn "$PREFIX is not in \$PATH. Add it:"
       printf '\n    export PATH="%s:$PATH"\n\n' "$PREFIX" ;;
esac

cat <<EOF

Next steps
  1. cd into a project and run:  rijo
     The first start shows a short onboarding (provider, base URL, model, API key)
     and writes ~/.rijo/config.yaml with mode 600.
  2. Useful flags:  rijo --tools          list the registered tools
                    rijo --query "…"      one-shot run, no TUI
                    rijo --resume         continue the last session
  3. State lives in ~/.rijo/ (config.yaml, data/, skills/). Uninstall with:
       $0 --uninstall   &&   rm -rf ~/.rijo

Docs: $REPO_URL#readme   ·   Install details: $REPO_URL/blob/master/docs/INSTALL.md
EOF
