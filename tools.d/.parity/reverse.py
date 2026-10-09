#!/usr/bin/env python3
"""pi_rs → upstream 反向扫描（查「多余」）。

方向与 scan.py 相反：提取 Rust 侧每个 crate 的 pub 符号（顶层 fn/type/const +
impl 块内 pub 方法），按 snake_case→camelCase 在 TS 侧同名查找。
找不到的记为「Rust 独有」候选，需人工判定是：
  (a) 真实多余（TS 没有对应业务逻辑）
  (b) 语言机制等价（trait 方法 / derive / 标准库 / 合并函数改名）
"""
import os
import re
import sys
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
UPSTREAM = os.path.join(ROOT, "upstream/packages")

TARGETS = [
    ("agent", "crates/pi-agent-core/src"),
    ("ai", "crates/pi-ai/src"),
    ("telemetry", "crates/pi-telemetry/src"),
    ("durable", "crates/pi-durable/src"),
    ("chord", "crates/pi-durable/src/chord"),
]

CHORD_SCOPE = {
    "delta/index.ts", "delta/apply-immutable-trusted.ts", "delta/tracker.ts",
    "json.ts", "context/index.ts", "services/state.ts",
    "services/state-internals.ts", "services/state-codec.ts",
}

# 这些名字是 Rust 语言机制 / 通用约定，不作为「多余业务符号」上报
IGNORE_NAMES = {
    "new", "default", "clone", "clone_from", "eq", "ne", "partial_cmp", "cmp",
    "hash", "fmt", "drop", "into", "from", "try_from", "try_into", "from_iter",
    "into_iter", "iter", "iter_mut", "as_ref", "as_mut", "borrow", "to_owned",
    "serialize", "deserialize", "deserialize_in_place", "to_string", "to_vec",
    "deref", "deref_mut", "index", "index_mut", "write", "flush", "read",
    "poll", "call", "call_mut", "poll_next", "type_id",
    "debug", "display", "error", "source", "description", "cause",
    "is_none", "is_some", "unwrap", "expect", "unwrap_or", "unwrap_or_else",
    "unwrap_or_default", "ok", "err", "map", "map_err", "and_then", "or_else",
    "and", "or", "then", "filter", "collect", "fold", "reduce", "enumerate",
    "next", "size_hint", "count", "len", "is_empty", "get", "get_mut", "insert",
    "remove", "contains_key", "contains", "push", "pop", "clear", "extend",
    "keys", "values", "values_mut", "entry", "range", "split", "join",
    "strip_prefix", "strip_suffix", "starts_with", "ends_with", "trim",
    "trim_start", "trim_end", "replace", "to_lowercase", "to_uppercase",
    "parse", "is_err", "as_str", "as_bytes", "as_slice", "into_bytes",
    "into_inner", "into_boxed_str", "is_ok", "or_default", "cloned", "copied",
    "get_or_insert", "get_or_insert_with", "get_or_insert_default",
    "is_sync", "is_send", "name", "deserialize_map", "visit_str", "visit_map",
}

IGNORE_TRAITS = {
    "default", "clone", "copy", "debug", "display", "error", "eq", "ord",
    "partialeq", "partialord", "hash", "serde", "serialize", "deserialize",
    "deref", "derefmut", "from", "into", "tryfrom", "tryinto", "fromiterator",
    "intoiterator", "asref", "asmut", "borrow", "toborrow", "fromstr",
    "iterator", "exactsizeiterator", "doubleendediterator", "future",
    "futuresink", "futuresstream", "stream", "sink", "asyncread",
    "asyncwrite", "unpin", "send", "sync", "str", "string", "lowerhex",
    "upperhex", "writefmt", "std", "core", "alloc", "formatter",
}


def camel_to_snake(name):
    s = re.sub(r"(.)([A-Z][a-z]+)", r"\1_\2", name)
    s = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", s)
    return s.lower()


def snake_to_camel(name):
    parts = name.split("_")
    return parts[0] + "".join(p.title() for p in parts[1:])


def compact(name):
    return re.sub(r"[^a-z0-9]", "", name.lower())


def strip_comment_lines(lines):
    out = []
    for line in lines:
        code = line
        idx = code.find("//")
        if idx >= 0:
            code = code[:idx]
        out.append((line, code))
    return out


def rust_symbols(path):
    """返回 (顶层符号 {name: kind}, impl 方法 {name: kind})。"""
    top = {}
    methods = {}
    try:
        lines = open(path, encoding="utf-8").read().splitlines()
    except Exception:
        return top, methods

    depth = 0
    impl_depth = None  # impl 块的基准 depth（`impl` 行当时的 depth）

    for raw, code in strip_comment_lines(lines):
        s = code.strip()
        if not s or s.startswith(("//", "/*", "*")):
            continue

        # impl 块内的 pub fn
        if impl_depth is not None and depth == impl_depth + 1:
            m = re.match(r"^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)", s)
            if m:
                methods.setdefault(m.group(1), "method")

        # 顶层 pub 符号（depth == 0）
        if depth == 0:
            m = re.match(r"^pub(?:\([^)]*\))?\s+(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)", s)
            if m:
                top.setdefault(m.group(1), "fn")
            else:
                m = re.match(r"^pub(?:\([^)]*\))?\s+(?:struct|enum|trait|union)\s+(\w+)", s)
                if m:
                    top.setdefault(m.group(1), "type")
                else:
                    m = re.match(r"^pub(?:\([^)]*\))?\s+type\s+(\w+)", s)
                    if m:
                        top.setdefault(m.group(1), "type")
                    else:
                        m = re.match(r"^pub(?:\([^)]*\))?\s+(?:static|const)\s+(\w+)", s)
                        if m:
                            top.setdefault(m.group(1), "const")

        # 维护 depth；检测 impl 块开启
        if re.match(r"^impl(?:\s*<[^>]*>)?\s", s):
            impl_depth = depth
        opens = code.count("{")
        closes = code.count("}")
        depth += opens - closes
        if impl_depth is not None and depth <= impl_depth:
            impl_depth = None

    return top, methods


def ts_symbols(path):
    """返回 {name: kind}，含顶层导出与 class 方法（复用 scan.py 口径）。"""
    out = {}
    try:
        lines = open(path, encoding="utf-8").read().splitlines()
    except Exception:
        return out
    depth = 0
    class_depths = []
    for raw, code in strip_comment_lines(lines):
        s = code.strip()
        if not s or s.startswith(("*", "/*")):
            continue
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
        if class_depths and depth == class_depths[-1] + 1:
            m = re.match(
                r"^(?:public\s+|private\s+|protected\s+|static\s+|async\s+|get\s+|set\s+"
                r"|readonly\s+|override\s+|abstract\s+)*"
                r"([a-zA-Z_$][\w$]*)\s*(?:<[^>]*>)?\s*[<(]",
                s,
            )
            if m:
                out.setdefault(m.group(1), "method")
        opens = code.count("{")
        closes = code.count("}")
        if re.match(r"^(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+\w+", s):
            class_depths.append(depth)
        depth += opens - closes
        while class_depths and depth <= class_depths[-1]:
            class_depths.pop()
    return out


def declared_scope(pkg):
    if pkg == "chord":
        return CHORD_SCOPE
    agent_md = os.path.join(ROOT, f"crates/pi-{pkg}/AGENT.md")
    if not os.path.exists(agent_md):
        return None
    text = open(agent_md, encoding="utf-8").read()
    paths = set(re.findall(r"src/[\w./-]+\.ts", text))
    if not paths:
        return None
    return {p[len("src/"):] for p in paths}


def rust_files(root):
    out = {}
    for dp, _, files in os.walk(root):
        if "/target/" in dp or dp.endswith("/target"):
            continue
        for f in files:
            if f.endswith(".rs"):
                out[os.path.join(dp, f)] = f
    return out


def main():
    only = sys.argv[1] if len(sys.argv) > 1 else None
    print("# Rust → TS 反向「多余」扫描\n")

    for pkg, rs_rel in TARGETS:
        if only and pkg != only:
            continue
        rs_root = os.path.join(ROOT, rs_rel)
        pkg_src = os.path.join(UPSTREAM, pkg, "src")
        if not os.path.isdir(pkg_src) or not os.path.isdir(rs_root):
            continue

        scope = declared_scope(pkg)

        # TS 全量符号（范围内）
        ts_all = {}
        for dp, _, files in os.walk(pkg_src):
            for f in files:
                if not f.endswith(".ts") or f.endswith((".test.ts", ".d.ts")):
                    continue
                rel = os.path.relpath(os.path.join(dp, f), pkg_src)
                if scope is not None and rel not in scope:
                    continue
                for k, v in ts_symbols(os.path.join(dp, f)).items():
                    ts_all.setdefault(k, v)
        ts_keys = {compact(k) for k in ts_all}

        print(f"\n## packages/{pkg} → `{rs_rel}`\n")
        n_extra = 0
        for path, fname in sorted(rust_files(rs_root).items()):
            top, methods = rust_symbols(path)
            extras = []
            for name in sorted(set(top) | set(methods)):
                c = compact(name)
                if len(c) < 2:
                    continue
                if name in IGNORE_NAMES or c in IGNORE_NAMES:
                    continue
                if c in IGNORE_TRAITS:
                    continue
                # 反查 TS：snake→camel 后 compact 匹配
                cam = snake_to_camel(name)
                if compact(cam) in ts_keys or c in ts_keys:
                    continue
                extras.append(name)
            if extras:
                rel = os.path.relpath(path, rs_root)
                n_extra += len(extras)
                print(f"**`{rel}`**: " + ", ".join(f"`{e}`" for e in extras))
        if n_extra == 0:
            print("（无 Rust 独有符号）")
        else:
            print(f"\n> 小计 {n_extra} 项候选（需人工判定真实多余 vs 语言机制等价）")


if __name__ == "__main__":
    main()
