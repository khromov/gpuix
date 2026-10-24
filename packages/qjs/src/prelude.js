// Installed by main.rs before the app loads. It turns the handful of natives the
// host sets on globalThis (`__gpuix_*`, removed again below) into the globals a
// GPUIX app expects of Node: `console`, `process`, UTF-8 text coding, `URL`, and
// `fetch` with `Headers`/`Request`/`Response` over the host's HTTP client. Bodies
// are buffered whole, there are no streams, and `AbortSignal` is accepted but ignored.
(() => {
	const native = {};
	for (const key of Object.keys(globalThis)) {
		if (key.startsWith('__gpuix_')) {
			native[key.slice('__gpuix_'.length)] = globalThis[key];
			delete globalThis[key];
		}
	}

	// ── console ────────────────────────────────────────────────────────
	const describe = (value, depth = 0) => {
		if (typeof value === 'string') return value;
		if (value instanceof Error) return value.stack ? `${value.name}: ${value.message}\n${value.stack}` : `${value.name}: ${value.message}`;
		if (value === null || typeof value !== 'object') return String(value);
		if (depth > 0 && typeof value.toJSON !== 'function' && !Array.isArray(value) && Object.getPrototypeOf(value) !== Object.prototype) {
			return String(value);
		}
		const seen = new WeakSet();
		try {
			return JSON.stringify(value, (key, inner) => {
				if (typeof inner === 'object' && inner !== null) {
					if (seen.has(inner)) return '[Circular]';
					seen.add(inner);
				}
				if (typeof inner === 'bigint') return `${inner}n`;
				if (typeof inner === 'function') return `[Function ${inner.name || 'anonymous'}]`;
				return inner;
			});
		} catch {
			return String(value);
		}
	};
	// Node's printf directives, which Svelte's dev warnings use (`%c` for styling).
	const format = (args) => {
		if (typeof args[0] !== 'string' || !/%[sdifoOjc%]/.test(args[0])) return args.map((value) => describe(value)).join(' ');
		const rest = args.slice(1);
		const text = args[0].replace(/%([sdifoOjc%])/g, (match, kind) => {
			if (kind === '%') return '%';
			if (!rest.length) return match;
			const value = rest.shift();
			if (kind === 'c') return '';
			if (kind === 'd' || kind === 'i') return String(Math.trunc(Number(value)));
			if (kind === 'f') return String(Number(value));
			return describe(value);
		});
		return [text, ...rest.map((value) => describe(value))].join(' ');
	};
	let indent = '';
	const emit = (fd) => (...args) => {
		const text = format(args);
		native.write(fd, (indent ? text.replace(/^/gm, indent) : text) + '\n');
	};
	const timers = new Map();
	const counters = new Map();
	const console = {
		log: emit(1),
		info: emit(1),
		debug: emit(1),
		warn: emit(2),
		error: emit(2),
		trace: (...args) => emit(2)('Trace:', ...args, '\n' + (new Error().stack ?? '')),
		dir: (value) => emit(1)(value),
		dirxml: (value) => emit(1)(value),
		table: (value) => emit(1)(value),
		assert: (condition, ...args) => {
			if (!condition) emit(2)('Assertion failed' + (args.length ? ':' : ''), ...args);
		},
		group: (...args) => {
			if (args.length) emit(1)(...args);
			indent += '  ';
		},
		groupCollapsed: (...args) => console.group(...args),
		groupEnd: () => {
			indent = indent.slice(2);
		},
		count: (label = 'default') => {
			const n = (counters.get(label) ?? 0) + 1;
			counters.set(label, n);
			emit(1)(`${label}: ${n}`);
		},
		countReset: (label = 'default') => counters.delete(label),
		time: (label = 'default') => timers.set(label, performance.now()),
		timeLog: (label = 'default', ...args) => {
			const start = timers.get(label);
			if (start !== undefined) emit(1)(`${label}: ${(performance.now() - start).toFixed(3)}ms`, ...args);
		},
		timeEnd: (label = 'default') => {
			console.timeLog(label);
			timers.delete(label);
		}
	};
	globalThis.console = console;

	// ── process ─────────────────────────────────────────────────────────
	const info = native.process;
	const stream = (fd) => ({
		write(chunk) {
			native.write(fd, String(chunk));
			return true;
		},
		isTTY: false
	});
	globalThis.process = {
		...info,
		exit: (code = 0) => native.exit(code | 0),
		cwd: () => info.cwd,
		stdout: stream(1),
		stderr: stream(2),
		on() {
			return this;
		},
		off() {
			return this;
		},
		once() {
			return this;
		},
		nextTick: (fn, ...args) => queueMicrotask(() => fn(...args))
	};
	// What the host adds beyond the web's globals, under one name an app can test for.
	globalThis.gpuixHost = Object.freeze({
		runtime: 'quickjs',
		openUrl: (url) => native.open_url(String(url))
	});
	if (typeof globalThis.queueMicrotask !== 'function') {
		globalThis.queueMicrotask = (fn) => {
			Promise.resolve().then(fn);
		};
	}
	if (typeof globalThis.structuredClone !== 'function') {
		globalThis.structuredClone = (value) => JSON.parse(JSON.stringify(value));
	}

	// ── TextEncoder / TextDecoder (UTF-8 only) ─────────────────────────
	const asBytes = (input) => {
		if (input instanceof Uint8Array) return input;
		if (input instanceof ArrayBuffer) return new Uint8Array(input);
		if (ArrayBuffer.isView(input)) return new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
		throw new TypeError('expected an ArrayBuffer or ArrayBufferView');
	};
	class TextEncoder {
		get encoding() {
			return 'utf-8';
		}
		encode(input = '') {
			return native.utf8_encode(String(input));
		}
	}
	class TextDecoder {
		#ignoreBOM;
		constructor(label = 'utf-8', options = {}) {
			const name = String(label).toLowerCase();
			if (name !== 'utf-8' && name !== 'utf8') throw new RangeError(`TextDecoder: only utf-8 is supported, not ${label}`);
			this.#ignoreBOM = Boolean(options.ignoreBOM);
		}
		get encoding() {
			return 'utf-8';
		}
		decode(input) {
			if (input == null) return '';
			let bytes = asBytes(input);
			if (!this.#ignoreBOM && bytes.length >= 3 && bytes[0] === 0xef && bytes[1] === 0xbb && bytes[2] === 0xbf) bytes = bytes.subarray(3);
			return native.utf8_decode(bytes);
		}
	}
	globalThis.TextEncoder = TextEncoder;
	globalThis.TextDecoder = TextDecoder;

	// ── URL / URLSearchParams ──────────────────────────────────────────
	// application/x-www-form-urlencoded: `+` is a space, and a bad escape is kept
	// rather than thrown, which is what `new URL('…?q=100%')` needs.
	const formDecode = (text) => {
		const spaced = text.replace(/\+/g, ' ');
		try {
			return decodeURIComponent(spaced);
		} catch {
			return spaced.replace(/%[0-9a-fA-F]{2}/g, (hex) => {
				try {
					return decodeURIComponent(hex);
				} catch {
					return hex;
				}
			});
		}
	};
	const formEncode = (text) => encodeURIComponent(text).replace(/%20/g, '+');

	class URLSearchParams {
		#list = [];
		#onchange = null;
		constructor(init = '') {
			if (typeof init === 'string') {
				for (const pair of init.replace(/^\?/, '').split('&')) {
					if (!pair) continue;
					const at = pair.indexOf('=');
					const key = at === -1 ? pair : pair.slice(0, at);
					const value = at === -1 ? '' : pair.slice(at + 1);
					this.#list.push([formDecode(key), formDecode(value)]);
				}
			} else if (init instanceof URLSearchParams) {
				this.#list = init.#list.map((pair) => [...pair]);
			} else if (init && typeof init[Symbol.iterator] === 'function') {
				this.#list = [...init].map(([key, value]) => [String(key), String(value)]);
			} else if (init && typeof init === 'object') {
				this.#list = Object.entries(init).map(([key, value]) => [key, String(value)]);
			}
		}
		// A URL's searchParams write back into its href.
		static _bind(params, onchange) {
			params.#onchange = onchange;
		}
		#changed() {
			this.#onchange?.(this.toString());
		}
		get(name) {
			const pair = this.#list.find(([key]) => key === name);
			return pair ? pair[1] : null;
		}
		getAll(name) {
			return this.#list.filter(([key]) => key === name).map(([, value]) => value);
		}
		has(name) {
			return this.#list.some(([key]) => key === name);
		}
		set(name, value) {
			this.#list = this.#list.filter(([key]) => key !== name);
			this.#list.push([String(name), String(value)]);
			this.#changed();
		}
		append(name, value) {
			this.#list.push([String(name), String(value)]);
			this.#changed();
		}
		delete(name) {
			this.#list = this.#list.filter(([key]) => key !== name);
			this.#changed();
		}
		sort() {
			this.#list.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
			this.#changed();
		}
		forEach(callback, thisArg) {
			for (const [key, value] of this.#list) callback.call(thisArg, value, key, this);
		}
		entries() {
			return this.#list.map((pair) => [...pair])[Symbol.iterator]();
		}
		keys() {
			return this.#list.map(([key]) => key)[Symbol.iterator]();
		}
		values() {
			return this.#list.map(([, value]) => value)[Symbol.iterator]();
		}
		[Symbol.iterator]() {
			return this.entries();
		}
		get size() {
			return this.#list.length;
		}
		toString() {
			return this.#list.map(([key, value]) => `${formEncode(key)}=${formEncode(value)}`).join('&');
		}
	}

	// The parsing is the `url` crate's (WHATWG). The parts are re-read from it
	// whenever `searchParams` changes; the other setters are not implemented.
	class URL {
		#parts;
		#params;
		constructor(url, base) {
			const parts = native.url_parse(String(url instanceof URL ? url.href : url), base === undefined ? undefined : String(base));
			if (!parts) throw new TypeError(`Invalid URL: ${url}`);
			this.#parts = parts;
			this.#params = new URLSearchParams(parts.search);
			URLSearchParams._bind(this.#params, (search) => {
				const reparsed = native.url_parse(`${parts.href.split('#')[0].split('?')[0]}${search ? `?${search}` : ''}${this.#parts.hash}`);
				if (reparsed) this.#parts = reparsed;
			});
		}
		static canParse(url, base) {
			return native.url_parse(String(url), base === undefined ? undefined : String(base)) != null;
		}
		static parse(url, base) {
			return URL.canParse(url, base) ? new URL(url, base) : null;
		}
		get href() {
			return this.#parts.href;
		}
		get origin() {
			return this.#parts.origin;
		}
		get protocol() {
			return this.#parts.protocol;
		}
		get username() {
			return this.#parts.username;
		}
		get password() {
			return this.#parts.password;
		}
		get host() {
			return this.#parts.host;
		}
		get hostname() {
			return this.#parts.hostname;
		}
		get port() {
			return this.#parts.port;
		}
		get pathname() {
			return this.#parts.pathname;
		}
		get search() {
			return this.#parts.search;
		}
		get hash() {
			return this.#parts.hash;
		}
		get searchParams() {
			return this.#params;
		}
		toString() {
			return this.#parts.href;
		}
		toJSON() {
			return this.#parts.href;
		}
	}
	globalThis.URL = URL;
	globalThis.URLSearchParams = URLSearchParams;

	// ── fetch ──────────────────────────────────────────────────────────
	class Headers {
		#map = new Map();
		constructor(init) {
			if (init instanceof Headers) {
				for (const [key, value] of init) this.append(key, value);
			} else if (init && typeof init[Symbol.iterator] === 'function') {
				for (const [key, value] of init) this.append(key, value);
			} else if (init && typeof init === 'object') {
				for (const [key, value] of Object.entries(init)) this.append(key, value);
			}
		}
		get(name) {
			return this.#map.get(String(name).toLowerCase()) ?? null;
		}
		has(name) {
			return this.#map.has(String(name).toLowerCase());
		}
		set(name, value) {
			this.#map.set(String(name).toLowerCase(), String(value));
		}
		append(name, value) {
			const key = String(name).toLowerCase();
			const old = this.#map.get(key);
			this.#map.set(key, old === undefined ? String(value) : `${old}, ${value}`);
		}
		delete(name) {
			this.#map.delete(String(name).toLowerCase());
		}
		forEach(callback, thisArg) {
			for (const [key, value] of this.#map) callback.call(thisArg, value, key, this);
		}
		entries() {
			return this.#map.entries();
		}
		keys() {
			return this.#map.keys();
		}
		values() {
			return this.#map.values();
		}
		[Symbol.iterator]() {
			return this.#map.entries();
		}
	}

	const toBytes = (body) => {
		if (body == null) return null;
		if (typeof body === 'string') return native.utf8_encode(body);
		if (body instanceof URLSearchParams) return native.utf8_encode(body.toString());
		return asBytes(body);
	};

	class Body {
		#bytes;
		#used = false;
		constructor(bytes) {
			this.#bytes = bytes;
		}
		get bodyUsed() {
			return this.#used;
		}
		_rawBytes() {
			return this.#bytes;
		}
		#take() {
			if (this.#used) return Promise.reject(new TypeError('body already used'));
			this.#used = true;
			return Promise.resolve(this.#bytes ?? new Uint8Array(0));
		}
		async bytes() {
			return this.#take();
		}
		async arrayBuffer() {
			const bytes = await this.#take();
			return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength);
		}
		async text() {
			return new TextDecoder().decode(await this.#take());
		}
		async json() {
			return JSON.parse(await this.text());
		}
	}

	class Request extends Body {
		constructor(input, init = {}) {
			const from = input instanceof Request ? input : null;
			const body = init.body !== undefined ? toBytes(init.body) : from ? from._rawBytes() : null;
			super(body);
			// `new URL` here gives the spec's TypeError for junk, and a normalised href.
			this.url = from && input === from ? from.url : new URL(input instanceof URL ? input.href : String(input)).href;
			this.method = String(init.method ?? from?.method ?? 'GET').toUpperCase();
			this.headers = new Headers(init.headers ?? from?.headers);
			this.signal = init.signal ?? from?.signal ?? null;
		}
	}

	class Response extends Body {
		#status;
		#statusText;
		#headers;
		#url;
		constructor(body = null, init = {}) {
			super(toBytes(body));
			this.#status = init.status ?? 200;
			this.#statusText = init.statusText ?? '';
			this.#headers = new Headers(init.headers);
			this.#url = init.url ?? '';
		}
		get status() {
			return this.#status;
		}
		get ok() {
			return this.#status >= 200 && this.#status < 300;
		}
		get statusText() {
			return this.#statusText;
		}
		get headers() {
			return this.#headers;
		}
		get url() {
			return this.#url;
		}
		get type() {
			return 'basic';
		}
		get redirected() {
			return false;
		}
		clone() {
			if (this.bodyUsed) throw new TypeError('body already used');
			return new Response(this._rawBytes(), { status: this.#status, statusText: this.#statusText, headers: this.#headers, url: this.#url });
		}
		static json(data, init = {}) {
			const headers = new Headers(init.headers);
			if (!headers.has('content-type')) headers.set('content-type', 'application/json');
			return new Response(JSON.stringify(data), { ...init, headers });
		}
	}

	async function fetch(input, init = {}) {
		const request = new Request(input, init);
		const result = await native.fetch(request.url, JSON.stringify({ method: request.method, headers: [...request.headers] }), request._rawBytes());
		return new Response(result.body, { status: result.status, statusText: result.statusText, headers: result.headers, url: result.url });
	}

	globalThis.Headers = Headers;
	globalThis.Request = Request;
	globalThis.Response = Response;
	globalThis.fetch = fetch;
})();
