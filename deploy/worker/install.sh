#!/usr/bin/env bash
# GPU Goblin worker installer (Linux x86_64, no root).
#
#   curl -fsSLO https://grid.gpugoblin.com/releases/latest/install.sh
#   less install.sh                      # read it: it is short, and it is what you trust
#   bash install.sh [--gpu | --cuda] [--gpus all|N] [--server-url URL] [--release-url URL] [--allowed-signers FILE]
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
#   7. installs and starts the systemd user service gg-worker (unless --no-start); with --gpus, one
#      worker per graphics card: gg-worker for the first, gg-worker@1, gg-worker@2... for the others;
#   8. when the release has it, installs the GPU Goblin application (gg-desk): in the terminal
#      (`gg-desk status`, `gg-desk console`) always, with a menu entry started with the session on
#      a desktop (unless --no-desktop).
# sudo is used for two things only, each if you say yes: installing bubblewrap from your
# distribution, and, where AppArmor restricts user namespaces without a profile for bubblewrap
# (Ubuntu 24.04), installing that profile (/etc/apparmor.d/gpu-goblin-bwrap).
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
RELEASE_ALLOWED_SIGNERS='release@gpu-goblin namespaces="gg-release" ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEVp1adRtKn6v2iU8hG/jSMsIALjae/LkpY3AODuih9v'
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
desktop=yes
gpus=1
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
  --gpus all|N            with --gpu or --cuda: one worker per graphics card, all attached to the
                          same account (all: every card this machine has)
  --libtorch-archive FILE with --cuda: use this copy of the LibTorch archive instead of downloading
                          it (checked against the same signed digest)
  --enroll CODE           enrollment code from the contributor portal (gge_...): the machine
                          attaches itself to your account on its first start
  --allowed-signers FILE  OpenSSH allowed_signers file with the release key (instead of the one above)
  --no-start              install and check, but do not enable or start the service
  --no-desktop            no menu entry nor start with the session (servers: gg-desk still works
                          in the terminal)
  --yes                   answer yes to questions (bubblewrap installation, its AppArmor profile)
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
	--gpus) gpus="${2:?}" && shift 2 ;;
	--libtorch-archive) libtorch_archive="${2:?}" && shift 2 ;;
	--enroll) enroll="${2:?}" && shift 2 ;;
	--allowed-signers) allowed_signers_file="${2:?}" && shift 2 ;;
	--no-start) start=no && shift ;;
	--no-desktop) desktop=no && shift ;;
	--yes) assume_yes=yes && shift ;;
	-h | --help) usage && exit 0 ;;
	*) die "unknown option $1 (see --help)" ;;
	esac
done
enroll="${enroll:-}"
[[ -z "$libtorch_archive" || "$variant" == cuda ]] || die "--libtorch-archive goes with --cuda"
[[ -z "$enroll" || "$enroll" =~ ^gge_[A-Za-z0-9_-]{16,120}$ ]] || die "--enroll expects the code shown on the contributor portal (gge_...)"
# The graphics cards of this machine: as nvidia-smi numbers them (the order CUDA and the worker
# use), else the AMD and NVIDIA cards the kernel sees.
count_gpus() {
	if command -v nvidia-smi >/dev/null; then
		nvidia-smi -L 2>/dev/null | grep -c '^GPU ' || true
		return
	fi
	local n=0 v
	for v in /sys/class/drm/card*/device/vendor; do
		[[ "$v" =~ /card[0-9]+/device/vendor$ ]] || continue
		case "$(cat "$v")" in 0x1002 | 0x10de) n=$((n + 1)) ;; esac
	done
	echo "$n"
}
if [[ "$gpus" != 1 ]]; then
	[[ "$variant" != cpu ]] || die "--gpus goes with --gpu or --cuda"
	if [[ "$gpus" == all ]]; then
		gpus="$(count_gpus)"
		[[ "$gpus" -ge 1 ]] || die "--gpus all: no graphics card found (give their number: --gpus N)"
		echo "graphics cards found: $gpus"
	fi
	[[ "$gpus" =~ ^[0-9]+$ && "$gpus" -ge 1 && "$gpus" -le 16 ]] || die "--gpus expects all or a number from 1 to 16"
fi
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

# Ubuntu 23.10 and later grant unprivileged user namespaces only to programs whose AppArmor profile
# allows them (kernel.apparmor_restrict_unprivileged_userns = 1): without one, bubblewrap cannot
# build the sandbox. Ubuntu 24.10 and later ship a profile for it; 24.04 does not. The profile below
# is the one Ubuntu documents for a single program: bwrap may create user namespaces, as it may on
# systems without the restriction; nothing else changes.
bwrap_works() { bwrap --ro-bind / / --unshare-all --die-with-parent true 2>/dev/null; }
aa_profile=/etc/apparmor.d/gpu-goblin-bwrap
if ! bwrap_works; then
	bwrap_path="$(readlink -f "$(command -v bwrap)")"
	restricted="$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null || echo 0)"
	[[ "$restricted" == 1 && -d /etc/apparmor.d ]] ||
		die "bubblewrap cannot create its sandbox here: unprivileged user namespaces are disabled (sysctl kernel.unprivileged_userns_clone or user.max_user_namespaces)"
	others="$(grep -lsE "^[[:space:]]*profile[[:space:]]+[^[:space:]]+[[:space:]]+$bwrap_path([[:space:]]|$)" /etc/apparmor.d/* | grep -vx "$aa_profile" || true)"
	[[ -z "$others" ]] ||
		die "bubblewrap cannot create its sandbox although AppArmor has a profile for it ($others): check 'sudo aa-status' and the kernel log (journalctl -k | grep -i apparmor)"
	echo "AppArmor restricts unprivileged user namespaces on this system and has no profile for bubblewrap:"
	echo "the sandbox cannot be built. The fix is one small profile, $aa_profile, that lets $bwrap_path"
	echo "create user namespaces (remove it with: sudo apparmor_parser -R $aa_profile && sudo rm $aa_profile)."
	answer=no
	if [[ "$assume_yes" == yes ]]; then
		answer=yes
	elif [[ -t 0 ]]; then
		read -r -p "Install this AppArmor profile now (sudo)? [y/N] " reply
		[[ "$reply" =~ ^[yY] ]] && answer=yes
	fi
	[[ "$answer" == yes ]] || die "the sandbox needs this AppArmor profile; run this installer again and answer yes, or see INSTALL.md"
	aa_tmp="$(mktemp)"
	printf '%s\n' \
		"# GPU Goblin (install.sh): lets bubblewrap create the user namespaces of its sandbox, which" \
		"# AppArmor restricts on this system. Remove: sudo apparmor_parser -R $aa_profile && sudo rm $aa_profile" \
		'abi <abi/4.0>,' \
		'include <tunables/global>' \
		"profile gpu-goblin-bwrap $bwrap_path flags=(unconfined) {" \
		'  userns,' \
		'  include if exists <local/gpu-goblin-bwrap>' \
		'}' >"$aa_tmp"
	sudo install -m 0644 -o root -g root "$aa_tmp" "$aa_profile" || die "cannot write $aa_profile"
	rm -f "$aa_tmp"
	sudo apparmor_parser -r "$aa_profile" || die "AppArmor refused $aa_profile"
	bwrap_works || die "bubblewrap still cannot create its sandbox with $aa_profile loaded"
	echo "AppArmor profile installed: bubblewrap can build the sandbox."
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
# The application, when the release carries it (ADR-0043).
desk=no
if awk '$2 == "gg-desk"' "$work/SHA256SUMS" | grep -q .; then
	needed+=(gg-desk)
	desk=yes
fi
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
[[ "$desk" == yes ]] && install -m 0755 "$work/gg-desk" "$dest.partial/gg-desk"
install -m 0644 "$work/SHA256SUMS" "$work/SHA256SUMS.sshsig" "$dest.partial/"
rm -rf "$dest"
mv "$dest.partial" "$dest"
previous="$(readlink "$prefix/current" 2>/dev/null || true)"
ln -sfn "$version" "$prefix/current.new"
mv -T "$prefix/current.new" "$prefix/current"
mkdir -p "$bin_dir"
ln -sfn "$prefix/current/gg-worker" "$bin_dir/gg-worker"
[[ "$desk" == yes ]] && ln -sfn "$prefix/current/gg-desk" "$bin_dir/gg-desk"
# No desktop here (a server, SSH): the application runs in the terminal, nothing to start with a
# session that has no screen.
if [[ "$desk" == yes && "$desktop" == yes && -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}${XDG_CURRENT_DESKTOP:-}" ]]; then
	desktop=no
	echo "no desktop session: gg-desk is installed for the terminal only (gg-desk status, gg-desk console)"
fi
if [[ "$desk" == yes && "$desktop" == yes ]]; then
	# A menu entry, and the application started with the session (its tray icon).
	apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
	autostart="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
	mkdir -p "$apps" "$autostart"
	cat >"$apps/gpu-goblin.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=GPU Goblin
Comment=Lend this computer to research: pause, hours, limits
Exec=$prefix/current/gg-desk
Icon=utilities-system-monitor
Terminal=false
Categories=Utility;Science;
DESKTOP
	# With the session: straight into the system tray, the window opens from its icon.
	sed 's|^Exec=.*|Exec='"$prefix"'/current/gg-desk --tray|' "$apps/gpu-goblin.desktop" >"$autostart/gpu-goblin.desktop"
fi
[[ -n "$previous" && "$previous" != "$version" ]] && echo "previous version kept: $prefix/$previous"

# --- 5. Configuration ------------------------------------------------------------------------------
config="$config_dir/worker.toml"
mkdir -p "$config_dir" "$data_dir"
chmod 700 "$data_dir"
install -m 0644 "$work/allowed_signers" "$pinned"
name="$(hostname -s 2>/dev/null || echo worker)"
# Sets `key = value` in configuration $1, replacing an existing line or adding one at the top.
set_config() {
	if grep -q "^$2 *=" "$1"; then
		sed -i "s#^$2 *=.*#$2 = $3#" "$1"
	else
		printf '%s = %s\n' "$2" "$3" >"$1.new" && cat "$1" >>"$1.new" && mv "$1.new" "$1"
	fi
}
# One worker's configuration: file, data directory, display name, engine, card number ("" for a
# machine with one worker).
configure() {
	local file="$1" dir="$2" label="$3" eng="$4" card="$5"
	mkdir -p "$dir"
	chmod 700 "$dir"
	if [[ -f "$file" ]]; then
		echo "keeping your existing $file"
	else
		sed -e "s#@SERVER_URL@#$server_url#" -e "s#@DATA_DIR@#$dir#" \
			-e "s#@DISPLAY_NAME@#$label#" -e "s#@ENGINE@#$eng#" \
			-e "s#@ENROLLMENT_TOKEN@#$enroll#" \
			"$work/worker.toml.template" >"$file.new"
		! grep -q '@[A-Z_]*@' "$file.new" || die "template markers left in $file.new"
		chmod 600 "$file.new"
		mv "$file.new" "$file"
		echo "wrote $file"
	fi
	if [[ -n "$card" && "$card" != 0 ]]; then
		# The first worker answers the application on the usual port, the others on any free one
		# (each writes its address next to its key, where gg-desk finds it).
		set_config "$file" control_listen '"127.0.0.1:0"'
		set_config "$file" engine "\"$eng\""
	fi
	if [[ "$variant" == cuda ]]; then
		# Asked for explicitly: also on a reinstall over an existing configuration.
		set_config "$file" engine "\"$eng\""
		set_config "$file" libtorch_dir "\"$libtorch_lib\""
		if ! grep -q '^device_memory_bytes *=' "$file" && command -v nvidia-smi >/dev/null; then
			total_mib="$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits -i "${card:-0}" 2>/dev/null | head -n 1 | tr -dc 0-9)"
			if [[ -n "$total_mib" ]]; then
				# 85 % of the card, a margin for the display and the driver.
				set_config "$file" device_memory_bytes "$(( total_mib * 1048576 * 85 / 100 ))"
				echo "device_memory_bytes set to 85 % of card ${card:-0}'s $total_mib MiB (edit $file to change it)"
			fi
		fi
		echo "engine = \"$eng\", libtorch_dir = $libtorch_lib"
	fi
	if [[ -n "$enroll" ]]; then
		# Also on a reinstall: the code in an existing configuration is replaced, nothing else.
		set_config "$file" enrollment_token "\"$enroll\""
	fi
	chmod 600 "$file"   # the enrollment code attaches machines to your account: keep it private
}
# One worker per card: the first where a single worker lives, the others in gpu1, gpu2...
configs=("$config")
if [[ "$gpus" -gt 1 ]]; then
	configure "$config" "$data_dir" "$name" "$engine" 0
	for ((k = 1; k < gpus; k++)); do
		configure "$config_dir/worker-gpu$k.toml" "$data_dir/gpu$k" "$name-gpu$k" "$engine:$k" "$k"
		configs+=("$config_dir/worker-gpu$k.toml")
	done
else
	configure "$config" "$data_dir" "$name" "$engine" ""
fi

# --- 6. Identity and checks -------------------------------------------------------------------------
worker="$prefix/current/gg-worker"
pubkeys=()
for c in "${configs[@]}"; do
	step "Worker identity ($c)"
	pubkey="$("$worker" --config "$c" init)" || die "gg-worker init failed ($c)"
	echo "worker public key: $pubkey"
	pubkeys+=("$pubkey")

	step "gg-worker doctor (sandbox, cgroups, engine): $c"
	if ! "$worker" --config "$c" doctor; then
		cat >&2 <<'EOF'

The doctor refused this machine. Common causes:
  - cgroup_limits: your user session does not delegate cgroups. See "cgroup delegation" in the
    install guide (a drop-in for user@.service), or set cgroups = "auto" in worker.toml if you
    accept limits by rlimits only.
  - sandbox: unprivileged user namespaces are disabled (kernel.unprivileged_userns_clone=0, or an
    AppArmor restriction without a bwrap profile).
  - engine: --gpu without a usable Vulkan device, or --cuda without a working NVIDIA driver; with
    --gpus, a card number this machine does not have.
Fix it, then run this installer again (it is idempotent).
EOF
		die "gg-worker doctor did not pass ($c)"
	fi
done

# --- 7. Service -----------------------------------------------------------------------------------
mkdir -p "$unit_dir"
install -m 0644 "$work/gg-worker.service" "$unit_dir/gg-worker.service"
# The other cards: the same service, one instance per card (gg-worker@1 reads worker-gpu1.toml).
units=(gg-worker.service)
if [[ "$gpus" -gt 1 ]]; then
	sed -e 's#/worker\.toml run#/worker-gpu%i.toml run#' \
		-e 's#^Description=.*#Description=GPU Goblin worker for graphics card %i (gg-worker)#' \
		"$work/gg-worker.service" >"$unit_dir/gg-worker@.service.new"
	grep -q 'worker-gpu%i.toml run' "$unit_dir/gg-worker@.service.new" || die "cannot derive gg-worker@.service"
	chmod 0644 "$unit_dir/gg-worker@.service.new"
	mv "$unit_dir/gg-worker@.service.new" "$unit_dir/gg-worker@.service"
	for ((k = 1; k < gpus; k++)); do units+=("gg-worker@$k.service"); done
fi
if [[ "$start" == yes ]]; then
	step "Starting the user service${units[1]:+s} ${units[*]}"
	systemctl --user daemon-reload || die "systemctl --user is not available in this session"
	for u in "${units[@]}"; do
		systemctl --user enable "$u" >/dev/null
		systemctl --user restart "$u"
	done
	sleep 3
	for u in "${units[@]}"; do
		systemctl --user is-active --quiet "$u" ||
			die "$u did not stay up: journalctl --user -u $u -n 50"
	done
	echo "running. Logs: journalctl --user -u gg-worker -f"
	if [[ "$(loginctl show-user "$USER" -p Linger --value 2>/dev/null || echo no)" != yes ]]; then
		echo "note: it stops when you log out; to keep it running: loginctl enable-linger $USER"
	fi
else
	echo "service${units[1]:+s} installed, not started (--no-start): systemctl --user enable --now ${units[*]}"
fi

if grep -q '^enrollment_token = "gge_' "$config"; then
	attach="It attaches itself to your contributor account on its first start: it appears on your
contributor page within a minute."
else
	attach="No enrollment code: send this public key to the operator so that they authorize it,
or run this installer again with --enroll <code from the contributor portal>:
$(printf '  %s\n' "${pubkeys[@]}")
Until then the worker waits."
fi
cat <<EOF

Done: GPU Goblin worker $version ($engine) installed$([[ "$gpus" -gt 1 ]] && echo ", one per graphics card ($gpus)").
$attach
$([[ "$desk" == yes ]] && echo "See and control it: gg-desk (window), gg-desk status or gg-desk console (terminal).")
Peer-to-peer sharing is ON: your public ip:port is visible to the other workers authorized on the
same data. Turn it off in $config ([p2p] enabled = false).
EOF
