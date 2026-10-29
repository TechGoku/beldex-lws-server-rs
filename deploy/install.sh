#!/usr/bin/env bash
# Installs beldex-lws as a systemd service, configured and running.
#
#   sudo ./install.sh                       # install, configure (asks on a terminal), start
#   sudo ./install.sh --yes --network test  # no questions; any `beldex-lws setup` option works
#   sudo ./install.sh --import-lmdb /var/lib/beldex-lws-old   # bring the LMDB server's accounts
#   sudo ./install.sh --upgrade             # new binaries, same config, restart
#   sudo ./install.sh --uninstall           # remove the service and binaries, keep data
#
# Run it from an unpacked release (bin/, lib/, deploy/) or from a source tree
# after `make release`.
set -euo pipefail

PREFIX=/opt/beldex-lws
CONFIG_DIR=/etc/beldex-lws
DATA_DIR=/var/lib/beldex-lws
SERVICE_USER=beldex-lws
UNIT=/etc/systemd/system/beldex-lws.service

mode=install
start=1
import_lmdb=
destdir=
setup_args=()

usage() { sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }
say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
    case "$1" in
        --upgrade) mode=upgrade ;;
        --uninstall) mode=uninstall ;;
        --no-start) start=0 ;;
        --import-lmdb) import_lmdb=${2:?--import-lmdb needs the LMDB server\'s --db-path}; shift ;;
        --prefix) PREFIX=${2:?}; shift ;;
        --destdir) destdir=${2:?}; shift ;;   # stage files only, for packaging
        -h|--help) usage ;;
        *) setup_args+=("$1") ;;
    esac
    shift
done

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
if [ -d "$here/deploy/systemd" ]; then   # top of a release archive
    top=$here deploy=$here/deploy
else                                     # deploy/ of a release or source tree
    top=$(dirname "$here") deploy=$here
fi
if [ -x "$top/bin/beldex-lws" ]; then
    bin=$top/bin/beldex-lws lib=$top/lib/libbeldex_lws_kernel.so
else
    bin=$top/target/release/beldex-lws lib=$top/build/kernel/libbeldex_lws_kernel.so
fi

install_files() {
    local root=$1
    [ -x "$bin" ] && [ -f "$lib" ] || die "no build found: run \`make release\` first, or use a release archive"
    install -d -m 0755 "$root$PREFIX/bin" "$root$PREFIX/lib" "$root/usr/local/bin"
    # Replace by rename, so a running server keeps its old files.
    install -m 0755 "$bin" "$root$PREFIX/bin/.beldex-lws.new"
    install -m 0644 "$lib" "$root$PREFIX/lib/.libbeldex_lws_kernel.so.new"
    mv -f "$root$PREFIX/bin/.beldex-lws.new" "$root$PREFIX/bin/beldex-lws"
    mv -f "$root$PREFIX/lib/.libbeldex_lws_kernel.so.new" "$root$PREFIX/lib/libbeldex_lws_kernel.so"
    ln -sfn "$PREFIX/bin/beldex-lws" "$root/usr/local/bin/beldex-lws"
    install -d -m 0755 "$root$(dirname "$UNIT")"
    sed "s|/opt/beldex-lws|$PREFIX|g" "$deploy/systemd/beldex-lws.service" > "$root$UNIT"
    chmod 0644 "$root$UNIT"
}

check_libraries() {
    local missing
    missing=$(ldd "$PREFIX/bin/beldex-lws" 2>/dev/null | awk '/not found/ {print $1}' || true)
    [ -z "$missing" ] && return
    printf 'Missing shared libraries: %s\n' "$missing" >&2
    if command -v apt-get >/dev/null; then
        echo "Install them with: apt-get install libsodium23" >&2
    elif command -v dnf >/dev/null; then
        echo "Install them with: dnf install libsodium" >&2
    fi
    exit 1
}

as_service() { runuser -u "$SERVICE_USER" -- "$@"; }

if [ -n "$destdir" ]; then
    install_files "$destdir"
    say "staged under $destdir"
    exit 0
fi

[ "$(id -u)" -eq 0 ] || die "run as root (with sudo)"
command -v systemctl >/dev/null || die "systemd is required; see docs/OPERATIONS.md for other ways to run"

case $mode in
uninstall)
    systemctl disable --now beldex-lws 2>/dev/null || true
    rm -f "$UNIT" /usr/local/bin/beldex-lws
    rm -rf "$PREFIX"
    systemctl daemon-reload
    say "removed the service and binaries"
    echo "Kept $CONFIG_DIR and $DATA_DIR (database and keys). Delete them to remove everything."
    exit 0
    ;;
upgrade)
    [ -f "$CONFIG_DIR/lws.toml" ] || die "nothing installed yet (no $CONFIG_DIR/lws.toml); run without --upgrade"
    install_files ""
    check_libraries
    systemctl daemon-reload
    systemctl restart beldex-lws
    say "upgraded to $("$PREFIX/bin/beldex-lws" --version) and restarted"
    exit 0
    ;;
esac

say "installing to $PREFIX"
install_files ""
check_libraries
"$PREFIX/bin/beldex-lws" --version

if ! id "$SERVICE_USER" >/dev/null 2>&1; then
    say "creating the $SERVICE_USER system user"
    useradd --system --user-group --home-dir "$DATA_DIR" --shell /usr/sbin/nologin "$SERVICE_USER"
fi
install -d -m 0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$DATA_DIR"
install -d -m 0750 -o root -g "$SERVICE_USER" "$CONFIG_DIR"

if [ -f "$CONFIG_DIR/lws.toml" ]; then
    say "keeping the existing $CONFIG_DIR/lws.toml"
else
    say "configuring"
    # As the service user, so keys belong to it and the beldexd socket check
    # sees what the service will see.
    tmp=$DATA_DIR/.lws.toml.setup
    rm -f "$tmp"
    as_service "$PREFIX/bin/beldex-lws" setup --data-dir "$DATA_DIR" --out "$tmp" ${setup_args[@]+"${setup_args[@]}"}
    install -m 0640 -o root -g "$SERVICE_USER" "$tmp" "$CONFIG_DIR/lws.toml"
    rm -f "$tmp"
fi

if [ -n "$import_lmdb" ]; then
    [ -f "$import_lmdb/data.mdb" ] || die "$import_lmdb has no data.mdb"
    say "importing accounts from the LMDB server at $import_lmdb (stop that server first)"
    copy=$DATA_DIR/.lmdb-import
    rm -rf "$copy"
    install -d -m 0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$copy"
    cp "$import_lmdb/data.mdb" "$copy/"
    chown -R "$SERVICE_USER:$SERVICE_USER" "$copy"
    as_service "$PREFIX/bin/beldex-lws" admin import-lmdb --config "$CONFIG_DIR/lws.toml" "$copy"
    rm -rf "$copy"
fi

systemctl daemon-reload
if [ "$start" -eq 0 ]; then
    systemctl enable beldex-lws
    say "installed; start it with: systemctl start beldex-lws"
    exit 0
fi
systemctl enable --now beldex-lws
sleep 3
if ! systemctl is-active --quiet beldex-lws; then
    journalctl -u beldex-lws -n 30 --no-pager >&2 || true
    die "the service did not stay up (log above)"
fi

say "checking the installation"
as_service "$PREFIX/bin/beldex-lws" doctor --config "$CONFIG_DIR/lws.toml" || true

cat <<EOF

beldex-lws is running.
  config     $CONFIG_DIR/lws.toml
  data       $DATA_DIR (back up the database together with beldex-lws-seal.key)
  logs       journalctl -u beldex-lws -f
  check      sudo -u $SERVICE_USER beldex-lws doctor --config $CONFIG_DIR/lws.toml
  accounts   sudo -u $SERVICE_USER beldex-lws admin list-accounts --config $CONFIG_DIR/lws.toml
EOF
