#!/usr/bin/env node
// `node npm/make.mjs <rust target> <version> [binary]` assembles the npm package
// for one platform, `npm/gpuix-qjs-<platform>-<arch>/`, from a built host binary.
// One package per platform with `os`/`cpu` set, the way @gpuix/native ships
// its prebuilds, so a consumer's optionalDependencies pull in only its own.

import { chmodSync, copyFileSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const TARGETS = {
	'aarch64-apple-darwin': { os: 'darwin', cpu: 'arm64' },
	'x86_64-apple-darwin': { os: 'darwin', cpu: 'x64' },
	'x86_64-unknown-linux-gnu': { os: 'linux', cpu: 'x64' },
	'aarch64-unknown-linux-gnu': { os: 'linux', cpu: 'arm64' },
	'x86_64-pc-windows-msvc': { os: 'win32', cpu: 'x64' },
	'aarch64-pc-windows-msvc': { os: 'win32', cpu: 'arm64' }
};

const [target, version, binary_arg] = process.argv.slice(2);
const platform = TARGETS[target];
if (!platform || !version) {
	console.error(`usage: node npm/make.mjs <${Object.keys(TARGETS).join('|')}> <version> [binary]`);
	process.exit(2);
}

const here = dirname(fileURLToPath(import.meta.url));
const exe = platform.os === 'win32' ? 'gpuix-qjs.exe' : 'gpuix-qjs';
const binary = binary_arg ?? join(here, '..', '..', 'native', 'target', target, 'release', exe);
const name = `gpuix-qjs-${platform.os}-${platform.cpu}`;
const dir = join(here, name);

rmSync(dir, { recursive: true, force: true });
mkdirSync(dir, { recursive: true });
copyFileSync(binary, join(dir, exe));
if (platform.os !== 'win32') chmodSync(join(dir, exe), 0o755);
writeFileSync(
	join(dir, 'package.json'),
	JSON.stringify(
		{
			name,
			version,
			description: `The gpuix-qjs QuickJS host for GPUIX apps, built for ${platform.os} ${platform.cpu}`,
			license: 'Apache-2.0',
			repository: { type: 'git', url: 'https://github.com/khromov/gpuix', directory: 'packages/qjs' },
			os: [platform.os],
			cpu: [platform.cpu],
			files: [exe],
			// gpuix-svelte resolves `<name>/package.json` and takes the binary beside it.
			exports: { './package.json': './package.json' }
		},
		null,
		2
	) + '\n'
);
writeFileSync(
	join(dir, 'README.md'),
	`# ${name}\n\nThe \`gpuix-qjs\` host binary for ${platform.os} ${platform.cpu}. Installed as an optional dependency of gpuix-svelte; see https://github.com/khromov/gpuix/tree/main/packages/qjs.\n`
);
console.log(`${dir} (${name}@${version})`);
