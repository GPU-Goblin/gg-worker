#!/usr/bin/env bash
# GPU Goblin worker installer (Linux x86_64, no root).
#
#   curl -fsSLO https://grid.gpugoblin.com/releases/latest/install.sh
#   less install.sh                      # read it: it is short, and it is what you trust
#   bash install.sh [--gpu | --cuda] [--server-url URL] [--release-url URL] [--allowed-signers FILE]
#
# What it does, in order, stopping at the first failure:
#   1. downloads SHA256SUMS and SHA256SUMS.sshsig from the release directory;
#   2. checks the signature with ssh-keygen against the release key written below (or the
#      --allowed-signers file you pass): nothing downloaded is run to verify anything;
#   3. checks the network and downloads only the files it needs, then checks them with sha256sum;
#      with --cuda, also the official LibTorch archive, checked against the digest the release signs;
#   4. installs them under ~/.local/lib/gpu-goblin/<version>/ and points "current" at it;
#   5. writes ~/.config/gpu-goblin/worker.toml from the signed template (never overwrites yours);
#   6. creates the worker key, runs `gg-worker doctor` and refuses to go on unless it passes;
#   7. installs and starts the systemd user service gg-worker (unless --no-start).
# The only use of sudo is to install bubblewrap from your distribution, and only if you say yes.
#
# The release key used the first time is pinned in ~/.config/gpu-goblin/allowed_signers; a later run
# with another key stops. Update and rollback: INSTALL.md (verify the new install.sh with the pinned
# key before running it, then run it: the new version is installed next to the old one).
set -euo pipefail
umask 022

# --- What the operator of the grid sets before handing out this file ---------------------------
# Public address of the grid (one place to change on the contributor side).
DEFAULT_SERVER_URL="https://grid.gpugoblin.com"
# Network identifier of the grid; a release built for another network is refused.
DEFAULT_NETWORK="gg-beta-1"
# The release public key, as printed by `gg release keygen` / `gg release ssh-key <hex>`. Compare it
# with the one the operator gave you through another channel (in person, signed mail, lab page).
RELEASE_ALLOWED_SIGNERS='release@gpu-goblin namespaces="gg-release" REPLACE_WITH_THE_RELEASE_PUBLIC_KEY'
# ------------------------------------------------------------------------------------------------

PRINCIPAL="release@gpu-goblin"
NAMESPACE="gg-release"

server_url="$DEFAULT_SERVER_URL"
release_url=""
network="$DEFAULT_NETWORK"
variant="cpu"
allowed_signers_file=""
libtorch_archive=""
start=yes
assume_yes=no
prefix="$HOME/.local/lib/gpu-goblin"
config_dir="${XDG_CONFIG_HOME:-$HOME/.config}/gpu-goblin"
data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/gpu-goblin"
bin_dir="$HOME/.local/bin"
unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

usage() {
	sed -n '2,21p' "$0" | sed 's/^# \{0,1\}//'
	cat <<EOF
Options:
  --server-url URL        grid address (default $DEFAULT_SERVER_URL)
  --release-url URL       directory holding the release (default <server-url>/releases/latest)
  --network ID            expected network (default $DEFAULT_NETWORK)
  --gpu                   install the GPU (Vulkan) build instead of the CPU one
  --cuda                  NVIDIA cards: the native CUDA engine, several times faster on convolutional
                          networks; downloads LibTorch (3.9 GB, 6.0 GB unpacked) from PyTorch
  --libtorch-archive FILE with --cuda: use this copy of the LibTorch archive instead of downloading
                          it (checked against the same signed digest)
  --enroll CODE           enrollment code from the contributor portal (gge_...): the machine
                          attaches itself to your account on its first start
  --allowed-signers FILE  OpenSSH allowed_signers file with the release key (instead of the one above)
  --no-start              install and check, but do not enable or start the service
  --yes                   answer yes to questions (bubblewrap installation)
EOF
}

die() {
	printf '\nERROR: %s\nNothing was started.\n' "$*" >&2
	exit 1
}
step() { printf '\n==> %s\n' "$*"; }

while [[ $# -gt 0 ]]; do
	case "$1" in
	--server-url) server_url="${2:?}" && shift 2 ;;
	--release-url) release_url="${2:?}" && shift 2 ;;
	--network) network="${2:?}" && shift 2 ;;
	--gpu) variant=gpu && shift ;;
	--cuda) variant=cuda && shift ;;
	--libtorch-archive) libtorch_archive="${2:?}" && shift 2 ;;
	--enroll) enroll="${2:?}" && shift 2 ;;
	--allowed-signers) allowed_signers_file="${2:?}" && shift 2 ;;
	--no-start) start=no && shift ;;
	--yes) assume_yes=yes && shift ;;
	-h | --help) usage && exit 0 ;;
	*) die "unknown option $1 (see --help)" ;;
	esac
done
enroll="${enroll:-}"
[[ -z "$libtorch_archive" || "$variant" == cuda ]] || die "--libtorch-archive goes with --cuda"
[[ -z "$enroll" || "$enroll" =~ ^gge_[A-Za-z0-9_-]{16,120}$ ]] || die "--enroll expects the code shown on the contributor portal (gge_...)"
server_url="${server_url%/}"
release_url="${release_url:-$server_url/releases/latest}"
release_url="${release_url%/}"

# --- 0. Preconditions --------------------------------------------------------------------------
step "Checking this machine"
[[ "$(uname -s)" == Linux && "$(uname -m)" == x86_64 ]] || die "Linux x86_64 only (this is $(uname -sm))"
[[ "$(id -u)" -ne 0 ]] || die "run this as the user who lends the machine, not as root"
for c in curl ssh-keygen sha256sum systemctl; do
	command -v "$c" >/dev/null || die "$c is required (curl, openssh-client, coreutils, systemd)"
done
if [[ "$variant" == cuda ]]; then
	[[ -e /dev/nvidiactl ]] || die "--cuda needs the NVIDIA driver (/dev/nvidiactl is missing); use --gpu for other cards"
	command -v unzip >/dev/null || die "unzip is required for --cuda (the LibTorch archive is a zip)"
	[[ -z "$libtorch_archive" || -f "$libtorch_archive" ]] || die "--libtorch-archive: $libtorch_archive not found"
fi
[[ "$server_url" =~ ^https?://[A-Za-z0-9.:[-]+\]?(:[0-9]+)?$ ]] || die "--server-url must look like https://host[:port]"
case "$server_url" in
https://*) ;;
http://127.0.0.1* | http://localhost* | http://\[::1\]*) echo "note: plain http is only accepted on this machine" ;;
*) die "--server-url must use https" ;;
esac

if ! command -v bwrap >/dev/null; then
	echo "bubblewrap (bwrap) is missing: the executor cannot be sandboxed without it."
	installer=""
	if command -v apt-get >/dev/null; then
		installer="apt-get install -y bubblewrap"
	elif command -v dnf >/dev/null; then
		installer="dnf install -y bubblewrap"
	fi
	[[ -n "$installer" ]] || die "install the bubblewrap package with your package manager, then run this again"
	answer=no
	if [[ "$assume_yes" == yes ]]; then
		answer=yes
	elif [[ -t 0 ]]; then
		read -r -p "Install it now with 'sudo $installer'? [y/N] " reply
		[[ "$reply" =~ ^[yY] ]] && answer=yes
	fi
	[[ "$answer" == yes ]] || die "bubblewrap is required: sudo $installer"
	# shellcheck disable=SC2086
	sudo $installer || die "bubblewrap installation failed"
	command -v bwrap >/dev/null || die "bwrap still not found after installation"
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

fetch() {
	curl -fsSL --proto '=https,http' --retry 3 --max-time 1800 -o "$work/$1" "$release_url/$1" ||
		die "cannot download $release_url/$1"
}

# --- 1-2. Signature of the checksum list, with OpenSSH ------------------------------------------
step "Verifying the release signature ($release_url)"
if [[ -n "$allowed_signers_file" ]]; then
	cp "$allowed_signers_file" "$work/allowed_signers"
else
	[[ "$RELEASE_ALLOWED_SIGNERS" != *REPLACE_WITH* ]] ||
		die "this copy of install.sh carries no release key: ask the operator for it (--allowed-signers FILE)"
	printf '%s\n' "$RELEASE_ALLOWED_SIGNERS" >"$work/allowed_signers"
fi
echo "trusted release key: $(awk '{print $NF}' "$work/allowed_signers" | head -n 1)"
# The key of the first installation is pinned: a later run with another key stops here, so that a
# key change is always a deliberate step (rotation announced by the operator), never a silent one.
pinned="$config_dir/allowed_signers"
if [[ -f "$pinned" && -z "$allowed_signers_file" ]] && ! cmp -s "$pinned" "$work/allowed_signers"; then
	die "the release key differs from the one pinned at installation ($pinned). If the operator announced a new key, check it out of band, then run again with --allowed-signers <file>"
fi
fetch SHA256SUMS
fetch SHA256SUMS.sshsig
ssh-keygen -Y verify -f "$work/allowed_signers" -I "$PRINCIPAL" -n "$NAMESPACE" \
	-s "$work/SHA256SUMS.sshsig" <"$work/SHA256SUMS" ||
	die "the signature of SHA256SUMS does not verify with the trusted release key: do not use this release"

# From here on, SHA256SUMS is trusted: it is exactly what the release key signed.
release_network="$(sed -n 's/^# network: //p' "$work/SHA256SUMS")"
version="$(sed -n 's/^# version: //p' "$work/SHA256SUMS")"
[[ "$release_network" == "$network" ]] ||
	die "this release is for network '$release_network', this grid is '$network'"
[[ "$version" =~ ^[0-9A-Za-z._+-]{1,64}$ ]] || die "unexpected version string in SHA256SUMS"
echo "signed release $version for network $network"

# --- 3. Files, checked against the signed list ------------------------------------------------
if [[ "$variant" == gpu ]]; then
	worker_src=gg-worker-gpu executor_src=gg-executor-gpu engine=gpu
elif [[ "$variant" == cuda ]]; then
	# The worker itself never computes: only its executor links LibTorch (ADR-0039).
	worker_src=gg-worker-gpu executor_src=gg-executor-cuda engine=cuda
else
	worker_src=gg-worker executor_src=gg-executor engine=cpu
fi
needed=("$worker_src" "$executor_src" worker.toml.template gg-worker.service)
[[ "$variant" == cuda ]] && needed+=(libtorch.sha256)
: >"$work/needed.sums"
for f in "${needed[@]}"; do
	n="$(awk -v f="$f" '$2 == f' "$work/SHA256SUMS" | wc -l)"
	[[ "$n" -eq 1 ]] || die "$f is not listed exactly once in the signed SHA256SUMS"
	awk -v f="$f" '$2 == f' "$work/SHA256SUMS" >>"$work/needed.sums"
done
step "Downloading ${needed[*]}"
for f in "${needed[@]}"; do fetch "$f"; done
step "Checking SHA-256 digests"
(cd "$work" && sha256sum --strict -c needed.sums) || die "a downloaded file does not match the signed digest"

# --- 3b. LibTorch for --cuda, checked against the digest in the signed libtorch.sha256 ---------------
libtorch_lib=""
if [[ "$variant" == cuda ]]; then
	lt_url="$(sed -n 's/^# url: //p' "$work/libtorch.sha256")"
	lt_size="$(sed -n 's/^# size: //p' "$work/libtorch.sha256")"
	lt_sum="$(awk '!/^#/ && NF == 2 {print $1; exit}' "$work/libtorch.sha256")"
	lt_name="$(awk '!/^#/ && NF == 2 {print $2; exit}' "$work/libtorch.sha256")"
	[[ "$lt_url" =~ ^https://download\.pytorch\.org/libtorch/[A-Za-z0-9./%+_-]+\.zip$ ]] || die "unexpected LibTorch address in the signed libtorch.sha256"
	[[ "$lt_sum" =~ ^[0-9a-f]{64}$ && "$lt_size" =~ ^[0-9]+$ && "$lt_name" =~ ^[A-Za-z0-9.+_-]+\.zip$ ]] || die "malformed signed libtorch.sha256"
	# Content-addressed by the archive's digest: an unpacked copy is reused only for that digest.
	lt_dir="$prefix/libtorch/$lt_sum"
	libtorch_lib="$lt_dir/libtorch/lib"
	if [[ -f "$lt_dir/.verified" && "$(cat "$lt_dir/.verified")" == "$lt_sum" && -f "$libtorch_lib/libtorch_cuda.so" ]]; then
		echo "LibTorch already installed and verified: $lt_dir"
	else
		mkdir -p "$prefix/libtorch"
		avail="$(df --output=avail -B1 "$prefix/libtorch" | tail -n 1)"
		(( avail > lt_size * 4 )) || die "not enough free space in $prefix for LibTorch (about $(( lt_size * 4 / 1000000000 )) GB needed)"
		archive="$prefix/libtorch/.download-$lt_sum.zip"
		if [[ -n "$libtorch_archive" ]]; then
			step "Using the LibTorch archive $libtorch_archive"
			cp "$libtorch_archive" "$archive"
		else
			step "Downloading LibTorch from PyTorch ($(( lt_size / 1000000 )) MB): $lt_url"
			curl -fSL --proto '=https' --retry 3 --max-time 7200 -o "$archive" "$lt_url" || die "cannot download $lt_url"
		fi
		step "Checking the LibTorch archive against the signed digest"
		[[ "$(stat -c %s "$archive")" == "$lt_size" ]] || { rm -f "$archive"; die "the LibTorch archive has the wrong size: do not use it"; }
		printf '%s  %s\n' "$lt_sum" "$archive" | sha256sum --strict -c >/dev/null || { rm -f "$archive"; die "the LibTorch archive does not match the signed digest: do not use it"; }
		rm -rf "$lt_dir.partial" && mkdir -p "$lt_dir.partial"
		unzip -q "$archive" -d "$lt_dir.partial" || die "cannot unpack the LibTorch archive"
		[[ -f "$lt_dir.partial/libtorch/lib/libtorch_cuda.so" ]] || die "the LibTorch archive holds no libtorch/lib/libtorch_cuda.so"
		printf '%s\n' "$lt_sum" >"$lt_dir.partial/.verified"
		rm -rf "$lt_dir" && mv "$lt_dir.partial" "$lt_dir"
		rm -f "$archive"
		echo "LibTorch installed: $lt_dir"
	fi
fi

# --- 4. Install side by side, then switch -------------------------------------------------------
step "Installing version $version into $prefix/$version"
dest="$prefix/$version"
mkdir -p "$dest.partial"
install -m 0755 "$work/$worker_src" "$dest.partial/gg-worker"
install -m 0755 "$work/$executor_src" "$dest.partial/gg-executor"
install -m 0644 "$work/SHA256SUMS" "$work/SHA256SUMS.sshsig" "$dest.partial/"
rm -rf "$dest"
mv "$dest.partial" "$dest"
previous="$(readlink "$prefix/current" 2>/dev/null || true)"
ln -sfn "$version" "$prefix/current.new"
mv -T "$prefix/current.new" "$prefix/current"
mkdir -p "$bin_dir"
ln -sfn "$prefix/current/gg-worker" "$bin_dir/gg-worker"
[[ -n "$previous" && "$previous" != "$version" ]] && echo "previous version kept: $prefix/$previous"

# --- 5. Configuration ------------------------------------------------------------------------------
config="$config_dir/worker.toml"
mkdir -p "$config_dir" "$data_dir"
chmod 700 "$data_dir"
install -m 0644 "$work/allowed_signers" "$pinned"
if [[ -f "$config" ]]; then
	echo "keeping your existing $config"
else
	name="$(hostname -s 2>/dev/null || echo worker)"
	sed -e "s#@SERVER_URL@#$server_url#" -e "s#@DATA_DIR@#$data_dir#" \
		-e "s#@DISPLAY_NAME@#$name#" -e "s#@ENGINE@#$engine#" \
		-e "s#@ENROLLMENT_TOKEN@#$enroll#" \
		"$work/worker.toml.template" >"$config.new"
	! grep -q '@[A-Z_]*@' "$config.new" || die "template markers left in $config.new"
	chmod 600 "$config.new"
	mv "$config.new" "$config"
	echo "wrote $config"
fi
# Sets `key = value` in the configuration, replacing an existing line or adding one at the top.
set_config() {
	if grep -q "^$1 *=" "$config"; then
		sed -i "s#^$1 *=.*#$1 = $2#" "$config"
	else
		printf '%s = %s\n' "$1" "$2" >"$config.new" && cat "$config" >>"$config.new" && mv "$config.new" "$config"
	fi
}
if [[ "$variant" == cuda ]]; then
	# Asked for explicitly: also on a reinstall over an existing configuration.
	set_config engine '"cuda"'
	set_config libtorch_dir "\"$libtorch_lib\""
	if ! grep -q '^device_memory_bytes *=' "$config" && command -v nvidia-smi >/dev/null; then
		total_mib="$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits -i 0 2>/dev/null | head -n 1 | tr -dc 0-9)"
		if [[ -n "$total_mib" ]]; then
			# 85 % of the card, a margin for the display and the driver.
			set_config device_memory_bytes "$(( total_mib * 1048576 * 85 / 100 ))"
			echo "device_memory_bytes set to 85 % of the card's $total_mib MiB (edit $config to change it)"
		fi
	fi
	chmod 600 "$config"
	echo "engine = \"cuda\", libtorch_dir = $libtorch_lib"
fi
if [[ -n "$enroll" ]]; then
	# Also on a reinstall: the code in an existing configuration is replaced, nothing else.
	if grep -q '^enrollment_token *=' "$config"; then
		sed -i "s#^enrollment_token *=.*#enrollment_token = \"$enroll\"#" "$config"
	else
		printf 'enrollment_token = "%s"\n' "$enroll" >"$config.new" && cat "$config" >>"$config.new" && mv "$config.new" "$config"
	fi
	chmod 600 "$config"   # the enrollment code attaches machines to your account: keep it private
fi

# --- 6. Identity and checks -------------------------------------------------------------------------
step "Worker identity"
worker="$prefix/current/gg-worker"
pubkey="$("$worker" --config "$config" init)" || die "gg-worker init failed"
echo "worker public key: $pubkey"

step "gg-worker doctor (sandbox, cgroups, engine)"
if ! "$worker" --config "$config" doctor; then
	cat >&2 <<'EOF'

The doctor refused this machine. Common causes:
  - cgroup_limits: your user session does not delegate cgroups. See "cgroup delegation" in the
    install guide (a drop-in for user@.service), or set cgroups = "auto" in worker.toml if you
    accept limits by rlimits only.
  - sandbox: unprivileged user namespaces are disabled (kernel.unprivileged_userns_clone=0, or an
    AppArmor restriction without a bwrap profile).
  - engine: --gpu without a usable Vulkan device, or --cuda without a working NVIDIA driver.
Fix it, then run this installer again (it is idempotent).
EOF
	die "gg-worker doctor did not pass"
fi

# --- 7. Service -----------------------------------------------------------------------------------
mkdir -p "$unit_dir"
install -m 0644 "$work/gg-worker.service" "$unit_dir/gg-worker.service"
if [[ "$start" == yes ]]; then
	step "Starting the user service gg-worker"
	systemctl --user daemon-reload || die "systemctl --user is not available in this session"
	systemctl --user enable gg-worker.service >/dev/null
	systemctl --user restart gg-worker.service
	sleep 3
	systemctl --user is-active --quiet gg-worker.service ||
		die "gg-worker did not stay up: journalctl --user -u gg-worker -n 50"
	echo "running. Logs: journalctl --user -u gg-worker -f"
	if [[ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null || echo no)" != yes ]]; then
		echo "note: it stops when you log out; to keep it running: loginctl enable-linger $USER"
	fi
else
	echo "service installed, not started (--no-start): systemctl --user enable --now gg-worker"
fi

if grep -q '^enrollment_token = "gge_' "$config"; then
	attach="It attaches itself to your contributor account on its first start: it appears on your
contributor page within a minute."
else
	attach="No enrollment code: send this public key to the operator so that they authorize it,
or run this installer again with --enroll <code from the contributor portal>:
  $pubkey
Until then the worker waits."
fi
cat <<EOF

Done: GPU Goblin worker $version ($engine) installed.
$attach
Peer-to-peer sharing is ON: your public ip:port is visible to the other workers authorized on the
same data. Turn it off in $config ([p2p] enabled = false).
EOF
