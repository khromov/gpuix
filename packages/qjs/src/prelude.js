// Installed by main.rs before the app loads. It turns the handful of natives the
// host sets on globalThis (`__gpuix_*`, removed again below) into the globals a
// GPUIX app expects of Node: `process`, UTF-8 text coding, `URL`, and `fetch`
// with `Headers`/`Request`/`Response` over the host's HTTP client. Bodies are
// buffered whole, there are no streams, and `AbortSignal` is accepted but ignored.
(() => {
	const native = {};
	for (const key of Object.keys(globalThis)) {
		if (key.startsWith('__gpuix_')) {
			native[key.slice('__gpuix_'.length)] = globalThis[key];
			delete globalThis[key];
		}
	}

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
	class TextEncoder {
		get encoding() {
			return 'utf-8';
		}
		encode(input = '') {
			return native.utf8_encode(String(input));
		}
	}
	class TextDecoder {
		constructor(label = 'utf-8') {
			const name = String(label).toLowerCase();
			if (name !== 'utf-8' && name !== 'utf8') throw new RangeError(`TextDecoder: only utf-8 is supported, not ${label}`);
		}
		get encoding() {
			return 'utf-8';
		}
		decode(input) {
			if (input == null) return '';
			const bytes = input instanceof Uint8Array ? input : input instanceof ArrayBuffer ? new Uint8Array(input) : new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
			return native.utf8_decode(bytes);
		}
	}
	globalThis.TextEncoder = TextEncoder;
	globalThis.TextDecoder = TextDecoder;

	// ── URL / URLSearchParams ──────────────────────────────────────────
	class URLSearchParams {
		#list = [];
		constructor(init = '') {
			if (typeof init === 'string') {
				for (const pair of init.replace(/^\?/, '').split('&')) {
					if (!pair) continue;
					const at = pair.indexOf('=');
					const key = at === -1 ? pair : pair.slice(0, at);
					const value = at === -1 ? '' : pair.slice(at + 1);
					this.#list.push([decodeURIComponent(key.replace(/\+/g, ' ')), decodeURIComponent(value.replace(/\+/g, ' '))]);
				}
			} else if (init instanceof URLSearchParams) {
				this.#list = init.#list.map((pair) => [...pair]);
			} else if (Array.isArray(init)) {
				this.#list = init.map(([key, value]) => [String(key), String(value)]);
			} else if (init && typeof init === 'object') {
				this.#list = Object.entries(init).map(([key, value]) => [key, String(value)]);
			}
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
			this.delete(name);
			this.#list.push([String(name), String(value)]);
		}
		append(name, value) {
			this.#list.push([String(name), String(value)]);
		}
		delete(name) {
			this.#list = this.#list.filter(([key]) => key !== name);
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
			return this.#list.map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(value)}`).join('&');
		}
	}

	// The parsing is the `url` crate's (WHATWG); the object is a read-only
	// snapshot of its parts, which is what URLs are used for in an app.
	class URL {
		#parts;
		#params;
		constructor(url, base) {
			const parts = native.url_parse(String(url), base === undefined ? undefined : String(base));
			if (!parts) throw new TypeError(`Invalid URL: ${url}`);
			this.#parts = parts;
			this.#params = new URLSearchParams(parts.search);
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
			} else if (Array.isArray(init)) {
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
		if (body instanceof Uint8Array) return body;
		if (body instanceof ArrayBuffer) return new Uint8Array(body);
		if (ArrayBuffer.isView(body)) return new Uint8Array(body.buffer, body.byteOffset, body.byteLength);
		if (body instanceof URLSearchParams) return native.utf8_encode(body.toString());
		throw new TypeError('fetch: unsupported body type');
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
			return native.utf8_decode(await this.#take());
		}
		async json() {
			return JSON.parse(await this.text());
		}
	}

	class Request extends Body {
		constructor(input, init = {}) {
			const from = input instanceof Request ? input : null;
			super(toBytes(init.body ?? (from ? from._bytes : null)));
			this.url = from ? from.url : String(input instanceof URL ? input.href : input);
			this.method = String(init.method ?? from?.method ?? 'GET').toUpperCase();
			this.headers = new Headers(init.headers ?? from?.headers);
			this.signal = init.signal ?? from?.signal ?? null;
			this._bytes = toBytes(init.body ?? (from ? from._bytes : null));
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
		static json(data, init) {
			return new Response(JSON.stringify(data), { ...init, headers: { 'content-type': 'application/json', ...init?.headers } });
		}
	}

	async function fetch(input, init = {}) {
		const request = new Request(input, init);
		const headers = [...request.headers];
		const result = await native.fetch(request.url, JSON.stringify({ method: request.method, headers }), request._bytes);
		return new Response(result.body, { status: result.status, statusText: result.statusText, headers: result.headers, url: result.url });
	}

	globalThis.Headers = Headers;
	globalThis.Request = Request;
	globalThis.Response = Response;
	globalThis.fetch = fetch;
})();
