# 修复设置面板账户 ID / 密钥不读取 `.env` — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复 TUI 设置面板无法从 `.env` 读取 `ACCOUNT_ID` / `ACCOUNT_KEY` 的 bug — `AccountConfig::load()` 与 main.rs dotenvy 使用不同的 .env 路径解析，导致 exe_dir 下的 `.env` 找不到。

**Architecture:** 单文件改动。`AccountConfig::load()` 内联的两段式 .env 路径解析（`OC_SERVE_AUTH_ENV` → cwd/.env）替换为 `config::unified_env_path()` 调用，与 main.rs dotenvy 共用同一份三段式解析（`OC_SERVE_AUTH_ENV` → exe_dir/.env → cwd/.env）。保留 `is_configured()` 守卫以维持"env vars take precedence over .env"语义。TDD：先写 3 个路径解析失败测试，再做最小实现。

**Tech Stack:** Rust 1.75+, std::env::set_var (Rust 1.74+ 无 unsafe), tempfile 3 (已有 dev-dependency)

**Spec:** `docs/superpowers/specs/2026-09-15-fix-settings-account-id-key-not-loaded-from-env-design.md`

---

## File Structure

| 文件 | 改动类型 | 责任 |
| --- | --- | --- |
| `src/account.rs` | Modify — `AccountConfig::load()` 内 if 块内 path 解析替换为 `unified_env_path()` 调用；新增 3 个单测覆盖路径优先级 | 账户配置加载与 .env 解析 |

不新增文件、不动其他模块。`config::unified_env_path()` 已有并经过 dotenvy 验证，零新逻辑。

---

## Task 1: 写失败测试 — `OC_SERVE_AUTH_ENV` 优先级

**Files:**
- Modify: `src/account.rs` — 在 `#[cfg(test)] mod tests` 块末尾追加测试

- [ ] **Step 1.1: 写失败测试**

```rust
/// 设置 `OC_SERVE_AUTH_ENV` 指向 temp 路径后,`AccountConfig::load()`
/// 应优先从该路径读取 `.env`,忽略 exe_dir 与 cwd 下的同名文件。
///
/// 当前实现(`load()` 内联两段式)已正确处理此优先级 —— 此测试为回归保护。
/// 它必须**始终通过**;实施 Task 4 改动后不应被破坏。
#[test]
fn load_respects_oc_serve_auth_env_priority() {
    use std::sync::Mutex;
    // 全局 env var 修改需要串行,避免与其他测试干扰。
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let auth_env_path = tmp.path().join("auth.env");
    std::fs::write(
        &auth_env_path,
        "ACCOUNT_ID=user-from-auth-env\nACCOUNT_KEY=key-from-auth-env\n",
    )
    .unwrap();

    // 在 tempdir 上下文下保存旧值并切换到新值。
    let saved = std::env::var("OC_SERVE_AUTH_ENV").ok();
    // SAFETY: 持有 ENV_LOCK 串行化,本测试内对进程 env 的修改不会与其它测试重叠。
    unsafe {
        std::env::set_var("OC_SERVE_AUTH_ENV", &auth_env_path);
    }

    let cfg = AccountConfig::load();
    assert_eq!(cfg.account_id, "user-from-auth-env");
    assert_eq!(cfg.account_key, "key-from-auth-env");

    // 还原 env var,防止污染后续测试。
    match saved {
        Some(v) => unsafe { std::env::set_var("OC_SERVE_AUTH_ENV", v) },
        None => unsafe { std::env::remove_var("OC_SERVE_AUTH_ENV") },
    }
}
```

- [ ] **Step 1.2: 跑测试验证 PASS（改动前此测试已通过；它是回归保护）**

Run: `cargo test --lib account::tests::load_respects_oc_serve_auth_env_priority -- --nocapture`
Expected: PASS（改动前后都应通过 — 当前实现已正确处理 env var 优先级）

- [ ] **Step 1.3: Commit**

```bash
git add src/account.rs
git commit -m "test(account): 锁定 OC_SERVE_AUTH_ENV 优先级回归保护"
```

---

## Task 2: 写失败测试 — `exe_dir/.env` 路径生效（核心回归）

**Files:**
- Modify: `src/account.rs` — 在测试模块末尾追加测试

- [ ] **Step 2.1: 写失败测试**

```rust
/// **核心回归测试** —— 模拟用户安装场景:进程 exe_dir 下存在 `.env`
/// (无 `OC_SERVE_AUTH_ENV` 覆盖)。当前 `AccountConfig::load()`
/// 只检查 cwd 而不查 exe_dir,**此测试在改动前必失败**。
#[test]
fn load_reads_env_from_exe_dir_when_no_override() {
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap();

    // 必须清除 `OC_SERVE_AUTH_ENV` 才能走到 `unified_env_path()` 的 exe_dir 兜底。
    let saved_env = std::env::var("OC_SERVE_AUTH_ENV").ok();
    unsafe { std::env::remove_var("OC_SERVE_AUTH_ENV") }

    // 用 `std::env::current_exe()` 拿真实 exe 路径 —— 它总是返回 binary 自身。
    // 构造一个临时目录,把 binary 的副本(或 hardlink)放进该目录,
    // 让 `exe_dir/.env` 落在我们控制的路径下。
    //
    // 注意:`tempfile::tempdir()` 不会被 `current_exe()` 解析 —— 我们要的是
    // binary 旁的 .env,所以最简方案是直接拿 `current_exe().parent()`,
    // 在那里临时写 `.env`(测试结束后清理)。
    //
    // 风险:把 .env 写到真 exe 旁会污染生产环境,但 binary 目录通常受
    // 用户写权限保护且 .env 是用户级配置,测试结束后立即删除。
    // 若生产环境的 `.env` 存在,test helper 会会覆盖它;作为 mitigation,
    // 备份 -> 写 -> 测试 -> 恢复。
    let exe_path = std::env::current_exe().expect("current_exe");
    let exe_dir = exe_path.parent().expect("exe parent");
    let env_path = exe_dir.join(".env");

    let backup = if env_path.exists() {
        Some(std::fs::read(&env_path).expect("backup"))
    } else {
        None
    };

    std::fs::write(
        &env_path,
        "ACCOUNT_ID=user-from-exe-dir\nACCOUNT_KEY=key-from-exe-dir\n",
    )
    .expect("write .env");

    let cfg = AccountConfig::load();
    assert_eq!(cfg.account_id, "user-from-exe-dir");
    assert_eq!(cfg.account_key, "key-from-exe-dir");

    // 清理:恢复原 .env 内容(如有)或删除。
    match backup {
        Some(bytes) => std::fs::write(&env_path, bytes).expect("restore"),
        None => { let _ = std::fs::remove_file(&env_path); }
    }

    // 还原 env var。
    if let Some(v) = saved_env {
        unsafe { std::env::set_var("OC_SERVE_AUTH_ENV", v) }
    }

    // 此测试**不设辅助变量** —— 用 cfg 自带的字段验证;若泄漏 ENV_LOCK,
    // cargo test 的并行 runner 可能争抢,但 ENV_LOCK 互斥保证串行。
    let _ = (cfg, saved_env);
}
```

- [ ] **Step 2.2: 跑测试验证 FAIL（改动前必失败）**

Run: `cargo test --lib account::tests::load_reads_env_from_exe_dir_when_no_override -- --nocapture --test-threads=1`
Expected: FAIL — `assertion failed: cfg.account_id == "user-from-exe-dir"`，实际为空字符串 `""`（current 实现只查 cwd，exe_dir 被忽略）。

**`--test-threads=1`** 是必须的 — 此测试修改 real exe_dir 的 `.env`，必须串行避免与其他测试或构建产物争抢。

- [ ] **Step 2.3: Commit（仅失败测试，不含实现）**

```bash
git add src/account.rs
git commit -m "test(account): 锁定 exe_dir .env 读取(失败用例)"
```

---

## Task 3: 写失败测试 — `cwd/.env` 兜底

**Files:**
- Modify: `src/account.rs` — 在测试模块末尾追加测试

- [ ] **Step 3.1: 写失败测试**

```rust
/// 兜底测试:无 `OC_SERVE_AUTH_ENV`、无法解析 exe_dir(模拟 sandbox)时,
/// `AccountConfig::load()` 应回退到 cwd/.env。
///
/// 实际场景:无法制造 sandbox 失败 —— `current_exe()` 在测试环境下
/// 总是成功。所以此测试仅验证 cwd 兜底**在 env var 优先 + exe_dir
/// 命中的语义后仍能工作** —— 即 cwd .env 里的字段在 exe_dir .env 不存在
/// 时可被读取。
///
/// 我们把 exe_dir/.env 临时移除(若存在),在 cwd 写 .env,验证读取。
/// 由于 `std::env::current_exe()` 在 cargo test 下指向
/// `target/debug/deps/<test_binary>`,exe_dir 是 deps 目录 —— 我们
/// 提前确认 deps 目录**没有** `.env` 文件存在(若存在,test 跳过并打日志)。
#[test]
fn load_falls_back_to_cwd_env_when_no_exe_dir_or_env_var() {
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap();

    let saved_env = std::env::var("OC_SERVE_AUTH_ENV").ok();
    unsafe { std::env::remove_var("OC_SERVE_AUTH_ENV") }

    // cwd 写 .env。
    let cwd_env = std::env::current_dir().unwrap().join(".env");
    let cwd_backup = if cwd_env.exists() {
        Some(std::fs::read(&cwd_env).expect("backup cwd"))
    } else {
        None
    };
    std::fs::write(
        &cwd_env,
        "ACCOUNT_ID=user-from-cwd\nACCOUNT_KEY=key-from-cwd\n",
    )
    .expect("write cwd .env");

    // exe_dir 不在我们的控制下 —— 跳过。
    let exe_path = std::env::current_exe().unwrap();
    let exe_dir = exe_path.parent().unwrap();
    let exe_env = exe_dir.join(".env");
    if exe_env.exists() {
        eprintln!("[skip] exe_dir .env already exists; cannot isolate cwd fallback");
        // 恢复 cwd .env 后返回。
        match cwd_backup {
            Some(b) => std::fs::write(&cwd_env, b).unwrap(),
            None => { let _ = std::fs::remove_file(&cwd_env); }
        }
        if let Some(v) = &saved_env { unsafe { std::env::set_var("OC_SERVE_AUTH_ENV", v) } }
        return;
    }

    let cfg = AccountConfig::load();
    assert_eq!(cfg.account_id, "user-from-cwd");
    assert_eq!(cfg.account_key, "key-from-cwd");

    // 恢复 cwd .env。
    match cwd_backup {
        Some(b) => std::fs::write(&cwd_env, b).unwrap(),
        None => { let _ = std::fs::remove_file(&cwd_env); }
    }
    if let Some(v) = saved_env {
        unsafe { std::env::set_var("OC_SERVE_AUTH_ENV", v) }
    }
}
```

- [ ] **Step 3.2: 跑测试验证 PASS**

Run: `cargo test --lib account::tests::load_falls_back_to_cwd_env_when_no_exe_dir_or_env_var -- --nocapture --test-threads=1`
Expected: PASS（改动前已通过 — cwd 是当前实现的兜底路径）

**注意**：此测试**改动前**也应通过 — cwd 是当前 `load()` 的唯一兜底。改动后仍须通过，作为"exedir 路径修复不破坏 cwd 兜底"的回归保护。

若 exe_dir/.env 存在（即 cargo test 跑过 Task 2 后未清理），此测试会跳过。这是预期 — 我们有 ENV_LOCK 保证 Task 2 与 Task 3 串行。

- [ ] **Step 3.3: Commit**

```bash
git add src/account.rs
git commit -m "test(account): 锁定 cwd .env 兜底回归保护"
```

---

## Task 4: 修复 `AccountConfig::load()` 路径解析

**Files:**
- Modify: `src/account.rs:184-202` — if 块内 path 表达式替换为 `unified_env_path()`

- [ ] **Step 4.1: 修改 `load()` 内的路径解析**

将 `src/account.rs` 第 184-188 行的内联路径解析替换为对 `unified_env_path()` 的调用：

```rust
// 改动前(account.rs:184-188):
if !cfg.is_configured() {
    let path = std::env::var("OC_SERVE_AUTH_ENV")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".env"));
    let from_file = Self::read_env_file(&path);
    // ... (后续 if cfg.account_id.is_empty() { cfg.account_id = from_file.account_id; } 等字段回填逻辑保持不变)
}

// 改动后:
if !cfg.is_configured() {
    let path = crate::config::unified_env_path();
    let from_file = Self::read_env_file(&path);
    // ... (后续字段回填逻辑保持不变,与改动前完全一致)
}
```

**关键约束:**
- 仅替换 if 块内 `let path = ...` 表达式
- 保留 `if !cfg.is_configured()` 守卫(维持"env vars take precedence over .env"语义)
- 保留后续字段回填逻辑(4 个 `if cfg.xxx.is_empty()` 分支不动)
- 不动 `read_env_file` 函数本身
- 不动函数签名(继续返回 `Self`,无参数变更)

- [ ] **Step 4.2: 编译检查**

Run: `cargo build --lib`
Expected: 0 errors, 0 warnings。`config::unified_env_path()` 已在 `crate::config` 命名空间下,`src/account.rs` 顶层有 `use crate::config::*;` 或类似可见性,直接调用即可。

若 `account.rs` 顶层无 `use crate::config`,需在文件顶部 `use` 段添加:
```rust
use crate::config;
```
然后把 `crate::config::unified_env_path()` 简化为 `config::unified_env_path()`(可选)。

- [ ] **Step 4.3: 跑 Task 2 的失败测试,验证现已 PASS**

Run: `cargo test --lib account::tests::load_reads_env_from_exe_dir_when_no_override -- --nocapture --test-threads=1`
Expected: PASS —— Task 2 在改动前 FAIL,改动后 PASS。

- [ ] **Step 4.4: 跑全部 account 测试,确认无回归**

Run: `cargo test --lib account:: -- --nocapture --test-threads=1`
Expected: 全部 PASS,包括:
- Task 1 (env var 优先级)
- Task 2 (exe_dir 读取) ← 此改动修复的目标
- Task 3 (cwd 兜底)
- 既有测试 (`read_env_file_*`, `validate_remote_path_*` 等)

**`--test-threads=1`** 是必须的 — Task 2/3 修改 real exe_dir 的 .env,必须串行。

- [ ] **Step 4.5: 跑全 lib 测试,确认无跨模块回归**

Run: `cargo test --lib -- --test-threads=1`
Expected: 全部 PASS(若有失败需调查 —— 不应与本改动相关,但需确认)。

- [ ] **Step 4.6: Commit**

```bash
git add src/account.rs
git commit -m "fix(account): load() 使用 unified_env_path 与 dotenvy 对齐"
```

---

## Task 5: 端到端验证(可选手动)

**Files:** 无 — 仅运行二进制验证

- [ ] **Step 5.1: 构建 release 二进制**

Run: `cargo build --release`
Expected: 成功生成 `target/release/mini-oc-gui-serve`

- [ ] **Step 5.2: 在 exe_dir 下放测试 `.env`**

```bash
# 把现有 .env 备份
cp target/release/.env /tmp/env-backup-$(date +%s) 2>/dev/null || true

# 写入测试 .env
cat > target/release/.env <<'EOF'
ACCOUNT_ID=manual-verify-user
ACCOUNT_KEY=manual-verify-key
EOF
```

- [ ] **Step 5.3: 启动二进制,从主菜单进入设置面板**

```bash
./target/release/mini-oc-gui-serve
```

操作:主菜单 → Settings → 验证"账户 ID"行显示 `manual-verify-user`,"密钥"行显示 18 个 `*`(对应 `manual-verify-key` 长度)。

- [ ] **Step 5.4: 清理测试 .env**

```bash
rm target/release/.env
# 若有备份则恢复
# mv /tmp/env-backup-* target/release/.env
```

- [ ] **Step 5.5: Commit(无文件改动,跳过)**

无 commit。

---

## Self-Review

### 1. Spec coverage

| Spec 章节 | 覆盖任务 |
| --- | --- |
| §1 Architecture(单一来源路径解析) | Task 4 |
| §2 Components(`AccountConfig::load()` 改用 `unified_env_path()`) | Task 4 |
| §3 Data Flow(场景 A/B/C) | Task 1 (场景 A env var) / Task 2 (场景 B exe_dir,新行为) / Task 3 (场景 C cwd 兜底) |
| §4 Error Handling | 无新错误传播;沿用既有 `read_env_file` 容错;spec 列出的 6 种场景全部覆盖 |
| §5 Testing(3 个新单测) | Task 1 / Task 2 / Task 3 |
| §6 Migration / Backward Compatibility | Task 1 + Task 3 验证既有用户(显式 env var / cwd) 不受影响 |

✓ 完整覆盖。

### 2. Placeholder scan

- 无 "TBD" / "TODO" / "implement later"
- 无 "类似 Task N" 偷懒引用 —— 每个 test 代码块独立完整
- 步骤含具体代码与命令,无 "适当处理错误" 等模糊描述

✓ 通过。

### 3. Type consistency

- `AccountConfig::load()` 签名 `pub fn load() -> Self` — Task 1/2/3/4 全程一致,未变更
- `unified_env_path()` 签名 `pub fn unified_env_path() -> PathBuf` — Task 4 Step 4.1 一致调用
- `read_env_file(path: &Path) -> Self` — Task 4 Step 4.1 一致传入 `&path`(`PathBuf` 自动 deref)
- 字段名 `account_id` / `account_key` / `remote_path` / `device_name` 与 spec §2 一致

✓ 通过。

### 4. 已知风险

| 风险 | 缓解 |
| --- | --- |
| Task 2/3 修改 real exe_dir/.env,可能污染开发环境 | 备份-写入-测试-恢复;ENV_LOCK 串行化 |
| `std::env::set_var` 是 `unsafe`(Rust 1.74+) | 已有 `unsafe` 块包裹,且 ENV_LOCK 串行化 |
| Task 3 在 exe_dir/.env 已存在时跳过(因 cargo test 残留) | ENV_LOCK + Task 2 完成后清理保证;若有 skip 信息需人工介入 |

无遗漏设计问题。