#!/usr/bin/env python3
"""生成每个目标模块的「逐文件审计简报」：
  - TS 文件 → Rust 文件配对表（含未配对）
  - 正向：TS 导出符号在 Rust 侧 local/moved/missing
  - 反向：Rust pub 符号（顶层 fn/type/const + impl pub fn）在 TS 侧是否可见

输出到 tools.d/parity/audit-<pkg>.txt，供逐方法人工复核使用。
"""
import os, re, sys
from collections import defaultdict

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
UPSTREAM = os.path.join(ROOT, "upstream/packages")
OUTDIR = os.path.join(ROOT, "tools.d", "parity")

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

EXCLUDE_TS_SUFFIX = ("models.generated.ts", "image-models.generated.ts")
EXCLUDE_PATH_PARTS = ("/pico3/",)

RESERVED_TS = {
    "if","for","while","switch","return","catch","constructor","else","do","try",
    "finally","new","typeof","await","yield","super","this","function","class",
    "const","let","var","import","export","default",
}

IGNORE_RUST = {
    "new","default","clone","clone_from","eq","ne","partial_cmp","cmp","hash","fmt",
    "drop","into","from","try_from","try_into","from_iter","into_iter","iter",
    "iter_mut","as_ref","as_mut","borrow","to_owned","serialize","deserialize",
    "deref","deref_mut","index","index_mut","write","flush","read","poll","call",
    "debug","display","error","source","description","cause","is_none","is_some",
    "unwrap","expect","unwrap_or","unwrap_or_else","unwrap_or_default","ok","err",
    "map","map_err","and_then","or_else","and","or","then","filter","collect",
    "fold","reduce","enumerate","next","size_hint","count","len","is_empty","get",
    "get_mut","insert","remove","contains_key","contains","push","pop","clear",
    "extend","keys","values","values_mut","entry","range","split","join",
    "strip_prefix","strip_suffix","starts_with","ends_with","trim","trim_start",
    "trim_end","replace","to_lowercase","to_uppercase","parse","is_err","as_str",
    "as_bytes","as_slice","into_bytes","into_inner","into_boxed_str","is_ok",
    "cloned","copied","get_or_insert","get_or_insert_with","name","type_id",
    "to_string","to_vec","unwrap_or_default","or_default","is_sync","is_send",
    "deref","deref_mut","partial_eq","to_owned","to_vec","to_string","borrow",
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

def ts_symbols(path):
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
            if m: out.setdefault(m.group(1), "class"); continue
            m = re.match(r"^export\s+(?:async\s+)?function\s+(\w+)", s)
            if m: out.setdefault(m.group(1), "fn"); continue
            m = re.match(r"^export\s+const\s+(\w+)\s*[=:]", s)
            if m: out.setdefault(m.group(1), "const"); continue
            m = re.match(r"^export\s+(?:declare\s+)?(?:interface|type|enum)\s+(\w+)", s)
            if m: out.setdefault(m.group(1), "type")
        if class_depths and depth == class_depths[-1] + 1:
            m = re.match(r"^(?:public\s+|private\s+|protected\s+|static\s+|async\s+|get\s+|set\s+|readonly\s+|override\s+|abstract\s+)*([a-zA-Z_$][\w$]*)\s*(?:<[^>]*>)?\s*[<(]", s)
            if m and m.group(1) not in RESERVED_TS:
                out.setdefault(m.group(1), "method")
        opens = code.count("{")
        closes = code.count("}")
        if re.match(r"^(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+\w+", s):
            class_depths.append(depth)
        depth += opens - closes
        while class_depths and depth <= class_depths[-1]:
            class_depths.pop()
    return out

def rust_symbols(path):
    top = {}
    methods = {}
    try:
        lines = open(path, encoding="utf-8").read().splitlines()
    except Exception:
        return top, methods
    depth = 0
    impl_depth = None
    for raw, code in strip_comment_lines(lines):
        s = code.strip()
        if not s or s.startswith(("//", "/*", "*")):
            continue
        if impl_depth is not None and depth == impl_depth + 1:
            m = re.match(r"^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)", s)
            if m: methods.setdefault(m.group(1), "method")
        if depth == 0:
            m = re.match(r"^pub(?:\([^)]*\))?\s+(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)", s)
            if m: top.setdefault(m.group(1), "fn"); 
            else:
                m = re.match(r"^pub(?:\([^)]*\))?\s+(?:struct|enum|trait|union)\s+(\w+)", s)
                if m: top.setdefault(m.group(1), "type")
                else:
                    m = re.match(r"^pub(?:\([^)]*\))?\s+type\s+(\w+)", s)
                    if m: top.setdefault(m.group(1), "type")
                    else:
                        m = re.match(r"^pub(?:\([^)]*\))?\s+(?:static|const)\s+(\w+)", s)
                        if m: top.setdefault(m.group(1), "const")
        if re.match(r"^impl(?:\s*<[^>]*>)?\s", s):
            impl_depth = depth
        opens = code.count("{")
        closes = code.count("}")
        depth += opens - closes
        if impl_depth is not None and depth <= impl_depth:
            impl_depth = None
    return top, methods

def rust_files(root):
    out = {}
    for dp, _, files in os.walk(root):
        if "/target/" in dp or dp.endswith("/target"):
            continue
        for f in files:
            if f.endswith(".rs"):
                rel = os.path.relpath(os.path.join(dp, f), root)
                out[rel[:-3]] = os.path.join(dp, f)
    return out

def pair_rust_file(stem, rust_index):
    cand = stem.replace("-", "_")
    if cand in rust_index: return rust_index[cand]
    if stem in rust_index: return rust_index[stem]
    base = os.path.basename(cand)
    d = os.path.dirname(cand)
    if base == "index":
        for alt in ("mod", "lib"):
            key = f"{d}/{alt}" if d else alt
            if key in rust_index: return rust_index[key]
    if base == "index" and d:
        if d in rust_index: return rust_index[d]
    for alt in (f"{cand}/mod", f"{stem}/mod"):
        if alt in rust_index: return rust_index[alt]
    return None

def declared_scope(pkg):
    """只有 pi-ai 与 chord 是「声明子集」，其余（agent/durable/telemetry）为完整复刻。
    注意：durable 的 AGENT.md 虽然含 src/xxx.ts 路径，但那是 67 文件的逐项状态表，
    不是子集声明 —— 若照读会把 52 个文件误判为范围外。"""
    if pkg == "chord":
        return CHORD_SCOPE
    if pkg != "ai":
        return None
    md = os.path.join(ROOT, f"crates/pi-{pkg}/AGENT.md")
    if not os.path.exists(md): return None
    text = open(md, encoding="utf-8").read()
    paths = set(re.findall(r"src/[\w./-]+\.ts", text))
    if not paths: return None
    return {p[len("src/"):] for p in paths}

def main():
    only = sys.argv[1] if len(sys.argv) > 1 else None
    os.makedirs(OUTDIR, exist_ok=True)
    for pkg, rs_rel in TARGETS:
        if only and pkg != only: continue
        rs_root = os.path.join(ROOT, rs_rel)
        pkg_src = os.path.join(UPSTREAM, pkg, "src")
        if not os.path.isdir(pkg_src) or not os.path.isdir(rs_root): continue
        scope = declared_scope(pkg)
        rindex = rust_files(rs_root)
        rbyfile = {rel: rust_symbols(abs) for rel, abs in rindex.items()}

        lines = [f"# 审计简报：packages/{pkg} → {rs_rel}\n", f"范围：{'AGENT.md 声明子集' if scope else '完整复刻'}\n"]
        ts_all = {}
        # 收集范围内 TS 符号
        for dp, _, files in os.walk(pkg_src):
            for f in sorted(files):
                if not f.endswith(".ts") or f.endswith((".test.ts", ".d.ts")): continue
                rel = os.path.relpath(os.path.join(dp, f), pkg_src)
                if any(p in rel for p in EXCLUDE_PATH_PARTS): continue
                if any(rel.endswith(x) for x in EXCLUDE_TS_SUFFIX): continue
                if scope is not None and rel not in scope: continue
                for k,v in ts_symbols(os.path.join(dp,f)).items():
                    ts_all.setdefault(k, v)
        ts_keys = {compact(k) for k in ts_all}

        lines.append("\n## 文件配对表\n")
        pairs = []
        unpaired = []
        for dp, _, files in os.walk(pkg_src):
            for f in sorted(files):
                if not f.endswith(".ts") or f.endswith((".test.ts", ".d.ts")): continue
                rel = os.path.relpath(os.path.join(dp, f), pkg_src)
                if any(p in rel for p in EXCLUDE_PATH_PARTS): continue
                if any(rel.endswith(x) for x in EXCLUDE_TS_SUFFIX): continue
                stem = rel[:-3]
                rp = pair_rust_file(stem, rindex)
                if scope is not None and rel not in scope:
                    continue
                if rp is None:
                    unpaired.append(rel)
                else:
                    pairs.append((rel, os.path.relpath(rp, rs_root)))
        for rel, rrel in pairs:
            lines.append(f"- `{rel}` → `{rrel}`")
        if unpaired:
            lines.append("\n未配对（范围内 TS 文件在 Rust 侧无对应）：")
            for rel in unpaired:
                lines.append(f"- `{rel}`")

        lines.append("\n## 正向差异（TS 符号在 Rust 侧 local/moved/missing）\n")
        for rel, rrel in pairs:
            rkey = rrel[:-3] if rrel.endswith(".rs") else rrel
            top, methods = rbyfile.get(rkey, ({}, {}))
            local_syms = {}
            local_syms.update(top); local_syms.update(methods)
            local_keys = {compact(k) for k in local_syms}
            # global
            global_keys = set()
            for relk, (t, m) in rbyfile.items():
                for k in list(t) + list(m):
                    global_keys.add(compact(k))
            ts = ts_symbols(os.path.join(pkg_src, rel))
            moved, missing = [], []
            for name, kind in sorted(ts.items()):
                ck = compact(name)
                if len(ck) < 2: continue
                if ck in local_keys: continue
                if ck in global_keys: moved.append(name)
                else: missing.append(f"{name}({kind})")
            if moved or missing:
                lines.append(f"**`{rel}`**")
                if moved: lines.append(f"  - moved: {', '.join(moved)}")
                if missing: lines.append(f"  - missing: {', '.join(missing)}")

        lines.append("\n## 反向候选（Rust pub 符号在范围内 TS 未见同名，含噪音）\n")
        for rel in sorted(rindex):
            top, methods = rust_symbols(rindex[rel])
            extras = []
            for name in sorted(set(top) | set(methods)):
                c = compact(name)
                if len(c) < 2: continue
                if c in IGNORE_RUST: continue
                cam = snake_to_camel(name)
                if compact(cam) in ts_keys or c in ts_keys: continue
                extras.append(name)
            if extras:
                lines.append(f"- `{rel}`: {', '.join(extras)}")

        out = os.path.join(OUTDIR, f"audit-{pkg}.txt")
        with open(out, "w", encoding="utf-8") as fh:
            fh.write("\n".join(lines) + "\n")
        print(f"wrote {out}: pairs={len(pairs)} unpaired={len(unpaired)}")

if __name__ == "__main__":
    main()
