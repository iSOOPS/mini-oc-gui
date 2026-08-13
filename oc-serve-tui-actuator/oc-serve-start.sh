#!/usr/bin/env bash
#
# oc-serve-start.sh
#
# 一体化 TUI 入口：
#   - 启动 opencode serve（可选 + rathole 内网穿透）
#   - 升级 OpenCode + oh-my-openagent（omo）
#
# 用法：
#   ./oc-serve-start.sh
#
# 菜单选项：
#   1. 🚀 启动 OC Serve（默认）
#   2. 🚀 启动 OC Serve + Rathole（全部）
#   3. ⬆️  升级 OpenCode + omo
#   4. 🚪 退出
#

set -euo pipefail

# ---- 路径解析 ---------------------------------------------------------------
SCRIPT_DIR="$(dirname "$(realpath "${BASH_SOURCE[0]}")")"
RATHOLE_DIR="$SCRIPT_DIR/../rathole"
RATHOLE_BIN="$RATHOLE_DIR/rathole/rathole"
RATHOLE_CONFIG="$RATHOLE_DIR/settings/33-9464.toml"

OC_PID=""
RATHOLE_PID=""

# ---- 鉴权密码解析 -----------------------------------------------------------
AUTH_ENV_FILE="$SCRIPT_DIR/.oc-serve-auth.env"

resolve_password() {
    if [[ -n "${OPENCODE_SERVER_PASSWORD:-}" ]]; then
        AUTH_USER="${OPENCODE_SERVER_USERNAME:-opencode}"
        AUTH_PASSWORD="$OPENCODE_SERVER_PASSWORD"
        AUTH_SOURCE="env"
        return 0
    fi

    if [[ -f "$AUTH_ENV_FILE" ]]; then
        set +u
        source "$AUTH_ENV_FILE" 2>/dev/null
        set -u
        if [[ -n "${OPENCODE_SERVER_PASSWORD:-}" ]]; then
            AUTH_USER="${OPENCODE_SERVER_USERNAME:-opencode}"
            AUTH_PASSWORD="$OPENCODE_SERVER_PASSWORD"
            AUTH_SOURCE=".oc-serve-auth.env"
            return 0
        fi
    fi

    if command -v openssl &>/dev/null; then
        AUTH_PASSWORD="$(openssl rand -base64 24 | tr -d '\n=/+' | cut -c1-20)"
    else
        AUTH_PASSWORD="$(LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c 20)"
    fi
    AUTH_USER="opencode"
    cat > "$AUTH_ENV_FILE" <<EOF
OPENCODE_SERVER_USERNAME=$AUTH_USER
OPENCODE_SERVER_PASSWORD=$AUTH_PASSWORD
EOF
    chmod 600 "$AUTH_ENV_FILE"
    AUTH_SOURCE="auto-generated"
    return 0
}

# ---- gum 依赖检查 -----------------------------------------------------------
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

check_gum

resolve_password
export OPENCODE_SERVER_USERNAME="$AUTH_USER"
export OPENCODE_SERVER_PASSWORD="$AUTH_PASSWORD"

# ---- 升级流程配置（与 upgrade.sh 保持一致）---------------------------------
OC_CONFIG_DIR="${OC_CONFIG_DIR:-$HOME/.config/opencode}"
OC_CACHE_DIR="${OC_CACHE_DIR:-$HOME/.cache/opencode}"
SKIP_VERIFY="${OC_OMO_SKIP_VERIFY:-0}"

# ---- 清理函数 ---------------------------------------------------------------
cleanup() {
    echo ""
    if command -v gum &>/dev/null; then
        gum style --foreground 208 "⏳ 正在停止服务..."
    else
        echo "正在停止服务..."
    fi
    if [[ -n "$RATHOLE_PID" ]] && kill -0 "$RATHOLE_PID" 2>/dev/null; then
        kill "$RATHOLE_PID" 2>/dev/null && echo "   rathole 已停止 (PID: $RATHOLE_PID)"
    fi
    if [[ -n "$OC_PID" ]] && kill -0 "$OC_PID" 2>/dev/null; then
        kill "$OC_PID" 2>/dev/null && echo "   opencode serve 已停止 (PID: $OC_PID)"
    fi
    if command -v gum &>/dev/null; then
        gum style --foreground 99 "✅ 所有服务已停止。"
    else
        echo "所有服务已停止。"
    fi
    exit 0
}

trap cleanup SIGINT SIGTERM

# ---- banner 辅助函数 --------------------------------------------------------
banner_start() {
    local title="$1"
    gum style \
        --border rounded \
        --border-foreground 39 \
        --padding "0 2" \
        --align center \
        --width 56 \
        --bold \
        --foreground 39 \
        "$title"
}

banner_done() {
    local title="$1"
    gum style \
        --border double \
        --border-foreground 76 \
        --padding "0 2" \
        --align center \
        --width 56 \
        --bold \
        --foreground 76 \
        "$title"
}

banner_error() {
    local msg="$1"
    gum style \
        --border rounded \
        --border-foreground 196 \
        --padding "0 2" \
        --align center \
        --width 56 \
        --bold \
        --foreground 196 \
        "$msg"
}

# ---- UI helpers (升级流程) -------------------------------------------------
ok()   { gum style --foreground 76 "[✓] $*"; }
fail() { gum style --foreground 196 "[✗] $*" >&2; }
warn() { gum style --foreground 214 "[!] $*"; }
info() { gum style --foreground 39 "[·] $*"; }

step_banner() {
    echo ""
    gum style \
        --border rounded \
        --border-foreground 39 \
        --padding "0 2" \
        --align center \
        --width 50 \
        --bold \
        --foreground 39 \
        "$1"
}

# ---- 升级流程：工具检测 ----------------------------------------------------
detect_bun() {
    if command -v bun &>/dev/null; then
        printf '%s' "$(command -v bun)"
        return 0
    fi
    for candidate in "$HOME/.bun/bin/bun" "/opt/homebrew/bin/bun" "/usr/local/bin/bun"; do
        if [[ -x "$candidate" ]]; then
            printf '%s' "$candidate"
            return 0
        fi
    done
    return 1
}

detect_npm() {
    command -v npm &>/dev/null && printf '%s' "$(command -v npm)" && return 0
    return 1
}

# ---- 升级流程：步骤 1 -------------------------------------------------------
upgrade_opencode() {
    step_banner "Step 1/3: 升级 OpenCode"

    local oc_bin
    oc_bin="$(command -v opencode 2>/dev/null || true)"

    if [[ -z "$oc_bin" ]]; then
        fail "opencode 未找到，请先安装 OpenCode"
        return 1
    fi

    local before_ver
    before_ver="$(opencode --version 2>/dev/null || echo "unknown")"
    info "当前 OpenCode 版本: ${before_ver}"

    if gum spin \
        --spinner dot \
        --spinner.foreground 39 \
        --title "正在升级 OpenCode..." \
        --title.foreground 252 \
        --show-output \
        -- opencode upgrade; then
        local after_ver
        after_ver="$(opencode --version 2>/dev/null || echo "unknown")"
        if [[ "$before_ver" != "$after_ver" ]]; then
            ok "OpenCode 已升级: ${before_ver} → ${after_ver}"
        else
            ok "OpenCode 已是最新版本: ${after_ver}"
        fi
    else
        fail "opencode upgrade 执行失败"
        return 1
    fi
}

# ---- 升级流程：步骤 2 -------------------------------------------------------
upgrade_omo() {
    step_banner "Step 2/3: 升级 oh-my-openagent"

    local bun_bin
    if bun_bin="$(detect_bun)"; then
        info "检测到 bun: ${bun_bin}"
        info "更新 ${OC_CONFIG_DIR}/node_modules/oh-my-openagent ..."

        if [[ -d "$OC_CONFIG_DIR" ]]; then
            if "$bun_bin" add --cwd "$OC_CONFIG_DIR" oh-my-openagent@latest 2>&1; then
                ok "omo 插件已通过 bun 更新"
                return 0
            else
                warn "bun add 失败，尝试备用方案..."
            fi
        fi
    fi

    local npm_bin
    if npm_bin="$(detect_npm)"; then
        info "使用 npm 拉取 oh-my-openagent@latest ..."

        if [[ -d "$OC_CACHE_DIR/packages" ]]; then
            info "清理插件缓存: ${OC_CACHE_DIR}/packages/"
            rm -rf "$OC_CACHE_DIR"/packages/oh-my-openagent* 2>/dev/null || true
            rm -rf "$OC_CACHE_DIR"/packages/oh-my-opencode* 2>/dev/null || true
        fi

        if npx --yes oh-my-openagent@latest version 2>&1; then
            ok "omo 插件已通过 npm 拉取最新版到缓存"
        else
            warn "npx 拉取 omo 时出现问题，但不影响后续使用（OpenCode 启动时会自动解析 @latest）"
        fi

        if [[ -d "$OC_CONFIG_DIR/node_modules/oh-my-openagent" ]]; then
            info "尝试更新 ${OC_CONFIG_DIR}/node_modules/oh-my-openagent ..."
            if (cd "$OC_CONFIG_DIR" && "$npm_bin" install oh-my-openagent@latest --save 2>&1); then
                ok "omo node_modules 副本已更新"
            else
                warn "npm install 更新 node_modules 失败（不影响 @latest 自动解析）"
            fi
        fi
    else
        fail "既未检测到 bun 也未检测到 npm，无法更新 omo"
        return 1
    fi
}

# ---- 升级流程：步骤 3 -------------------------------------------------------
verify_upgrade() {
    step_banner "Step 3/3: 验证升级结果"

    local all_ok=true
    local opencode_version
    local omo_version

    echo ""
    if opencode_version="$(opencode --version 2>/dev/null)"; then
        ok "OpenCode 版本: ${opencode_version}"
    else
        fail "无法获取 OpenCode 版本"
        all_ok=false
    fi

    if omo_version="$(npx --yes oh-my-openagent@latest get-local-version 2>/dev/null || npx --yes oh-my-openagent@latest version 2>/dev/null)"; then
        ok "omo 版本信息:"
        printf "   %s\n" "$omo_version"
    else
        warn "无法通过 CLI 获取 omo 版本（不影响功能——OpenCode 启动时自动解析 @latest）"
    fi

    echo ""
    if [[ -f "$OC_CONFIG_DIR/opencode.json" ]] || [[ -f "$OC_CONFIG_DIR/opencode.jsonc" ]]; then
        local config_file="$OC_CONFIG_DIR/opencode.json"
        [[ -f "$OC_CONFIG_DIR/opencode.jsonc" ]] && config_file="$OC_CONFIG_DIR/opencode.jsonc"

        if grep -q 'oh-my-openagent' "$config_file" 2>/dev/null; then
            ok "opencode.json 中 omo 插件声明存在"
        else
            warn "opencode.json 中未找到 oh-my-openagent 插件声明"
            all_ok=false
        fi
    fi

    if $all_ok; then
        echo ""
        gum style \
            --border double \
            --border-foreground 76 \
            --padding "1 3" \
            --align center \
            --width 50 \
            --bold \
            --foreground 76 \
            "✨ 升级完成！" "OpenCode 和 omo 均已更新"
    else
        echo ""
        gum style \
            --border double \
            --border-foreground 214 \
            --padding "1 3" \
            --align center \
            --width 50 \
            --bold \
            --foreground 214 \
            "⚠️  升级完成" "部分验证未通过，请检查"
    fi
}

# ---- 升级入口 ---------------------------------------------------------------
run_upgrade_flow() {
    if ! command -v opencode &>/dev/null; then
        banner_error "❌ 未找到 opencode 命令"
        return 1
    fi

    gum style \
        --border double \
        --border-foreground 39 \
        --padding "1 3" \
        --align center \
        --bold \
        --foreground 39 \
        "OpenCode + oh-my-openagent" "一 键 升 级 脚 本"

    local bun_bin
    bun_bin="$(detect_bun 2>/dev/null || true)"
    local npm_bin
    npm_bin="$(detect_npm 2>/dev/null || true)"

    echo ""
    info "OpenCode 配置目录: ${OC_CONFIG_DIR}"
    if [[ -n "$bun_bin" ]]; then
        info "bun:  ${bun_bin}"
    else
        warn "bun 未安装（将使用 npm 备用方案）"
    fi
    info "npm:  ${npm_bin:-未安装}"
    echo ""

    if ! upgrade_opencode; then
        return 1
    fi
    if ! upgrade_omo; then
        return 1
    fi

    if [[ "$SKIP_VERIFY" != "1" ]]; then
        verify_upgrade
    else
        ok "已跳过验证（OC_OMO_SKIP_VERIFY=1）"
    fi
    return 0
}

# ---- 端口输入与校验 --------------------------------------------------------
DEFAULT_PORT="${DEFAULT_PORT:-9464}"
OC_PORT=""

is_port_valid() {
    local port="$1"
    [ -n "$port" ] || return 1
    case "$port" in
        *[!0-9]*) return 1 ;;
    esac
    if [ "$port" -lt 1 ] || [ "$port" -gt 65535 ]; then
        return 1
    fi
    return 0
}

is_port_busy() {
    local port="$1"
    if command -v lsof >/dev/null 2>&1; then
        lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1 && return 0
    elif command -v nc >/dev/null 2>&1; then
        nc -z 127.0.0.1 "$port" >/dev/null 2>&1 && return 0
    fi
    return 1
}

port_owner_hint() {
    local port="$1"
    if command -v lsof >/dev/null 2>&1; then
        lsof -nP -iTCP:"$port" -sTCP:LISTEN 2>/dev/null \
            | awk 'NR>1 {print "   "$1" (PID "$2")"}' \
            | head -3
    fi
}

prompt_for_port() {
    local default_port="${DEFAULT_PORT:-9464}"
    while true; do
        local raw
        raw="$(gum input \
            --prompt "🔌 端口号 ➜ " \
            --placeholder "1-65535" \
            --header "请输入 opencode serve 端口（默认 ${default_port}）：" \
            --header.foreground 99 \
            --value "${default_port}")" || {
                banner_error "❌ 已取消端口输入"
                return 1
            }

        raw="${raw// /}"
        [ -z "$raw" ] && raw="$default_port"

        if ! is_port_valid "$raw"; then
            gum style --foreground 196 "❌ 无效端口: '$raw'（必须是 1–65535 之间的数字）"
            echo ""
            continue
        fi

        if is_port_busy "$raw"; then
            gum style --foreground 196 "❌ 端口 $raw 已被占用："
            port_owner_hint "$raw"
            echo ""
            continue
        fi

        OC_PORT="$raw"
        return 0
    done
}

# ---- 启动流程 ---------------------------------------------------------------
launch_oc_serve() {
    local port="$1"

    if ! command -v opencode &>/dev/null; then
        banner_error "❌ 未找到 opencode 命令"
        return 1
    fi

    banner_start "🚀 启动 opencode serve 服务（端口 ${port}）"

    opencode serve --port "$port" &
    OC_PID=$!

    if gum spin \
        --spinner dot \
        --spinner.foreground 39 \
        --title "等待 opencode serve 就绪 (PID: $OC_PID)..." \
        --title.foreground 252 \
        -- bash -c '
            for i in $(seq 1 10); do
                kill -0 '"$OC_PID"' 2>/dev/null && exit 0
                sleep 1
            done
            exit 1
        '; then
        gum style --foreground 76 "   ✓ opencode serve 已启动 (PID: $OC_PID, 端口: $port)"
    else
        banner_error "❌ opencode serve 未能成功启动"
        OC_PID=""
        return 1
    fi
    return 0
}

launch_rathole() {
    if [[ ! -x "$RATHOLE_BIN" ]]; then
        banner_error "❌ 未找到 rathole: $RATHOLE_BIN"
        return 1
    fi
    if [[ ! -f "$RATHOLE_CONFIG" ]]; then
        banner_error "❌ 未找到配置文件: $RATHOLE_CONFIG"
        return 1
    fi

    echo ""
    banner_start "🔗 启动 rathole 内网穿透"

    "$RATHOLE_BIN" "$RATHOLE_CONFIG" &
    RATHOLE_PID=$!

    if gum spin \
        --spinner dot \
        --spinner.foreground 39 \
        --title "启动 rathole 隧道 (PID: $RATHOLE_PID)..." \
        --title.foreground 252 \
        -- sleep 1.5; then
        if kill -0 "$RATHOLE_PID" 2>/dev/null; then
            gum style --foreground 76 "   ✓ rathole 已启动 (PID: $RATHOLE_PID)"
            return 0
        fi
        banner_error "❌ rathole 进程异常退出"
        RATHOLE_PID=""
        return 1
    fi
    banner_error "❌ rathole 启动超时"
    RATHOLE_PID=""
    return 1
}

run_launch_flow() {
    local launch_rathole="$1"

    if ! prompt_for_port; then
        return 1
    fi
    local port="$OC_PORT"

    if ! launch_oc_serve "$port"; then
        return 1
    fi
    if [[ "$launch_rathole" -eq 1 ]]; then
        if ! launch_rathole; then
            return 1
        fi
    fi

    echo ""
    banner_done "✨ 服务启动完成！"

    if [[ "$launch_rathole" -eq 1 ]]; then
        RATHOLE_LINE="{{ Color \"99\" \"│\" }}     rathole          {{ Faint \"PID:\" }} {{ Color \"39\" \"$RATHOLE_PID\" }}"
    else
        RATHOLE_LINE=""
    fi

    gum format --type template <<EOF
{{ Color "99" "┌──────────────────────────────────────────┐" }}
{{ Color "99" "│" }}  {{ Color "76" "🌐" }} {{ Bold "Web 端访问地址:" }}
{{ Color "99" "│" }}     {{ Color "39" "http://localhost:${port}" }}
{{ Color "99" "│" }}
{{ Color "99" "│" }}  {{ Color "76" "🖥️" }} {{ Bold "启动本地 TUI:" }}
{{ Color "99" "│" }}     {{ Faint "./oc-serve-tui-actuator/oc-serve-tui-actuator.sh" }}
{{ Color "99" "│" }}
{{ Color "99" "│" }}  {{ Color "76" "📋" }} {{ Bold "后台进程:" }}
{{ Color "99" "│" }}     opencode serve  {{ Faint "PID:" }} {{ Color "39" "$OC_PID" }}  {{ Faint "(端口 $port)" }}
${RATHOLE_LINE}
{{ Color "99" "│" }}
{{ Color "99" "│" }}  {{ Color "76" "🔐" }} {{ Bold "Web 鉴权 (HTTP Basic):" }}
{{ Color "99" "│" }}     {{ Faint "用户名:" }} {{ Color "39" "$AUTH_USER" }}
{{ Color "99" "│" }}     {{ Faint "密码  :" }} {{ Color "39" "$AUTH_PASSWORD" }}  {{ Faint "(来源: $AUTH_SOURCE)" }}
{{ Color "99" "│" }}
{{ Color "99" "│" }}  {{ Faint "从菜单选择「退出」会终止所有服务" }}
{{ Color "99" "└──────────────────────────────────────────┘" }}
EOF
    return 0
}

# ---- 主菜单循环 -------------------------------------------------------------
main_menu() {
    while true; do
        local SELECTED
        SELECTED="$(gum choose \
            --cursor.foreground 39 \
            --selected.foreground 76 \
            --header "OC Serve 主菜单（↓/↑/Tab 切换，回车确认）" \
            --header.foreground 99 \
            "🚀 启动 OC Serve（默认）" \
            "🚀 启动 OC Serve + Rathole（全部）" \
            "⬆️  升级 OpenCode + omo" \
            "🚪 退出")" || {
                gum style --foreground 99 "👋 已退出菜单"
                cleanup
                return 0
            }

        case "$SELECTED" in
            *"OC Serve + Rathole"*)
                run_launch_flow 1 || true
                ;;
            *"升级"*)
                if run_upgrade_flow; then
                    echo ""
                fi
                ;;
            *"退出")
                gum style --foreground 99 "👋 正在退出..."
                cleanup
                return 0
                ;;
            *)
                run_launch_flow 0 || true
                ;;
        esac

        echo ""
        gum style --foreground 240 "↩  按回车键返回主菜单..."
        read -r _ || true
    done
}

main_menu
