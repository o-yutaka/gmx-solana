#!/usr/bin/env bash
# GMTrade PoCs — Codespaces setup (idempotent; safe to re-run).
#
# Invoked automatically by .devcontainer/devcontainer.json
# (postCreateCommand), or by hand:  bash poc-pack/setup.sh
#
# Assumed layout (see README-codespaces.md):
#   <repo root>/                  a checkout of gmsol-labs/gmx-solana
#                                 (your fork, opened as the codespace)
#   <repo root>/poc-pack/         THIS pack (setup.sh, run_all.sh,
#                                 both test .rs files, both snippets)
# If no gmx-solana checkout is found above the pack, the repo is
# cloned into <cwd>/gmx-solana and the pack is expected at
# gmx-solana/poc-pack/ after you copy it in — the clone branch below
# only fires outside Codespaces.
#
# Pinned versions (from the PoC READMEs / repo Anchor.toml):
#   rustup toolchain 1.81.0 | agave (solana) 2.1.21 | anchor-cli 0.31.1
# TODO-VERIFY: the repo has no rust-toolchain file; 1.81.0 is the
# known-good host toolchain from run_poc.sh. If the build disagrees,
# match the rustc used by the repo's CI workflows.
set -euo pipefail

REPO_URL="https://github.com/gmsol-labs/gmx-solana.git"
REPO_REF="a10a6599a42442aac2b4873fb058616a40e07f27" # analysed main
PACK_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ---------------------------------------------------------------- repo
# Locate (or create) the gmx-solana checkout. In Codespaces the
# workspace root is the repo itself and poc-pack/ sits inside it.
if [ -f "$PACK_DIR/../programs/store/Cargo.toml" ]; then
  REPO_ROOT="$(cd "$PACK_DIR/.." && pwd)"
elif [ -f "$PWD/programs/store/Cargo.toml" ]; then
  REPO_ROOT="$PWD"
else
  REPO_ROOT="$PWD/gmx-solana"
  if [ ! -d "$REPO_ROOT/.git" ]; then
    echo "==> Cloning gmx-solana into $REPO_ROOT"
    git clone "$REPO_URL" "$REPO_ROOT"
  fi
fi
echo "==> Repo root: $REPO_ROOT"
cd "$REPO_ROOT"

CURRENT_REF="$(git rev-parse HEAD)"
if [ "$CURRENT_REF" != "$REPO_REF" ]; then
  git fetch origin "$REPO_REF" || git fetch origin
  git checkout "$REPO_REF"
fi

# ---------------------------------------------------------------- rust
echo "==> Rust toolchain 1.81.0"
if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
rustup toolchain install 1.81.0
rustup default 1.81.0

# ------------------------------------------------------- agave (solana)
echo "==> Agave / Solana 2.1.21"
if command -v solana >/dev/null 2>&1 && solana --version | grep -q "2\.1\.21"; then
  echo "    already at $(solana --version)"
else
  # Official installer pulls prebuilt release binaries (fast route,
  # same as run_poc.sh) — no source build.
  sh -c "$(curl -sSfL https://release.anza.xyz/v2.1.21/install)"
fi
export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"

# ---------------------------------------------------------------- anchor
# Anchor CLI is OPTIONAL for these PoCs — nothing in the build/test
# path invokes it: the Actions workflows build the SBF programs with
# `cargo build-sbf` and run the suites with plain `cargo test`, and
# the tests themselves are solana-program-test integration tests
# (they link the anchor *framework crates* as libraries; they never
# shell out to the anchor CLI). `anchor build` survives only as a
# documented fallback if cargo-build-sbf ever goes missing, and the
# Codespaces venue's run_all.sh uses it. So this install is
# BEST-EFFORT: try hard, but never let it fail the setup.
#
# Why the avm build below runs under a newer toolchain: avm is
# compiled from source, and its dependency tree now contains
# edition2024 crates that the pinned Rust 1.81.0 cargo cannot even
# parse ("feature `edition2024` is required", exit 101 — observed
# in Actions run 40901967661, where this step killed both jobs
# before any test ran). Building avm with RUSTUP_TOOLCHAIN=stable
# fixes that while leaving the rustup default at 1.81.0, so the
# project build/test steps still use the pinned toolchain.
# Building anchor-cli itself from source
#   cargo install --git https://github.com/coral-xyz/anchor \
#     --tag v0.31.1 anchor-cli --locked
# remains the manual last resort (the run_poc.sh route) if the avm
# route fails too.
echo "==> Anchor CLI 0.31.1 (optional, best-effort)"
install_anchor() {
  if ! command -v avm >/dev/null 2>&1; then
    rustup toolchain install stable
    RUSTUP_TOOLCHAIN=stable \
      cargo install --git https://github.com/coral-xyz/anchor avm --locked --force
  fi
  export PATH="$HOME/.cargo/bin:$PATH"
  # avm downloads the prebuilt anchor 0.31.1 binary from the anchor
  # GitHub releases (no compilation, so the toolchain pin is moot).
  avm install 0.31.1
  avm use 0.31.1
}
if command -v anchor >/dev/null 2>&1 && anchor --version | grep -q "0\.31\.1"; then
  echo "    already at $(anchor --version)"
elif ! install_anchor; then
  # install_anchor is invoked in a condition, so `set -e` is
  # suppressed inside it and any failure lands here instead of
  # aborting the setup.
  echo "WARNING: Anchor CLI install failed — continuing WITHOUT anchor." >&2
  echo "         Safe for the Actions suites (cargo build-sbf + cargo test)." >&2
  echo "         Only the Codespaces run_all.sh path (anchor build) needs it." >&2
fi

# ------------------------------------------------------------ PoC files
echo "==> Installing PoC tests + dev-dependency snippets"
# Test files and the treasury snippet live in poc-pack/ per the
# README's layout; if the pack was copied without them, fall back to
# the parent directory (the layout of poc-implementation/ itself).
pick() { # pick <filename> -> path
  if [ -f "$PACK_DIR/$1" ]; then echo "$PACK_DIR/$1";
  elif [ -f "$PACK_DIR/../$1" ]; then echo "$PACK_DIR/../$1";
  else echo "ERROR: $1 not found in $PACK_DIR or its parent" >&2; exit 1; fi
}
cp "$(pick gt_exchange_freeze.rs)"    programs/treasury/tests/gt_exchange_freeze.rs
cp "$(pick gt_referral_close_time.rs)" programs/store/tests/gt_referral_close_time.rs

# Idempotent snippet application: skip if a previous run already
# added solana-program-test to that crate's manifest.
if grep -q "solana-program-test" programs/treasury/Cargo.toml; then
  echo "    treasury dev-deps already present, skipping snippet"
else
  cat "$(pick Cargo.dev-deps.snippet.toml)" >> programs/treasury/Cargo.toml
fi
if grep -q "solana-program-test" programs/store/Cargo.toml; then
  echo "    store dev-deps already present, skipping snippet"
else
  cat "$PACK_DIR/Cargo.dev-deps.store.snippet.toml" >> programs/store/Cargo.toml
fi

# ---------------------------------------------------------------- verify
echo "==> Toolchain check"
rustc --version
cargo --version
solana --version
# Anchor is optional (best-effort above): report, never fail on it.
anchor --version || echo "anchor: not installed (optional — see the anchor section above)"
echo "==> Repo at $(git rev-parse --short HEAD) (expect ${REPO_REF:0:7})"
echo "==> Setup complete. Run the tests with:  bash poc-pack/run_all.sh"
