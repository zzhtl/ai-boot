#!/usr/bin/env bash
# 用服务的身份和环境运行 ai-boot doctor（需要 root：要读 0600 的 EnvironmentFile）。
# 身份、环境和 ai-boot.service 保持一致，检查结果才代表服务真实的运行条件。
set -euo pipefail

UNIT=/etc/systemd/system/ai-boot.service
[[ $EUID -eq 0 ]] || {
    echo "doctor.sh: 需要 root 运行" >&2
    exit 1
}
[[ -f $UNIT ]] || {
    echo "doctor.sh: 没有 $UNIT，先运行 install.sh" >&2
    exit 1
}

props=()
# drop-in（ai-boot.service.d/*.conf）排在后面：同名属性后出现的覆盖前面的，和 systemd 一致
while IFS= read -r line; do
    case $line in
    User=* | Group=* | EnvironmentFile=* | Environment=* | UnsetEnvironment=* | StateDirectory=* | StateDirectoryMode=* | UMask=* | ReadWritePaths=* | BindPaths=* | InaccessiblePaths=* | ProtectHome=* | ProtectSystem=* | PrivateTmp=*)
        props+=(-p "$line")
        ;;
    esac
done < <(cat "$UNIT" "$UNIT".d/*.conf 2>/dev/null)

exec systemd-run --quiet --pipe --wait --collect "${props[@]}" \
    /opt/ai-boot/bin/ai-boot doctor --config /etc/ai-boot/config.toml
