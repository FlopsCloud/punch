#!/usr/bin/env bash
# 把本机仓库同步到服务器，编译，并用 systemd 重启。
# 下次部署：./deploy.sh
# 主机写在环境变量 PUNCH_HOST，或写在未纳入版本库的 deploy.local.env。
# 已有的 yaml 和证书不会被覆盖。

set -euo pipefail

DEST="${PUNCH_DEST:-/var/local/punch}"
CONTROL_PORT="${PUNCH_CONTROL_PORT:-6100}"
DATA_PORT="${PUNCH_DATA_PORT:-6101}"
WEB_PORT="${PUNCH_WEB_PORT:-6102}"

if [[ "${1:-}" == "--remote" ]]; then
  DEST="$2"
  CONTROL_PORT="$3"
  DATA_PORT="$4"
  WEB_PORT="$5"
  cd "$DEST"

  if ! rpm -q openssl-devel >/dev/null 2>&1 || ! command -v pkg-config >/dev/null 2>&1; then
    dnf install -y --disablerepo=mariadb gcc pkgconf-pkg-config openssl-devel
  fi

  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  if ! command -v rustup >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain stable
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
  if ! cargo --version >/dev/null 2>&1; then
    rustup default stable
  fi

  mkdir -p "$HOME/.cargo"
  if [[ ! -f "$HOME/.cargo/config.toml" ]]; then
    cat > "$HOME/.cargo/config.toml" << 'EOF'
[source.crates-io]
replace-with = "rsproxy-sparse"

[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
EOF
  fi

  port_busy() {
    ss -lnt | awk '{print $4}' | grep -Eq "(^|:)$1([^0-9]|$)"
  }

  if [[ ! -f "$DEST/punch-server.yaml" ]]; then
    for port in "$CONTROL_PORT" "$DATA_PORT" "$WEB_PORT"; do
      if port_busy "$port"; then
        echo "port $port is already in use" >&2
        exit 1
      fi
    done
    KEY="$(openssl rand -hex 24)"
    cat > "$DEST/punch-server.yaml" << YAML
control_listen: "0.0.0.0:${CONTROL_PORT}"
data_listen: "0.0.0.0:${DATA_PORT}"

api_key: "${KEY}"

web:
  listen: "0.0.0.0:${WEB_PORT}"
  cert: "certs/cert.pem"
  key: "certs/key.pem"

tcp:
  nodelay: true
  recv_buffer: 4194304
  send_buffer: 4194304
  pipe_size: 262144
  backlog: 4096

pool:
  idle: 8
  max_idle: 32
YAML
    chmod 600 "$DEST/punch-server.yaml"
    echo "PUNCH_API_KEY_CREATED=${KEY}"
  else
    echo "PUNCH_CONFIG_KEPT=${DEST}/punch-server.yaml"
  fi

  export CARGO_BUILD_JOBS=1
  export CARGO_PROFILE_RELEASE_LTO=false
  export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
  cargo build --release --locked

  install -d /etc/systemd/system
  cat > /etc/systemd/system/punch.service << UNIT
[Unit]
Description=punch reverse TCP tunnel
After=network.target

[Service]
Type=simple
WorkingDirectory=${DEST}
ExecStart=${DEST}/target/release/punch server -c ${DEST}/punch-server.yaml
Restart=on-failure
RestartSec=2
LimitNOFILE=1048576
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
UNIT

  systemctl daemon-reload
  systemctl enable punch
  systemctl restart punch
  sleep 1
  if ! systemctl is-active --quiet punch; then
    journalctl -u punch -n 40 --no-pager
    exit 1
  fi
  echo "PUNCH_ACTIVE=yes"
  ss -lnt | awk '{print $4}' | grep -E ":${CONTROL_PORT}([^0-9]|\$)|:${DATA_PORT}([^0-9]|\$)|:${WEB_PORT}([^0-9]|\$)" || true
  exit 0
fi

ROOT="$(cd "$(dirname "$0")" && pwd)"
if [[ -f "${ROOT}/deploy.local.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  . "${ROOT}/deploy.local.env"
  set +a
fi
HOST="${PUNCH_HOST:-}"
if [[ -z "$HOST" ]]; then
  echo "需要 PUNCH_HOST，例如 PUNCH_HOST=user@host ./deploy.sh" >&2
  echo "也可以写在 ${ROOT}/deploy.local.env，这个文件不要提交。" >&2
  exit 1
fi

echo "sync ${ROOT}/ -> ${HOST}:${DEST}/"
rsync -az --delete \
  --exclude '.git/' \
  --exclude 'target/' \
  --exclude 'punch-server.yaml' \
  --exclude 'certs/' \
  --exclude 'deploy.local.env' \
  -e "ssh -o BatchMode=yes" \
  "${ROOT}/" "${HOST}:${DEST}/"

ssh -o BatchMode=yes "$HOST" \
  "bash $(printf '%q' "$DEST/deploy.sh") --remote $(printf '%q ' "$DEST" "$CONTROL_PORT" "$DATA_PORT" "$WEB_PORT")"
