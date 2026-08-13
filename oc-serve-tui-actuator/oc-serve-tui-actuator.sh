#!/usr/bin/env bash
#
# oc-serve-tui-actuator.sh
#
# 启动 opencode 的交互式入口（attach 模式）：
#   - 阶段一：通过 opencode serve API (GET /project) 读取所有已知项目
#   - 阶段二：使用 gum choose 罗列项目，让用户用 ↑/↓/Tab 切换选择
#   - 阶段三：
#       · 选中某项目  → POST /api/session 创建会话 → attach --dir <path> --session <id>
#       · 选中末尾项 → 手动输入本地路径（URL/域名等非路径会被拒绝）→ attach --dir <path>
#
# 用法：
#   ./oc-serve-tui-actuator.sh
#   OC_DEFAULT_DIR=/some/path ./oc-serve-tui-actuator.sh   # 影响"手动输入"占位符
#   ATTACH_URL=http://other:port ./oc-serve-tui-actuator.sh  # 覆盖 attach URL（默认 http://127.0.0.1:9464）
#
# ⚠️ 注意：opencode 的 attach 模式**不会**调 projects.open()，所以手动输入的路径
#    下次不会出现在 GET /project 清单里。这是 opencode 1.18.1 客户端的设计：
#    项目注册由 opencode 主模式 (`opencode [project]`) 触发，而非 attach。
#

# 强制 bash：本脚本使用 [[ ]]、process substitution、local、$BASH_SOURCE 等
# bash 特有的语法。macOS 的 /bin/sh 是 bash 3.2.57 的 POSIX 兼容模式，
# 它设置了 $BASH_VERSION（让旧的 `[[ -z "${BASH_VERSION:-}" ]]` 检测失效），
# 但禁用了 process substitution，所以脚本会在 line 299 的 `done < <(...)` 报错。
# 用 $BASH 的 basename 做检测：BASH 在真正 bash 下是 /bin/bash；在 sh
# (POSIX 模式) 下是 /bin/sh；在 dash 下未设置。
case "${BASH##*/}" in
    bash|bash-*) : ;;  # genuine bash, keep running
    *) exec bash "$0" "$@" ;;  # sh/dash/zsh masquerading — re-exec
esac

set -euo pipefail

# ---- 配置 -----------------------------------------------------------------

DEFAULT_DIR="${OC_DEFAULT_DIR:-/Users/samuel/.config/opencode}"
ATTACH_URL="${ATTACH_URL:-http://127.0.0.1:9464}"
SCRIPT_DIR="$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
AUTH_ENV="${OC_SERVE_AUTH_ENV:-$SCRIPT_DIR/.oc-serve-auth.env}"
source "$SCRIPT_DIR/lib-path-list.sh"

# ---- gum / jq 依赖检查 ---------------------------------------------------

check_gum() {
    if command -v gum &>/dev/null; then
        return 0
    fi
    cat >&2 <<'EOF'

  ╭──────────────────────────────────────────────────────────────╮
  │  ❌ 未找到 gum 命令                                          │
  │                                                              │
  │  本脚本使用 gum (charmbracelet/gum) 美化终端输出。           │
  │  请先安装：                                                  │
  │                                                              │
  │    macOS:  brew install gum                                  │
  │    Linux:  sudo pacman -S gum  或  go install ...            │
  │                                                              │
  │  详见: https://github.com/charmbracelet/gum                   │
  ╰──────────────────────────────────────────────────────────────╯

EOF
    exit 1
}

check_jq() {
    if command -v jq &>/dev/null; then
        return 0
    fi
    cat >&2 <<'EOF'

  ╭──────────────────────────────────────────────────────────────╮
  │  ❌ 未找到 jq 命令                                           │
  │                                                              │
  │  本脚本使用 jq 解析 opencode serve API JSON 响应。           │
  │  请先安装：                                                  │
  │                                                              │
  │    macOS:  brew install jq                                   │
  │    Linux:  sudo apt install jq  /  yum install jq            │
  ╰──────────────────────────────────────────────────────────────╯

EOF
    exit 1
}

check_curl() {
    if command -v curl &>/dev/null; then
        return 0
    fi
    gum style --foreground 196 --bold "❌ 未找到 curl 命令" >&2
    exit 1
}

check_gum
check_jq
check_curl

# ---- 鉴权注入 ------------------------------------------------------------
# 把 OPENCODE_SERVER_USERNAME/PASSWORD 注入到当前 shell，供后续 curl 使用。
# 同时 export SilverBullet 凭据（SB_URL/SB_AUTH_TOKEN/SB_USER/SB_PASSWORD），
# 让 lib-path-list.sh 的 path_list_read/write 直接走远端 API。
inject_auth() {
    if [[ -n "${OPENCODE_SERVER_PASSWORD:-}" ]]; then
        return 0
    fi
    if [[ -f "$AUTH_ENV" ]]; then
        set +u
        # shellcheck disable=SC1090
        source "$AUTH_ENV" 2>/dev/null
        set -u
    fi
    if [[ -z "${OPENCODE_SERVER_PASSWORD:-}" ]]; then
        gum style --foreground 196 --bold "❌ 未找到鉴权凭据：设置 OPENCODE_SERVER_PASSWORD 或准备 $AUTH_ENV" >&2
        exit 1
    fi
    # SilverBullet 凭据是可选的：缺失时 lib-path-list.sh 会回退本地缓存并 warn。
    # 不在此处硬性 exit — 远端不可用不应阻塞整个 actuator 启动。
}

# ---- curl 通用选项 --------------------------------------------------------
# 把鉴权注入到一个临时文件，避免每次 curl 重写命令行。
CURL_AUTH_FILE="$(mktemp -t oc-serve-curl.XXXXXX)"
cleanup_curl_auth() { rm -f "$CURL_AUTH_FILE" 2>/dev/null || true; }
trap cleanup_curl_auth EXIT

basic_auth_args() {
    # 把 -u user:pass 写入临时文件（curl -K 形式），避免密码出现在 ps 输出。
    local user="${OPENCODE_SERVER_USERNAME:-opencode}"
    local pass="${OPENCODE_SERVER_PASSWORD:-}"
    {
        printf 'user = "%s:%s"\n' "$user" "$pass"
    } > "$CURL_AUTH_FILE"
}

# 安全 curl 封装：超时 10s，失败时通过 stderr 报告。
api_curl() {
    local method="$1"
    local path="$2"
    local body="${3:-}"

    local url="$ATTACH_URL$path"
    local args=(
        --silent --show-error
        --max-time 10
        --config "$CURL_AUTH_FILE"
        -X "$method"
        -H "Content-Type: application/json"
    )
    if [[ -n "$body" ]]; then
        args+=(--data "$body")
    fi
    curl "${args[@]}" "$url"
}

# ---- opencode serve API 调用 ----------------------------------------------

# POST /api/session?directory=<dir> → 创建会话，输出 session id 到 stdout。
create_session() {
    local target_dir="$1"
    local title="${OC_SESSION_TITLE:-TUI-Launched-$(date +%s)}"
    local body
    # v2 API (POST /api/session): directory 通过 body 的 location.directory 传入，
    # 而非 query 参数。query 参数 ?directory= 在 v2 API 中不生效。
    body="$(jq -n --arg t "$title" --arg d "$target_dir" \
        '{title:$t, location:{directory:$d}}')"

    local tmp rc response
    tmp="$(mktemp -t oc-serve-cs.XXXXXX)"

    ( api_curl POST "/api/session" "$body" > "$tmp" 2>/dev/null ) &
    local pid=$!

    gum spin -s dot --title "  正在创建新会话…" \
        -- bash -c "while kill -0 $pid 2>/dev/null; do sleep 0.05; done" 2>/dev/null || true

    rc=0
    wait $pid || rc=$?
    response="$(cat "$tmp" 2>/dev/null || true)"
    rm -f "$tmp"

    if [[ $rc -ne 0 ]]; then
        gum style --foreground 196 --bold "❌ 创建会话请求失败 (POST /api/session)" >&2
        return 1
    fi

    local session_id
    if ! session_id="$(printf '%s' "$response" | jq -r '.data.id // empty' 2>/dev/null)"; then
        gum style --foreground 196 --bold "❌ 创建会话响应解析失败" >&2
        printf 'Response: %s\n' "${response:0:300}" >&2
        return 1
    fi

    if [[ -z "$session_id" || "$session_id" == "null" ]]; then
        gum style --foreground 196 --bold "❌ 创建会话失败：响应未包含 session id" >&2
        printf 'Response: %s\n' "${response:0:300}" >&2
        return 1
    fi

    printf '%s' "$session_id"
}

# ---- 路径校验 ------------------------------------------------------------

validate_local_path() {
    local p="$1"

    if [[ "$p" == *"://"* ]]; then
        gum style --foreground 196 "[拒绝] 检测到协议符号 \"://\"，这不是一个本地路径。" >&2
        return 1
    fi

    if [[ "$p" == *\\* ]]; then
        gum style --foreground 196 '[拒绝] 路径包含反斜杠 "\"，请使用 POSIX 风格（正斜杠）。' >&2
        return 1
    fi

    if [[ "$p" =~ ^.*[\$\`\;\&\|\<\>\(\)\{\}\"\']+.*$ ]]; then
        gum style --foreground 196 "[拒绝] 路径包含 shell 元字符，禁止。" >&2
        return 1
    fi

    # 控制字符 (0x00-0x1F, 0x7F) 检测。
    if [[ "$p" == *[[:cntrl:]]* ]]; then
        gum style --foreground 196 "[拒绝] 路径包含控制字符（如换行符 / NUL / DEL 等）。" >&2
        return 1
    fi

    # Bare "~" is literal $HOME — refusing here blocks normalize_local_path
    # from writing the HOME string into path-list.md (which later breaks the
    # menu fold "$HOME/*" because $HOME has no trailing slash).
    if [[ "$p" == "~" ]]; then
        gum style --foreground 196 '[拒绝] "波浪号 ~" 单独使用代表 $HOME 本身，不是项目目录。请输入 ~/Projects/MyApp 等带子目录的路径，或留空使用默认目录。' >&2
        return 1
    fi

    if [[ "$p" == /* ]] || [[ "$p" == ./* ]] || [[ "$p" == ../* ]] || [[ "$p" == ~* ]]; then
        return 0
    fi

    gum style --foreground 196 '[拒绝] 不是合法路径（必须以 "/"、"./"、"../" 或 "~" 开头）。' >&2
    return 1
}

normalize_local_path() {
    local p="$1"

    if [[ "$p" == "~" ]]; then
        printf '%s' "${HOME:-/}"
        return 0
    fi
    if [[ "$p" == "~"/* ]]; then
        printf '%s/%s' "${HOME:-/}" "${p#\~/}"
        return 0
    fi

    printf '%s' "$p"
}

# ---- UI 横幅 --------------------------------------------------------------

banner() {
    gum style \
        --border rounded \
        --border-foreground 39 \
        --padding "0 2" \
        --align center \
        --width 52 \
        --bold \
        --foreground 39 \
        "opencode TUI 启动器"
}

# ---- 主流程 ---------------------------------------------------------------

banner

echo ""
gum style --foreground 39 "attach URL：" && printf "  "
gum style --foreground 214 "$ATTACH_URL"
echo ""

# 鉴权注入（脚本内全程有效）
inject_auth
basic_auth_args

# ---- path-list 模式：path 选择 ----

# 通用加载包装：在 gum spin 中执行命令字符串，stdout 透传。
# ⚠ gum spin 启动独立子 shell — 主 shell 的函数/变量不可见，
#    命令字符串内若要调用 lib-path-list.sh 的函数，必须重新 source。
run_loading_spinner() {
    local title="$1"
    local cmd="$2"
    local spinner="${3:-dot}"
    gum spin -s "$spinner" --title "$title" -- bash -c "$cmd"
}

# 读取 path-list.md 原始 JSON 到 stdout。文件缺失返回空。损坏自动还原 .bak。
read_path_list() {
    if ! path_list_validate; then
        exit 1
    fi
    # lib-path-list.sh 依赖 $SCRIPT_DIR / $PATH_LIST_FILE / $SB_URL /
    # $SB_COOKIE_NAME — 子 shell 中显式注入这些环境变量。
    run_loading_spinner \
        "  正在同步 path-list.md（远端 SilverBullet）…" \
        "export SCRIPT_DIR='$SCRIPT_DIR' PATH_LIST_FILE='$PATH_LIST_FILE' SB_URL='$SB_URL' SB_COOKIE_NAME='$SB_COOKIE_NAME'; source '$SCRIPT_DIR/lib-path-list.sh'; path_list_validate && path_list_read"
}

# 把 ISO 8601 时间戳格式化为人类可读相对时间。
# 7 天内：「刚刚」「5 分钟前」「2 小时前」「昨天」「3 天前」
# 超过 7 天：「YYYY-MM-DD」（固定宽度，便于 gum choose 对齐）
# 空时间戳 → "—"
format_relative_time() {
    local iso="$1"
    [[ -z "$iso" ]] && {
        printf '—'
        return 0
    }

    # 解析 "+08:00" / "Z" → epoch 秒
    local epoch
    if ! epoch="$(date -j -f '%Y-%m-%dT%H:%M:%S%z' "$iso" +%s 2>/dev/null)"; then
        # GNU date fallback
        epoch="$(date -d "$iso" +%s 2>/dev/null || echo 0)"
    fi
    [[ "$epoch" -eq 0 ]] && {
        printf '%s' "${iso:0:10}"
        return 0
    }

    local now diff
    now="$(date +%s)"
    diff=$(( now - epoch ))
    (( diff < 0 )) && diff=0

    if (( diff < 60 )); then
        printf '刚刚'
    elif (( diff < 3600 )); then
        printf '%d 分钟前' $(( diff / 60 ))
    elif (( diff < 86400 )); then
        printf '%d 小时前' $(( diff / 3600 ))
    elif (( diff < 172800 )); then
        printf '昨天'
    elif (( diff < 604800 )); then
        printf '%d 天前' $(( diff / 86400 ))
    else
        printf '%s' "${iso:0:10}"
    fi
}

# 把 path-list 原始 JSON 转成 gum choose 选项。
# 顺序固定：➕ 新建 path / <known paths>。
# 已知 path 顺序由 lib-path-list.sh 按 lastOpenedAt 倒序保证。
# 每行格式：<relative_time>  —  N sessions  —  <path>
# 注：不做视觉对齐 — bash 的 printf 按字节填充，对全角中文字符（2 列宽）
# 无法保证列视觉对齐。保持简单字符串拼接，gum choose 直接渲染。
paths_to_choices() {
    local json="${1:-}"
    if [[ -z "$json" ]]; then
        json="[]"
    fi

    # 1) 第一个：新建 path
    printf '➕  新建 path\n'

    # 2) 中间：每个已知 path（已按 lastOpenedAt 倒序）
    if [[ "$json" != "[]" ]]; then
        while IFS= read -r line; do
            [[ -z "$line" ]] && continue
            local p count last
            p="$(printf '%s' "$line" | jq -r '.path // ""')"
            count="$(printf '%s' "$line" | jq -r '.sections | length // 0')"
            last="$(printf '%s' "$line" | jq -r '.lastOpenedAt // ""')"

            # 防御性：跳过虚拟根
            [[ "$p" == "/" ]] && continue

            # home 折叠：$HOME 本身和 $HOME/* 都折叠为 ~ / ~/*，
            # 这样下游 *) 分支的 awk 还原逻辑才能 cover 全部情形。
            local shown="$p"
            if [[ -n "$HOME" && "$shown" == "$HOME" ]]; then
                shown="~"
            elif [[ -n "$HOME" && "$shown" == "$HOME"/* ]]; then
                shown="~${shown#$HOME}"
            fi

            local rel
            rel="$(format_relative_time "$last")"
            printf '%s  —  %s sessions  —  %s\n' "$rel" "$count" "$shown"
        done < <(printf '%s' "$json" | jq -c '.[]')
    fi
}

# GET /session?directory=<path> → 输出 JSON 数组。失败时返回 1，但
# 调用方仍允许「➕ 新建会话」项（POST 可能会唤醒 serve）。
fetch_sessions_for_path() {
    local target_dir="$1"
    local tmp rc body
    tmp="$(mktemp -t oc-serve-sess.XXXXXX)"

    ( api_curl GET "/session?directory=$(printf '%s' "$target_dir" | jq -sRr @uri)" > "$tmp" 2>/dev/null ) &
    local pid=$!

    gum spin -s dot --title "  正在拉取该项目的会话列表…" \
        -- bash -c "while kill -0 $pid 2>/dev/null; do sleep 0.05; done" 2>/dev/null || true

    rc=0
    wait $pid || rc=$?
    body="$(cat "$tmp" 2>/dev/null || true)"
    rm -f "$tmp"

    if [[ $rc -ne 0 ]]; then
        warn "GET /session 失败（可能 opencode serve 未就绪）"
        return 1
    fi
    if ! printf '%s' "$body" | jq -e 'type == "array"' >/dev/null 2>&1; then
        warn "GET /session 返回非预期响应"
        return 1
    fi
    printf '%s' "$body"
}

# 把 session 列表 JSON 转成 gum choose 选项。
# 格式：<完整 session_id>  —  <title>（ID 在前，便于快速识别 / 复制）
# 第一项：➕ 新建会话（attach）
# 末项：🗑️ 删除此项目记录（删除 path-list.md 中该 path 的条目，含远端 + 本地）
sessions_to_choices() {
    local json="${1:-[]}"

    printf '➕ 新建会话（attach）\n'

    if [[ "$json" != "[]" ]]; then
        while IFS= read -r line; do
            [[ -z "$line" ]] && continue
            local title sid
            title="$(printf '%s' "$line" | jq -r '.title // "(untitled)"')"
            # 把空字符串也归一为 "(untitled)"，避免菜单行出现裸 sid
            [[ -z "$title" ]] && title="(untitled)"
            sid="$(printf '%s' "$line" | jq -r '.id // ""')"
            [[ -z "$sid" ]] && continue
            printf '%s  —  %s\n' "$sid" "$title"
        done < <(printf '%s' "$json" | jq -c '.[]')
    fi

    printf '🗑️  删除此项目记录\n'
}

# 同步：把 path 写入 path-list.md（合并或新增）
sync_path_list_for_new_path() {
    local target="$1"
    local new_content
    new_content="$(path_list_upsert_path "$target")"
    if ! path_list_write "$new_content"; then
        warn "path-list.md 同步失败（path 仍可使用）"
        return 1
    fi
    return 0
}

# 同步：把 session id push 到对应 path 的 sections
sync_path_list_for_new_session() {
    local target="$1"
    local sid="$2"
    local new_content
    new_content="$(path_list_append_session "$target" "$sid")" || return 1
    if ! path_list_write "$new_content"; then
        warn "path-list.md 同步失败（session 仍可使用）"
        return 1
    fi
    return 0
}

# 阶段一：读取本地 path-list.md
PATHS_RAW="$(read_path_list)" || exit 1

# 阶段二：把 path-list 转成 gum 菜单选项
PATH_CHOICE_FILE="$(mktemp -t oc-serve-paths.XXXXXX)"
SESSION_CHOICE_FILE=""
del_tmp=""
trap 'cleanup_curl_auth; rm -f "$PATH_CHOICE_FILE" "$SESSION_CHOICE_FILE" "$del_tmp" 2>/dev/null || true' EXIT

paths_to_choices "$PATHS_RAW" > "$PATH_CHOICE_FILE"
PATH_CHOICES=()
while IFS= read -r line || [[ -n "$line" ]]; do
    PATH_CHOICES+=("$line")
done < "$PATH_CHOICE_FILE"

echo ""
gum style --foreground 39 "阶段 2/3：请选择要 attach 的项目（↑/↓ 切换，回车确认）" >&2
SELECTED_PATH="$(gum choose \
    --cursor.foreground 39 \
    --selected.foreground 76 \
    --header "📂 已知项目（来自 path-list.md）" \
    --header.foreground 99 \
    "${PATH_CHOICES[@]}")" || {
        gum style --foreground 196 "❌ 已取消选择 (Ctrl+C)" >&2
        exit 1
    }

# 阶段二.五：分发
SELECTED_TARGET=""
case "$SELECTED_PATH" in
    "➕  新建 path")
        echo ""
        gum style --foreground 252 "请输入需要 opencode 启动的目录路径"
        gum style --foreground 245 "  占位默认: ${DEFAULT_DIR}"
        gum style --foreground 245 "  仅接受本地路径，如 /abs/path 或 ./my-project 或 ~/code"
        gum style --foreground 196 "  不接受 URL（http://...）、域名、或其他非路径内容"

        echo ""
        if ! input=$(gum input \
            --placeholder "$DEFAULT_DIR" \
            --prompt "📂 新建 path: " \
            --prompt.foreground 76 \
            --width 60 \
            --value ""); then
            echo ""
            gum style --foreground 245 "(输入取消，按默认处理)"
            input=""
        fi

        trimmed="${input#"${input%%[![:space:]]*}"}"
        trimmed="${trimmed%"${trimmed##*[![:space:]]}"}"

        if [[ -z "$trimmed" ]]; then
            SELECTED_TARGET="$DEFAULT_DIR"
        else
            if ! validate_local_path "$trimmed"; then
                echo ""
                gum style --foreground 196 --bold "输入被拒绝：必须是合法本地路径，URL / 域名等不被接受。" >&2
                exit 1
            fi
            SELECTED_TARGET="$(normalize_local_path "$trimmed")"
        fi

        # 同步写回 path-list.md
        sync_path_list_for_new_path "$SELECTED_TARGET" || true
        ;;

    *)
        # SELECTED_PATH 形如 "<rel_time>  —  N sessions  —  <path>"（' — ' 分隔）。
        # 用最后一次出现的 ' — ' 做分隔，因为 path 里可能含空格。
        # 两次空格 + em-dash + 两次空格（U+2014）作唯一锚点。
        SELECTED_TARGET="$(printf '%s' "$SELECTED_PATH" | awk -F'  —  ' '{print $NF}')"

        # home 还原
        if [[ "$SELECTED_TARGET" == "~" ]]; then
            SELECTED_TARGET="${HOME:-/}"
        elif [[ "$SELECTED_TARGET" == "~"/* ]]; then
            SELECTED_TARGET="${HOME:-/}/${SELECTED_TARGET#\~/}"
        fi

        # 刷新该 path 的 lastOpenedAt，让下次菜单排序时它排到最前。
        # 失败仅 warn：会话仍可正常 attach，只是不影响下次排序。
        if ! new_content="$(path_list_touch_path "$SELECTED_TARGET" 2>/dev/null)"; then
            warn "刷新 lastOpenedAt 失败（下一次排序可能不准）"
        elif ! path_list_write "$new_content" 2>/dev/null; then
            warn "写入 path-list.md 失败（lastOpenedAt 未持久化）"
        fi
        ;;
esac

# 阶段三：拉取该 path 下的 session 列表
SESSIONS_JSON="$(fetch_sessions_for_path "$SELECTED_TARGET" 2>/dev/null || echo '[]')"

SESSION_CHOICE_FILE="$(mktemp -t oc-serve-sessions.XXXXXX)"
sessions_to_choices "$SESSIONS_JSON" > "$SESSION_CHOICE_FILE"
SESSION_CHOICES=()
while IFS= read -r line || [[ -n "$line" ]]; do
    SESSION_CHOICES+=("$line")
done < "$SESSION_CHOICE_FILE"

echo ""
gum style --foreground 39 "阶段 3/3：请选择要恢复的会话" >&2
SELECTED_SESSION="$(gum choose \
    --cursor.foreground 39 \
    --selected.foreground 76 \
    --header "📋 $SELECTED_TARGET 下的会话" \
    --header.foreground 99 \
    "${SESSION_CHOICES[@]}")" || {
        gum style --foreground 196 "❌ 已取消选择 (Ctrl+C)" >&2
        exit 1
    }

# 阶段三分发：新建会话 / attach 已有会话 / 删除项目记录
# ⚠ 用中文模式匹配而非精确匹配 — gum choose 在某些终端/locale 下会对 emoji
#   产生双重 UTF-8 编码（如 🗑 的 f09f9791 变成 c3b0c29fc297c291），导致 == 精确
#   匹配失败。中文部分编码稳定，用 *中文* 模式匹配可避免此问题。
if [[ "$SELECTED_SESSION" == *"新建会话"* ]]; then
    echo ""
    gum style --foreground 252 "🚀 正在为目标项目创建会话: $SELECTED_TARGET" >&2
    SESSION_ID="$(create_session "$SELECTED_TARGET")" || exit 1

    # 同步写回 path-list.md
    sync_path_list_for_new_session "$SELECTED_TARGET" "$SESSION_ID" || true

    echo ""
    gum style --border rounded --border-foreground 39 --padding "0 2" --width 60 --align center <<EOF
$(gum style --foreground 39 --bold "模式:") attach（项目恢复 · 新建会话）
$(gum style --foreground 39 --bold "URL:")  $ATTACH_URL
$(gum style --foreground 39 --bold "目录:") $SELECTED_TARGET
$(gum style --foreground 39 --bold "会话:") $SESSION_ID
EOF

    inject_auth
    exec opencode attach "$ATTACH_URL" --dir "$SELECTED_TARGET" --session "$SESSION_ID" \
        -u "${OPENCODE_SERVER_USERNAME:-opencode}" \
        -p "${OPENCODE_SERVER_PASSWORD:-}"
elif [[ "$SELECTED_SESSION" == *"删除此项目记录"* ]]; then
    # 删除当前选中的 path 条目（path-list.md 远端 + 本地缓存）。
    # opencode serve 内的 session 数据不在 actuator 管辖范围，不会被删除。
    echo ""
    gum style --border rounded --border-foreground 214 --padding "0 2" --width 64 --align left <<EOF
$(gum style --foreground 214 --bold "⚠️  即将删除项目记录")
$(gum style --foreground 252 "目录: $SELECTED_TARGET")
$(gum style --foreground 245 "  · path-list.md 中的 path 条目（远端 SilverBullet + 本地缓存）")
$(gum style --foreground 245 "  · opencode serve 内的 session 数据不会被删除（需另行清理）")
EOF

    if gum confirm "确认删除？" \
        --default=false \
        --affirmative="删除" \
        --negative="取消" \
        --prompt.foreground 214; then

        removed_json=""
        if ! removed_json="$(path_list_remove_path "$SELECTED_TARGET" 2>/dev/null)"; then
            gum style --foreground 196 --bold "❌ 删除失败：path_list_remove_path 出错" >&2
            exit 1
        fi

        # path_list_write = 远端 PUT + 本地缓存刷新（可能数秒）。
        del_rc=0
        del_tmp="$(mktemp -t oc-serve-del.XXXXXX)"
        ( path_list_write "$removed_json" > "$del_tmp" 2>/dev/null ) &
        del_pid=$!
        gum spin -s dot --title "  正在删除（远端同步）…" \
            -- bash -c "while kill -0 $del_pid 2>/dev/null; do sleep 0.05; done" 2>/dev/null || true
        wait $del_pid || del_rc=$?
        rm -f "$del_tmp"

        if [[ $del_rc -ne 0 ]]; then
            gum style --foreground 196 --bold "❌ 删除失败：远端写入失败（本地可能已删除但远端未同步）" >&2
            exit 1
        fi

        echo ""
        gum style --foreground 76 "✅ 项目记录已删除：$SELECTED_TARGET"
        echo ""
        gum style --foreground 245 "（脚本将退出，不会进入 attach 流程）"
        exit 0
    else
        echo ""
        gum style --foreground 245 "已取消删除，脚本退出。"
        exit 0
    fi
else
    # SELECTED_SESSION 形如 "<sid>  —  <title>"（ID 在前）。
    # sid 永远不含空格，所以用 awk '{print $1}' 安全；title 可能含 ' — '，
    # 避免被二次分隔符误切（不需要用 awk -F 拆分 title，所以这里只取 sid 即可）。
    SESSION_ID="$(printf '%s' "$SELECTED_SESSION" | awk '{print $1}')"

    echo ""
    gum style --border rounded --border-foreground 39 --padding "0 2" --width 60 --align center <<EOF
$(gum style --foreground 39 --bold "模式:") attach（项目恢复 · 已有会话）
$(gum style --foreground 39 --bold "URL:")  $ATTACH_URL
$(gum style --foreground 39 --bold "目录:") $SELECTED_TARGET
$(gum style --foreground 39 --bold "会话:") $SESSION_ID
EOF

    inject_auth
    exec opencode attach "$ATTACH_URL" --dir "$SELECTED_TARGET" --session "$SESSION_ID" \
        -u "${OPENCODE_SERVER_USERNAME:-opencode}" \
        -p "${OPENCODE_SERVER_PASSWORD:-}"
fi
