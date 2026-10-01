#!/usr/bin/env python3
"""pi_rs ↔ upstream 方法级 1:1 对比扫描。

对每个目标包：
  1. 文件级：TS 文件是否在 Rust 侧有对应文件（配对规则见 pair_rust_file）
  2. 符号级：TS 顶层导出（class/function/const/interface/type/enum）与 class 方法，
     是否在 Rust 侧存在同名（camelCase→snake_case）符号
       - local : 同名符号出现在「配对到的那个 Rust 文件」里  → 视为 OK
       - global: 出现在同 crate 的其他文件里                → 需人工确认（拆分/移位）
       - missing: 整个 crate 都没有                          → 真实候选缺口

输出：UPSTREAM-PARITY.md
"""
import os
import re
import sys
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
UPSTREAM = os.path.join(ROOT, "upstream/packages")
OUT = os.path.join(ROOT, "UPSTREAM-PARITY.md")

TARGETS = [
    ("agent", "crates/pi-agent/src"),
    ("ai", "crates/pi-ai/src"),
    ("telemetry", "crates/pi-telemetry/src"),
]

EXCLUDE_TS_SUFFIX = ("models.generated.ts", "image-models.generated.ts")
EXCLUDE_PATH_PARTS = ("/pico3/",)

RESERVED_TS = {
    "if", "for", "while", "switch", "return", "catch", "constructor", "else",
    "do", "try", "finally", "new", "typeof", "await", "yield", "super", "this",
    "function", "class", "const", "let", "var", "import", "export", "default",
}

REVIEW_SECTION = """

---

# 本轮修复记录（2026-10-01，全量逻辑对齐）

以下 8 项已逐个对照上游源码核实并修复，`cargo check/clippy/test/fmt` 全绿：

1. **`AgentToolResult.isError` 传递丢失（逻辑 bug）**：`execute_prepared_tool_call` 原返回
   `AgentToolResult`，`finalize_executed_tool_call` 硬编码 `is_error = false`，导致工具抛错/panic 时
   `ToolResultMessage.isError` 仍为 false（模型会当成成功）。现新增 `ExecutedToolCallOutcome{result,is_error}`，
   对齐上游 `ExecutedToolCallOutcome`；`create_error_tool_result` 置 `is_error: true`。
2. **`AgentToolResult` 缺字段**：补 `structured_content`（对应 `structuredContent`）与 `is_error`
   （对应 `isError`），26 处构造点同步。
3. **`finalizeExecutedToolCall` 缺 `structuredContent` 联动**：上游「钩子只换 content 时丢弃旧
   structuredContent」的语义已补齐；`AfterToolCallResult` 同步补 `structured_content`。
4. **`tool_execution_update.partialResult` 降级**：原只传 `partial.details`（JSON），现改为完整
   `AgentToolResult`（对齐上游 `partialResult: any` 实际值）。
5. **`runToolCall` 公开入口缺失**：新增 `run_tool_call` / `RunToolCallOptions` / `ToolCallHooks` /
   `ToolUpdateSink` / `AgentToolCallOutcome`，并把 `prepare_tool_call`、`finalize_executed_tool_call`
   从 `&AgentLoopConfig` 解耦为 `&ToolCallHooks`（对齐上游签名）。已导出。
6. **`jsonl/storage.ts` torn 处理**：原用普通 `write_file`，改为 `publish_file_atomically`（上游实测
   原子发布，失败不再静默）。同时错误不再被丢弃。
7. **`session.ts` `SessionInvalidBranchError` 消息缺包装**：补齐上游
   `Invalid branch ${JSON.stringify(case)}: ${reason}` 外层格式。
8. **工具 `details` 丢失**：`bash` 现返回 `BashToolDetails{truncation,fullOutputPath}` 并补三种截断提示
   （含 `lastLinePartial` / `truncatedBy` 分支、`timeout`/`aborted` 错误文案、退出码消息顺序）；`read` 现返回
   `ReadToolDetails{truncation}`。`TruncationResult` 补 `Serialize`（camelCase）。

## 仍属“适配”而非缺口的项（本轮再次核实）

- **session 具名错误类型**：上游 4 个错误类（InvalidBranch/BranchExists/PendingAssistant/UnknownTarget）
  Rust 用 `Result<_, String>` 承载，**消息文本已逐字对齐**；上游自身无任何 `instanceof` 分支（已 grep 确认），
  故无行为差异，仅缺静态类型粒度。
- **`HarnessFault` / `HarnessClosed`**：lane 侧 `faulted` 语义已对齐（见上）；harness 对外仍统一
  `HarnessError::Closed`，上游唯一消费处（`lane.ts` 的 `instanceof HarnessFault`）已在 lane 层消化。
- **`events.ts` 三方法**：`enqueueBarrier` → Rust `delivery_tail` 互斥锁（`install_watcher` 内），
  `setUnsubscribe` → `unsubscribe_callback` + `watch_listeners` 重筛，`watchFromSnapshot` → `watch<T>()`。
  三者行为已逐一核对等价。
- **`jsonl/storage.ts` 私有方法**：`applyCommit` / `replayCommitted` 已在 `storage.rs::open`/`commit` 内联
  （validate→apply 两阶段完整）；`withImportedUsage` / `isLegacyV3` / `openLegacyV3` / `upgradeLegacyV3ToV4`
  属 legacy-v3（**用户已明确豁免**）。
- **`AgentToolResult.addedToolNames`**：Rust 有而上游 `AgentToolResult` 没有（上游在别处承载），非缺口。

## 未处理（完整台账见 `todos.md`；概要见 `UPSTREAM.md`「未完成清单」）

- **C1** 工具入参类型（`BashToolInput` / `EditToolInput` / `ReadToolInput` / `WriteToolInput`）
  —— 无运行时差异，仅缺 API 形态与编译期类型安全。〔小，可补〕
- **C2** `harness/telemetry.ts` 的 16 个 span 类型 vs Rust 的常量表 + schema JSON（无运行时行为）。〔小–中，可补〕
- **C3 / C4** session 具名错误、`HarnessFault` 变体 —— 已核实无行为差异，不建议投入。
- **B2 / C5** `harness/session/testing/` 下 8 个文件（gating/instrumented/storage-decorator +
  benchmark×3 + conformance×2）——纯测试基础设施；`conformance` 现有版本仍是「空 case 列表」。〔中–大〕
- **B1 / B3** 平台与迁移：Node 环境细节（已等价覆盖）、legacy-v3 JSONL（**用户明确不复刻**）。
- **上游已知偏差**：`onProviderStreamEvent` / Z.AI CN overflow / HTTP-date `Retry-After`。

---

# 人工复核结论（方法级 1:1 判定）

> 以下为对上方机械扫描结果的逐项核查结论。三档：**豁免**（语言/结构适配，非缺失）、
> **适配**（能力等价，命名或组织不同）、**缺口**（确认缺失或行为不等价）。

## A. 系统性误报（已确认豁免）

- **A1 `tagged_error!` 宏**：`harness/result.ts` 的 `LaneBusy`/`OperationMismatch`/`NoActiveRun`/
  `NoActiveOperation`/`NothingToResume`/`NothingToCompact`/`InvalidMessage`/`InvalidNavigation`/
  `UnknownSkill`/`UnknownTemplate`/`UnknownTarget`/`InvalidLane`/`Closed` 共 13 个错误类，
  Rust 侧由 `harness/result.rs` 的 `tagged_error!` 宏生成，**全部存在**。机械扫描未展开宏，误报。
- **A2 `Result` / `ok` / `err`**：`harness/types.ts` 的 `Result<T,E>`（tagged union）+ `ok()`/`err()`
  构造器，Rust 直接用标准库 `Result` 与 `Ok`/`Err`（`getOrThrow`→`get_or_throw`、
  `getOrUndefined`→`get_or_undefined` 已 1:1）。**豁免**。
- **A3 `*Operation` 接口**：`harness/session/types.ts` 的 `StartingOperation`/`CheckpointOperation`/
  `AssistantReadyOperation`/`ToolsOperation`/`SummaryReadyOperation`/`OperationAt` 等 16 个接口，
  Rust 对应 `harness/session/types.rs` 的 `OperationState` **enum variants** 与 `OperationState::scope()`。**豁免**。
- **A4 缩写命名**：`lazyOAuth`↔`lazy_oauth`、`pollOAuthDeviceCodeFlow`↔`poll_oauth_device_code_flow`
  等已由脚本的 compact 键（去分隔符小写）消除。
- **A5 pi-telemetry 12 项**：`InferEventAttributes`/`ExactTelemetryAttributes`/
  `TelemetrySchemaSpanUnion` 等全部是 TypeScript **类型级编程**（条件类型/映射类型/`UnionToIntersection`），
  Rust 无对应物也不需要。**豁免**。
- **A6 pi-ai 107 项**：绝大多数是 `Classifier*`/`Image*`/`*Compat`/`*Routing` 等
  **声明范围外**（见 `crates/pi-ai/AGENT.md`：只复刻 44 文件子集）。

## B. 命名/结构适配（能力等价）

- `AgentHarness`（interface + const 工厂） → Rust `AgentHarnessApi` + `Harness`
- `restoreSession` → `restore_session_arc`
- `captureLaneSnapshot`（private） → `capture_lane_snapshot_inner`
- `setConfiguration`（private） → `set_configuration_identity`
- `requestOperationAbort` → `request_abort`
- `invokeToolRegistration`（private） → `invoke_registration`
- `operationScopeOf` → `OperationState::scope()`
- `openRecord`/`reserveId`/`wrapBranch`（memory.ts private） → `MemorySessionRepo::open`/`create` 内联
- `getConfig`/`setConfig`（runtime/harness.ts private） → 直接 `self.config.lock()`
- `utf8ByteLength` → `str::len()`（Rust String 天然 UTF-8）
- `toError` → `drive/response.rs::normalize_error`
- `splitDeferredTools` → 已删除（对齐 v0.99.2）
- `HarnessFault` / `HarnessClosed` → 统一 `HarnessError::Closed`（**见 C2-2**）

## C. 真实缺口清单（基线；✅ = 本轮已修）

> 本节保留修复前的原始清单，便于对照。已修项见上方「本轮修复记录」。

### C1 文件级（pi-agent）

1. `harness/session/jsonl/legacy-v3.ts` — **用户已明确豁免**。
2. `harness/session/testing/{gating-storage,instrumented-storage,storage-decorator}.ts`
   — 测试用 storage 装饰器，Rust 侧未实现。
3. `harness/session/testing/benchmark/{datasets,session-repo,storage}.ts` — 基准测试设施，未实现。
4. `harness/session/testing/conformance/{session-repo,storage}.ts` — 契约测试；Rust 侧
   `testing/conformance.rs` 注明「返回空 conformance case 列表」（上游 1000+ 行）。

### C2 类型级（pi-agent）

1. session 具名错误 4 个（判定为适配，见上）。
2. **`HarnessFault` / `HarnessClosed`**：上游 `runtime/harness.ts` 用 `HarnessFault`（storage/invariant
   fault，带 `cause`）与 `HarnessClosed`（关闭时操作仍在跑）区分两类终止；`lane.ts` 依赖
   `closedError instanceof HarnessFault` 计算 `faulted` 标志。
   **本轮已修**：lane 新增 `SealReason::{Fault,Closed}`，`LaneSnapshot.faulted` 按「首次 seal 原因」
   取值（对齐上游 `??=` 语义）；`apply_fault` 传 `Fault`、`close` 传 `Closed`。
   残留：harness 对外仍统一返回 `HarnessError::Closed`（tag `Closed`），未建独立 `HarnessFault` 错误变体。
3. ✅ **工具入参/详情类型**：`BashToolDetails`/`ReadToolDetails` 已落地；`*ToolInput` 仍无同名导出
   （Rust 用 serde 内联反序列化）。
4. **Options 类型**：`AcquireLaneOptions`/`RunToolCallOptions`/`AgentToolCallOutcome`/
   `SummaryGenerationOptions`/`ShellCaptureOptions`/`AgentHarnessToolContextSource`/
   `AgentHarnessResources`/`StorageFixture` 无对应导出。
5. **`PrepareRequest` / `FinishTurn` / `CustomAgentMessages`**：上游 `harness/types.ts` 的类型；
   逻辑已随 v0.99.2 同步落地，但 Rust 侧没有同名公开类型。
6. **telemetry span 类型**：`harness/telemetry.ts` 的 `AiSpanName`/`AiSpanAttributes`/
   `AiSpanStartAttributes`/`HarnessSpanName`/`HarnessSpan*` 等 16 个类型。Rust 侧
   `harness/telemetry.rs` 只有 `HOOK_NAMES`/`EVENT_TYPES` 常量表 + `agent_telemetry_schemas()`，
   **无类型层**。

### C3 行为/API 级（pi-agent）

1. ✅ **`runToolCall`**：已新增公开 `run_tool_call` + `RunToolCallOptions`（见修复记录 5）。
2. **`events.ts` 三个方法**：行为已核对等价（见上「仍属适配」）。
3. ✅ **jsonl storage**：torn 已改为原子发布；`applyCommit` / `replayCommitted` 内联已核对等价。

### C4 已确认无需处理

- `onProviderStreamEvent`（0.99.0）：上游 provider 流层回调，本项目未用。
- overflow 的 Z.AI CN 端点检测：本项目未接入 Z.AI。
- HTTP-date 形式 `Retry-After`：Rust 侧走指数退避，上游用 `Date.parse`。
- telemetry：上游本版只改 CHANGELOG/package.json。
"""


def camel_to_snake(name: str) -> str:
    s = re.sub(r"(.)([A-Z][a-z]+)", r"\1_\2", name)
    s = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", s)
    return s.lower()


def compact(name: str) -> str:
    """比较键：去掉所有非字母数字并小写。

    消除缩写带来的命名差异：lazyOAuth ↔ lazy_oauth 都归一到 `lazyoauth`。
    """
    return re.sub(r"[^a-z0-9]", "", name.lower())


def strip_comment_lines(lines):
    """逐行返回 (原始行, 去掉行尾 // 注释后的代码行)。粗略即可。"""
    out = []
    for line in lines:
        code = line
        # 去掉行内 // 注释（不处理字符串里的 //，源码中极少）
        idx = code.find("//")
        if idx >= 0:
            code = code[:idx]
        out.append((line, code))
    return out


def ts_symbols(path):
    """返回 {name: kind}；kind ∈ class|fn|const|type|method。"""
    out = {}
    try:
        lines = open(path, encoding="utf-8").read().splitlines()
    except Exception:
        return out

    depth = 0
    class_depths = []  # 栈：每个 class 体的基准 depth

    for raw, code in strip_comment_lines(lines):
        s = code.strip()
        if not s or s.startswith(("*", "/*")):
            # 仍需处理 } 缩进行？空行无花括号，直接跳过
            continue

        # ---- 顶层导出（depth == 0）----
        if depth == 0:
            m = re.match(r"^export\s+(?:default\s+)?(?:abstract\s+)?class\s+(\w+)", s)
            if m:
                out.setdefault(m.group(1), "class")
            else:
                m = re.match(r"^export\s+(?:async\s+)?function\s+(\w+)", s)
                if m:
                    out.setdefault(m.group(1), "fn")
                else:
                    m = re.match(r"^export\s+const\s+(\w+)\s*[=:]", s)
                    if m:
                        out.setdefault(m.group(1), "const")
                    else:
                        m = re.match(r"^export\s+(?:declare\s+)?(?:interface|type|enum)\s+(\w+)", s)
                        if m:
                            out.setdefault(m.group(1), "type")

        # ---- class 方法：恰在某个 class 体顶层 ----
        if class_depths and depth == class_depths[-1] + 1:
            m = re.match(
                r"^(?:public\s+|private\s+|protected\s+|static\s+|async\s+|get\s+|set\s+"
                r"|readonly\s+|override\s+|abstract\s+)*"
                r"([a-zA-Z_$][\w$]*)\s*(?:<[^>]*>)?\s*[<(]",
                s,
            )
            if m and m.group(1) not in RESERVED_TS:
                out.setdefault(m.group(1), "method")

        # ---- 维护花括号深度 ----
        opens = code.count("{")
        closes = code.count("}")
        # class 体的基准：class 行的 { 之前 depth
        if re.match(r"^(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+\w+", s):
            class_depths.append(depth)
        depth += opens - closes
        while class_depths and depth <= class_depths[-1]:
            class_depths.pop()

    return out


def rust_symbols_of_file(path):
    """单个 Rust 文件的符号集合 {name: kind}。"""
    out = {}
    try:
        lines = open(path, encoding="utf-8").read().splitlines()
    except Exception:
        return out
    for line in lines:
        s = line.strip()
        if not s or s.startswith(("//", "/*", "*", "#[")):
            continue
        s = s.split("//")[0].strip()
        m = re.match(r"^(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)", s)
        if m:
            out.setdefault(m.group(1), "fn")
            continue
        m = re.match(r"^(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum|trait|union)\s+(\w+)", s)
        if m:
            out.setdefault(m.group(1), "type")
            continue
        m = re.match(r"^(?:pub(?:\([^)]*\))?\s+)?type\s+(\w+)", s)
        if m:
            out.setdefault(m.group(1), "type")
            continue
        m = re.match(r"^(?:pub(?:\([^)]*\))?\s+)?(?:static|const)\s+(\w+)", s)
        if m:
            out.setdefault(m.group(1), "const")
    return out


def rust_files(root):
    """{相对路径去 .rs: 绝对路径}"""
    out = {}
    for dp, _, files in os.walk(root):
        for f in files:
            if f.endswith(".rs"):
                rel = os.path.relpath(os.path.join(dp, f), root)
                out[rel[:-3]] = os.path.join(dp, f)
    return out


def pair_rust_file(stem, rust_index):
    """TS 相对路径 stem（无扩展名）→ Rust 文件绝对路径。"""
    cand = stem.replace("-", "_")
    if cand in rust_index:
        return rust_index[cand]
    if stem in rust_index:
        return rust_index[stem]
    base = os.path.basename(cand)
    d = os.path.dirname(cand)
    if base == "index":
        for alt in ("mod", "lib"):
            key = f"{d}/{alt}" if d else alt
            if key in rust_index:
                return rust_index[key]
    # 目录级 index → 目录名.rs
    if base == "index" and d:
        if d in rust_index:
            return rust_index[d]
    # x.ts → x/mod.rs（TS 单文件在 Rust 侧拆成目录模块）
    for alt in (f"{cand}/mod", f"{stem}/mod"):
        if alt in rust_index:
            return rust_index[alt]
    return None


def fuzzy_candidates(snake, rust_all, limit=4):
    """在 Rust 符号里找与 snake 可能对应的近似名（用于人工分流）。"""
    if len(snake) < 5:
        return []
    cands = []
    for name in rust_all:
        if len(name) < 5:
            continue
        if snake in name or name in snake:
            cands.append(name)
    cands.sort(key=lambda n: (abs(len(n) - len(snake)), n))
    return cands[:limit]


def declared_scope(pkg):
    """pi-ai/AGENT.md 声明的复刻范围（TS 相对路径集合）。返回 None 表示无声明。"""
    agent_md = os.path.join(ROOT, f"crates/pi-{pkg}/AGENT.md")
    if not os.path.exists(agent_md):
        return None
    text = open(agent_md, encoding="utf-8").read()
    paths = set(re.findall(r"src/[\w./-]+\.ts", text))
    if not paths:
        return None
    return {p[len("src/"):] for p in paths}


def main():
    only = sys.argv[1] if len(sys.argv) > 1 else None
    report = []
    stats = defaultdict(int)

    for pkg, rs_rel in TARGETS:
        if only and pkg != only:
            continue
        rs_root = os.path.join(ROOT, rs_rel)
        pkg_src = os.path.join(UPSTREAM, pkg, "src")
        if not os.path.isdir(pkg_src):
            report.append(f"\n## packages/{pkg}\n\n（未找到 {pkg_src}）\n")
            continue
        if not os.path.isdir(rs_root):
            report.append(f"\n## packages/{pkg} → {rs_rel}\n\n（未找到 Rust crate）\n")
            continue

        rust_index = rust_files(rs_root)
        rust_by_file = {rel: rust_symbols_of_file(p) for rel, p in rust_index.items()}
        rust_all = {}
        for syms in rust_by_file.values():
            for k, v in syms.items():
                rust_all.setdefault(k, v)

        scope = declared_scope(pkg)

        report.append(f"\n## packages/{pkg} → `{rs_rel}`\n")
        missing_files, out_of_scope, symbol_gaps = [], [], []

        for dp, _, files in os.walk(pkg_src):
            for f in sorted(files):
                if not f.endswith(".ts") or f.endswith(".test.ts") or f.endswith(".d.ts"):
                    continue
                rel = os.path.relpath(os.path.join(dp, f), pkg_src)
                if any(part in rel for part in EXCLUDE_PATH_PARTS):
                    continue
                if any(rel.endswith(x) for x in EXCLUDE_TS_SUFFIX):
                    stats[(pkg, "excluded")] += 1
                    continue

                stem = rel[:-3]
                rp = pair_rust_file(stem, rust_index)
                if rp is None:
                    if scope is not None and rel not in scope:
                        out_of_scope.append(rel)
                        stats[(pkg, "out_of_scope")] += 1
                    else:
                        missing_files.append(rel)
                        stats[(pkg, "missing_file")] += 1
                    continue
                stats[(pkg, "paired")] += 1

                rrel = os.path.relpath(rp, rs_root)
                local_syms = rust_by_file.get(rrel[:-3], {})
                local_keys = {compact(k) for k in local_syms}
                global_keys = {compact(k) for k in rust_all}
                ts = ts_symbols(os.path.join(dp, f))

                moved, missing = [], []
                for name, kind in sorted(ts.items()):
                    sn = camel_to_snake(name)
                    ck = compact(name)
                    if len(ck) < 2:
                        continue
                    if ck in local_keys:
                        continue
                    if ck in global_keys:
                        moved.append((name, kind, rust_all.get(sn) or rust_all.get(name)))
                    else:
                        missing.append((name, kind, fuzzy_candidates(sn, rust_all)))
                if moved:
                    stats[(pkg, "moved")] += len(moved)
                if missing:
                    stats[(pkg, "missing_symbol")] += len(missing)
                    symbol_gaps.append((rel, rrel, missing, moved))

        report.append(
            f"\n统计：配对 {stats[(pkg,'paired')]} 文件 · "
            f"文件缺失 {stats[(pkg,'missing_file')]} · "
            f"范围外 {stats[(pkg,'out_of_scope')]} · "
            f"符号缺失 {stats[(pkg,'missing_symbol')]} · "
            f"符号移位 {stats[(pkg,'moved')]}\n"
        )

        report.append("\n### 1) 文件级缺失（范围内，Rust 侧无对应文件）\n\n")
        if missing_files:
            for rel in missing_files:
                report.append(f"- `{rel}`\n")
        else:
            report.append("（无）\n")

        if out_of_scope:
            report.append(f"\n<details><summary>范围外文件 {len(out_of_scope)} 个"
                          f"（AGENT.md 已声明不复刻）</summary>\n\n")
            for rel in out_of_scope:
                report.append(f"- `{rel}`\n")
            report.append("\n</details>\n")

        report.append(f"\n### 2) 符号级缺失（TS 有，Rust 整 crate 未见同名）\n\n")
        if symbol_gaps:
            for rel, rrel, missing, moved in symbol_gaps:
                report.append(f"\n**`{rel}`** → `{rrel}`\n\n")
                for name, kind, fz in missing:
                    tail = f"  ← 候选: {', '.join('`'+c+'`' for c in fz)}" if fz else ""
                    report.append(f"- MISSING `{name}` ({kind}) → 期望 `{camel_to_snake(name)}`{tail}\n")
                if moved:
                    report.append(f"- <sub>移位 {len(moved)} 项："
                                  + ", ".join(f"`{n}`" for n, _, _ in moved)
                                  + "</sub>\n")
        else:
            report.append("（无）\n")

    header = [
        "# pi_rs ↔ upstream 方法级对比报告\n\n",
        "> 基线：`upstream/` 检出于 `v0.99.2`（HEAD `005af57d8`）\n\n",
        "> 方法：提取 TS 顶层导出（class/function/const/interface/type/enum）与 class 方法，\n",
        "> 按 camelCase→snake_case 在 Rust 侧同名查找。`local` 命中视为 OK；\n",
        "> `global`（同 crate 其他文件）记为「移位」；整 crate 无同名记 MISSING。\n",
        "> 机械扫描，用于定位可疑缺口；命名差异、结构合并、宏生成（如 `tagged_error!`）、\n",
        "> trait 默认实现会产生误报，需人工确认。**文末附人工逐项复核结论（豁免/适配/缺口三档）**。\n\n",
        "## 总览\n\n",
        "上游包 → Rust crate 映射（用于确认范围）：\n\n",
        "- `packages/agent` → `crates/pi-agent`（**完整复刻目标**）\n",
        "- `packages/ai` → `crates/pi-ai`（**声明子集**，范围见 `crates/pi-ai/AGENT.md`）\n",
        "- `packages/telemetry` → `crates/pi-telemetry`\n",
        "- 未复刻：`chord`、`client`、`codemode`、`coding-agent`、`durable`、`evals`、\n",
        "  `mcp`、`protocol`、`server`、`session-backends`、`tui`（以及 agent 包内的 `pico3`、`polymarket`）\n\n",
        "扫描统计：\n\n",
    ]
    for pkg, rs_rel in TARGETS:
        if only and pkg != only:
            continue
        header.append(
            f"- **{pkg}** → `{rs_rel}`：配对 {stats[(pkg,'paired')]} · "
            f"缺文件 {stats[(pkg,'missing_file')]} · 范围外 {stats[(pkg,'out_of_scope')]} · "
            f"缺符号 {stats[(pkg,'missing_symbol')]} · 移位 {stats[(pkg,'moved')]}\n"
        )

    with open(OUT, "w", encoding="utf-8") as fh:
        fh.writelines(header)
        fh.writelines(report)
        fh.write(REVIEW_SECTION)
    print(f"written: {OUT}")
    for pkg, _ in TARGETS:
        if only and pkg != only:
            continue
        print(f"{pkg}: paired={stats[(pkg,'paired')]} missing_file={stats[(pkg,'missing_file')]} "
              f"out_of_scope={stats[(pkg,'out_of_scope')]} missing_symbol={stats[(pkg,'missing_symbol')]} "
              f"moved={stats[(pkg,'moved')]}")


if __name__ == "__main__":
    main()
