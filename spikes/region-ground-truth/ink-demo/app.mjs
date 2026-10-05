// Plain Ink app (no JSX build step). The only region-specific code is the
// installInkRegions() call and the optional `regionName` props.
// Renders inline (main screen, like Claude Code) and exits after ~3 s.
import React, { useEffect, useState } from 'react';
import { Box, Text, render } from 'ink';
import { installInkRegions } from './ink-regions.mjs';

const h = React.createElement;
const items = ['alpha.rs', 'beta.rs', 'gamma.rs', 'delta.rs'];

function App() {
	const [tick, setTick] = useState(0);
	useEffect(() => {
		const t = setInterval(() => setTick((n) => n + 1), 250);
		return () => clearInterval(t);
	}, []);
	useEffect(() => {
		if (tick >= 12) process.exit(0);
	}, [tick]);
	const sel = tick % items.length;
	return h(
		Box,
		{ regionName: 'app', flexDirection: 'column', borderStyle: 'round', width: 80 },
		h(Text, null, ' ink region demo'),
		h(
			Box,
			{ flexDirection: 'row' },
			h(
				Box,
				{ regionName: 'files', 'aria-role': 'list', flexDirection: 'column', borderStyle: 'single', width: 30 },
				...items.map((it, i) =>
					h(Box, { key: it, 'aria-role': 'listitem', 'aria-state': { selected: i === sel } }, h(Text, { inverse: i === sel }, `${i === sel ? '> ' : '  '}${it}`)),
				),
			),
			h(
				Box,
				{ regionName: 'preview', flexDirection: 'column', borderStyle: 'single', flexGrow: 1 },
				h(Text, null, `selected: ${items[sel]}`),
				h(Box, { regionName: 'details', borderStyle: 'single' }, h(Text, null, `tick ${tick}, nested two deep`)),
			),
		),
		h(Box, { regionName: 'composer', 'aria-role': 'textbox', borderStyle: 'round', marginX: 4 }, h(Text, null, `> type here${'_'.repeat(tick % 5)}`)),
		tick >= 4 && tick < 9
			? h(Box, { regionName: 'confirm', 'aria-role': 'dialog', position: 'absolute', top: 3, left: 20, borderStyle: 'double', width: 30, height: 4 }, h(Text, null, 'Overwrite file? [y/N]'))
			: null,
	);
}

console.log('scrollback above the Ink frame');
render(h(App));
installInkRegions(process.stdout);
