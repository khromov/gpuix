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

const url = new URL('https://user:pw@news.ycombinator.com:8443/item?id=1&x=y#top');
check('URL parts', url.hostname === 'news.ycombinator.com' && url.port === '8443' && url.pathname === '/item' && url.search === '?id=1&x=y' && url.hash === '#top' && url.origin === 'https://news.ycombinator.com:8443');
check('URL relative + searchParams', new URL('../b?q=1', 'https://a.test/x/y/z').href === 'https://a.test/x/b?q=1' && url.searchParams.get('x') === 'y' && URL.canParse('nope') === false);
const bytes = new TextEncoder().encode('héllo €');
check('TextEncoder/TextDecoder round trip', bytes.length === 10 && new TextDecoder().decode(bytes) === 'héllo €');
const headers = new Headers({ 'Content-Type': 'text/plain' });
headers.append('x-a', '1');
headers.append('X-A', '2');
check('Headers are case-insensitive and append joins', headers.get('content-type') === 'text/plain' && headers.get('x-a') === '1, 2' && [...headers].length === 2);
check('Response.json', await Response.json({ a: 1 }).json().then((v) => v.a === 1) && new Response('x', { status: 404 }).ok === false);
check('fetch exists and rejects a bad scheme', typeof fetch === 'function' && (await fetch('nope://x').then(() => false, (e) => e instanceof Error)));

let threwOnUndefined = false;
try {
	clearTimeout(undefined);
	clearTimeout(null);
	clearInterval(undefined);
	setTimeout(() => {}, undefined);
	setTimeout(() => {}, Infinity);
} catch (e) {
	threwOnUndefined = true;
}
check('timers accept undefined/null/Infinity like Node', !threwOnUndefined);
check('console has the methods Svelte dev tracing calls', ['group', 'groupCollapsed', 'groupEnd', 'table', 'assert', 'time', 'timeEnd', 'count', 'dir'].every((m) => typeof console[m] === 'function'));
check('URL tolerates a malformed escape in the query', new URL('https://x.test/?q=100%').searchParams.get('q') === '100%');
const u = new URL('https://x.test/p?a=1');
u.searchParams.set('b', 'two words');
check('searchParams writes back into href with + for spaces', u.href === 'https://x.test/p?a=1&b=two+words' && u.search === '?a=1&b=two+words');
check('TextDecoder strips a BOM', new TextDecoder().decode(new Uint8Array([0xef, 0xbb, 0xbf, 0x68, 0x69])) === 'hi');
check('Response.json keeps extra headers', Response.json({}, { headers: { 'x-a': '1' } }).headers.get('content-type') === 'application/json' && Response.json({}, { headers: { 'x-a': '1' } }).headers.get('x-a') === '1');
check('fetch rejects a non-http scheme up front', await fetch('mailto:a@b.c').then(() => false, (e) => e instanceof TypeError));

// Two timers due in the same pass, the earlier one cancelling the later: it must not run.
let secondRan = false;
await new Promise((resolve) => {
	let second;
	setTimeout(() => clearTimeout(second), 5);
	second = setTimeout(() => (secondRan = true), 5);
	setTimeout(resolve, 40);
});
check('a timer cleared by an earlier callback in the same pass does not run', !secondRan);
let intervalRuns = 0;
await new Promise((resolve) => {
	let interval;
	setTimeout(() => clearInterval(interval), 5);
	interval = setInterval(() => intervalRuns++, 5);
	setTimeout(resolve, 60);
});
check('an interval cleared in the same pass as its due time stays cleared', intervalRuns === 0);

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
