# lib-path-list.sh
# Shared Bash helpers for the path-list.md index.
#
# 存储后端：SilverBullet HTTP API（PUT/GET /.fs/serv/opencode/path-list.md）。
#   - 双端合并策略：path 是主键，sections 是去重无序集合。
#     · 远端空 + 本地非空 → 用本地 seed 远端（首次部署场景）
#     · 远端非空 + 本地空 → 用远端填充本地（首次接入已有共享空间）
#     · 远端非空 + 本地非空 → 按 path 合并 sections（去重并集）→ 写本地 → 同步远端
#     · 远端不可达 → 用本地缓存（warn）
#
# 鉴权：Cookie Session 模式（SilverBullet 设计）。
#   - /.fs/* 只接受 cookie（`auth_<host>=<JWT>`），不接受 Basic Auth 直连
#   - lib 自动 POST /.auth 登录 → 拿 Set-Cookie → 存到临时文件（chmod 600）
#   - 后续请求带 cookie；解析 JWT exp 字段判断过期，过期则自动重新登录
#   - 复用本仓库的 .oc-serve-auth.env。SB 相关键：
#       SB_URL        SilverBullet base URL（默认 https://md.isoops.com）
#       SB_USER       SilverBullet username（POST /.auth 用）
#       SB_PASSWORD   SilverBullet password
#     任一组合存在即可工作，缺失则按 warn 处理并允许走本地 fallback。
#
# Requires: jq, bash 4+, curl. PATH_LIST_FILE 是本地缓存路径（可被外部覆盖）。

set -euo pipefail

# ---- 配置 -------------------------------------------------------------------

: "${PATH_LIST_FILE:="$SCRIPT_DIR/path-list.md"}"
: "${SB_URL:=https://md.isoops.com}"
# 远端路径固定 — 由任务约定。尾随 / 规范化。
SB_REMOTE_PATH="/serv/opencode/path-list.md"
SB_URL="${SB_URL%/}"
# 从 SB_URL 推导出 SB cookie 名（SilverBullet 的 Set-Cookie 形如 auth_<host>=<jwt>）。
# 把 host 中的 '.' 替换为 '_'。例：md.isoops.com → md_isoops_com, 127.0.0.1 → 127_0_0_1。
# 用 sed 而非 tr：BSD tr 不支持 [:alnum:] 字符类，且 -c 行为和 GNU 不一致。
SB_COOKIE_HOST="$(printf '%s' "${SB_URL#*://}" | cut -d/ -f1 | cut -d: -f1 | tr '.' '_')"
SB_COOKIE_NAME="auth_${SB_COOKIE_HOST}"

# 临时文件：curl --config（保留兼容性）+ cookie 缓存（chmod 600）
SB_CURL_CFG="$(mktemp -t oc-sb-curl.XXXXXX)"
SB_COOKIE_FILE="$(mktemp -t oc-sb-cookie.XXXXXX)"
chmod 600 "$SB_COOKIE_FILE" 2>/dev/null || true
sb_cleanup() {
    rm -f "$SB_CURL_CFG" "$SB_COOKIE_FILE" 2>/dev/null || true
}
trap sb_cleanup EXIT

# ---- 鉴权加载 ---------------------------------------------------------------
# 与 oc-serve-tui-actuator.sh 复用同一份 .oc-serve-auth.env — inject_auth
# 会 source 该 env，并 export 凭据。lib-path-list.sh 被 source 时假定
# OPENCODE_SERVER_* 和 SB_* 已在当前 shell 中（或调用方负责注入）。
inject_sb_auth() {
    if [[ -n "${SB_USER:-}${SB_PASSWORD:-}" ]]; then
        return 0
    fi

    local auth_env="${OC_SERVE_AUTH_ENV:-$SCRIPT_DIR/.oc-serve-auth.env}"
    if [[ -f "$auth_env" ]]; then
        set +u
        if ! grep -qE '^SB_(USER|PASSWORD|URL)=' "$auth_env" 2>/dev/null; then
            return 0
        fi
        # shellcheck disable=SC1090
        source "$auth_env" 2>/dev/null || true
        set -u
    fi
}

# ---- Cookie Session 登录 ----------------------------------------------------
# 从 JWT payload 的 'exp' 字段判断过期（Unix 秒）。当前时间 < exp 则未过期。
# 返回 0 未过期；1 已过期或无法解析；2 cookie 文件缺失。
sb_cookie_valid() {
    [[ -s "$SB_COOKIE_FILE" ]] || return 2
    local cookie
    cookie="$(cat "$SB_COOKIE_FILE" 2>/dev/null || true)"
    [[ -n "$cookie" ]] || return 2

    # 从 "<name>=<jwt>" 提取 jwt
    local jwt="${cookie#*=}"
    [[ "$jwt" != "$cookie" ]] || return 2

    # JWT 三段：header.payload.signature
    local payload="${jwt#*.}"
    payload="${payload%.*}"
    # base64url 解码（padding 补齐）
    local p="$payload"
    while (( ${#p} % 4 != 0 )); do p="${p}="; done
    local exp
    exp="$(printf '%s' "$p" | tr '_-' '/+' | base64 -d 2>/dev/null | jq -r '.exp // empty' 2>/dev/null)"
    [[ -n "$exp" ]] || return 1

    local now
    now="$(date +%s)"
    (( now < exp )) || return 1
    return 0
}

# POST $SB_URL/.auth 用 form 登录 → 提取 Set-Cookie → 写 SB_COOKIE_FILE。
# 返回 0 成功；1 失败。失败原因写到 stderr。
sb_login() {
    [[ -n "${SB_USER:-}" && -n "${SB_PASSWORD:-}" ]] || {
        printf 'sb_login: SB_USER/SB_PASSWORD 未配置\n' >&2
        return 1
    }

    local login_url="${SB_URL}/.auth"
    local resp_headers
    resp_headers="$(curl -s -i \
        --max-time 10 \
        -X POST "$login_url" \
        -H 'Content-Type: application/x-www-form-urlencoded' \
        --data-urlencode "username=${SB_USER}" \
        --data-urlencode "password=${SB_PASSWORD}" 2>/dev/null)" || {
        printf 'sb_login: 网络错误，登录请求失败\n' >&2
        return 1
    }

    # 提取 Set-Cookie 头里的 auth_<host>=<jwt>;...
    local cookie
    cookie="$(printf '%s' "$resp_headers" | grep -i "^set-cookie:[[:space:]]*${SB_COOKIE_NAME}=" | head -1 \
        | sed -E 's/^[Ss]et-[Cc]ookie:[[:space:]]*//' \
        | cut -d';' -f1)"
    if [[ -z "$cookie" ]]; then
        printf 'sb_login: 登录响应中没有 %s cookie\n' "$SB_COOKIE_NAME" >&2
        return 1
    fi

    # 写入 chmod 600 文件（密码安全）
    printf '%s' "$cookie" > "$SB_COOKIE_FILE"
    chmod 600 "$SB_COOKIE_FILE" 2>/dev/null || true
    return 0
}

# 确保 cookie 有效（必要时登录）。返回 0 ok，1 失败。
sb_ensure_cookie() {
    if sb_cookie_valid; then
        return 0
    fi
    sb_login
}

# ---- 通用 curl --------------------------------------------------------------
# 用法: sb_curl METHOD REMOTE_PATH [BODY]
# 鉴权：自动 cookie session（sb_ensure_cookie）。
# 输出契约（避免与 stderr 混合，且与 set -e 安全）：
#   - stdout: 响应体（任何情况下都安全 capture，不会触发 set -e 中断）
#   - SB_HTTP_STATUS  全局变量：HTTP 状态码（数字；0 表示网络/超时未拿到响应）
#   - SB_HTTP_BODY    全局变量：响应体（始终有效）
#   - 函数返回码 **永远为 0**（避免 set -e 在 command substitution subshell 里中断调用方）。
#     调用方按 SB_HTTP_STATUS 判断：
#       0   = 网络/超时/curl 失败
#       2xx = 成功
#       4xx = 客户端错误（401/403/404）
#       5xx = 服务端错误
#   - stderr 只用于人类可读的失败摘要。
#   - 当 cookie 失效（401）时，本函数自动重新登录一次并重试；仍失败则保留 401。
SB_HTTP_STATUS=0
SB_HTTP_BODY=""

sb_curl() {
    local method="$1"
    local rel_path="$2"
    local body="${3:-}"

    SB_HTTP_STATUS=0
    SB_HTTP_BODY=""

    local rel="${rel_path#/}"
    local url="${SB_URL%/}/.fs/${rel}"
    # 折叠 rel 段的多余 /（保护 SB_URL 的 http:// 双斜杠不被破坏）。
    local prefix="${url%%/.fs/*}"
    local suffix="${url#"$prefix"}"
    suffix="$(printf '%s' "$suffix" | tr -s '/')"
    url="${prefix}${suffix}"

    # 自动登录拿 cookie（缺凭据则视为网络错 0 — 让 path_list_read fallback）
    if ! sb_ensure_cookie; then
        printf 'sb_curl: 无法获取 SB cookie session（请检查 SB_USER/SB_PASSWORD）\n' >&2
        SB_HTTP_STATUS=0
        return 0
    fi

    local args=(
        --silent --show-error
        -X "$method"
        --max-time 10
        -H 'Accept: */*'
        -H "Cookie: $(cat "$SB_COOKIE_FILE" 2>/dev/null || true)"
    )
    case "$method" in
        PUT)
            args+=(-H 'Content-Type: text/markdown; charset=utf-8')
            if [[ -n "$body" ]]; then
                args+=(--data-binary "$body")
            fi
            ;;
    esac

    local tmp_body tmp_code
    tmp_body="$(mktemp -t oc-sb-body.XXXXXX)"
    tmp_code="$(mktemp -t oc-sb-code.XXXXXX)"
    local rc=0

    curl "${args[@]}" "$url" -o "$tmp_body" -w '%{http_code}' > "$tmp_code" 2>/dev/null || rc=$?

    local code
    code="$(cat "$tmp_code" 2>/dev/null || echo 000)"
    rm -f "$tmp_code"

    SB_HTTP_BODY="$(cat "$tmp_body" 2>/dev/null || true)"
    rm -f "$tmp_body"

    if [[ $rc -ne 0 || "$code" =~ ^0+$ ]]; then
        printf 'network error (curl rc=%s, http=%s)' "$rc" "$code" >&2
        SB_HTTP_STATUS=0
        return 0
    fi

    # 401 → cookie 失效（虽然 sb_ensure_cookie 已检查 exp，但服务端可能强制过期）。
    # 自动重登一次并重试；避免无限循环（最多一次）。
    if [[ "$code" == "401" ]]; then
        if sb_login; then
            # 用新 cookie 重试
            local tmp_body2 tmp_code2
            tmp_body2="$(mktemp -t oc-sb-body.XXXXXX)"
            tmp_code2="$(mktemp -t oc-sb-code.XXXXXX)"
            local args2=("${args[@]}")
            # 替换 cookie header
            local i
            for ((i=0; i<${#args2[@]}; i++)); do
                if [[ "${args2[$i]}" == "-H" && "${args2[$((i+1))]}" == "Cookie: "* ]]; then
                    args2[$((i+1))]="Cookie: $(cat "$SB_COOKIE_FILE" 2>/dev/null || true)"
                    break
                fi
            done
            curl "${args2[@]}" "$url" -o "$tmp_body2" -w '%{http_code}' > "$tmp_code2" 2>/dev/null
            local code2
            code2="$(cat "$tmp_code2" 2>/dev/null || echo 000)"
            rm -f "$tmp_code2"
            SB_HTTP_BODY="$(cat "$tmp_body2" 2>/dev/null || true)"
            rm -f "$tmp_body2"
            if [[ "$code2" =~ ^[1-5] ]] && [[ "$code2" != "000" ]]; then
                SB_HTTP_STATUS="$code2"
            else
                # 重试后仍网络错，保持原 401 状态
                SB_HTTP_STATUS="401"
            fi
        else
            SB_HTTP_STATUS="401"
        fi
        if [[ "$SB_HTTP_STATUS" =~ ^2 ]]; then
            printf '%s' "$SB_HTTP_BODY"
        else
            printf 'http %s (after relogin): %s' "$SB_HTTP_STATUS" "${SB_HTTP_BODY:0:200}" >&2
        fi
        return 0
    fi

    SB_HTTP_STATUS="$code"

    if [[ "$code" =~ ^2 ]]; then
        printf '%s' "$SB_HTTP_BODY"
        return 0
    fi

    printf 'http %s: %s' "$code" "${SB_HTTP_BODY:0:200}" >&2
    return 0
}

# ---- 远端读写 ---------------------------------------------------------------

# GET https://$SB_URL/.fs/serv/opencode/path-list.md。
# 通过 stdout 输出 body；副作用：设置 SB_HTTP_STATUS / SB_HTTP_BODY（在当前 shell）。
# **不**用 $(sb_curl …) command substitution，因为 subshell 会隔离全局变量 — 调用方
# 在当前 shell 直接调用此函数即可。
# 永远返回 0。调用方按 SB_HTTP_STATUS 判断：
#   2xx → body 合法 JSON 数组，正常返回
#   404 → 资源不存在 = 空数组
#   0   → 网络/超时，body 无效
#   4xx/5xx → 配置/服务端错误，body 无效
sb_read_remote() {
    SB_HTTP_BODY=""
    # 直接调用 sb_curl（不进 subshell），让全局变量 SB_HTTP_STATUS 在当前 shell 生效。
    # sb_curl 在 status=2xx 时已经 printf body 到 stdout — 这里直接透传，避免重复输出。
    sb_curl GET "$SB_REMOTE_PATH"

    local status="${SB_HTTP_STATUS:-0}"
    local body="${SB_HTTP_BODY:-}"

    case "$status" in
        2*)
            # 校验 body 是 JSON 数组；否则修正为空数组。stdout 上不再二次 printf
            # body — sb_curl 已经写过了。
            if [[ -z "$body" ]] || ! printf '%s' "$body" | jq -e 'type == "array"' >/dev/null 2>&1; then
                printf '[]'
            fi
            return 0
            ;;
        404)
            printf '[]'
            return 0
            ;;
        *)
            return 0
            ;;
    esac
}

# PUT 写入远端。$1 = JSON 内容。返回值: 0 ok, 1 失败
sb_write_remote() {
    local content="$1"
    sb_curl PUT "$SB_REMOTE_PATH" "$content" >/dev/null
}

# 后台重试：把 JSON 内容推上远端。失败最多重试 3 次，间隔 1s。
# 用于 path_list_read 触发 seed/merge 时的非阻塞同步。
sb_push_async() {
    local content="$1"
    local attempt=0
    while (( attempt < 3 )); do
        ((attempt++))
        if sb_write_remote "$content"; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# 合并远端 + 本地两份 JSON 数组，按 path 主键 union，sections 取去重并集。
# 时间字段策略：
#   - createdAt    取最早值（双端第一次出现的不可变小时间）
#   - lastOpenedAt 取最近值（谁更新谁说了算）
# 输出顺序：按 lastOpenedAt 倒序（新 → 旧），让 actuator 脚本直接拿到倒序数组。
sb_merge_path_lists() {
    local remote_json="$1"
    local local_json="$2"
    jq -n --argjson r "${remote_json:-[]}" --argjson l "${local_json:-[]}" '
        ($r + $l)
        | group_by(.path)
        | map({
            path: .[0].path,
            sections: (map(.sections // []) | add | unique),
            createdAt:    ((map(.createdAt // "") | map(select(. != ""))) | min // ""),
            lastOpenedAt: ((map(.lastOpenedAt // "") | map(select(. != ""))) | max // "")
          })
        | sort_by(.lastOpenedAt) | reverse
    '
}

# ---- 本地缓存更新 ----------------------------------------------------------
# 把远端结果写到本地 cache。失败仅 warn，不中断主流程。
sb_cache_write() {
    local content="$1"
    if [[ -z "$content" ]]; then
        return 0
    fi
    if ! printf '%s' "$content" | jq -e 'type == "array"' >/dev/null 2>&1; then
        return 0
    fi

    # 沿用 path_list_write 的 .bak + .tmp + atomic rename 模式（保持兼容）。
    if [[ -f "$PATH_LIST_FILE" ]]; then
        cp -f "$PATH_LIST_FILE" "$PATH_LIST_FILE.bak" 2>/dev/null || true
    fi
    local tmp="$PATH_LIST_FILE.tmp.$$"
    if printf '%s\n' "$content" > "$tmp" 2>/dev/null; then
        if mv -f "$tmp" "$PATH_LIST_FILE" 2>/dev/null; then
            rm -f "$PATH_LIST_FILE.bak" 2>/dev/null || true
        fi
    else
        rm -f "$tmp" 2>/dev/null || true
    fi
}

# ---- 公开 API（保持向后兼容）--------------------------------------------------

# Validate that the cached content is a JSON array. Tries .bak restore on parse
# failure. Returns 0 if usable (possibly empty), 1 if unrecoverable.
path_list_validate() {
    if [[ ! -f "$PATH_LIST_FILE" ]]; then
        return 0
    fi
    if jq -e 'type == "array"' "$PATH_LIST_FILE" >/dev/null 2>&1; then
        return 0
    fi
    if [[ -f "$PATH_LIST_FILE.bak" ]] && jq -e 'type == "array"' "$PATH_LIST_FILE.bak" >/dev/null 2>&1; then
        gum style --foreground 214 "[!] path-list.md 损坏，已从 .bak 还原" >&2
        mv -f "$PATH_LIST_FILE.bak" "$PATH_LIST_FILE"
        return 0
    fi
    gum style --foreground 196 --bold "❌ path-list.md 和 .bak 都损坏，请手动修复" >&2
    return 1
}

# Read the JSON array. 分支基于 SB_HTTP_STATUS：
#   SB_HTTP_STATUS = 0     → 网络/超时/凭据未配 (fallback 到本地缓存，warn)
#   SB_HTTP_STATUS = 2xx   → 远端可达；按 (远端空/非空 × 本地空/非空) 4 种分支处理
#   SB_HTTP_STATUS = 401/403/5xx → 凭据/服务端错误 (硬错误，return 1 不 fallback)
#   SB_HTTP_STATUS = 404   → 远端空 (与 2xx + 远端空 走同一路径)
# 返回值: 0 成功；1 远端配置/凭据错误（让上层 exit）；2 网络/超时（fallback 但明确标记）
# Read the JSON array. 简化策略：任何"连不上"（网络/超时/4xx/5xx/凭据未配）都
# fallback 到本地缓存 + warn，不阻塞主流程。仅 2xx 走 merge/seed 路径。
# 返回值: 0 总是（不阻塞调用方），body 经 stdout 输出。
path_list_read() {
    inject_sb_auth

    local remote=""
    # sb_curl/sb_read_remote 通过 stdout 输出 body + 副作用设置 SB_HTTP_STATUS。
    # 必须先让 sb_curl 在当前 shell 执行副作用，再用临时文件 capture body
    # （直接 $() 进 subshell 会隔离全局变量）。
    local _sb_body_tmp
    _sb_body_tmp="$(mktemp -t oc-sb-rd-body.XXXXXX)"
    sb_read_remote > "$_sb_body_tmp" 2>/dev/null
    remote="$(cat "$_sb_body_tmp" 2>/dev/null || true)"
    rm -f "$_sb_body_tmp"
    local status="${SB_HTTP_STATUS:-0}"

    local local_json=""
    if [[ -f "$PATH_LIST_FILE" ]] && jq -e 'type == "array"' "$PATH_LIST_FILE" >/dev/null 2>&1; then
        local_json="$(cat "$PATH_LIST_FILE")"
    else
        local_json="[]"
    fi

    # ---- "连不上"分支：status == 0（网络/超时/凭据未配） 或 status 在 [45]xx 且不是 2xx/404 ----
    # 401/403/5xx 都视为"远端不可达"，fallback 到本地（用户要求"连不上就用本地"）。
    # 在 warn 中区分原因，便于用户诊断。
    local not_ok=0
    if [[ "${status:-0}" == "0" ]]; then
        not_ok=1
        gum style --foreground 214 "[!] SilverBullet 连不上（网络/超时/凭据未配），使用本地缓存 path-list.md" >&2
    elif [[ "${status:-}" =~ ^[45] ]] && [[ "${status:-}" != "404" ]]; then
        not_ok=1
        # 401/403 → 服务端认证问题；5xx → 服务端故障。两者对用户都表现为"连不上"
        gum style --foreground 214 "[!] SilverBullet 连不上（HTTP ${status}，可能是认证/服务端问题），使用本地缓存 path-list.md" >&2
    fi

    if [[ "$not_ok" -eq 1 ]]; then
        if [[ "$local_json" != "[]" ]]; then
            printf '%s' "$local_json"
            return 0
        fi
        printf '[]'
        return 0
    fi

    # status 是 2xx 或 404。remote 已被 sb_read_remote 校验/规范化为 JSON 数组。
    # 但 sb_read_remote 在 2xx 但 body 非 JSON 时仍会 printf '[]' — 兜底再校验一次。
    if ! printf '%s' "$remote" | jq -e 'type == "array"' >/dev/null 2>&1; then
        remote="[]"
    fi
    local remote_count local_count
    remote_count="$(printf '%s' "$remote" | jq 'length')"
    local_count="$(printf '%s' "$local_json" | jq 'length')"

    # ---- A. 远端非空 + 本地空 ----
    if [[ "$remote_count" -gt 0 && "$local_count" -eq 0 ]]; then
        # 远端可能未按时间排序（不同设备写入顺序不一），按 lastOpenedAt 重排后再落本地。
        local sorted_remote
        sorted_remote="$(printf '%s' "$remote" | jq 'sort_by(.lastOpenedAt) | reverse')"
        sb_cache_write "$sorted_remote"
        printf '%s' "$sorted_remote"
        return 0
    fi

    # ---- C. 远端空 + 本地非空 → seed 远端 ----
    if [[ "$remote_count" -eq 0 && "$local_count" -gt 0 ]]; then
        # 本地 JSON 已经是 path_list_read 之前的形状（可能未排序），排序后再 seed。
        local sorted_local
        sorted_local="$(printf '%s' "$local_json" | jq 'sort_by(.lastOpenedAt) | reverse')"
        gum style --foreground 214 "[!] SilverBullet 远端为空，正在把本地 $local_count 条记录推上远端…" >&2
        sb_cache_write "$sorted_local"
        printf '%s' "$sorted_local"
        ( sb_push_async "$sorted_local" >/dev/null 2>&1 || true ) &
        return 0
    fi

    # ---- 远端 + 本地都空 ----
    if [[ "$remote_count" -eq 0 && "$local_count" -eq 0 ]]; then
        printf '[]'
        return 0
    fi

    # ---- B. 双端都非空 → 合并 → 写本地 → 同步远端 ----
    local merged
    merged="$(sb_merge_path_lists "$remote" "$local_json")"
    local added_remote added_local
    added_remote="$(jq -n --argjson a "$remote" --argjson b "$merged" '($b | length) - ($a | length)')"
    added_local="$(jq -n --argjson a "$local_json" --argjson b "$merged" '($b | length) - ($a | length)')"
    if [[ "$added_remote" -gt 0 || "$added_local" -gt 0 ]]; then
        gum style --foreground 214 "[!] SilverBullet 双端合并：远端新增/更新 $added_remote 条，本地新增/更新 $added_local 条" >&2
    fi
    sb_cache_write "$merged"
    printf '%s' "$merged"
    ( sb_push_async "$merged" >/dev/null 2>&1 || true ) &
    return 0
}

# Atomic write: PUT to remote first, then refresh local cache.
# Args: $1 = JSON content to write
path_list_write() {
    local content="$1"

    if ! printf '%s' "$content" | jq -e 'type == "array"' >/dev/null 2>&1; then
        echo "[!] path_list_write: refusing to write non-array JSON" >&2
        return 1
    fi

    inject_sb_auth

    if ! sb_write_remote "$content"; then
        echo "[!] path_list_write: 远端 PUT 失败，拒绝写入以保证一致性" >&2
        return 1
    fi

    sb_cache_write "$content"
}

# Mutator: insert a new path if absent.
# 新条目会写入 createdAt + lastOpenedAt 两个 ISO 8601 时间戳（createdAt 不可变，
# lastOpenedAt 与 createdAt 同值起步；后续 path_list_touch_path 会刷新 lastOpenedAt）。
# Args: $1 = absolute path string
# Emits the new full JSON array on stdout.
path_list_upsert_path() {
    local target="$1"
    local current
    current="$(path_list_read)"

    if [[ -z "$current" ]]; then
        current="[]"
    fi

    local now
    now="$(date +%Y-%m-%dT%H:%M:%S%z)"

    printf '%s' "$current" | jq --arg p "$target" --arg now "$now" '
        if any(.[]; .path == $p) then
            .
        else
            . + [{"path": $p, "sections": [], "createdAt": $now, "lastOpenedAt": $now}]
        end
    '
}

# Mutator: refresh lastOpenedAt on an existing path (and seed createdAt if missing
# for legacy entries that predate the timestamp fields).
# Args: $1 = absolute path string
# Emits the new full JSON array on stdout.
path_list_touch_path() {
    local target="$1"
    local current
    current="$(path_list_read)"

    if [[ -z "$current" ]]; then
        echo "[!] path_list_touch_path: no path-list.md yet — call path_list_upsert_path first" >&2
        return 1
    fi

    local now
    now="$(date +%Y-%m-%dT%H:%M:%S%z)"

    printf '%s' "$current" | jq --arg p "$target" --arg now "$now" '
        map(
            if .path == $p then
                .lastOpenedAt = $now
                | if (.createdAt // "") == "" then .createdAt = $now else . end
            else
                .
            end
        )
    '
}

# Mutator: append a session id to a path's sections, deduplicated.
# Args: $1 = absolute path, $2 = session id
# Emits the new full JSON array on stdout.
path_list_append_session() {
    local target="$1"
    local sid="$2"
    local current
    current="$(path_list_read)"

    if [[ -z "$current" ]]; then
        echo "[!] path_list_append_session: no path-list.md yet — call path_list_upsert_path first" >&2
        return 1
    fi

    printf '%s' "$current" | jq --arg p "$target" --arg s "$sid" '
        map(if .path == $p then .sections += [$s] | .sections |= unique else . end)
    '
}

# Mutator: remove a path entry.
# Args: $1 = absolute path
# Emits the new full JSON array on stdout.
path_list_remove_path() {
    local target="$1"
    local current
    current="$(path_list_read)"

    if [[ -z "$current" ]]; then
        echo "[]"
        return 0
    fi

    printf '%s' "$current" | jq --arg p "$target" '
        map(select(.path != $p))
    '
}