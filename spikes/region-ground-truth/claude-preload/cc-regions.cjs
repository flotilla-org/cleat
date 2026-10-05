// SPIKE (cleat#319): instrument the closed, Bun-compiled Claude Code binary
// without touching it. Load with:
//   BUN_OPTIONS="--preload /abs/path/cc-regions.cjs" claude
// (NODE_OPTIONS is ignored by the Bun binary; BUN_OPTIONS --preload works.)
//
// 1. Find Ink's root DOM node. Claude Code's forked Ink creates nodes as
//    plain objects ({nodeName, childNodes, parentNode, yogaNode,
//    cachedLayout, ...}) and appends with `parent.childNodes.push(child)`
//    after setting child.parentNode. A temporary Array.prototype.push trap
//    sees the first such push and walks parentNode up to the root, then
//    removes itself.
// 2. Wrap process.stdout.write. When a chunk ends a synchronized update
//    (ESC[?2026l), insert a region snapshot just before it, built from each
//    node's `cachedLayout` ({x, y, width, height}), which the renderer
//    assigns while painting that frame.
// Property names survive minification; minified identifiers are not used.
'use strict';
const fs = require('fs');
const LOG = process.env.CC_REGIONS_LOG; // optional debug log path
const log = (m) => LOG && fs.appendFileSync(LOG, `${m}\n`);

let root;
const origPush = Array.prototype.push;
function trap(...items) {
	const item = items[0];
	if (!root && item && typeof item === 'object' && typeof item.nodeName === 'string' && item.nodeName.startsWith('ink-') && item.parentNode) {
		let n = item;
		while (n.parentNode) n = n.parentNode;
		if (n.nodeName === 'ink-root') {
			root = n;
			Array.prototype.push = origPush;
			log(`root found via ${item.nodeName}`);
		}
	}
	return origPush.apply(this, items);
}
Array.prototype.push = trap;

const esc = (v) => String(v).replace(/[;\\=\x00-\x1f\x7f-￿]/g, (c) => `\\x${c.charCodeAt(0).toString(16).padStart(2, '0')}`);

function snapshot() {
	const out = [];
	// CC_REGIONS_SOURCE=cached uses the renderer's per-paint cachedLayout;
	// the default sums Yoga's computed offsets (as Ink's own helpers do).
	const useCached = process.env.CC_REGIONS_SOURCE === 'cached';
	const walk = (node, parentId, ox = 0, oy = 0) => {
		const counts = {};
		for (const child of node.childNodes ?? []) {
			if (child.nodeName === '#text') continue;
			const y = child.yogaNode;
			const yl = y && { x: ox + y.getComputedLeft(), y: oy + y.getComputedTop(), width: y.getComputedWidth(), height: y.getComputedHeight() };
			const l = useCached ? child.cachedLayout : yl;
			if (LOG && (child.scrollTop || child.style?.position === 'absolute' || child.style?.overflowY === 'scroll' || child.nodeName === 'ink-raw-ansi'))
				log(`special ${child.nodeName} pos=${child.style?.position} overflowY=${child.style?.overflowY} scrollTop=${child.scrollTop} cached=${JSON.stringify(child.cachedLayout)} yoga=${JSON.stringify(yl)}`);
			const kind = (child.accessibility?.role ?? child.nodeName).replace(/^ink-/, '');
			const n = (counts[kind] = (counts[kind] ?? -1) + 1);
			const id = `${parentId ? `${parentId}/` : ''}${kind}.${n}`;
			if (l && l.width > 0 && l.height > 0) {
				let rec = `\x1b]7701;R;id=${esc(id)}`;
				if (parentId) rec += `;parent=${esc(parentId)}`;
				rec += `;kind=${esc(kind)};x=${Math.floor(l.x)};y=${Math.floor(l.y)};w=${Math.floor(l.width)};h=${Math.floor(l.height)}\x1b\\`;
				out.push(rec);
			}
			walk(child, id, yl ? yl.x : ox, yl ? yl.y : oy);
		}
	};
	walk(root, undefined);
	return `\x1b]7701;B;o=a\x1b\\${out.join('')}\x1b]7701;E\x1b\\`;
}

const ESU = '\x1b[?2026l';
const origWrite = process.stdout.write.bind(process.stdout);
let frames = 0;
process.stdout.write = function (chunk, ...rest) {
	if (root && typeof chunk === 'string') {
		const i = chunk.lastIndexOf(ESU);
		if (i >= 0) {
			try {
				chunk = chunk.slice(0, i) + snapshot() + chunk.slice(i);
				frames++;
				if (frames <= 3) log(`frame ${frames} emitted`);
			} catch (e) {
				log(`snapshot failed: ${e}`);
			}
		}
	} else if (root && chunk instanceof Uint8Array && Buffer.from(chunk).includes(ESU)) {
		log('ESU in a binary chunk: not instrumented');
	}
	return origWrite(chunk, ...rest);
};
log(`preload active pid=${process.pid}`);
