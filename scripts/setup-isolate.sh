#!/usr/bin/env bash
# Prepares an Ubuntu 22.04/24.04 machine (bare metal, VM or WSL2) to run codexec:
#
#   1. builds and installs isolate (setuid root) and its cgroup keeper service
#   2. installs Go and pre-builds a read-only standard-library cache
#   3. installs a Rust toolchain under /opt/rustup
#
# The resulting paths match languages.toml and codexec.toml in this repository.
# Run as root:   sudo bash scripts/setup-isolate.sh
#
# Use a dedicated VM. Never run isolate inside a privileged Docker container:
# that combination is what turned the 2024 Judge0 bugs into host root.
set -euo pipefail

ISOLATE_VERSION="${ISOLATE_VERSION:-v2.7}"
GO_VERSION="${GO_VERSION:-1.24.7}"
NUM_BOXES="${NUM_BOXES:-100}"
GOCACHE_DIR=/var/cache/codexec/gocache

if [[ $EUID -ne 0 ]]; then
  echo "run as root: sudo bash $0" >&2
  exit 1
fi

step() { printf '\n==> %s\n' "$*"; }

step "Installing build dependencies"
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
  build-essential git pkg-config ca-certificates curl \
  libcap-dev libsystemd-dev libseccomp-dev python3

step "Building isolate ${ISOLATE_VERSION}"
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
git clone --depth 1 --branch "$ISOLATE_VERSION" https://github.com/ioi/isolate.git "$workdir/isolate"
make -C "$workdir/isolate" install

step "Configuring /usr/local/etc/isolate"
# Fixed UID block instead of /etc/subuid, which would need an 'isolate' system user.
sed -i \
  -e 's|^subid_user = isolate|# subid_user = isolate|' \
  -e 's|^# first_uid = 60000|first_uid = 60000|' \
  -e 's|^# first_gid = 60000|first_gid = 60000|' \
  -e "s|^# num_boxes = 1000|num_boxes = ${NUM_BOXES}|" \
  /usr/local/etc/isolate
isolate --check-config || true

cgroup_ok=no
if [[ "$(stat -fc %T /sys/fs/cgroup)" == "cgroup2fs" && -d /run/systemd/system ]]; then
  step "Starting isolate.service (delegates a cgroup v2 subtree to isolate)"
  systemctl daemon-reload
  systemctl enable --now isolate.service
  cgroup_ok=yes
fi

if ! command -v /usr/local/go/bin/go >/dev/null; then
  step "Installing Go ${GO_VERSION}"
  arch="$(dpkg --print-architecture)"
  curl -fsSL "https://go.dev/dl/go${GO_VERSION}.linux-${arch}.tar.gz" | tar -C /usr/local -xz
fi

step "Pre-building the Go standard library cache (read-only for sandboxes)"
# Must use the same environment as [go.compile.env] in languages.toml, or the cache keys will not match.
# Re-run this step after every Go upgrade.
rm -rf "$GOCACHE_DIR"
mkdir -p "$GOCACHE_DIR"
env -i PATH=/usr/local/go/bin:/usr/bin:/bin HOME=/tmp GOCACHE="$GOCACHE_DIR" GOPATH=/tmp/gopath \
  GOFLAGS=-buildvcs=false GOTOOLCHAIN=local CGO_ENABLED=0 \
  /usr/local/go/bin/go build std
# The go tool rewrites trim.txt once a day and fails the build if it cannot.
# Inside the sandbox the cache is read-only, so point trim.txt at the box's private /tmp.
ln -sfn /tmp/trim.txt "$GOCACHE_DIR/trim.txt"
chmod -R a+rX /var/cache/codexec

if [[ ! -x /opt/rustup/toolchains/stable-$(uname -m)-unknown-linux-gnu/bin/rustc ]]; then
  step "Installing Rust (minimal profile) under /opt/rustup"
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | \
    RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo sh -s -- -y --profile minimal --no-modify-path --default-toolchain stable
  chmod -R a+rX /opt/rustup /opt/cargo
fi

step "Smoke test"
isolate --version | head -1
cg_flag=(); [[ $cgroup_ok == yes ]] && cg_flag=(--cg)
isolate "${cg_flag[@]}" --box-id=99 --cleanup >/dev/null 2>&1 || true
isolate "${cg_flag[@]}" --box-id=99 --init >/dev/null
echo "hello from the sandbox" | isolate "${cg_flag[@]}" --box-id=99 --silent --run -- /usr/bin/cat
isolate "${cg_flag[@]}" --box-id=99 --cleanup

echo
if [[ $cgroup_ok == yes ]]; then
  echo "Done. cgroup v2 is active: keep 'use_cgroups = true' in codexec.toml."
else
  cat <<'EOF'
Done, but cgroup v2 with systemd was NOT detected, so isolate cannot use --cg.
Set 'use_cgroups = false' in codexec.toml for development, or enable cgroup v2:
  WSL2:  /etc/wsl.conf            [boot]  systemd=true
         %UserProfile%\.wslconfig [wsl2]  kernelCommandLine = cgroup_no_v1=all
         then run 'wsl --shutdown' from Windows and re-run this script.
EOF
fi
echo "Next: cargo build --release && ./target/release/codexec check-problem problems/*"
