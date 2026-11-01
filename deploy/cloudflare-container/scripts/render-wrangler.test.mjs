// node --test deploy/cloudflare-container/scripts/render-wrangler.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { render } from './render-wrangler.mjs';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const manifest = JSON.parse(readFileSync(resolve(root, 'environments.json'), 'utf8'));
const flavors = readdirSync(resolve(root, 'workers')).filter((f) => f !== 'shared');
const T = 'aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee';
const P = '11111111-2222-4333-8444-555555555555';
const withIds = (e) => ({ testnet: { ...e.testnet, database_id: T }, production: { ...e.production, database_id: P } });

test('every worker flavor is in the manifest and has no account_id in the repo', () => {
	for (const f of flavors) {
		assert.ok(manifest.flavors[f], f);
		assert.doesNotMatch(readFileSync(resolve(root, 'workers', f, 'wrangler.toml'), 'utf8'), /^\s*account_id\s*=/m, f);
	}
});

test('refuses while the manifest id is missing (current repo state)', () => {
	for (const f of flavors) {
		const tpl = readFileSync(resolve(root, 'workers', f, 'wrangler.toml'), 'utf8');
		for (const env of ['testnet', 'production']) {
			assert.throws(() => render(tpl, manifest.flavors[f], env), /database_id is not set/);
		}
	}
});

test('renders each env with only its own D1 id and no placeholder', () => {
	for (const f of flavors) {
		const tpl = readFileSync(resolve(root, 'workers', f, 'wrangler.toml'), 'utf8');
		const e = withIds(manifest.flavors[f]);
		const t = render(tpl, e, 'testnet');
		assert.match(t, new RegExp(T));
		assert.doesNotMatch(t, new RegExp(P));
		assert.doesNotMatch(t, /env\.production/);
		const p = render(tpl, e, 'production');
		assert.match(p, new RegExp(P));
		assert.doesNotMatch(p, new RegExp(T));
		assert.doesNotMatch(p, /REPLACE_ME/);
	}
});

test('rejects a stray placeholder, a bad id, and a repo account_id', () => {
	const tpl = readFileSync(resolve(root, 'workers/webcash/wrangler.toml'), 'utf8');
	const e = withIds(manifest.flavors.webcash);
	assert.throws(() => render(tpl + '\n[vars]\nX = "REPLACE_ME"\n', e, 'testnet'), /REPLACE_ME/);
	assert.throws(() => render('account_id = "abc"\n' + tpl, e, 'testnet'), /CLOUDFLARE_ACCOUNT_ID/);
	assert.throws(() => render(tpl, { ...e, testnet: { ...e.testnet, database_id: 'REPLACE_ME' } }, 'testnet'), /not set/);
});

test('production keeps its own durable_objects binding', () => {
	const tpl = readFileSync(resolve(root, 'workers/webcash/wrangler.toml'), 'utf8');
	const e = withIds(manifest.flavors.webcash);
	assert.match(render(tpl, e, 'production'), /\[env\.production\.durable_objects\]/);
	const stripped = tpl.replace(/\[env\.production\.durable_objects\][\s\S]*$/, '');
	assert.throws(() => render(stripped, e, 'production'), /durable_objects/);
});
