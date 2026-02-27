#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
One-shot installer for rapid_driver systemd service.

Usage:
  ./scripts/install_systemd.sh [options]

Options:
  --user <name>            Service user (default: current user, or SUDO_USER)
  --group <name>           Service group (default: same as user)
  --bind <host:port>       API bind address (default: 0.0.0.0:7400)
  --service-name <name>    systemd unit name without suffix (default: rapid_driver)
  --binary-path <path>     Install target path (default: /usr/local/bin/rapid_driver)
  --workdir <path>         Runtime base dir (default: /var/lib/rapid_driver)
  --skip-build             Skip cargo build --release
  --no-mask                Pass --no-mask to rapid_driver serve
  --no-act-blink           Disable ACT LED 3-blink on successful service start
  -h, --help               Show this help

Examples:
  ./scripts/install_systemd.sh
  ./scripts/install_systemd.sh --user pi --group pi --bind 0.0.0.0:7400
EOF
}

if ! command -v systemctl >/dev/null 2>&1; then
  echo "Error: systemctl not found. This script requires systemd." >&2
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "Error: cargo not found in PATH." >&2
  exit 1
fi

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_USER="${SUDO_USER:-$(id -un)}"

SERVICE_USER="$DEFAULT_USER"
SERVICE_GROUP=""
BIND_ADDR="0.0.0.0:7400"
SERVICE_NAME="rapid_driver"
BINARY_PATH="/usr/local/bin/rapid_driver"
WORKDIR="/var/lib/rapid_driver"
SKIP_BUILD="false"
NO_MASK="false"
ACT_BLINK="true"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --user)
      SERVICE_USER="${2:-}"
      shift 2
      ;;
    --group)
      SERVICE_GROUP="${2:-}"
      shift 2
      ;;
    --bind)
      BIND_ADDR="${2:-}"
      shift 2
      ;;
    --service-name)
      SERVICE_NAME="${2:-}"
      shift 2
      ;;
    --binary-path)
      BINARY_PATH="${2:-}"
      shift 2
      ;;
    --workdir)
      WORKDIR="${2:-}"
      shift 2
      ;;
    --skip-build)
      SKIP_BUILD="true"
      shift
      ;;
    --no-mask)
      NO_MASK="true"
      shift
      ;;
    --no-act-blink)
      ACT_BLINK="false"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Error: unknown option: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [[ -z "$SERVICE_GROUP" ]]; then
  SERVICE_GROUP="$SERVICE_USER"
fi

if [[ -z "$SERVICE_USER" || -z "$SERVICE_GROUP" || -z "$BIND_ADDR" || -z "$SERVICE_NAME" || -z "$BINARY_PATH" || -z "$WORKDIR" ]]; then
  echo "Error: invalid empty argument." >&2
  exit 2
fi

if ! id "$SERVICE_USER" >/dev/null 2>&1; then
  echo "Error: user '$SERVICE_USER' does not exist." >&2
  exit 2
fi

if ! getent group "$SERVICE_GROUP" >/dev/null 2>&1; then
  echo "Error: group '$SERVICE_GROUP' does not exist." >&2
  exit 2
fi

SUDO=""
if [[ "${EUID}" -ne 0 ]]; then
  if command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
  else
    echo "Error: root permissions required (run as root or install sudo)." >&2
    exit 1
  fi
fi

BIN_DIR="$(dirname "$BINARY_PATH")"
HELPER_DIR="/usr/local/lib/${SERVICE_NAME}"
ACT_BLINK_SCRIPT="${HELPER_DIR}/blink_act_led.sh"
STATE_HOME="${WORKDIR}/state"
CONFIG_HOME="${WORKDIR}/config"
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
JOURNALD_DROPIN_DIR="/etc/systemd/journald.conf.d"
JOURNALD_DROPIN_FILE="${JOURNALD_DROPIN_DIR}/${SERVICE_NAME}.conf"

SERVE_ARGS=(serve --bind "$BIND_ADDR")
if [[ "$NO_MASK" == "true" ]]; then
  SERVE_ARGS+=(--no-mask)
fi

echo "[1/9] Building release binary..."
if [[ "$SKIP_BUILD" != "true" ]]; then
  (cd "$PROJECT_DIR" && cargo build --release)
else
  echo "      Skipped (--skip-build)."
fi

if [[ ! -x "${PROJECT_DIR}/target/release/rapid_driver" ]]; then
  echo "Error: ${PROJECT_DIR}/target/release/rapid_driver not found or not executable." >&2
  exit 1
fi

echo "[2/9] Installing binary to ${BINARY_PATH}..."
$SUDO install -d -m 0755 "$BIN_DIR"
$SUDO install -m 0755 "${PROJECT_DIR}/target/release/rapid_driver" "$BINARY_PATH"

echo "[3/9] Installing ACT LED helper..."
$SUDO install -d -m 0755 "$HELPER_DIR"
$SUDO install -m 0755 "${PROJECT_DIR}/scripts/blink_act_led.sh" "$ACT_BLINK_SCRIPT"

POST_START_LINE=""
if [[ "$ACT_BLINK" == "true" ]]; then
  POST_START_LINE="ExecStartPost=+${ACT_BLINK_SCRIPT}"
fi

echo "[4/9] Creating runtime directories under ${WORKDIR}..."
$SUDO mkdir -p "${CONFIG_HOME}" "${STATE_HOME}"
$SUDO chown -R "${SERVICE_USER}:${SERVICE_GROUP}" "$WORKDIR"

echo "[5/9] Writing systemd unit: ${SERVICE_FILE}"
cat <<EOF | $SUDO tee "$SERVICE_FILE" >/dev/null
[Unit]
Description=rapid_driver daemon
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=${SERVICE_USER}
Group=${SERVICE_GROUP}
WorkingDirectory=${WORKDIR}
Environment=XDG_CONFIG_HOME=${CONFIG_HOME}
Environment=XDG_STATE_HOME=${STATE_HOME}
Environment=RUST_BACKTRACE=1
ExecStart=${BINARY_PATH} ${SERVE_ARGS[*]}
${POST_START_LINE}
Restart=always
RestartSec=2
KillMode=control-group
TimeoutStopSec=20
StandardOutput=journal
StandardError=journal
SyslogIdentifier=${SERVICE_NAME}

[Install]
WantedBy=multi-user.target
EOF

echo "[6/9] Enabling persistent journald storage..."
$SUDO mkdir -p "$JOURNALD_DROPIN_DIR"
cat <<EOF | $SUDO tee "$JOURNALD_DROPIN_FILE" >/dev/null
[Journal]
Storage=persistent
SystemMaxUse=300M
EOF

echo "[7/9] Reloading journald..."
$SUDO systemctl restart systemd-journald

echo "[8/9] Reloading systemd and enabling service..."
$SUDO systemctl daemon-reload
$SUDO systemctl enable --now "${SERVICE_NAME}.service"

echo "[9/9] Service status:"
$SUDO systemctl --no-pager --full status "${SERVICE_NAME}.service" || true

echo
echo "Done."
echo "Health checks:"
echo "  journalctl -u ${SERVICE_NAME} -f -o short-iso"
echo "  curl -s http://127.0.0.1:${BIND_ADDR##*:}/ready"
echo
echo "State dirs:"
echo "  config: ${CONFIG_HOME}/rapid_driver"
echo "  logs:   ${STATE_HOME}/rapid_driver/logs"
echo "ACT blink on start: ${ACT_BLINK}"
