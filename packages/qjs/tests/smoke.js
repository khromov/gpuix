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
