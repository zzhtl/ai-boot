#!/usr/bin/env bash
# 本机 Codex + qtmcp 部署：配置写到 ~/config/ai-boot，服务以当前用户运行。
set -euo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$HERE/.." && pwd)
REFERENCE=${1:-$HOME/qt/ai-boot/config.toml}
CONFIG_DIR=$HOME/config/ai-boot
CODEX_DIR=${CODEX_HOME:-$HOME/.codex}
QTMCP_CONFIG_PATH=${QTMCP_CONFIG:-$HOME/.config/qtmcp/config.toml}
START=${2:-}

[[ $START == "" || $START == --no-start ]] || { echo "用法：setup-codex.sh [飞书参考配置目录] [--no-start]" >&2; exit 1; }
for program in codex qtmcp cargo go python3 systemctl; do
    command -v "$program" >/dev/null || { echo "缺少 $program" >&2; exit 1; }
done
[[ -e $REFERENCE ]] || { echo "找不到参考配置 $REFERENCE" >&2; exit 1; }
[[ -f $QTMCP_CONFIG_PATH ]] || { echo "找不到 qtmcp 配置 $QTMCP_CONFIG_PATH" >&2; exit 1; }
codex login status

umask 077
mkdir -p "$CONFIG_DIR/bin" "$HOME/.config/systemd/user" "$CODEX_DIR"
chmod 700 "$CONFIG_DIR" "$CONFIG_DIR/bin"

SETUP_REFERENCE=$REFERENCE SETUP_CONFIG=$CONFIG_DIR SETUP_CODEX=$CODEX_DIR \
SETUP_QTMCP=$QTMCP_CONFIG_PATH SETUP_CODEX_BIN=$(command -v codex) \
python3 - <<'PY'
import json
import os
import pathlib
import tomllib
from urllib.parse import urlparse

env = os.environ
home = pathlib.Path.home()
reference = pathlib.Path(env['SETUP_REFERENCE'])
destination = pathlib.Path(env['SETUP_CONFIG'])
codex = pathlib.Path(env['SETUP_CODEX'])
qtmcp_path = pathlib.Path(env['SETUP_QTMCP'])
if reference.is_dir():
    reference = reference / ('config.toml' if (reference / 'config.toml').exists() else 'dev.toml')
source = tomllib.loads(reference.read_text())
qtmcp = tomllib.loads(qtmcp_path.read_text())
quote = lambda v: json.dumps(v, ensure_ascii=False)
hosts = sorted({urlparse(section['base_url']).hostname for section in qtmcp.values()
                if isinstance(section, dict) and section.get('base_url')})
hosts = source.get('render', {}).get('link_hosts', hosts)
config = destination / 'config.toml'
if not config.exists():
    config.write_text('\n'.join([
        '[feishu]', f'app_id = {quote(source["feishu"]["app_id"])}',
        f'base_url = {quote(source["feishu"].get("base_url", "https://open.feishu.cn/"))}',
        f'go_sdk_program = {quote(str(destination / "bin/feishu-go-ws"))}',
        '[access]', f'allowed_emails = {quote(source["access"].get("allowed_emails", []))}',
        f'allowed_open_ids = {quote(source["access"].get("allowed_open_ids", []))}',
        '[storage]', f'data_dir = {quote(str(home / ".local/share/ai-boot"))}',
        '[agent]', 'backend = "codex"', 'model = "gpt-6.1-sol"', 'effort = "high"',
        f'timeout_secs = {source.get("agent", {}).get("timeout_secs", 7200)}',
        f'max_concurrent = {source.get("agent", {}).get("max_concurrent", 1)}',
        '[agent.codex]', f'program = {quote(env["SETUP_CODEX_BIN"])}', 'profile = "ai-boot"',
        '[agent.hook]', f'program = {quote(str(destination / "bin/ai-boot"))}',
        '[render]', f'link_hosts = {quote(hosts)}', '',
    ]))
    config.chmod(0o600)
    context = source.get('context', {})
    if context:
        with config.open('a') as f:
            f.write('[context]\n')
            for key, value in context.items():
                if key == 'knowledge_file' and not pathlib.Path(value).is_file():
                    print(f'参考 knowledge_file 本机不存在，暂不配置：{value}')
                    continue
                f.write(f'{key} = {str(value).lower() if isinstance(value, bool) else quote(value)}\n')
data = pathlib.Path(tomllib.loads(config.read_text())['storage']['data_dir'])
data.mkdir(parents=True, exist_ok=True, mode=0o700)
secret = destination / 'ai-boot.env'
if not secret.exists():
    env_file = next((reference.parent / name for name in ['ai-boot.env', 'dev.env']
                     if (reference.parent / name).exists()), None)
    if env_file is None:
        secret.write_text('FEISHU_APP_SECRET=\n')
        secret.chmod(0o600)
        raise SystemExit(f'请将对应应用的 FEISHU_APP_SECRET 填入 {secret}，再重跑部署')
    lines = [line for line in env_file.read_text().splitlines()
             if line.startswith('FEISHU_APP_SECRET=')]
    if len(lines) != 1 or not lines[0].partition('=')[2].strip():
        raise SystemExit(f'{env_file} 需要非空的 FEISHU_APP_SECRET')
    secret.write_text(lines[0] + '\n')
    secret.chmod(0o600)
profile = codex / 'ai-boot.config.toml'
if not profile.exists():
    lines = ['model = "gpt-6.1-sol"', 'model_reasoning_effort = "high"', 'web_search = "live"']
    base_config = codex / 'config.toml'
    base = tomllib.loads(base_config.read_text()) if base_config.exists() else {}
    if 'qtmcp' in base.get('mcp_servers', {}):
        # 只补无人值守所需的审批设置，command、args、env 等沿用终端配置。
        lines += ['[mcp_servers.qtmcp]', 'default_tools_approval_mode = "approve"']
    profile.write_text('\n'.join(lines) + '\n')
    profile.chmod(0o600)

unit = home / '.config/systemd/user/ai-boot.service'
# systemd 的路径值与 shell 引号规则不同；环境值、可执行文件和参数单独引用。
systemd_quote = lambda value: json.dumps(str(value)).replace('%', '%%').replace('$', '$$')
unit.write_text('\n'.join([
    '[Unit]', 'Description=ai-boot Feishu Codex bridge',
    '[Service]', 'Type=simple',
    f'ExecStart={systemd_quote(destination / "bin/ai-boot")} run --config {systemd_quote(config)}',
    f'EnvironmentFile={str(secret).replace("%", "%%")}',
    f'Environment={systemd_quote("HOME=" + str(home))}',
    f'Environment={systemd_quote("CODEX_HOME=" + str(codex))}',
    f'Environment={systemd_quote("PATH=" + env["PATH"])}',
    *[f'Environment={systemd_quote(key + "=" + env[key])}' for key in (
        'LANG', 'JAVA_HOME', 'GOPATH', 'GOROOT', 'CARGO_HOME', 'RUSTUP_HOME',
        'GIT_SSH', 'GIT_SSH_COMMAND', 'GIT_ASKPASS', 'KUBECONFIG',
        'DOCKER_HOST', 'DOCKER_CONTEXT', 'DOCKER_CONFIG', 'DOCKER_TLS_VERIFY', 'DOCKER_CERT_PATH',
        'HTTPS_PROXY', 'HTTP_PROXY', 'ALL_PROXY', 'NO_PROXY',
        'https_proxy', 'http_proxy', 'all_proxy', 'no_proxy',
    ) if env.get(key)],
    'Environment=AI_BOOT_LOG=info', 'Restart=on-failure', 'RestartSec=5',
    'KillMode=control-group', 'TimeoutStopSec=30', 'UMask=0077',
    'MemoryMax=4G', 'TasksMax=512', 'NoNewPrivileges=true',
    '[Install]', 'WantedBy=default.target', '',
]))
print(f'配置：{config}\nCodex profile：{profile}\n服务：{unit}')
PY

(
    cd "$ROOT/tools/feishu-go-ws"
    go build -trimpath -o "$CONFIG_DIR/bin/feishu-go-ws.new" .
)
chmod 0755 "$CONFIG_DIR/bin/feishu-go-ws.new"
mv -f "$CONFIG_DIR/bin/feishu-go-ws.new" "$CONFIG_DIR/bin/feishu-go-ws"
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p ai-boot
install -m 0755 "$ROOT/target/release/ai-boot" "$CONFIG_DIR/bin/ai-boot.new"
mv -f "$CONFIG_DIR/bin/ai-boot.new" "$CONFIG_DIR/bin/ai-boot"
systemctl --user daemon-reload

# 按最终 unit 的环境自检，包括 SSH agent 与构建、部署工具的配置。
mapfile -d '' -t SERVICE_ENV < <(systemctl --user show ai-boot.service --property=Environment --value |
    python3 -c 'import shlex,sys; [sys.stdout.buffer.write(v.encode() + b"\0") for v in shlex.split(sys.stdin.read())]')
DOCTOR_PROPERTIES=("-p" "EnvironmentFile=$CONFIG_DIR/ai-boot.env")
for value in "${SERVICE_ENV[@]}"; do
    DOCTOR_PROPERTIES+=("-p" "Environment=$value")
done
systemd-run --user --quiet --pipe --wait --collect \
    "${DOCTOR_PROPERTIES[@]}" \
    "$CONFIG_DIR/bin/ai-boot" doctor --config "$CONFIG_DIR/config.toml"

if [[ $START != --no-start ]]; then
    systemctl --user enable ai-boot
    systemctl --user restart ai-boot
    systemctl --user --no-pager --lines=0 status ai-boot
fi
echo "查看日志：journalctl --user -u ai-boot -f"
