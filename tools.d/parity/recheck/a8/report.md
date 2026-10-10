# A8 · durable tools + testing 复刻审计报告

- 基线：`earendil-works/pi` v1.1.0 @ `abe508e1b`（storage-conformance 审计子代理已核对 `upstream/.git/HEAD` 与 `package.json`）。
- 对比对象：`upstream/packages/durable/src/{tools,testing}` ↔ `crates/pi-durable/src/{tools,testing}`。
- 方法：逐文件枚举导出符号 + 实现导出行为的私有函数，逐方法比对参数/返回/控制流/分支/边界/错误处理/默认值/常量/字符串字面量；父代理亲自审计 edit、edit-diff 两对并复核了 bash/path-utils/mod.rs/env_conformance/storage_conformance 的关键结论，其余对由 10 个只读子代理并行审计（agent_id 见各节）。
- 结论：**未达 1:1**。工具实现层整体高保真但有若干小差异（超时文案、AMPM 变体、截断字段、溢出边界等）；testing 层是真实缺口：env-conformance 仅 10/24 case、storage-conformance 仅 7/24 case，且 5+5 个已移植 case 被削弱；集成测试对 10 个工具零端到端覆盖。

---

## 1. tools/bash.ts ↔ tools/bash.rs（子代理 agent_880198a2，父代理已亲验 bash.rs 全文）

| TS 符号 | Rust | 判定 |
|---|---|---|
| validateTimeout / MAX_TIMEOUT_SECONDS | validate_timeout / 2147483.647 | 一致（错误串逐字节一致） |
| prepareExecution | prepare_execution | **缺失**：无 `prepare` 钩子调用 |
| BashExecution{command,cwd,env,inheritEnv} | BashExecution{command,cwd} | **缺失**：`env`/`inheritEnv` 字段随 `prepare` 一起丢失 |
| BashToolOptions{commandPrefix,prepare} | BashToolOptions{command_prefix} | **缺失**：`prepare` 字段缺失 |
| PowerShellToolOptions{programs} | 复用 BashToolOptions，程序列表硬编码 | **缺失**：`programs` 选项不可配置（默认值 ["pwsh","powershell"] 一致） |
| runCommand | run_command | **逻辑差异**：超时消息 `format!("Command timed out after {timeout:?} seconds")` 输出 `Some(30.0)`，TS 为 `Command timed out after 30 seconds`（bash.rs:150-152，父代理亲验）；其余分支（spawn_error 循环、spill 诊断 info/full_output、aborted/timeout/exitCode 顺序）一致 |
| powershellSchema | 与 bash 共用 SCHEMA | **逻辑差异**：command 描述为 "Bash command to execute"，TS 为 "PowerShell command to execute"（父代理亲验） |
| — | `let is_last=false; let _=is_last;` | **多余**：死代码（bash.rs:121,126，父代理亲验） |
| UTF8_OUTPUT / POWERSHELL_ARGS / 描述串 / outputLimits tail | 同 | 一致（描述串由常量插值改为硬编码 2000/50KB，当前字节一致、常量变动时会漂移） |

计：缺失 3（prepare、env/inheritEnv、programs）、多余 1、逻辑差异 2。

## 2. tools/edit-diff.ts ↔ tools/edit_diff.rs（父代理亲自审计全文）

| TS 符号 | Rust | 判定 |
|---|---|---|
| detectLineEnding / normalizeToLF / restoreLineEndings | 同 | 一致（含 crlf<lf 判定） |
| normalizeForFuzzyMatch | 同 | 一致，除 **逻辑差异**：`trimEnd` 空白集不同——JS 裁剪 U+FEFF，Rust 裁剪 U+0085（NEL），异形空白边界输入下结果不同（极边缘） |
| splitLinesWithEndings / getLineSpans / applyReplacements | 同 | 一致（反向替换保持偏移；UTF-16 vs 字节索引各自自洽） |
| getReplacementLineRange | 同 | 豁免：TS throw ↔ Rust panic（消息一致，路径实际不可达） |
| applyReplacementsPreservingUnchangedLines | 同 | 一致（含行数校验、group 合并 `startLine<endLine` 逻辑） |
| fuzzyFindText | fuzzy_find_text | 一致（not-found 时 TS index=-1 vs Rust 0，调用方不用该值 → 豁免） |
| stripBom / countOccurrences | 同 | 一致 |
| 4 个错误文案 helper（notFound/duplicate/emptyOldText/noChange，8 条字符串） | 同 | 一致（逐字节相同） |
| applyEditsToNormalizedContent | 同 | 一致（LF 归一、空 oldText 检查顺序、fuzzy 空间判定、出现次数>1 报错、按 matchIndex 排序、重叠报错、无变更报错、保底行应用 vs 直接应用） |
| generateUnifiedPatch | generate_unified_patch | 豁免：jsdiff createTwoFilesPatch(FILE_HEADERS_ONLY) ↔ similar::unified_diff().header(path,path)；头格式（---/+++）匹配，均为 Myers 行 diff；残余风险：歧义输入的 tie-breaking 可能产生不同 hunk 边界 |
| generateDiffString | generate_diff_string | 一致（逐分支核对：+/- 前缀、行号补齐宽度、` ...` 跳过标记、leading/trailing 上下文裁剪、firstChangedLine 在删除首块时的语义）；库替换为豁免（同上 tie-break 风险） |

计：缺失 0、多余 0、逻辑差异 1（trimEnd 空白集）、豁免 4。

## 3. tools/edit.ts ↔ tools/edit.rs（父代理亲自审计全文）

| TS 符号 | Rust | 判定 |
|---|---|---|
| prepareEditArguments | prepare_edit_arguments | **一致**（重点核对：edits 为 JSON 字符串→解析为数组或单对象包装、解析失败保持原样；edits 为单对象→包装；legacy 顶层 oldText/newText→并入既有 edits 并删除顶层字段；非对象输入透传。所有分支/顺序/边界与 TS 相同） |
| validateEditInput | validate_edit_input | 一致，但 **逻辑差异（边缘）**：TS 透传原始值（非字符串 oldText 会在后续 normalizeToLF 抛 TypeError），Rust `as_str().unwrap_or_default()` 强转 ""（path 同理）→ 畸形输入下错误文案不同；schema 门控下不可达，低危 |
| editAccessError | 同 | 一致（消息逐字节相同；TS `{cause:error}` 为 JS 机制 → 豁免） |
| createEditTool.execute | 同 | 一致（fileInfo→kind 检查→readTextFile→BOM/行尾/归一→applyEdits→写回→diff/patch/details；"Operation aborted" 检查点位置一致；成功消息 `Successfully replaced N block(s) in {path}.` 一致） |
| editSchema replaceEdit 的 oldText/newText 描述 | Rust JSON schema 无这两条 description | 豁免（TypeBox→JSON schema；结构字段 type/required 一致，仅缺描述文案） |

计：缺失 0、多余 0、逻辑差异 1（畸形输入强转，低危）、豁免 2。

## 4. tools/env.ts ↔ tools/env.rs（子代理 agent_f885c8c9，父代理亲验 TS 全文）

requireEnv ↔ require_env：一致（`"No execution environment is configured"` 逐字节相同；field↔accessor、throw↔Result 为豁免）。计：缺失 0、多余 0、逻辑差异 0、豁免 2。

## 5. tools/file-mutation-queue.ts ↔ tools/file_mutation_queue.rs（子代理 agent_b2fb904c）

- mutationKey（`{env.id}\0{canonical}` 键格式）、canonical 四分支（ok/not_supported/not_found+递归）、withFileMutationQueue 的原子占位、FIFO、错误只回传当前调用者、释放后不再持有 —— 全部一致。
- **逻辑差异**：TS 在 tail 结算后 `queues.delete(key)` 清理空闲条目；Rust `QUEUES` 静态 map 永不删除 → 每路径一个 Arc<Mutex> 进程级保留（内存随不同文件数无界增长；功能无影响，低危）。
- 豁免：throw FileError ↔ SessionError 包装（code 字符串经 serde snake_case 一致）。
- 计：缺失 0、多余 0、逻辑差异 1、豁免 1。

## 6. tools/image.ts ↔ tools/image.rs（子代理 agent_15318f97）

- 两文件均为纯 MIME 嗅探（TS 无 sharp/jimp，Rust 无 image crate）。PNG/APNG acTL-IDAT 遍历、JPEG 0xF7 排除、GIF87a/89a、RIFF/WEBP、BMP 校验、readUint16LE/32BE/32LE、所有魔数字符串 —— 一致。
- **逻辑差异**：is_bmp 的 `pixel_data_offset < 14 + dib_header_size` 在 Rust 为 u32 加法，crafted 输入（dib_header_size ≥ 0xFFFFFFF2）下 release 包装后最终收敛于 TS 结果、debug 构建 panic（溢出检查）；TS 为 double 精确算术无溢出。
- 计：缺失 0、多余 0（`#[cfg(test)]` 测试模块归豁免）、逻辑差异 1、豁免 1。

## 7. tools/index.ts ↔ tools/mod.rs（子代理 agent_fe27152f，父代理亲验两文件全文）

- CodingTools ↔ CODING_TOOLS：name "coding-tools"、顺序 [read, write, edit, bash]、bash 默认参数 —— 一致（LazyLock 为豁免）。
- **缺失**：`EditToolDetails`、`ReadToolDetails` 在子模块为 pub 但未从 mod.rs re-export（类型本体存在，仅导出路径缺失，低危）；TS 的 type-only 导出（BashToolInput/PowerShellToolInput/EditToolInput/ReadToolInput/WriteToolInput/BashExecution/BashPrepare/PowerShellToolOptions）在 Rust 无对应（*ToolInput 为 TypeBox 静态类型 → 豁免；BashExecution/BashPrepare/PowerShellToolOptions 的实质缺失已计入 bash 节）。
- **多余**：mod.rs `pub mod` 暴露 edit_diff/env/file_mutation_queue/image/path_utils 5 个模块，TS index 未导出这些辅助文件（逻辑本身是工具依赖，仅可见面扩大，低危）。
- 计：缺失 1（re-export 路径）、多余 1、逻辑差异 0、豁免 2。

## 8. tools/path-utils.ts ↔ tools/path_utils.rs（子代理 agent_77bd19ac，父代理亲验两文件全文）

- UNICODE_SPACES 码点集、`@` 剥离顺序（先空格后 @、仅剥一个）—— 一致。
- **逻辑差异 1**：TS `resolved.replace(/ (AM|PM)\./gi, …)` 大小写不敏感；Rust `AMPM_DOT = " (?<ampm>AM|PM)\\."` 无 `(?i)` → 小写 `am.`/`pm.` 变体不再生成（macOS 文件名变体场景下结果可不同；path_utils.rs:18-19，父代理亲验）。
- **逻辑差异 2**：TS `for (const variant of new Set(variants))` 去重后探测；Rust 直接遍历 5 个变体 → 纯 ASCII 路径重复调用 `env.exists` 至多 5 次（结果相同，对副作用 env 可观察，低危）。
- 豁免：getOrThrow throw ↔ Result；NFD（ICU vs unicode_normalization 的 Unicode 版本理论差异）。
- 计：缺失 0、多余 0、逻辑差异 2、豁免 2。

## 9. tools/read.ts ↔ tools/read.rs（子代理 agent_2a4c0ca4）

| 项目 | 判定 |
|---|---|
| READ_CHUNK 64KiB、readHead 循环（skipBom/start==0→3、DEFAULT_MAX_LINES 提前退出、bytes > MAX_BYTES+1）、重试 0..2 次（size 变大或 size+mtime 相同）、"{path} changed while it was read"、三种诊断 severity/code、图像分支 is_error、LineScanner/characterEnd/truncateHeadOf 一致性 | 一致 |
| **缺失**：`ReadToolDetails.truncation` 序列化缺 `maxLines`/`maxBytes`（TS 展开 TruncationResult 携带 2000/51200，Rust TruncationDetails 8 字段不含） | 缺失 1 |
| **逻辑差异 1**：小数/负数/超大 offset·limit 的展示算术——TS 浮点（"Line 1.5…sed -n '1.5p'"、"offset=3.5"、limit=-1→"N+1 more lines…offset=0"、offset=1e20 原样打印）vs Rust 强转/饱和（"Line 1"、"offset=3"、0、"9223372036854775807"）；选中内容/扫描索引两侧一致，仅提示文案不同 | 逻辑差异 |
| **逻辑差异 2**：首行分支解码 TS `new TextDecoder().decode` 剥 U+FEFF，Rust `from_utf8_lossy` 保留（offset>1 且首行以 BOM 开头的边界）；另有 WHATWG TextDecoder vs from_utf8_lossy 在畸形 UTF-8 上的 U+FFFD 计数差异可平移截断边界 | 逻辑差异 |
| **逻辑差异 3**：format_size tie 舍入——JS `toFixed(1)` 远离零（1.25→"1.3"），Rust `{:.1}` 向偶（→"1.2"），1280B 等 tie 值在诊断文案中不同 | 逻辑差异 |
| **逻辑差异 4**：resolveReadToolPath 的 AMPM 大小写 + Set 去重（同 path-utils 节 2 项，根因共用） | 逻辑差异 |
| **逻辑差异 5（潜在）**：description 硬编码 "2000 lines or 50KB"，TS 由 DEFAULT_MAX_LINES/DEFAULT_MAX_BYTES 插值（当前字节一致） | 逻辑差异（低危） |

计：缺失 1、多余 0、逻辑差异 5（其中 1 项与 path-utils 同根因，主计归属 path-utils）、豁免 2。

## 10. tools/write.ts ↔ tools/write.rs（子代理 agent_b2dd393c）

全部一致：schema 结构、description 串、路径解析、父目录创建、覆盖/截断语义、无追加换行、UTF-8、"Successfully wrote to {path}"、abort 前后检查、队列包装。豁免 4（type-only 导出；FileError 渲染差异在 session/env 层——Rust 显示 `"message: path"` vs TS 仅 message，超出本对范围，列风险；std::fs::write 不可中断 vs TS 带 signal 的写取消竞态；TypeBox additionalProperties）。计：缺失 0、多余 0、逻辑差异 0、豁免 4。

## 11. testing/env-conformance.ts ↔ testing/env_conformance.rs（子代理 agent_a8b9c6dc，父代理亲验 Rust 全文与 10 个 case 名）

- TS 24 case（22 无条件 + 2 symlink 门控）；Rust 10 case，文件头自述"核心子集"。
- **缺失 14 case**：#6 目录读取器 end/关闭后拒绝；#7 目录读取器拒绝缺失路径与文件；#8 跳过枚举期间删除的条目；#9-#16 全部 8 个 watch case（创建/祖先补齐/内容同建/父重命名/排除条目/递归重叠/目录替换/关闭即停）；#21 windowed exec 精确 tail 与跳过计数；#23/#24 两个 symlink case。配套 helper（watching/readAll/covers/abortedContext）也未移植。
- **逻辑差异 5**（已移植 case 被削弱）：#1 缺负 offset/非整 len/aborted 上下文/close 后 read+info 断言；#2 缺 per-range firstLineEnd/firstLineBytes、EOF scanLines、startLine==endLine→invalid；#4 缺 aborted-open 断言与 file.txt fixture（case 名也缩短）；#5 缺 sub=directory、a.txt={kind,size} 断言；#22 仅 timeout 半侧、缺 abort 半侧（case 名仍称 "distinguishes timeout from abort"）。
- 一致 5：#3、#17-#20。豁免：assertions/runner/types/index 合并（但 case 本体不豁免）。
- 计：缺失 14、多余 0、逻辑差异 5、一致 5、豁免 1。

## 12. testing/storage-benchmark.ts ↔ testing/storage_benchmark.rs（子代理 agent_08622d38）

全部一致：12 read + 3 write 场景、名称/批次(100)/载荷/期望值、seed 两函数、常量（REPLAY_TAILS [0,16,128,1024]、HISTORY_SEGMENT_LENGTH 128、FORK_DEPTH 8、ENTRIES_PER_FORK 32）、STORAGE_MEMORY_SCALES/TIMING_SCALE、记录数公式。仅结构偏差（replay-tail 场景列表顺序、哨兵值 -1↔u64::MAX、`Promise.all` mint→顺序 mint 后一次 commit）与 `#[cfg(test)]` 脚手架（豁免）。计：缺失 0、多余 0、逻辑差异 0、豁免 2。

## 13. testing/storage-conformance.ts ↔ testing/storage_conformance.rs（子代理 agent_de871a4e，父代理亲验 Rust case 名与 TS 抽样 case）

- TS 24 case（1663 行）；Rust 7 case（392 行，自述"核心子集"；父代理确认 Rust 7 个 case 名）。
- **缺失 18 case**：#4 detaches prototype-like JSON keys；#6 entry cursor 在新 commit 后继续；#7 conversations 分页游标；#8 conversations owner 边过滤；#9 deep fork history 扫描；#10 三表双向游标；#12 task owners/status 扫描；#13 submission 按 requestId 索引与整记录替换；#14 无输入生命周期提交；#15-#22 全部 8 个 document case（rewindable 重建/长尾流式/copy base 独立/version transition/逻辑地址索引/生命周期原子/create+retire 空寿命/失败回滚/字符串标识无损）；#23 全局 ID 命名空间耗尽。**Rust 对 document 生命周期（document/findDocument/scanDocuments/create/change/copy/retire）零覆盖。**
- **逻辑差异 5**（已移植 case 与 TS 期望不符或削弱）：#2 回滚验证用相同记录重提交（TS 用变更后的 runningTask/doneInput/transient 证明"变更值回滚"），kind 串 "user"→"pi.user" 等，错误消息断言降级为 is_err；#3 无提交后变异输入对象/变异返回记录的再读断言，submission 断言整体缺失；#5 无 head:10 marker、无 findLatestHeadMarker 断言；#11 无 scanTasks 分页/过滤（case 名仍称 "pages filtered task scans"），终态期望值不同（result 99→null、abortRequested true→false）；#24 closed-op 集合不同（缺 mintId/空 commit，多 entry 读取）。
- **多余 1**："scans entries in either order and pages by cursor"（TS 无对应；且自相矛盾——只有升序、无降序、无游标顺序拒绝）。
- 一致仅 #1。豁免：assertions/runner/types/index 合并（case 本体不豁免）。
- 计：缺失 18、多余 1、逻辑差异 5、一致 1、豁免 1。

## 14. 集成测试覆盖（子代理 agent_0aa8f53b）

`crates/pi-durable/tests/` 共 15 个文件、约 97 个测试函数。覆盖：env_conformance（10 case×NodeExecutionEnv）、storage_conformance（7 case×memory/jsonl/sqlite）、storage_benchmark（×3 后端）、memory/jsonl/sqlite 存储契约、session/harness/submissions/task_graph/views/documents/replicated_state、env_node、output_parity（对照上游 output.ts 的夹具测试）。

**工具级覆盖为 0/10**：bash、powershell、edit、edit-diff、env(require_env)、file-mutation-queue、image(工具本体)、path-utils(resolve)、read(工具本体)、write 均无任何集成测试。单元测试仅 path_utils.rs（1 个）与 image.rs（2 个）有 `#[cfg(test)]`；tools/{bash,edit,edit_diff,env,file_mutation_queue,read,write}.rs 零单元测试。上游明确要求的端到端行为（powershell 程序回退、fuzzy 编辑匹配、CRLF 恢复、edit BOM、read 截断/offset、队列串行化）全部未测；BOM 与行扫描仅在 env/decode.rs、env/line_scan.rs 单元层与 BinaryReader 一致性层有覆盖。唯一对上 parity 测试是 output_parity（非工具）。

---

## 汇总

### 缺失 6 项（其中 2 项为 case 级批量缺口，共 32 个 case；另有 1 项低危导出路径）
1. bash：`prepare` 钩子 + `BashExecution.env/inheritEnv` 字段未移植（bash.rs）
2. bash：PowerShell `programs` 自定义程序列表未移植（硬编码默认值）
3. read：`details.truncation` 序列化缺 `maxLines`/`maxBytes`
4. env-conformance：14 个 TS case 缺失（8 watch、3 dir-reader、windowed exec、2 symlink）
5. storage-conformance：18 个 TS case 缺失（含全部 8 个 document case、ID 耗尽、fork 历史等）
6. index：`EditToolDetails`/`ReadToolDetails` 未从 mod.rs re-export（类型存在，低危）

### 多余 3 项
1. index：mod.rs 额外 `pub` 暴露 5 个辅助模块（TS index 未导出；低危）
2. bash：run_command 遗留死代码 `is_last`（bash.rs:121,126）
3. storage-conformance：多余 case "scans entries in either order and pages by cursor"

### 逻辑差异 14 项
1. bash：超时文案泄漏 `Some(...)`（"Command timed out after Some(30.0) seconds"）
2. bash：powershell schema 复用 bash 描述（"Bash command to execute"）
3. path-utils：AMPM 变体正则丢大小写不敏感（`/ (AM|PM)\./gi` → 仅大写）
4. path-utils：变体去重 `new Set` 未移植（重复 exists() 调用）
5. read：小数/负/超大 offset·limit 的提示算术（1.5、offset=3.5、limit=-1、1e20）
6. read：首行 BOM 解码差异（from_utf8_lossy 保留 U+FEFF）+ 畸形 UTF-8 计数差异
7. read：format_size tie 舍入（toFixed 远离零 vs Rust 向偶，1.25→"1.3" vs "1.2"）
8. read：description 硬编码 2000/50KB（当前字节一致，常量漂移风险；bash 同款）
9. image：is_bmp u32 加法溢出（debug 构建 panic on crafted 输入）
10. fmq：QUEUES 无空闲清理（内存无界增长，功能无影响）
11. edit：畸形输入强转 ""（TS 透传 → 后续 TypeError；schema 门控下不可达）
12. edit-diff：trimEnd 空白集差异（JS 裁 U+FEFF / Rust 裁 U+0085）
13. env-conformance：5 个已移植 case 被削弱（断言子集缺失）
14. storage-conformance：5 个已移植 case 期望不符/被削弱（rollback/detach/head-marker/scanTasks/close 集合）

### 豁免（语言机制等价，主要类别）
TypeBox→JSON schema（仅结构字段比对）；testing 的 assertions/runner/types/index 合并进 Rust conformance（**case 本体不豁免**）；throw↔Result/SessionError；field↔accessor；jsdiff→similar 库替换（generateUnifiedPatch/generateDiffString，Myers 行 diff、头格式匹配，tie-break 残余风险）；ByteSource 形态；NDF/NFKC Unicode 版本理论差异；`#[cfg(test)]` 测试脚手架；Node fs/path 与 std 机制的等价替换（HOME 未设、file://host、`..` 折叠等 env/node 层差异见风险）。

### 风险（未计入上述计数）
- session/env 层 FileError 渲染 `"message: path"` vs TS 仅 message（write/read/edit 失败路径文案可观察差异，超出本 13 对范围）。
- env/node 层路径解析差异（HOME 未设时回退 "."、`file://host/path` 前缀剥离、`..` 不折叠、尾部斜杠保留）——由 path-utils 子代理在 env/node.{ts,rs} 中核实存在。
- diff 库替换的 hunk 边界 tie-break 残余风险；NFKC/NFD 与 ICU 的 Unicode 版本差异。
- read.rs 的 Rust harness 参数校验假设来自文档注释，未跑测试验证。

### 证据
- 父代理亲验：edit.ts/edit.rs/edit-diff.ts/edit_diff.rs 全文；bash.rs、path_utils.ts/.rs、index.ts、env.ts、mod.rs、env_conformance.rs 全文；storage_conformance.rs case 列表与部分正文；TS storage-conformance.ts 抽样；truncate.ts/.rs 全文。
- 子代理报告（agent_id）：bash 880198a2、env f885c8c9、fmq b2fb904c、image 15318f97、index fe27152f、path-utils 77bd19ac、read 2a4c0ca4、write b2dd393c、env-conformance a8b9c6dc、storage-benchmark 08622d38、storage-conformance de871a4e、tests 0aa8f53b。子代理结论按"不可盲信"原则对关键项逐一复核，复核结果与子代理一致（见各节"父代理亲验"标注）。
- 本任务只读审计：未修改 crates/ 与 upstream/ 下任何文件；bash/exec 在本共享 checkout 中被拒绝，全部证据来自文件读取。
