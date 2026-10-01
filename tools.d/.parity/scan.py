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

# 人工复核结论

> 上方是机械扫描结果。本节的职责只有两件事：**(1) 说明哪些 MISSING 是假阳性，(2) 列出仍未对齐的项**。
> 已对齐项的改动明细见 git 历史，此处不再保留。

## 一、机械扫描的已知假阳性（非遗漏）

以下类别扫描器会报 MISSING，但 Rust 侧已有等价实现：

- **宏生成**：`harness/result.ts` 的 13 个错误类由 `result.rs` 的 `tagged_error!` 宏生成。
- **语言替换**：TS 的 `Result`/`ok`/`err` → Rust 标准库 `Result`；`utf8ByteLength` → `str::len()`。
- **类型合并**：`session/types.ts` 的 16 个 `*Operation` 接口 → `OperationState` enum variants。
- **类型级编程**：`pi-telemetry` 的 12 项（条件 / 映射类型 / `UnionToIntersection`）与
  `harness/telemetry.ts` 的 16 个 span 类型（`TelemetrySchemaSpanName<typeof SCHEMA>` 推导）
  —— Rust 无对应能力，运行时行为一致（`start_ai_span` / `start_harness_span` / 两个 `*_SCHEMA` 都在）。
- **命名适配**：`restoreSession`→`restore_session_arc`、`captureLaneSnapshot`→`capture_lane_snapshot_inner`、
  `setConfiguration`→`set_configuration_identity`、`requestOperationAbort`→`request_abort`、
  `operationScopeOf`→`OperationState::scope()` 等。
- **内联实现**：第 3 节的私有函数档多属此类（如 prompt-templates 的 3 个加载函数内联进
  `load_prompt_templates`、skills 的 `loadSkillsFromDirInternal`→`load_skills_from_dir_inner`）。
- **范围外**：`pi-ai` 107 项中的绝大多数（Classifier / Images / 各厂商 Compat / Routing）。

## 二、仍未对齐的项

详情与工作量见 `todos.md`：

- **B1** Node 平台细节（`findBashOnPath` / WSL 检测 / `killProcessTree`）—— `std::process` 已等价覆盖，不搬。
- **B2 / C5** 一致性测试套件（conformance + benchmark + storage 装饰器，约 2,275 行）—— 测试基建。
- **B3** legacy-v3 JSONL 迁移 —— **用户明确要求不复刻**。
- **C2** `harness/telemetry.ts` 的 16 个 span 类型 —— 类型级推导，豁免。
- **C3** session 4 个具名错误 —— 消息已逐字对齐、上游无 `instanceof` 分支，不建议投入。
- **上游能力偏差**：`onProviderStreamEvent` / Z.AI CN overflow / HTTP-date `Retry-After`。

## 三、扫描口径与已知盲区

- 符号匹配用「去分隔符 + 小写」的 compact 键，因此 `lazyOAuth` ↔ `lazy_oauth` 这类缩写差异不会误报。
- `local`（同文件命中）视为 OK；`global`（同 crate 其他文件）记为「移位」；整 crate 无同名记 MISSING。
- 第 3 节的私有函数档覆盖上游非导出 `function`，是导出符号扫描的补充 ——
  历史上正是靠人工读到这一层才发现 edit 的 `prepareEditArguments` 缺口。
- **已知盲区**：`tagged_error!` 等宏生成的类型扫不到（见第一节）；Rust 侧内联实现无法自动识别。
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


def ts_private_fns(path):
    """提取非导出的顶层函数名（上游私有实现逻辑）。

    导出符号扫描看不到这些函数，但真实缺口也可能藏在这里——
    例如 edit.ts 的 `prepareEditArguments`（处理 edits 为字符串/单对象/legacy 顶层字段）。
    """
    out = []
    try:
        lines = open(path, encoding="utf-8").read().splitlines()
    except Exception:
        return out
    for _raw, code in strip_comment_lines(lines):
        s = code.strip()
        if s.startswith("export"):
            continue
        m = re.match(r"^(?:async\s+)?function\s+(\w+)", s)
        if m:
            out.append(m.group(1))
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
        missing_files, out_of_scope, symbol_gaps, private_gaps = [], [], [], []

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

                # 私有顶层函数：上游的非导出实现函数（内联到 Rust 调用方时属正常）
                priv_missing = []
                for pname in sorted(set(ts_private_fns(os.path.join(dp, f)))):
                    ck2 = compact(pname)
                    if ck2 in local_keys or ck2 in global_keys:
                        continue
                    priv_missing.append(
                        (pname, fuzzy_candidates(camel_to_snake(pname), rust_all))
                    )
                if priv_missing:
                    stats[(pkg, "private_gap")] += len(priv_missing)
                    private_gaps.append((rel, rrel, priv_missing))

        report.append(
            f"\n统计：配对 {stats[(pkg,'paired')]} 文件 · "
            f"文件缺失 {stats[(pkg,'missing_file')]} · "
            f"范围外 {stats[(pkg,'out_of_scope')]} · "
            f"符号缺失 {stats[(pkg,'missing_symbol')]} · "
            f"符号移位 {stats[(pkg,'moved')]} · "
            f"私有函数差异 {stats[(pkg,'private_gap')]}\n"
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

        report.append("\n### 3) 私有顶层函数差异（TS 非导出实现函数，Rust 未见同名）\n\n")
        report.append(
            "> 上游 `function foo()` 这类非导出实现函数。Rust 常把它们内联进调用方，\n"
            "> 因此大量属于正常；但**真实缺口也藏在这里**——edit.ts 的 `prepareEditArguments`\n"
            "> 就是靠人工读到这一层才发现的（处理 `edits` 为字符串/单对象/legacy 顶层字段）。\n"
            "> 需人工逐条确认。\n\n"
        )
        if private_gaps:
            for rel, rrel, items in private_gaps:
                report.append(f"\n**`{rel}`** → `{rrel}`\n\n")
                for name, fz in items:
                    tail = f"  ← 候选: {', '.join('`'+c+'`' for c in fz)}" if fz else ""
                    report.append(f"- PRIVATE `{name}`{tail}\n")
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
            f"缺符号 {stats[(pkg,'missing_symbol')]} · 移位 {stats[(pkg,'moved')]} · "
            f"私有函数差异 {stats[(pkg,'private_gap')]}\n"
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
              f"moved={stats[(pkg,'moved')]} private_gap={stats[(pkg,'private_gap')]}")


if __name__ == "__main__":
    main()
