/**
 * harness/output.ts 的对照验证数据生成器。
 *
 * 用 Node（>= 22.6，`--experimental-strip-types`）直接加载**上游**的 `output.ts`，
 * 对一批固定用例（手写边界 + 确定性伪随机）计算真实结果，写出：
 *
 *   - `output-cases.json`    用例（Rust 侧据此重放）
 *   - `output-expected.json` 上游实现的结果（Rust 侧据此断言）
 *
 * 重跑：
 *
 *   node --experimental-strip-types tools.d/parity/output-parity.mjs
 *
 * 依赖只落在 tools.d 与 upstream（后者不入库），crate 本身不依赖 Node。
 */
import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

import {
	OutputBuffer,
	boundOutput,
	characterEnd,
	sanitizeOutput,
} from "../../upstream/packages/durable/src/harness/output.ts";

const here = dirname(fileURLToPath(import.meta.url));

const cases = [];
const expected = [];

function record(useCase, compute) {
	cases.push(useCase);
	try {
		expected.push({ name: useCase.name, ok: true, value: compute() });
	} catch (error) {
		expected.push({ name: useCase.name, ok: false, error: String(error?.message ?? error) });
	}
}

function limits(maxBytes, maxLines, retain) {
	return { maxBytes, maxLines, retain };
}

// ─── sanitizeOutput ──────────────────────────────────────────────────────────

for (const [name, text] of [
	["sanitize/plain", "hello\nworld"],
	["sanitize/tabs-and-newlines", "a\tb\nc\rd"],
	["sanitize/control-run", "a\u0000b\u0001c\u0008d"],
	["sanitize/keeps-0x09-0x0a", "x\u0009y\u000az"],
	["sanitize/drops-0x0b-0x1f", "p\u000bq\u001fr"],
	["sanitize/interlinear", "s\uFFF9t\uFFFBu"],
	["sanitize/keeps-fffc", "v\uFFFCw"],
	["sanitize/multibyte", "你好\n世界"],
]) {
	record({ name, kind: "sanitize", text }, () => ({ text: sanitizeOutput(text) }));
}

// ─── boundOutput ─────────────────────────────────────────────────────────────

const boundTexts = [
	"",
	"a",
	"one\ntwo\nthree\n",
	"one\ntwo\nthree",
	"\n\n\n",
	"é\néé\nééé\n",
	"中文输出\n第二行\n第三行更长一些\n",
	"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
];

for (const [index, text] of boundTexts.entries()) {
	for (const retain of ["head", "tail"]) {
		for (const [maxBytes, maxLines] of [
			[0, 0],
			[0, 5],
			[1024, 0],
			[1, 1],
			[4, 2],
			[8, 2],
			[10, 1],
			[7, 3],
			[1024, 2],
			[3, 10],
		]) {
			const name = `bound/${index}/${retain}/${maxBytes}x${maxLines}`;
			const spec = limits(maxBytes, maxLines, retain);
			record({ name, kind: "bound", text, limits: spec }, () => {
				const slice = boundOutput(text, spec);
				return {
					text: slice.text,
					bytes: slice.bytes,
					droppedBytes: slice.droppedBytes,
					droppedLines: slice.droppedLines,
				};
			});
		}
	}
}

// ─── characterEnd ────────────────────────────────────────────────────────────

for (const [name, text, index] of [
	["charEnd/ascii", "abc", 2],
	["charEnd/at-length", "abc", 3],
	["charEnd/inside-two-byte", "é", 1],
	["charEnd/before-two-byte", "é", 0],
	["charEnd/mixed", "aé b", 3],
	["charEnd/empty", "", 0],
]) {
	const bytes = Array.from(new TextEncoder().encode(text));
	record({ name, kind: "characterEnd", bytes, index }, () => ({
		index: characterEnd(Uint8Array.from(bytes), index),
	}));
}

// ─── OutputBuffer ────────────────────────────────────────────────────────────

function bufferCase(name, spec, chunks) {
	record({ name, kind: "buffer", limits: spec, chunks }, () => {
		const buffer = new OutputBuffer(spec);
		const pushes = [];
		for (const chunk of chunks) {
			if (chunk.kind === "flush") {
				buffer.end();
				pushes.push(true);
				continue;
			}
			const payload =
				chunk.kind === "text" ? chunk.text : Uint8Array.from(chunk.bytes ?? []);
			pushes.push(buffer.push(payload, chunk.skip ?? undefined));
		}
		const snapshot = buffer.snapshot();
		return {
			pushes,
			storedBytes: buffer.storedBytes,
			text: snapshot.text,
			droppedBytes: snapshot.droppedBytes,
			droppedLines: snapshot.droppedLines,
		};
	});
}

bufferCase("buf/tail-lines", limits(1024, 2, "tail"), [
	{ kind: "text", text: "one\n" },
	{ kind: "text", text: "two\n" },
	{ kind: "text", text: "three\n" },
]);

bufferCase("buf/head-lines", limits(8, 2, "head"), [
	{ kind: "text", text: "one\n" },
	{ kind: "text", text: "two\n" },
	{ kind: "text", text: "three\n" },
]);

bufferCase("buf/split-multibyte", limits(64, 10, "tail"), [
	{ kind: "bytes", bytes: Array.from(new TextEncoder().encode("aé")) },
]);

bufferCase("buf/byte-by-byte", limits(64, 10, "tail"), [
	...[..."aé中"].flatMap((character) =>
		Array.from(new TextEncoder().encode(character)).map((byte) => ({
			kind: "bytes",
			bytes: [byte],
		})),
	),
	{ kind: "flush" },
]);

bufferCase("buf/trailing-incomplete", limits(64, 10, "tail"), [
	{ kind: "bytes", bytes: [0xe2, 0x82] },
	{ kind: "flush" },
]);

bufferCase("buf/bom-leading", limits(64, 10, "tail"), [
	{ kind: "bytes", bytes: [0xef, 0xbb, 0xbf, 0x6f, 0x6b] },
]);

bufferCase("buf/bom-later-is-text", limits(64, 10, "tail"), [
	{ kind: "text", text: "ok" },
	{ kind: "bytes", bytes: [0xef, 0xbb, 0xbf] },
]);

bufferCase("buf/tail-skip", limits(64, 10, "tail"), [
	{ kind: "text", text: "head\n" },
	{
		kind: "text",
		text: "tail\n",
		skip: { bytes: 5, newlines: 1, endsWithNewline: true },
	},
]);

bufferCase("buf/tail-skip-zero", limits(64, 10, "tail"), [
	{ kind: "text", text: "keep\n" },
	{ kind: "text", text: "more\n", skip: { bytes: 0, newlines: 0, endsWithNewline: false } },
]);

bufferCase("buf/head-rejects-skip", limits(64, 10, "head"), [
	{ kind: "text", text: "tail\n", skip: { bytes: 5, newlines: 1, endsWithNewline: true } },
]);

bufferCase("buf/sanitized-snapshot", limits(64, 10, "tail"), [
	{ kind: "text", text: "a\u0000b\n" },
]);

bufferCase("buf/snapshots-are-repeatable", limits(8, 2, "tail"), [
	{ kind: "text", text: "one\n" },
	{ kind: "text", text: "two\n" },
	{ kind: "text", text: "three\n" },
	{ kind: "text", text: "four\n" },
]);

// ─── 确定性伪随机用例 ────────────────────────────────────────────────────────

function makeRandom(seed) {
	let state = seed >>> 0;
	return () => {
		state = (state * 1664525 + 1013904223) >>> 0;
		return state / 0x1_0000_0000;
	};
}

const alphabet = [
	"a",
	"b",
	"\n",
	" ",
	"é",
	"中",
	"\u0000",
	"\u001f",
	"\uFFFD",
	"\uFFF9",
	"x".repeat(7),
	"\t",
];

const random = makeRandom(0x5eed);
for (let index = 0; index < 24; index++) {
	const length = 1 + Math.floor(random() * 60);
	let text = "";
	for (let step = 0; step < length; step++) {
		text += alphabet[Math.floor(random() * alphabet.length)];
	}
	const maxBytes = Math.floor(random() * 24);
	const maxLines = 1 + Math.floor(random() * 5);
	const retain = random() < 0.5 ? "head" : "tail";
	const spec = limits(maxBytes, maxLines, retain);
	record({ name: `random/bound/${index}`, kind: "bound", text, limits: spec }, () => {
		const slice = boundOutput(text, spec);
		return {
			text: slice.text,
			bytes: slice.bytes,
			droppedBytes: slice.droppedBytes,
			droppedLines: slice.droppedLines,
		};
	});

	const chunks = [];
	const chunkCount = 1 + Math.floor(random() * 5);
	for (let step = 0; step < chunkCount; step++) {
		if (random() < 0.5) {
			chunks.push({ kind: "text", text: text.slice(0, 1 + Math.floor(random() * 20)) });
		} else {
			const bytes = Array.from(new TextEncoder().encode(text));
			const from = Math.floor(random() * Math.max(1, bytes.length));
			chunks.push({ kind: "bytes", bytes: bytes.slice(from, from + 1 + Math.floor(random() * 10)) });
		}
	}
	if (random() < 0.5) chunks.push({ kind: "text", text });
	if (random() < 0.5) chunks.push({ kind: "flush" });
	const bufferSpec = limits(Math.floor(random() * 20), 1 + Math.floor(random() * 4), retain);
	record({ name: `random/buffer/${index}`, kind: "buffer", limits: bufferSpec, chunks }, () => {
		const buffer = new OutputBuffer(bufferSpec);
		const pushes = [];
		for (const chunk of chunks) {
			if (chunk.kind === "flush") {
				buffer.end();
				pushes.push(true);
				continue;
			}
			const payload = chunk.kind === "text" ? chunk.text : Uint8Array.from(chunk.bytes ?? []);
			pushes.push(buffer.push(payload, chunk.skip ?? undefined));
		}
		const snapshot = buffer.snapshot();
		return {
			pushes,
			storedBytes: buffer.storedBytes,
			text: snapshot.text,
			droppedBytes: snapshot.droppedBytes,
			droppedLines: snapshot.droppedLines,
		};
	});
}

writeFileSync(join(here, "output-cases.json"), `${JSON.stringify(cases, null, "\t")}\n`);
writeFileSync(join(here, "output-expected.json"), `${JSON.stringify(expected, null, "\t")}\n`);
console.log(`wrote ${cases.length} cases (${expected.filter((entry) => !entry.ok).length} throwing)`);
