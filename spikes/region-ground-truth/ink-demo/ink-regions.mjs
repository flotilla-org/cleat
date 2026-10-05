// SPIKE (cleat#319): emit Ink's Yoga layout as a draft OSC 7701 snapshot.
//
// installInkRegions(stdout) wraps the Ink instance's log-update function so
// every frame it writes starts with the region records. The records sit at
// the start of the frame text, after log-update has erased the previous
// frame, so the cursor is at the frame's top-left: `o=c` makes y relative to
// that row. The OSCs are zero-width and contain no newline, so Ink's line
// accounting is unchanged. Records are deterministic (no frame counter), so
// unchanged frames are still skipped by log-update's diff.
//
// Ink does not export its instance map, so this imports the file directly;
// that resolves to the same module Ink itself uses.
import instances from './node_modules/ink/build/instances.js';

const esc = (v) => String(v).replace(/[;\\=\x00-\x1f\x7f-￿]/g, (c) => `\\x${c.charCodeAt(0).toString(16).padStart(2, '0')}`);

function collect(node, ox, oy, parentId, ordinals, out) {
	for (const child of node.childNodes ?? []) {
		const y = child.yogaNode;
		if (!y || child.nodeName === '#text') continue;
		const x0 = ox + y.getComputedLeft();
		const y0 = oy + y.getComputedTop();
		const role = child.internal_accessibility?.role;
		const kind = role ?? child.nodeName.replace(/^ink-/, '');
		const key = `${kind}`;
		const n = (ordinals[key] = (ordinals[key] ?? -1) + 1);
		const name = child.attributes?.['data-region'] ?? child.style?.regionName;
		const id = `${parentId ? `${parentId}/` : ''}${name ?? `${kind}.${n}`}`;
		out.push({ id, parent: parentId, kind, name, x: Math.round(x0), y: Math.round(y0), w: Math.round(y.getComputedWidth()), h: Math.round(y.getComputedHeight()) });
		collect(child, x0, y0, id, {}, out);
	}
}

export function regionSnapshot(rootNode) {
	const out = [];
	collect(rootNode, 0, 0, undefined, {}, out);
	let s = '\x1b]7701;B;o=c\x1b\\';
	for (const r of out) {
		s += `\x1b]7701;R;id=${esc(r.id)}`;
		if (r.parent) s += `;parent=${esc(r.parent)}`;
		s += `;kind=${esc(r.kind)}`;
		if (r.name) s += `;name=${esc(r.name)}`;
		s += `;x=${r.x};y=${r.y};w=${r.w};h=${r.h}\x1b\\`;
	}
	return `${s}\x1b]7701;E\x1b\\`;
}

export function installInkRegions(stdout = process.stdout) {
	const ink = instances.get(stdout);
	if (!ink) throw new Error('no Ink instance for this stdout; call after render()');
	const original = ink.log;
	const wrapped = (text) => original(text === '' ? text : regionSnapshot(ink.rootNode) + text);
	Object.assign(wrapped, original); // clear, done, sync, willRender, setCursorPosition, ...
	wrapped.willRender = (text) => original.willRender(text === '' ? text : regionSnapshot(ink.rootNode) + text);
	ink.log = wrapped;
}
