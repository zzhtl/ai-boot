#!/usr/bin/env bash
# 一键部署 ai-boot：用已经登录了 claude 的普通用户运行，需要 root 的步骤脚本自己 sudo。
#
#   deploy/setup.sh [--app-id cli_xxx] [--allow-open-id ou_xxx]... [--binary <ai-boot 可执行文件>]
#                   [--force-config] [--reset-secret] [--no-start]
#
# 服务以当前用户的身份运行，复用这台机器上已登录的 claude 和 qtmcp（配置、SSO 会话），
# 只安装 ai-boot 本身。可以重复运行，升级就是 git pull 后重跑：已有的配置和 App Secret
# 默认保留，服务重启后换上新版本。
#
# App Secret 只从环境变量 FEISHU_APP_SECRET 或终端隐藏输入读取，经 stdin 写进 root 所有、
# 0600 的 /etc/ai-boot/ai-boot.env，不出现在任何进程的命令行参数和输出里。
set -euo pipefail
# 脚本经手 App Secret：即使用 bash -x 调试，也不能把它打进执行轨迹
set +x

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(dirname "$HERE")
ETC=/etc/ai-boot
CONFIG=$ETC/config.toml
ENV_FILE=$ETC/ai-boot.env
DROPIN=/etc/systemd/system/ai-boot.service.d/host-claude.conf
AI_BOOT_BIN=/opt/ai-boot/bin/ai-boot
TOOLSETS=jira,confluence,gitlab,jenkins
# config.example.toml 里的占位 App ID：配置里还是它，说明是 install.sh 放的示例，可以直接覆盖
PLACEHOLDER_APP_ID=cli_xxxxxxxxxxxxxxxx
# 机器人要用的权限，用途见 README
REQUIRED_SCOPES=(
    im:message.p2p_msg:readonly
    im:message.group_at_msg:readonly
    im:message.group_msg
    im:message:readonly
    im:message:send_as_bot
    im:message:update
    im:resource
    im:chat.members:read
)
# 家目录整个绑进沙箱后要藏起来的凭据（相对家目录）
CREDENTIAL_PATHS=(
    .ssh .gnupg .lark-cli .local/share/lark-cli .local/share/keyrings
    .docker .kube .config/gh .npmrc .config/google-chrome
)
export LARKSUITE_CLI_NO_UPDATE_NOTIFIER=1 LARKSUITE_CLI_NO_SKILLS_NOTIFIER=1

# 马上从环境里拿走：后面起的 cargo、lark-cli 等子进程都不该继承它
secret=${FEISHU_APP_SECRET-}
unset FEISHU_APP_SECRET

step() { printf '\n==> %s\n' "$1"; }
info() { printf '    %s\n' "$@"; }
warn() {
    printf 'setup.sh: 注意：%s\n' "$1" >&2
    shift
    if [[ $# -gt 0 ]]; then printf '    %s\n' "$@" >&2; fi
}
die() {
    printf 'setup.sh: %s\n' "$1" >&2
    shift
    if [[ $# -gt 0 ]]; then printf '    %s\n' "$@" >&2; fi
    exit 1
}

usage() {
    cat <<'EOF'
用法：deploy/setup.sh [选项]

用已登录 claude 的普通用户运行，需要 root 的步骤脚本自己 sudo。可以重复运行，升级也是重跑它。

  --app-id cli_xxx         飞书应用的 App ID；默认取 lark-cli 绑定的应用
  --allow-open-id ou_xxx   白名单，可重复；默认是 lark-cli 用户身份的 open_id（仅当应用相同）
  --binary <路径>          用编译好的 ai-boot，不在本机编译
  --force-config           按本次环境重新生成 /etc/ai-boot/config.toml（旧文件先备份）
  --reset-secret           重新输入 App Secret（默认保留已有的）
  --no-start               只安装和自检，不启动服务
  -h, --help               显示帮助

App Secret 从环境变量 FEISHU_APP_SECRET 读取，没有就在终端隐藏输入。
EOF
}

need_value() {
    [[ $2 -ge 2 ]] || die "$1 缺少参数值" "用 -h 查看用法"
}

opt_app_id=
opt_open_ids=()
binary=
force_config=0
reset_secret=0
start=1
while [[ $# -gt 0 ]]; do
    case $1 in
    --app-id)
        need_value "$1" "$#"
        opt_app_id=$2
        shift 2
        ;;
    --allow-open-id)
        need_value "$1" "$#"
        opt_open_ids+=("$2")
        shift 2
        ;;
    --binary)
        need_value "$1" "$#"
        binary=$2
        shift 2
        ;;
    --force-config)
        force_config=1
        shift
        ;;
    --reset-secret)
        reset_secret=1
        shift
        ;;
    --no-start)
        start=0
        shift
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *) die "不认识的参数：$1" "用 -h 查看用法" ;;
    esac
done

user=
home=
claude_bin=
lark_app=
lark_open_id=
app_id=
open_ids=()
qtmcp_bin=
qtmcp_config=
qtmcp_state=
# none：没有配置；example：install.sh 放的示例；custom：配过的
config_state=none
need_secret=0
console_incomplete=0
work=

# 从 stdin 的 JSON 里按点分路径取值，取不到输出空串；布尔值输出 true/false
json_get() {
    python3 -c '
import json, sys
try:
    value = json.load(sys.stdin)
    for key in sys.argv[1].split("."):
        value = value[key]
except Exception:
    value = None
if isinstance(value, bool):
    print("true" if value else "false")
elif isinstance(value, str):
    print(value)
elif value is not None:
    print(json.dumps(value, ensure_ascii=False))
' "$1"
}

keeping_config() {
    [[ $config_state == custom && $force_config -eq 0 ]]
}

check_host() {
    step "检查环境"
    [[ $EUID -ne 0 ]] ||
        die "不要用 root 运行" "服务以运行本脚本的用户身份运行、复用这个用户的 claude 登录；换成已登录 claude 的普通用户，需要 root 的步骤脚本会自己 sudo"
    if [[ $(uname -s) != Linux || ! -d /run/systemd/system ]] || ! command -v systemd-run >/dev/null; then
        die "只支持用 systemd 管理服务的 Linux"
    fi
    command -v python3 >/dev/null || die "找不到 python3" "装上 python3：脚本用它解析 JSON、生成配置"
    command -v sudo >/dev/null || die "找不到 sudo" "装上 sudo，并给当前用户 sudo 权限"
    [[ -f $REPO/config.example.toml ]] || die "找不到 $REPO/config.example.toml" "在仓库里运行 deploy/setup.sh"
    info "安装服务、写 /etc 需要 root，可能要输入 sudo 密码"
    # 不用 sudo -v：sudoers 里同时有要密码和 NOPASSWD 的条目时，-v 按默认的 verifypw=all
    # 仍然要密码，真正执行命令却不用
    sudo true || die "sudo 验证失败" "当前用户需要 sudo 权限"

    user=$(id -un)
    home=${HOME%/}
    if [[ $home != /?* || ! -d $home ]]; then
        die "HOME=$HOME 不是有效的家目录"
    fi
    [[ $home != *[[:space:]]* ]] ||
        die "家目录路径里有空白字符：$home" "systemd 的 BindPaths 等配置按空白分隔，不支持这样的路径"
    local groups
    groups=" $(id -Gn "$user") "
    [[ $groups == *" $user "* ]] ||
        die "没有与用户同名的组 $user，或者你不在这个组里" "install.sh 用这个组做 /etc/ai-boot 的属组；多数发行版建用户时会自动建同名组"
}

check_claude() {
    claude_bin=$(command -v claude) ||
        die "PATH 里找不到 claude" "先装好 Claude Code 并登录（claude auth login），再重跑"
    [[ $claude_bin == /* ]] || die "claude 应该是一个可执行文件，现在是：$claude_bin"
    local status
    # 按服务的环境检查：服务里没有这几个变量，只认 ~/.claude 里的登录
    status=$(env -u CLAUDE_CONFIG_DIR -u CLAUDE_CODE_OAUTH_TOKEN -u ANTHROPIC_API_KEY \
        "$claude_bin" auth status --json 2>/dev/null) || true
    [[ $(json_get loggedIn <<<"$status") == true ]] ||
        die "claude 没有登录（按服务的环境检查，只认 $home/.claude 里的登录）" "修复：执行 claude auth login，登录后重跑"
    info "claude：$claude_bin，已登录"
}

read_lark_cli() {
    command -v lark-cli >/dev/null || return 0
    local status
    status=$(lark-cli auth status --json 2>/dev/null) || true
    lark_app=$(json_get appId <<<"$status")
    lark_open_id=$(json_get identities.user.openId <<<"$status")
}

inspect_config() {
    if sudo test -e "$CONFIG"; then
        if sudo grep -qF "$PLACEHOLDER_APP_ID" "$CONFIG"; then
            config_state=example
        else
            config_state=custom
        fi
    fi
}

resolve_app() {
    if keeping_config; then
        if [[ -n $opt_app_id || ${#opt_open_ids[@]} -gt 0 ]]; then
            warn "已有 $CONFIG，这次的 --app-id、--allow-open-id 不生效" "要按参数重新生成配置就加 --force-config（旧文件会先备份）"
        fi
        # 核对后台时用配置里实际的应用
        app_id=$(sudo sed -n '/^[[:space:]]*app_id[[:space:]]*=/{s/^[^"]*"\([^"]*\)".*/\1/p;q}' "$CONFIG")
        info "沿用已有配置 $CONFIG（应用 ${app_id:-未知}）"
        return
    fi
    app_id=${opt_app_id:-$lark_app}
    [[ -n $app_id ]] ||
        die "拿不到飞书应用的 App ID" "用 lark-cli config init 绑定机器人要用的应用，或者加 --app-id cli_xxx"
    [[ $app_id =~ ^cli_[A-Za-z0-9]+$ ]] || die "App ID 格式不对：$app_id（应形如 cli_xxx）"
    open_ids=("${opt_open_ids[@]}")
    if [[ ${#open_ids[@]} -eq 0 ]]; then
        # open_id 按应用区分：lark-cli 绑的不是这个应用时，它给的 open_id 对机器人无效
        if [[ -z $lark_open_id || $lark_app != "$app_id" ]]; then
            die "拿不到默认白名单：lark-cli 在这个应用下的用户 open_id" "用 lark-cli auth login 登录用户身份，或者加 --allow-open-id ou_xxx（可重复）"
        fi
        open_ids=("$lark_open_id")
    fi
    local id
    for id in "${open_ids[@]}"; do
        [[ $id =~ ^ou_[A-Za-z0-9]+$ ]] || die "open_id 格式不对：$id（应形如 ou_xxx）"
    done
    info "应用 $app_id，白名单 ${open_ids[*]}"
}

check_qtmcp() {
    # 机器人和交互式 qtmcp 共用这个 SSO 会话目录，跟着用户自己的设置走
    qtmcp_state=${QTMCP_STATE_DIR:-$home/.local/share/qtmcp}
    if keeping_config; then
        return
    fi
    if ! qtmcp_bin=$(command -v qtmcp); then
        qtmcp_bin=
        info "没找到 qtmcp：不接 Jira、Confluence、GitLab、Jenkins，只凭聊天内容回答"
        return
    fi
    qtmcp_config=${QTMCP_CONFIG:-$home/.config/qtmcp/config.toml}
    if [[ $qtmcp_bin != /* || $qtmcp_config != /* || $qtmcp_state != /* ]]; then
        die "qtmcp 相关路径要是绝对路径：$qtmcp_bin、QTMCP_CONFIG=$qtmcp_config、QTMCP_STATE_DIR=$qtmcp_state"
    fi
    if [[ ! -f $qtmcp_config ]]; then
        warn "找到了 qtmcp，但没有它的配置 $qtmcp_config，这次不接 qtmcp" "用 qtmcp init 配好（或用 QTMCP_CONFIG 指过去）后加 --force-config 重跑"
        qtmcp_bin=
        return
    fi
    info "qtmcp：$qtmcp_bin（配置 $qtmcp_config，SSO 会话 $qtmcp_state）"
}

check_optional_tools() {
    # 可选依赖：缺了只少对应的功能，不挡部署
    local missing=()
    command -v pdftotext >/dev/null || missing+=("poppler-utils（读 PDF 附件）")
    command -v dot >/dev/null || missing+=("graphviz（画答案里的流程图）")
    if [[ ${#missing[@]} -gt 0 ]]; then
        info "可选依赖没装，对应功能用不了，其余照常：" "${missing[@]/#/  - }"
    fi
}

check_build() {
    if [[ -n $binary ]]; then
        [[ -f $binary && -x $binary ]] || die "$binary 不存在或不可执行"
        binary=$(realpath -- "$binary")
    else
        command -v cargo >/dev/null ||
            die "找不到 cargo" "装上 rustup（仓库的 rust-toolchain.toml 会自动装好对应的工具链），或者用 --binary 指定编译好的 ai-boot"
    fi
}

ask_secret() {
    # 已有非空的 App Secret 就保留，重跑（升级）不用再输
    if [[ $reset_secret -eq 0 ]] && sudo grep -qE '^FEISHU_APP_SECRET=[^[:space:]]' "$ENV_FILE" 2>/dev/null; then
        if [[ -n $secret ]]; then
            warn "$ENV_FILE 里已有 App Secret，这次忽略环境变量 FEISHU_APP_SECRET" "要更换就加 --reset-secret"
        fi
        secret=
        need_secret=0
        return
    fi
    need_secret=1
    if [[ -z $secret ]]; then
        [[ -t 0 ]] || die "需要 App Secret，但没有终端可以输入" "用环境变量 FEISHU_APP_SECRET 传入"
        info "需要飞书应用的 App Secret：开发者后台 https://open.feishu.cn/app → 选中应用 → 凭证与基础信息 → 应用凭证"
        read -rsp "    粘贴 App Secret（输入不显示）：" secret
        echo
    fi
    secret=${secret//$'\r'/}
    [[ -n $secret ]] || die "App Secret 是空的"
    # EnvironmentFile 会解释空白、引号和反斜杠，开发者后台给的值里本来也没有这些
    case $secret in
    *[[:space:]\"\'\\]*) die "App Secret 里有空白、引号或反斜杠，不像是开发者后台给的值" "重新复制后重跑" ;;
    esac
    info "已读取 App Secret（${#secret} 位）"
}

build() {
    if [[ -n $binary ]]; then
        step "使用编译好的 $binary"
        return
    fi
    step "编译 ai-boot（cargo build --release -p ai-boot）"
    # 在仓库目录里跑：rustup 按当前目录找 rust-toolchain.toml
    (cd "$REPO" && cargo build --release -p ai-boot) ||
        die "编译失败" "看上面 cargo 的报错；缺 C 编译器就装 gcc（Debian/Ubuntu 装 build-essential）"
    local target
    target=$(cd "$REPO" && cargo metadata --format-version 1 --no-deps | json_get target_directory) ||
        die "取不到 cargo 的 target 目录" "编译好的文件可以用 --binary 直接指定"
    binary=$target/release/ai-boot
    [[ -x $binary ]] || die "编译完却找不到 $binary"
}

install_service() {
    step "安装 ai-boot 到 /opt/ai-boot/bin（deploy/install.sh）"
    sudo AI_BOOT_USER="$user" "$HERE/install.sh" "$binary" || die "install.sh 失败，见上面的报错"
    info "install.sh 提到的 qtmcp.toml 和后续步骤不用管：服务用你自己的 qtmcp 配置，自检和启动由本脚本接着做"
}

write_config() {
    if keeping_config; then
        step "保留已有配置 $CONFIG"
        info "要按本次环境重新生成：deploy/setup.sh --force-config（旧文件会先备份）"
        return
    fi
    step "生成 $CONFIG"
    SETUP_EXAMPLE=$REPO/config.example.toml \
        SETUP_APP_ID=$app_id \
        SETUP_OPEN_IDS=$(printf '%s\n' "${open_ids[@]}") \
        SETUP_CLAUDE=$claude_bin \
        SETUP_HOOK=$AI_BOOT_BIN \
        SETUP_QTMCP=$qtmcp_bin \
        SETUP_QTMCP_CONFIG=$qtmcp_config \
        SETUP_QTMCP_STATE=$qtmcp_state \
        SETUP_TOOLSETS=$TOOLSETS \
        python3 - >"$work/config.toml" <<'PY' || die "生成配置失败，见上面的报错"
import json
import os
import re
import sys
import urllib.parse

env = os.environ
qtmcp = env["SETUP_QTMCP"]
toolsets = env["SETUP_TOOLSETS"].split(",")


def string(value):
    # JSON 的字符串转义是 TOML 基本字符串转义的子集
    return json.dumps(value, ensure_ascii=False)


def array(values):
    return "[" + ", ".join(string(v) for v in values) + "]"


# 卡片里允许出现链接的 host：取 qtmcp 配置里各工具组的 base_url
hosts = []
if qtmcp:
    section = None
    with open(env["SETUP_QTMCP_CONFIG"], encoding="utf-8") as f:
        for raw in f:
            line = raw.strip()
            header = re.match(r"\[([^\[\]]+)\]", line)
            if header:
                section = header.group(1).strip()
                continue
            url = re.match(r"base_url\s*=\s*([\"'])(.*?)\1", line)
            if url and section in toolsets:
                host = urllib.parse.urlsplit(url.group(2)).hostname
                if host and host not in hosts:
                    hosts.append(host)
    if hosts:
        print("    卡片里允许链接的域名：" + "、".join(hosts), file=sys.stderr)
    else:
        print("    注意：qtmcp 配置里没找到 base_url，卡片里的链接会被去掉；需要时在 render.link_hosts 里补上", file=sys.stderr)

values = {
    ("feishu", "app_id"): string(env["SETUP_APP_ID"]),
    ("access", "allowed_emails"): "[]",
    ("access", "allowed_open_ids"): array(env["SETUP_OPEN_IDS"].split()),
    ("storage", "data_dir"): string("/var/lib/ai-boot"),
    ("agent", "effort"): string("xhigh"),
    ("agent", "budget_usd"): "20.0",
    ("agent", "max_concurrent"): "2",
    ("agent.claude", "program"): string(env["SETUP_CLAUDE"]),
    ("agent.hook", "program"): string(env["SETUP_HOOK"]),
    ("render", "link_hosts"): array(hosts),
    ("context", "window_messages"): "500",
    ("context", "window_minutes"): "0",
}
# 示例开头的说明换成下面的 PREAMBLE；[oauth]、[writeback] 是可选功能，一键部署不配
drop = {None, "oauth", "writeback"}
if qtmcp:
    values[("agent.mcp", "command")] = string(qtmcp)
    values[("agent.mcp", "args")] = array(["--toolsets", env["SETUP_TOOLSETS"]])
    values[("agent.mcp", "env")] = "{ QTMCP_CONFIG = %s, QTMCP_STATE_DIR = %s }" % (
        string(env["SETUP_QTMCP_CONFIG"]),
        string(env["SETUP_QTMCP_STATE"]),
    )
else:
    drop.add("agent.mcp")

PREAMBLE = [
    "# ai-boot 配置，由 deploy/setup.sh 按 config.example.toml 生成。",
    "# 密钥不写在这里：飞书 App Secret 在 /etc/ai-boot/ai-boot.env；claude 复用部署用户 ~/.claude 里的登录。",
    "# 按当前环境重新生成：deploy/setup.sh --force-config（旧文件会先备份）",
]
HEADER = re.compile(r"\s*\[\[?\s*([A-Za-z0-9_.-]+)\s*\]\]?\s*(#.*)?$")
KEY = re.compile(r"(\s*)([A-Za-z0-9_-]+)\s*=(.*)$")

with open(env["SETUP_EXAMPLE"], encoding="utf-8") as f:
    lines = f.read().splitlines()

out, block, section = [], [], None
# 末尾补一个假的节标题，让最后一节也走一遍收尾
for line in lines + ["[__end__]"]:
    header = HEADER.match(line)
    if header:
        # 紧挨着节标题的注释属于新的一节，删节时一起删
        cut = len(block)
        while cut > 0 and block[cut - 1].lstrip().startswith("#"):
            cut -= 1
        if section not in drop:
            out.extend(block[:cut])
        block, section = block[cut:] + [line], header.group(1)
        continue
    key = KEY.match(line)
    if key and (section, key.group(2)) in values:
        rest = key.group(3)
        if rest.count("[") != rest.count("]") or rest.count("{") != rest.count("}"):
            sys.exit(f"config.example.toml 里 [{section}] {key.group(2)} 跨了多行，脚本处理不了")
        line = f"{key.group(1)}{key.group(2)} = {values.pop((section, key.group(2)))}"
    block.append(line)

if values:
    missing = "、".join(f"[{s}] {k}" for s, k in values)
    sys.exit(f"config.example.toml 里找不到 {missing}：示例的格式变了，deploy/setup.sh 要跟着改")
while out and not out[-1].strip():
    out.pop()
text = "\n".join(PREAMBLE + [""] + out) + "\n"
try:
    import tomllib
except ImportError:  # Python 3.11 之前没有 tomllib，交给后面的 doctor 校验
    tomllib = None
if tomllib:
    try:
        tomllib.loads(text)
    except tomllib.TOMLDecodeError as err:
        sys.exit(f"生成的配置不是合法的 TOML：{err}")
sys.stdout.write(text)
PY
    if [[ $config_state == custom ]]; then
        local backup
        backup=$CONFIG.bak.$(date +%Y%m%d-%H%M%S)
        sudo cp -p "$CONFIG" "$backup"
        info "旧配置备份到 $backup"
    fi
    # 同目录临时文件 + rename：服务随时可能重启，不能读到写了一半的配置
    sudo install -m 0640 -o root -g "$user" "$work/config.toml" "$ETC/.config.toml.new"
    sudo mv -f "$ETC/.config.toml.new" "$CONFIG"
}

store_secret() {
    if [[ $need_secret -eq 0 ]]; then
        step "保留 $ENV_FILE 里已有的 App Secret（要更换就加 --reset-secret）"
        return
    fi
    step "写入 App Secret 到 $ENV_FILE（root 所有、0600）"
    local code
    code=$(
        cat <<'PY'
import os
import sys
import tempfile

path = sys.argv[1]
secret = sys.stdin.readline().rstrip("\n")
with open(path, encoding="utf-8") as f:
    lines = f.read().splitlines()
out, done = [], False
for line in lines:
    if line.startswith("FEISHU_APP_SECRET="):
        if not done:
            out.append("FEISHU_APP_SECRET=" + secret)
            done = True
        continue
    out.append(line)
if not done:
    out.append("FEISHU_APP_SECRET=" + secret)
# mkstemp 建的文件就是 0600，属主是当前的 root；rename 之前任何时刻都没有别人能读的副本
fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".ai-boot.env.")
try:
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write("\n".join(out) + "\n")
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)
except BaseException:
    os.unlink(tmp)
    raise
PY
    )
    # 密钥只走管道：printf 是 bash 内建，不会作为参数出现在任何进程的命令行里
    printf '%s\n' "$secret" | sudo python3 -c "$code" "$ENV_FILE"
    secret=
}

write_dropin() {
    step "写 systemd drop-in $DROPIN"
    local hidden=() rel
    for rel in "${CREDENTIAL_PATHS[@]}"; do
        hidden+=("-$home/$rel")
    done
    cat >"$work/host-claude.conf" <<EOF
# 由 deploy/setup.sh 生成，重跑会覆盖。服务以部署用户的身份运行，复用这台机器上
# 已登录的 claude 和 qtmcp，不用 unit 里给独立服务用户准备的目录和 token。
[Service]
User=$user
Group=$user
Environment=HOME=$home
# 只用 ~/.claude 里的登录：unit 设的 CLAUDE_CONFIG_DIR、ai-boot.env 里的 token 都去掉
UnsetEnvironment=CLAUDE_CONFIG_DIR CLAUDE_CODE_OAUTH_TOKEN
# claude 用同目录临时文件 + rename 更新 ~/.claude.json，只绑定这个文件的话 rename 会失败，
# 所以整个家目录都绑回来（unit 的 ProtectHome=tmpfs 把 /home 换成了空目录）……
BindPaths=$home
# ……再藏起其中的凭据：聊天内容、文档都是不可信输入，Agent 不该碰到它们。
# 带 - 前缀：现在不存在的路径会被忽略，以后建出来也照样藏住
InaccessiblePaths=${hidden[*]}
EOF
    sudo install -D -m 0644 "$work/host-claude.conf" "$DROPIN"
    sudo systemctl daemon-reload
}

secure_qtmcp_state() {
    # SSO 会话：机器人和交互式 qtmcp 共用，doctor 要求它存在且只有自己能访问
    if [[ -n $qtmcp_bin ]]; then
        mkdir -p "$qtmcp_state"
    fi
    if [[ -d $qtmcp_state ]]; then
        chmod 700 "$qtmcp_state"
        step "qtmcp 的 SSO 会话目录 $qtmcp_state 权限收紧到 0700"
    fi
}

check_console() {
    step "核对飞书开发者后台的配置（用 lark-cli，查不了就跳过）"
    if ! command -v lark-cli >/dev/null; then
        info "跳过：没装 lark-cli"
        return
    fi
    if [[ -z $app_id || $lark_app != "$app_id" ]]; then
        info "跳过：lark-cli 绑定的不是这个应用（${app_id:-未知}）"
        return
    fi
    local base=/open-apis/application/v6/applications/$app_id version_id state=none reason=
    lark-cli api GET "$base" --as bot --params '{"lang":"zh_cn"}' >"$work/app.json" 2>&1 || true
    if [[ $(json_get ok <"$work/app.json") != true ]]; then
        reason=$(json_get error.message <"$work/app.json")
        info "跳过：查不了应用信息（${reason:-lark-cli 没有返回可解析的结果}）"
        return
    fi
    version_id=$(json_get data.app.online_version_id <"$work/app.json")
    if [[ -n $version_id ]]; then
        state=error
        if [[ $version_id =~ ^[A-Za-z0-9_]+$ ]]; then
            lark-cli api GET "$base/app_versions/$version_id" --as bot --params '{"lang":"zh_cn"}' \
                >"$work/version.json" 2>&1 || true
            if [[ $(json_get ok <"$work/version.json") == true ]]; then
                state=ok
            else
                reason=$(json_get error.message <"$work/version.json")
            fi
        fi
    fi
    SETUP_WORK=$work \
        SETUP_VERSION_STATE=$state \
        SETUP_VERSION_ERROR=${reason:-未知原因} \
        SETUP_SCOPES="${REQUIRED_SCOPES[*]}" \
        python3 - <<'PY' || console_incomplete=1
import json
import os
import sys

env = os.environ


def load(name, key):
    with open(os.path.join(env["SETUP_WORK"], name), encoding="utf-8") as f:
        return json.load(f)["data"][key]


def main():
    state = env["SETUP_VERSION_STATE"]
    app = load("app.json", "app")
    missing = []
    # 回调只在应用信息里有（版本详情里没有），反映的是后台当前的设置
    callback = app.get("callback_info") or {}
    if callback.get("callback_type") != "websocket" or "card.action.trigger" not in (
        callback.get("subscribed_callbacks") or []
    ):
        missing.append("回调 card.action.trigger：开发配置 → 事件与回调 → 回调配置，订阅方式选「使用长连接接收回调」，添加「卡片回传交互」")
    if state == "none":
        missing.append("线上版本：应用发布 → 版本管理与发布，创建版本并申请发布")
    elif state == "error":
        print("    线上版本的详情查不了，没核对机器人能力、事件和权限：" + env["SETUP_VERSION_ERROR"])
    else:
        # 能力、事件、权限以线上版本为准：后台改了没发布，机器人也用不上
        version = load("version.json", "app_version")
        if "bot" not in (version.get("ability") or {}):
            missing.append("机器人能力：应用能力 → 添加应用能力 → 机器人")
        events = {e.get("event_type") for e in version.get("event_infos") or []}
        if "im.message.receive_v1" not in events:
            missing.append("事件 im.message.receive_v1：开发配置 → 事件与回调 → 事件配置，订阅方式选「使用长连接接收事件」，添加「接收消息」")
        granted = {s.get("scope") for s in version.get("scopes") or []}
        lacking = [s for s in env["SETUP_SCOPES"].split() if s not in granted]
        if lacking:
            missing.append("权限 " + "、".join(lacking) + "：开发配置 → 权限管理")
    if not missing:
        if state == "ok":
            print("    机器人能力、事件、回调、权限都已配好（事件的订阅方式接口里看不到，确认选的是长连接）")
        return 0
    for item in missing:
        print("    缺 " + item)
    print("    能力、事件、权限改完都要在「应用发布 → 版本管理与发布」发布新版本才生效")
    return 1


try:
    code = main()
except Exception as err:  # 尽力而为：返回的格式对不上就跳过，不影响部署
    print(f"    跳过：返回的内容看不懂（{err!r}）")
    code = 0
sys.exit(code)
PY
}

run_doctor() {
    step "自检（deploy/doctor.sh：用服务的身份、环境和沙箱运行 ai-boot doctor）"
    sudo "$HERE/doctor.sh" ||
        die "自检有失败项，服务没有启动" "按上面 ❌ 那几项的提示处理后重跑 deploy/setup.sh" "App Secret 输错了就加 --reset-secret 重跑"
}

start_service() {
    if [[ $start -eq 0 ]]; then
        step "按 --no-start 不启动服务"
        info "启动：sudo systemctl enable --now ai-boot（已经在运行的话用 sudo systemctl restart ai-boot 换上新版本）"
        return
    fi
    step "启动服务"
    sudo systemctl enable --quiet ai-boot
    # 用 restart 而不是 start：重跑（升级）时要换上新的二进制、配置和 drop-in
    sudo systemctl restart ai-boot
    sleep 3
    if ! systemctl is-active --quiet ai-boot; then
        sudo journalctl -u ai-boot -n 30 --no-pager >&2 || true
        die "服务没有起来，日志见上" "修好后重跑 deploy/setup.sh"
    fi
}

finish() {
    step "完成"
    if [[ $start -eq 1 ]]; then
        systemctl --no-pager --lines=0 status ai-boot || true
        info "" "看日志：journalctl -u ai-boot -f" "私聊机器人发一句试试"
    fi
    if [[ $console_incomplete -eq 1 ]]; then
        warn "开发者后台还有没配好的项（见上面「核对飞书开发者后台」一步），配好并发布新版本后才收得到消息"
    fi
}

work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

check_host
check_claude
read_lark_cli
inspect_config
resolve_app
check_qtmcp
check_optional_tools
check_build
ask_secret
build
install_service
write_config
store_secret
write_dropin
secure_qtmcp_state
check_console
run_doctor
start_service
finish
