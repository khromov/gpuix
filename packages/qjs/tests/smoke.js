// `gpuix-qjs tests/smoke.js`: the host's JS surface without a window. Exits 7 on
// success, so a wrapper can tell a clean run from a crash that happened to print.
import { GpuixRenderer, isTerminal, hasTestGpuixRenderer } from '@gpuix/native';
import { helper } from './helper.js';

const checks = [];
const check = (label, ok) => {
	checks.push(ok);
	console.log(`${ok ? 'ok  ' : 'FAIL'} ${label}`);
};

check('native module exports', typeof GpuixRenderer === 'function' && isTerminal() === false && hasTestGpuixRenderer() === false);
check('relative import', helper() === 'helper-ok');
check('process', ['darwin', 'linux', 'win32'].includes(process.platform) && typeof process.versions.quickjs === 'string' && typeof process.env === 'object');
check('globals', [queueMicrotask, performance.now, structuredClone, setTimeout, setInterval, clearTimeout].every((f) => typeof f === 'function'));

const r = new GpuixRenderer((err, event) => console.log('event', err, event));
check('requiresTick before init', r.requiresTick() === true && r.isInitialized() === false);
let threw = '';
try {
	r.tick();
} catch (e) {
	threw = e.message;
}
check('tick before init throws the napi message', threw.includes('not initialized'));

let ticks = 0;
const id = setInterval(() => ticks++, 1);
setTimeout(() => {
	clearInterval(id);
	check('interval ran', ticks > 0);
	Promise.resolve().then(() => check('microtask after a timer callback', true));
	queueMicrotask(() => {
		const ok = checks.every(Boolean);
		console.log(ok ? 'smoke ok' : 'smoke FAILED');
		process.exit(ok ? 7 : 1);
	});
}, 30);
