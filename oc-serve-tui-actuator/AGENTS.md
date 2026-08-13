# 项目知识库

## 概述

一组 shell + Python 脚本工具，围绕 `opencode serve` 启动 + 项目选择/attach 工作流构建。所有 UI 通过 `gum` TUI 实现，无构建步骤。

- **启动器**（`oc-serve-start.sh`）：TUI 菜单，负责启动 `opencode serve`（可选叠加 rathole 内网穿透）和升级 opencode/oh-my-openagent。
- **执行器**（`oc-serve-tui-actuator.sh`）：TUI 项目选择器 + 路径手动输入，最后 `exec opencode attach` 替换当前 shell。
- **路径管理工具**（`path-list-actor.py`）：纯 Python 工具（仅标准库），管理 `path-list.md` 索引——在指定路径下创建/读取 `AGENTS.md` 的小节 id 并同步进索引。
- **共享库**（`lib-path-list.sh`）：路径索引的 bash 端原子读写 + 与 SilverBullet 远端的合并同步；被 `oc-serve-tui-actuator.sh` 通过 `source` 加载。

## 目录结构

```
.
├── oc-serve-start.sh             # bash — 启动器 TUI
├── oc-serve-tui-actuator.sh      # bash — 执行器 TUI（attach 入口）
├── lib-path-list.sh              # bash — 路径索引共享函数（被 actuator source）
├── path-list-actor.py            # python — 路径索引管理工具（add/list/remove）
├── path-list.md                  # JSON 索引（path → sections[]）
├── .oc-serve-auth.env            # 自动生成，chmod 600，保存 HTTP Basic 凭据
├── .omo/run-continuation/         # opencode 会话状态（自动管理，不要编辑）
└── .test-opencode-register/      # 内部占位目录
```

## 修改指引

| 任务 | 位置 | 备注 |
|---|---|---|
| 修改启动或升级行为 | `oc-serve-start.sh` | 顶层 `main_menu`（约 L586）分发所有流程 |
| 修改项目选择或 attach 行为 | `oc-serve-tui-actuator.sh` | `inject_auth` 之后是顺序顶层逻辑 |
| 修改 HTTP Basic 凭据逻辑 | `resolve_password`（start.sh L33）/`inject_auth`（actuator.sh L101） | 都从 `.oc-serve-auth.env` 读取 |
| 新增可被环境变量覆盖的配置项 | 两个脚本里搜 `:-` | 全部遵循 `${VAR:-默认值}` 模式 |
| 修改 rathole 隧道接线 | `launch_rathole`（start.sh L502）+ `../rathole/settings/33-9464.toml` | 隧道配置在仓库外 |
| 修改路径索引的本地读写 | `lib-path-list.sh` | bash 端原子写 + SilverBullet 远端同步 |
| 修改路径索引的 CRUD 工具 | `path-list-actor.py` | `add`/`list`/`remove` 三个子命令 |
| 修改路径校验规则 | `validate_local_path`（actuator.sh L204）/`validate_path`（path-list-actor.py L60） | 两侧必须同步 |

## 关键函数

### oc-serve-start.sh

| 符号 | 行号 | 作用 |
|---|---|---|
| `main_menu` | 586–626 | gum 循环，分发所有流程 |
| `resolve_password` | 33–66 | 解析/创建 HTTP Basic 凭据 |
| `run_launch_flow` | 537–583 | 端口→OC→rathole→汇总流水线 |
| `launch_oc_serve` | 468–500 | 后台启动 `opencode serve` 并等待就绪 |
| `launch_rathole` | 502–535 | 后台启动 rathole 隧道进程 |
| `run_upgrade_flow` | 352–395 | 编排升级三步 |
| `upgrade_opencode` | 206–239 | 执行 `opencode upgrade` |
| `upgrade_omo` | 242–288 | 通过 bun/npm 更新 oh-my-openagent |
| `verify_upgrade` | 291–349 | 校验升级后版本与配置 |
| `prompt_for_port` | 432–465 | 交互式端口输入与校验 |
| `cleanup` | 103–122 | SIGINT/TERM 时杀死 OC + rathole 进程 |
| `is_port_busy` | 413–421 | 通过 lsof 或 nc 检查 TCP LISTEN |

### oc-serve-tui-actuator.sh

| 符号 | 行号 | 作用 |
|---|---|---|
| `inject_auth` | 101–115 | 加载 `.oc-serve-auth.env` → 导出凭据 |
| `basic_auth_args` | 123–130 | 把 `user:<pass>` 写入临时 curl 配置文件 |
| `api_curl` | 133–150 | curl 封装：10s 超时、通过 config 文件传 Basic 凭据 |
| `fetch_projects` | 155–171 | GET /project，校验 JSON 数组 |
| `create_session` | 174–200 | POST /api/session，提取 session id |
| `validate_local_path` | 204–234 | 拒绝 URL、反斜杠、shell 元字符 |
| `normalize_local_path` | 236–249 | 把 `~` 展开为 `$HOME` |
| `projects_to_choices` | 268–310 | JSON → gum 选项，附加手动输入项 |

### lib-path-list.sh

bash 端 helper，被 `oc-serve-tui-actuator.sh` 通过 `source` 加载。负责：
- `path-list.md` 的原子写（tempfile + `os.replace`，失败回滚）
- 与 SilverBullet 远端的双向合并（path 主键、sections 去重并集）
- 远端不可达时降级到本地缓存

### path-list-actor.py

| 符号 | 作用 |
|---|---|
| `validate_path` | 拒绝协议符号、反斜杠、shell 元字符、控制字符；要求路径以 `/`/`./`/`../`/`~/` 开头 |
| `load_index` / `save_index` | `path-list.md` 的 JSON 读写；含 `.bak` 备份与损坏恢复 |
| `section_ids_for_md` | 从 markdown 的二级标题生成 `seq_<8hex>` 形式的 id |
| `ensure_md` | 路径下若不存在 `AGENTS.md` 就用默认模板创建 |
| `add_path` / `remove_path` / `list_entries` | CLI 子命令的内部实现 |

## 约定

- `set -euo pipefail` 出现在所有 bash 脚本顶部；`SCRIPT_DIR` 通过 `BASH_SOURCE`+`realpath` 解析。
- UI 全部走 `gum`（`style`/`spin`/`choose`/`input`）。状态输出不用裸 `echo`——用 `gum style` 或 `fail()`。
- 变量全部双引号包裹；curl 参数用 `args+=(...)` 数组构建；任何地方都不用 `eval`。
- HTTP Basic 凭据通过 `mktemp` 配置文件传给 curl（绝不走 CLI `-u` 避免 `ps` 泄漏），临时文件通过 `trap EXIT` 删除。
- 凭据文件 `.oc-serve-auth.env` 自动生成时设 `chmod 600`；密码来源 `openssl rand`，fallback `/dev/urandom`。
- `path-list-actor.py` 使用 `tempfile.mkstemp` + `os.replace` 实现 `path-list.md` 原子写，临时文件失败时尽力清理。
- `actuator.sh` 末尾是 `exec opencode attach ...`——整个 shell 进程被替换，没有返回路径。
- `start.sh` 把 `OC_PID` 和 `RATHOLE_PID` 留在作用域内，cleanup trap 负责清理。
- 无测试、无 linter、无 formatter——保持最小 diff，肉眼 review。
- 脚本不接受位置参数；所有配置通过环境变量（每个都有默认值）。

## 反模式（项目内已固化，不要破坏）

- 不要把 `set -euo pipefail` 换成更宽松的设置——很多流程依赖 fail-fast。
- 不要把 Basic 凭据放到 curl CLI（`-u user:pass`）——会绕过临时文件凭据路径。
- 不要编辑 `.omo/run-continuation/` 下的文件——opencode 会话状态，会被自动重新生成。
- 不要用更严格 pattern 替换 `rm -rf "$OC_CACHE_DIR"/packages/oh-my-openagent*`（start.sh:265）——先确认 glob 范围再改。
- 不要在 `opencode serve &` 外加 `nohup`/`disown`（start.sh:478）——cleanup trap 依赖父进程保持存活。
- 不要把用户输入灌进 `eval` 或 `bash -c`——`validate_local_path` 显式拒绝 URL、反斜杠、shell 元字符。
- 不要去掉 `source "$AUTH_ENV"` 上的 `2>/dev/null`（actuator.sh:108）——除非换成更窄的错误检查。
- 不要破坏 `lib-path-list.sh` 的 tempfile + `os.replace` 原子写模式——并发或中途崩溃会损坏索引。

## 命令

```bash
# 启动 TUI（首次运行会写 .oc-serve-auth.env）
./oc-serve-start.sh

# 项目选择 + attach TUI（默认连 http://127.0.0.1:9464）
./oc-serve-tui-actuator.sh

# 覆盖 attach 目标或默认项目目录
ATTACH_URL=http://host:port ./oc-serve-tui-actuator.sh
OC_DEFAULT_DIR=/path/to/project ./oc-serve-tui-actuator.sh

# 路径索引管理
./path-list-actor.py add /path/to/project
./path-list-actor.py list
./path-list-actor.py remove /path/to/project

# 编辑后做语法检查
bash -n oc-serve-start.sh
bash -n oc-serve-tui-actuator.sh
bash -n lib-path-list.sh
python3 -c "import ast; ast.parse(open('path-list-actor.py').read())"
```

## 依赖

| 脚本 | 必需 | 可选 |
|---|---|---|
| `oc-serve-start.sh` | `gum`、`opencode`、`realpath` | `openssl`、`lsof`、`nc`、`rathole`、`bun`、`npm`/`npx` |
| `oc-serve-tui-actuator.sh` | `bash`、`gum`、`jq`、`curl`、`opencode`、`realpath` | — |
| `lib-path-list.sh` | `bash` 4+、`curl`、`jq` | — |
| `path-list-actor.py` | `python3` 标准库 | — |

## 备注

- 本仓库**未**纳入任何版本控制（`.git` 已删除，`.gitignore` 也已移除）。修改前请自行做好备份。
