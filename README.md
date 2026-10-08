# ai-boot

飞书里的问题排查机器人。通过飞书长连接收消息，在本机调起 `claude -p` 分析，经 qtmcp 查阅和操作 Jira / Confluence / GitLab / Jenkins，把结论渲染成卡片回复到飞书。

```
提问：飞书 ──长连接──▶ ai-boot ──▶ claude -p ──MCP──▶ qtmcp ──▶ Jira / Confluence / GitLab / Jenkins
回复：claude -p 的结构化答案 ──▶ ai-boot 渲染成卡片 ──▶ 飞书
```

- **不需要公网入口**：事件和卡片回调都走飞书长连接（WebSocket），机器能出网访问 `open.feishu.cn` 就行，不用域名和回调地址。
- **复用本机的 Claude Code**：直接用部署用户已登录的 `claude`，不另配 token；auto 权限模式，模型跟随 CLI 默认，推理强度 xhigh：正确优先，多花的时间用在逐条核对证据上。
- **qtmcp 可选**：装了就接上，Agent 能读写 Jira、Confluence、GitLab、Jenkins（GitLab 能按关键词搜代码、看文件历史和 blame、按版本号找 tag，要用带这些动作的 qtmcp）；没装就只凭聊天内容回答。
- **联网和命令行**：Agent 能搜索网页（WebSearch）、打开网页（WebFetch）、执行命令（Bash）。看 GitHub 等公开仓库时先 clone 到会话的工作目录里再读代码，比逐页打开快，读到的也是原文。命令行的 PATH 用部署用户自己的（`deploy/setup.sh` 写进 drop-in），node、cargo、go、java 等工具链都能用。命令直接开放、没有沙箱，见「安全说明」。

## 工作方式

- **触发**：私聊机器人，或者在群里 @ 它。只处理白名单里的人；白名单外的人私聊会收到一次提示，群里直接忽略。
- **会话**：一个群一个 AI 会话。同一个群里的提问都续接这个会话，每轮只把上一轮之后的新消息和新附件发给模型，聊天记录不重复发；不同人的提问各自排一轮、依次回答，不会揉成一轮。私聊上一轮结束两小时内发的新消息接着原来的会话（同一件事一问再问不用从零查起），隔了两小时以上新开一个会话。
  - 聊天记录（`context/transcript.md`）、之前各轮的完整答案（`context/answers.md`）、图片和文件都存在会话的工作目录里，模型需要细节时自己读，不从飞书重拉；清理时整个目录一起删。
  - 每次请求都要带上整段会话，越长每一步越慢：上一轮结束时上下文超过 12 万 token，下一轮就把前几轮收拢成问题和结论（含更正、待确认事项）、换新会话接着问；一轮之内涨过 20 万 token，Claude 自己先压缩再继续。会话意外续接不上时也是这样新开一个。
  - 改了给模型的规则，已有的会话下一轮就按新规则来（不沿用会话开始时的那份）。
- **回复**：用卡片引用回复提问那条消息，不开话题。卡片顶上是概述，排查类写成「根因」「解决」两行（没确认时是「可能原因」「下一步」），行首是彩色标签，一眼看到原因和怎么解决；紧跟着是要执行的关键命令（通常 1 条、最多露 3 条，命令单独放代码块，标明在哪执行、执行后看什么），按手敲来写，不给多种写法；细节（根因分析、注意事项、需要补充的信息）默认折叠；依据只在正文里写一两处，不列参考链接和查阅过程；卡片宽度铺满。标题旁的标签只说结论到了哪一步（根因已确认 / 待确认 / 需要补充信息），把握度不高时才标出；底部只有轮次和用时。
- **附件**：要交付脚本、报告、导出的数据时，模型把文件写到会话工作目录的 `out/` 下，答案发出后作为文件消息回复在答案卡下面（每个答案最多 5 个，单个不超过 30 MB）；短命令直接写在正文的代码块里。
- **清空上下文**：在群里 @ 机器人或私聊发「清空上下文」（或 `/clear`），会先停掉这个聊天里在跑和排队的分析，再删掉这个聊天的全部数据：库里的提问、答案和原始消息，工作目录（聊天记录副本、附件、clone 的仓库），Claude 的会话记录；之后整理数据库、换一份新备份，旧备份里也不留。之后的提问开新会话，清空之前的群消息不再带进上下文。只删机器人自己存的东西，飞书里的聊天记录不动。
- **图示**：比文字更清楚时，答案的段落里会附上截图（聊天里读到的附件）、图表（柱状、折线、面积、饼图，飞书原生图表）或流程图（Graphviz 渲染的调用链、处理流程），点开可以看大图。
- **补充和更正**：在答案卡最底下的输入框里写补充或纠正，回车发送（引用卡片回复也行）。新卡片回复在那张卡片上，顶上写出追问的话，群里的人看得到在回答什么；结论变了，新卡片标出「本轮更正」，被纠正的那张卡片置灰并注明以最新回复为准。
- **先查根因**：排查类问题先查明根因，确认后才给解决方案；根因没确认时只给可能原因和验证方法。
- **上下文分层**：不同层的说法矛盾时按 T1 > T2 > T3 取舍，并在卡片上写明冲突点和采信依据；没有冲突时每层都照常采用。
  - T1 本轮提问：提问原文，以及它引用的消息、附带的文件和图片、合并转发的聊天记录。
  - T2 群聊记录：话题和群里的消息、图片、分享的文件，问题现象、讨论和结论多在这里（Jira 上的结论也多半来自群聊）。消息和图片合计默认带最近 500 条；提问里写明「最近 20 条」这类范围时以提问为准。
  - T3 Jira：从提问和话题里识别出的单号，经 qtmcp 读取原文。
- **图片**：每轮最多 20 张，提问附带的优先，其余按从新到旧取。先按 Claude 看图的上限缩放（长边不超过 2000 像素、单张不超过 4784 个视觉 token，Lanczos3），再压小体积——图片每次请求都要随上下文重新上传，体积直接影响速度：
  - 截图（PNG 等无损来源）无损重新编码成 WebP，和原图、PNG 比取最小的，像素逐位不变；
  - 照片（JPEG）本来就压过，不用缩放就保留原文件，要缩放的存质量 80 的 JPEG（缩小后看不出和 90 的差别，体积小三成）；
  - 长截图整张缩下来字就看不清（1080×8000 会缩成 270×2000），改成沿长边切成最多 6 段，每段都在上限以内、相邻重叠 5%，每段算一张；再长的先整体缩一点让 6 段装下；
  - 同时最多处理 2 张，一张 2560×1440 的截图约 0.1 秒。
- **文件**：原件都留在工作目录里，模型需要时用 Grep、Read 查全文；prompt 里放解析出的文字（单个文件最多 24 KB）。
  - 日志等文本先按 UTF-8、GBK 解码，个别坏字节不影响整份；太长的保留开头、结尾和出错的地方，同类报错只留第一次和最后一次并注明次数，堆栈整段保留；
  - zip、tar、tar.gz、gz 解开放在原件旁边，prompt 里放文件清单和有报错的日志；条目数、解压量都有上限，路径穿越、链接、压缩炸弹直接跳过；7z、rar 这类请转成 zip 再发；
  - Word、PPT 里嵌的截图也会读，全文另存一份文本方便检索；表格在限了内存的子进程里解析，异常文件拖不垮服务。
- **进度**：收到提问立刻回一张「⏳ 分析中」卡片。
  - 读上下文时边读边累加：已读多少条消息、图片和文件各处理了几个；
  - 分析时显示模型所处的阶段（思考中、调用工具、整理结论）、最近的步骤和用时，没有新进展也每 10 秒刷新一次用时；
  - 模型说了进展（比如「已定位到代码……」）就露出最近一句；开始写结论后，概述和关键命令一写完就先显示，不用等整份答案写完（写完整份常要二三十秒到两分钟），可以先动手；
  - 90 秒没有任何输出会提示可能卡住了；登录失效、额度受限、服务报错等直接在卡片上报出原因；
  - 卡片上可以停止。
- **响应速度**：卡片在收到消息的同时回出；同一个人连发的几条（比如先发截图、再打字提问）并进同一轮，只等 1.5 秒，只发了图片、文件的等 6 秒，等后面的提问；群成员、引用的消息、群聊记录同时拉取，图片和文件 6 个一起下载。

## 准备

### 部署机器

- Linux + systemd，能访问 `open.feishu.cn` 和内网的 Jira、Confluence 等服务。
- 部署用户有 sudo 权限，`claude` 已经登录（`claude auth status` 显示已登录）。服务以这个用户的身份运行。
- lark-cli：绑定机器人要用的应用（`lark-cli config init`），并登录用户身份（`lark-cli auth login`）。部署脚本用它读取 App ID、把你的 open_id 作为默认白名单、核对开发者后台的配置。
- qtmcp（可选）：配置默认在 `~/.config/qtmcp/config.toml`（`qtmcp init` 生成），可以用环境变量 `QTMCP_CONFIG` 指到别处。
- 编译需要 rustup 和 gcc：`rust-toolchain.toml` 锁定了工具链版本，第一次编译会自动安装。也可以在别处编译好，部署时用 `--binary` 指定。
- python3：部署脚本用它解析 JSON、生成配置。
- 可选：`poppler-utils`（读 PDF 附件）、LibreOffice（读 doc、ppt 等旧版 Office 附件）、`graphviz`（画答案里的流程图）。没装的话自检给警告，对应的功能用不了，其余照常。

### 飞书开发者后台

在[开发者后台](https://open.feishu.cn/app)逐项完成。部署脚本最后会用 lark-cli 核对一遍，缺什么会列出来。

1. **创建企业自建应用**：可以直接 `lark-cli config init --new`，在浏览器里完成创建；已有应用就用 `lark-cli config init` 绑定。
2. **添加机器人能力**：应用能力 → 添加应用能力 → 机器人。
3. **订阅事件**：开发配置 → 事件与回调 → 事件配置，订阅方式选「使用长连接接收事件」，添加 `im.message.receive_v1`（接收消息）。保存时如果提示没有检测到长连接，先跑完部署脚本让服务连上，再回来保存。
4. **订阅回调**：事件与回调 → 回调配置，订阅方式选「使用长连接接收回调」，添加 `card.action.trigger`（卡片回传交互），卡片上的按钮靠它。
5. **开通权限**：开发配置 → 权限管理。

   | 权限 | 用途 |
   | --- | --- |
   | `im:message.p2p_msg:readonly` | 接收用户私聊机器人的消息 |
   | `im:message.group_at_msg:readonly` | 接收群里 @ 机器人的消息 |
   | `im:message.group_msg` | 读取群里的全部消息（不只是 @ 机器人的），T2 群聊记录靠它。敏感权限，可能要企业管理员审核 |
   | `im:message:readonly` | 读取会话里的历史消息：引用的消息、话题、合并转发 |
   | `im:message:send_as_bot` | 以机器人身份发送卡片 |
   | `im:message:update` | 更新卡片：进度累加、更正后把旧卡片置灰 |
   | `im:resource` | 下载消息里的图片和文件，上传答案里要显示的截图和流程图 |
   | `im:chat.members:read` | 读取群成员姓名，群聊记录里标出是谁说的 |

6. **发布版本**：应用发布 → 版本管理与发布，创建版本，可用范围要包含会用机器人的人，然后申请发布。能力、事件、权限改过之后都要发布新版本才生效。要在群里用，还得把机器人拉进群。

## 一键部署

用已登录 `claude` 的普通用户，在仓库目录里运行：

```bash
git clone <仓库地址> ai-boot && cd ai-boot
deploy/setup.sh
```

脚本需要 root 的步骤自己 sudo，依次完成：

1. 检查环境：systemd、sudo、claude 的登录状态（按服务的环境检查，只认 `~/.claude` 里的登录）、lark-cli、qtmcp、cargo。
2. `cargo build --release -p ai-boot` 编译。
3. 用 `deploy/install.sh` 安装 ai-boot、systemd unit 和 `/etc/ai-boot`。claude 和 qtmcp 用你自己的，不另装。
4. 按 `config.example.toml` 生成 `/etc/ai-boot/config.toml`：
   - App ID 和白名单来自 lark-cli；
   - claude、qtmcp 用你 PATH 里的绝对路径；
   - 卡片链接白名单取 qtmcp 配置里各服务的域名；
   - 不配可选的 `[oauth]`、`[writeback]`。

   已有配置默认不动。
5. 把 App Secret 写进 `/etc/ai-boot/ai-boot.env`（root 所有、0600）。
6. 写 systemd drop-in `/etc/systemd/system/ai-boot.service.d/host-claude.conf`：
   - 以你的身份运行，复用 `~/.claude` 的登录；
   - 家目录绑进沙箱；
   - 藏起 `~/.ssh`、`~/.gnupg`、lark-cli、keyring 等凭据目录。
7. qtmcp 的 SSO 会话目录权限收紧到 0700。
8. 用 lark-cli 核对开发者后台：机器人能力、事件、回调、权限、是否已发布。缺的列出来，查不了就跳过。
9. `deploy/doctor.sh` 自检，通过后设为开机自启并（重新）启动服务。

**脚本里唯一要手动的一步**：第一次运行时粘贴一次 App Secret（开发者后台 → 凭证与基础信息 → 应用凭证）。上面「飞书开发者后台」的几步 lark-cli 没有接口可以代劳，只能在后台做一次，脚本负责核对。
- 输入不回显，只经 stdin 写进 `ai-boot.env`，不出现在命令行参数和日志里。
- 没有终端时，用环境变量 `FEISHU_APP_SECRET` 传入。
- lark-cli 按设计加密保存 App Secret、不对外提供，所以脚本没法替你读取。

| 参数 | 说明 |
| --- | --- |
| `--app-id cli_xxx` | 飞书应用的 App ID，默认取 lark-cli 绑定的应用 |
| `--allow-open-id ou_xxx` | 白名单，可重复。默认是你在 lark-cli 里的 open_id；open_id 按应用区分，lark-cli 绑的是别的应用时必须显式指定 |
| `--binary <路径>` | 用编译好的 ai-boot，跳过编译 |
| `--force-config` | 按本次环境重新生成配置，旧文件备份为 `config.toml.bak.<时间>` |
| `--reset-secret` | 重新输入 App Secret（默认保留已有的） |
| `--no-start` | 只安装和自检，不启动服务 |

脚本可以重复运行：已有的配置和 App Secret 默认保留，服务重启后换上新版本。

## 验证与运维

- **自检**：`sudo deploy/doctor.sh`。
  - 用服务的身份、环境和沙箱运行 `ai-boot doctor`。
  - 逐项检查：配置、数据目录、数据库、飞书凭据和机器人能力、白名单、claude 登录、工具判决 hook、qtmcp、SSO 会话目录、PDF/Office/graphviz 工具。
  - 改了配置或环境，先跑它。
- **日志**：`journalctl -u ai-boot -f`，看不到就加 sudo。
- **重启**：`sudo systemctl restart ai-boot`。改了 `/etc/ai-boot/config.toml` 要重启才生效。
- **升级**：`git pull && deploy/setup.sh`。
  - 重新编译，原子替换二进制，运行中的进程不受影响。
  - 保留配置和 App Secret。
  - 自检通过后重启服务。
- **更换 App Secret**：`deploy/setup.sh --reset-secret`。

| 路径 | 内容 |
| --- | --- |
| `/etc/ai-boot/config.toml` | 配置（root:部署用户，0640） |
| `/etc/ai-boot/ai-boot.env` | App Secret（root，0600） |
| `/var/lib/ai-boot/` | `ai-boot.db`：会话、提问和答案。`sessions/`：每个会话的工作目录，存聊天记录和附件（图片、文件）。`backup/`：数据库每天备份一次，只留最新一份 |
| `~/.claude/projects/-var-lib-ai-boot-sessions-*` | claude 的会话记录。复用本机登录时写在你的 `~/.claude` 下 |

**信息最多保留 30 天**：会话创建满 30 天，它的全部数据一起删除——库里的提问、答案、原始消息和写回记录，工作目录里的聊天记录和附件，以及 claude 的会话记录；删完整理数据库文件，被删的内容不会留在空闲页里。备份在清理之后做、只留一份，所以备份里也没有过期数据。之后群里再提问会开新会话，重新拉最近的聊天记录，需要的图片和文件重新下载。每天自动执行一次，不用人管。
| `~/.local/share/qtmcp/` | 和交互式 qtmcp 共用的 SSO 会话 |
| `/opt/ai-boot/bin/ai-boot`、`/etc/systemd/system/ai-boot.service{,.d/}` | 程序、unit 和 drop-in |

卸载：

```bash
sudo systemctl disable --now ai-boot
sudo rm -rf /etc/systemd/system/ai-boot.service /etc/systemd/system/ai-boot.service.d /opt/ai-boot
sudo systemctl daemon-reload
# 配置、密钥和聊天数据，确定不要了再删
sudo rm -rf /etc/ai-boot /var/lib/ai-boot /var/lib/qtmcp
rm -rf ~/.claude/projects/-var-lib-ai-boot-sessions-*
```

## 常见问题

**机器人没反应**

1. 看日志 `journalctl -u ai-boot -f`：
   - 有没有「长连接已建立」；
   - 发消息后有没有记录；
   - 白名单外的人会记「非白名单用户，已忽略」。
2. 看是不是长连接断了：日志里有「长连接断开，准备重连」。断开后 30 秒重连，还连不上就每 120 秒试一次，恢复后自动接着收消息，不用重启；应用被停用、凭据失效这类后台问题修好之后也会自己连上。进程本身异常退出由 systemd 在 5 秒后拉起。
3. 检查同一个应用的长连接是不是被别的进程占了。
   - 飞书长连接是集群模式：同一应用有多个客户端在线时，每条事件只随机投给其中一个，表现为时灵时不灵。
   - 停掉其他连着这个应用的进程，常见的有 `lark-cli event consume`、另一台机器上的 ai-boot、别的用这个 App ID 的 SDK 程序。
4. 检查开发者后台：
   - 事件订阅方式是不是长连接，`im.message.receive_v1` 有没有加，改完有没有发布新版本；
   - 群里要 @ 机器人，机器人要在群里；
   - 提问的人要在应用的可用范围内。

**提示权限不足**

- 飞书的报错会写明缺哪个权限，部署脚本也会核对。
- 在权限管理里开通后要发布新版本。
- `im:message.group_msg` 是敏感权限，要等企业管理员审核通过。

**claude 没登录，或者自检的 claude 一项失败**

- 用部署用户执行 `claude auth login`，再跑 `sudo deploy/doctor.sh`。
- 服务不认 `CLAUDE_CONFIG_DIR`、`CLAUDE_CODE_OAUTH_TOKEN`，只用 `~/.claude` 里的登录，登录过期也会让这一项失败。

**qtmcp 的 SSO 登录**

- 机器人和你本机交互式使用的 qtmcp 共用 `QTMCP_STATE_DIR`（默认 `~/.local/share/qtmcp`）里的 `sessions.v2.json`，一边登录过，另一边就能直接用。
- 想确认是否真的发生了一次登录，看 `sessions.v2.json` 里的 `totp_last_step` 有没有变：只有它变了才说明登录过，其他字段的变化不算。
- 这个目录的权限要是 0700，自检会检查。
- 服务的沙箱里用不了系统 keyring：SSO 的账号、密码和 TOTP 密钥要写在 qtmcp 配置的 `[sso]` 段（0600），SSO 会话过期后机器人才能自己重新登录。只存在 keyring 里的话，过期后 qtmcp 那一项会失败。

## 配置

`/etc/ai-boot/config.toml` 的主要字段（完整说明见 `config.example.toml` 的注释）：

| 字段 | 说明 |
| --- | --- |
| `feishu.app_id` | 应用的 App ID；App Secret 不在配置里 |
| `access.allowed_open_ids` | 白名单 open_id（按应用区分）。也可以用 `allowed_emails`，但应用要有「通过手机号或邮箱获取用户 ID」的权限 |
| `agent.model` / `agent.effort` | 不写 model 就跟随 CLI 的默认模型；effort 取 low / medium / high / xhigh / max |
| `agent.timeout_secs` / `agent.budget_usd` | 单轮超时；单轮预算，订阅账号下是估算值，只起熔断作用 |
| `agent.max_concurrent` | 同时分析的轮数，1～4 |
| `agent.claude.program` | claude 的路径。用 PATH 里的那份会跟着自动升级，要锁版本就指向一份固定拷贝 |
| `[[agent.mcp]]` | qtmcp 的命令、`--toolsets`，以及 `QTMCP_CONFIG`、`QTMCP_STATE_DIR` |
| `render.link_hosts` | 卡片里允许出现链接的域名（含子域），比如 `jira.example.com` |
| `context.window_messages` / `context.window_minutes` | 群聊记录最多带几条（上限 500）、往前看多少分钟（0 不限） |
| `context.office_legacy` | doc、ppt 等旧格式先用 soffice 转换 |
| `context.knowledge_file` | 可选：环境速查（团队整理的部署方式、命名空间、常用路径、代码在哪个仓库），接在规则后面给模型参考；每轮读一次，改了不用重启 |
| `[oauth]`、`[writeback]` | 可选：以你的身份读群里贴的云文档；把闭环方案写回 Jira、Confluence。部署脚本不配，需要时对照示例补上 |

## 安全说明

聊天内容、附件、文档和 Jira 正文都是不可信输入，下面的限制都是按这个前提定的。

- **复用本机登录的取舍**：好处和代价如下。要隔离，就用独立的服务用户：`deploy/install.sh` 默认的 `ai-boot` 用户，配合 unit 里的 `CLAUDE_CONFIG_DIR` 和 `CLAUDE_CODE_OAUTH_TOKEN`。
  - 好处：省掉单独的 token 和配置，claude 跟着你的安装自动升级。
  - 代价：服务以你的 Unix 用户身份运行，用量计入你的 Claude 账号额度，和你本人的 claude 共用 `~/.claude.json` 等文件。
- **沙箱**：
  - systemd 层：
    - `ProtectSystem=strict`、`NoNewPrivileges`，不给任何 capability；
    - `ProtectHome=tmpfs` 后只绑回你的家目录，因为 claude 要写 `~/.claude.json`；
    - 用 `InaccessiblePaths` 藏起 `~/.ssh`、`~/.gnupg`、`~/.docker`、`~/.kube`、`~/.config/gh`、`~/.npmrc`、lark-cli 的配置、keyring 和 Chrome 的配置目录；
    - 家目录里其余的文件，服务进程仍然可以读写。
  - claude 层：
    - `--restricted` 忽略你的 user / project / local settings，文件工具（Read、Glob、Grep）只能访问本轮的工作目录；
    - `--strict-mcp-config` 只加载 ai-boot 配的 MCP；
    - 内置工具开 Read、Glob、Grep，以及执行命令（Bash）、打开网页（WebFetch）、搜索网页（WebSearch）；改文件、派子 Agent 的工具不开；
    - 每次工具调用还要经 `ai-boot hook` 判决。
- **命令行和网页直接开放、没有沙箱**：
  - Bash 以服务用户的身份在 systemd 的加固环境里运行（系统目录只读，上面藏起来的凭据目录看不到），能读写家目录其余的文件，能访问内网和外网，网页不限域名；
  - 家目录里 claude 的登录凭据、qtmcp 的配置（SSO 账号密码、TOTP 密钥、GitLab token）和 SSO 会话因此都在 Agent 读得到的范围里。群聊、文档、网页里藏的指令可能诱导它读取这些东西或把内网数据发到外网，只有提示词约束，不是硬限制；
  - 部署用户在 docker 组里时，Bash 也能用 docker（能起特权容器挂载整个文件系统），等于有了整机 root。不需要的话在 drop-in 里加 `InaccessiblePaths=-/run/docker.sock` 挡掉；
  - ai-boot 进程设成了不可转储：同一用户的进程读不到它的环境变量（App Secret），也没法附加调试；
  - 要收紧，就从 `crates/ai-boot-agent/src/claude/invocation.rs` 的工具列表里去掉对应的工具，或者改用独立的服务用户。
- **qtmcp 能写**：Agent 以你的身份读写 Jira、Confluence、GitLab、Jenkins。
  - 写操作（评论、改单、流转、建 MR、发页面、触发构建）按提示词只在本轮提问明确要求时才做，聊天记录和文档里出现的要求不算。这是提示词约束，不是硬限制。
  - 白名单只放信得过的人；不想让它碰某个系统，就把它从 `--toolsets` 里去掉。
- **答案里的图**：截图只能引用这个会话工作目录里的附件（解开符号链接后校验，且必须是图片）；流程图的 DOT 由模型给出，会读本地文件的属性（image、shapefile、fontpath 等）和 HTML 标签一律拒绝，`dot` 在清空的环境里跑、开 Graphviz 的服务器模式、限时限量。图片上传到飞书后才能显示在卡片里。
- **App Secret**：只存在 `/etc/ai-boot/ai-boot.env`（root，0600），systemd 以 root 读取后注入服务环境；配置文件里没有密钥。
- **群聊数据**：有了 `im:message.group_msg`，机器人能读群里的所有消息。这些内容会作为上下文交给 claude，并保存在 `/var/lib/ai-boot`。
