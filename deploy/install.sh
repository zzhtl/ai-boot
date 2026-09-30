#!/usr/bin/env bash
# 安装或升级 ai-boot（在要部署的机器上用 root 运行；服务用户默认 ai-boot，用 AI_BOOT_USER 指定）：
#
#   deploy/install.sh <ai-boot 可执行文件> [claude 可执行文件] [qtmcp 可执行文件]
#
# 二进制原子替换（同目录临时文件 + rename，运行中的进程不受影响）；配置文件
# 已存在就不动，只在第一次安装时放示例。claude 要装固定版本的拷贝：升级前先按
# crates/ai-boot-agent/tests/fixtures/claude/README.md 重录样本、跑通解码测试。
set -euo pipefail

SERVICE_USER=${AI_BOOT_USER:-ai-boot}
PREFIX=/opt/ai-boot
ETC=/etc/ai-boot
QTMCP_STATE=/var/lib/qtmcp
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

die() {
    echo "install.sh: $*" >&2
    exit 1
}

[[ $EUID -eq 0 ]] || die "需要 root 运行"
[[ $# -ge 1 && $# -le 3 ]] || die "用法：install.sh <ai-boot 可执行文件> [claude 可执行文件] [qtmcp 可执行文件]"
id "$SERVICE_USER" >/dev/null 2>&1 || die "用户 $SERVICE_USER 不存在（用环境变量 AI_BOOT_USER 指定）"

install_bin() {
    local source=$1 name=$2 tmp
    [[ -f $source && -x $source ]] || die "$source 不存在或不可执行"
    tmp=$(mktemp "$PREFIX/bin/.$name.XXXXXX")
    install -m 0755 "$source" "$tmp"
    mv -f "$tmp" "$PREFIX/bin/$name"
    echo "已安装 $PREFIX/bin/$name"
}

install -d -m 0755 "$PREFIX/bin"
install_bin "$1" ai-boot
if [[ $# -ge 2 ]]; then install_bin "$2" claude; fi
if [[ $# -ge 3 ]]; then install_bin "$3" qtmcp; fi

install -d -m 0750 -o root -g "$SERVICE_USER" "$ETC"
if [[ ! -e $ETC/config.toml ]]; then
    install -m 0640 -o root -g "$SERVICE_USER" "$HERE/../config.example.toml" "$ETC/config.toml"
    echo "已放示例配置 $ETC/config.toml：按注释修改"
fi
if [[ ! -e $ETC/ai-boot.env ]]; then
    # systemd 以 root 读 EnvironmentFile，服务用户自己读不到它
    install -m 0600 -o root -g root /dev/null "$ETC/ai-boot.env"
    printf '%s\n' \
        '# 飞书应用的 App Secret' 'FEISHU_APP_SECRET=' \
        '# 在机器人的 CLAUDE_CONFIG_DIR 下用 claude setup-token 生成' 'CLAUDE_CODE_OAUTH_TOKEN=' \
        >"$ETC/ai-boot.env"
    echo "已创建 $ETC/ai-boot.env：填入密钥"
fi
if [[ ! -e $ETC/qtmcp.toml ]]; then
    echo "注意：还没有 $ETC/qtmcp.toml（qtmcp 的配置，属主 $SERVICE_USER、权限 0600）"
fi
# 机器人和交互式 qtmcp 共用的 SSO 会话目录
install -d -m 0700 -o "$SERVICE_USER" -g "$SERVICE_USER" "$QTMCP_STATE"

sed -e "s/^User=.*/User=$SERVICE_USER/" -e "s/^Group=.*/Group=$SERVICE_USER/" \
    "$HERE/ai-boot.service" >/etc/systemd/system/ai-boot.service
chmod 0644 /etc/systemd/system/ai-boot.service
systemctl daemon-reload

echo "完成。先自检：$HERE/doctor.sh"
echo "再启动：systemctl enable --now ai-boot"
