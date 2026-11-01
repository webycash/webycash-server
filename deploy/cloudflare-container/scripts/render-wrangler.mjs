#!/usr/bin/env node
// Render a deployable wrangler config from the committed template + the one
// environment manifest (audit M12). Refuses to emit anything while a D1 id is
// missing or any REPLACE_ME placeholder would reach `wrangler deploy`.
//
//   cd workers/<flavor> && node ../../scripts/render-wrangler.mjs --env testnet|production
//
// Writes workers/<flavor>/wrangler.generated.toml (gitignored).
import { readFileSync, writeFileSync } from 'node:fs';
import { basename, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
export const ENVS = ['testnet', 'production'];

/** Split a TOML file into [header|null, lines[]] blocks. */
function blocks(text) {
	const out = [{ header: null, lines: [] }];
	for (const line of text.split('\n')) {
		const m = line.match(/^\s*(\[\[?[^\]]+\]\]?)\s*$/);
		if (m) out.push({ header: m[1], lines: [line] });
		else out[out.length - 1].lines.push(line);
	}
	return out;
}

const isProd = (h) => /^\[\[?env\.production[.\]]/.test(h);

/**
 * @param {string} template wrangler.toml text
 * @param {{ testnet: {database_name:string,database_id:string|null}, production: {database_name:string,database_id:string|null} }} entry
 * @param {'testnet'|'production'} env
 * @returns {string} rendered config
 */
export function render(template, entry, env) {
	if (!ENVS.includes(env)) throw new Error(`unknown env '${env}' (want ${ENVS.join('|')})`);
	const want = entry?.[env];
	if (!want) throw new Error(`manifest has no '${env}' entry`);
	if (!want.database_id || !UUID.test(want.database_id)) {
		throw new Error(`${want.database_name}: database_id is not set in environments.json (run \`wrangler d1 create ${want.database_name}\` and commit the id)`);
	}
	const kept = blocks(template).filter(({ header }) => {
		if (header === null) return true;
		if (env === 'testnet') return !isProd(header);
		// production: env.production.d1_databases replaces the top-level binding
		return header !== '[[d1_databases]]';
	});
	let text = kept.map((b) => b.lines.join('\n')).join('\n');
	let lastName = null;
	let filled = 0;
	text = text
		.split('\n')
		.map((line) => {
			const name = line.match(/^\s*database_name\s*=\s*"([^"]+)"/);
			if (name) lastName = name[1];
			if (/^\s*\[/.test(line)) lastName = null;
			if (/^\s*database_id\s*=/.test(line)) {
				if (lastName !== want.database_name) {
					throw new Error(`database '${lastName}' in the ${env} config does not match the manifest (${want.database_name})`);
				}
				filled += 1;
				return line.replace(/=\s*"[^"]*"/, `= "${want.database_id}"`);
			}
			return line;
		})
		.join('\n');
	if (filled === 0) throw new Error(`no database_id line for ${want.database_name}`);
	if (/REPLACE_ME/.test(text)) throw new Error('rendered config still contains a REPLACE_ME placeholder');
	if (env === 'production' && /^\[durable_objects\]/m.test(text) && !/^\[env\.production\.durable_objects\]/m.test(text)) {
		throw new Error('env.production has no durable_objects binding (not inherited from the top level)');
	}
	if (/^\s*account_id\s*=/m.test(text)) throw new Error('account_id must come from CLOUDFLARE_ACCOUNT_ID, not the repo');
	return text;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
	const args = process.argv.slice(2);
	const env = args[args.indexOf('--env') + 1] ?? 'testnet';
	const flavor = args.includes('--flavor') ? args[args.indexOf('--flavor') + 1] : basename(process.cwd());
	const here = dirname(fileURLToPath(import.meta.url));
	const manifest = JSON.parse(readFileSync(resolve(here, '../environments.json'), 'utf8'));
	const entry = manifest.flavors?.[flavor];
	try {
		if (!entry) throw new Error(`flavor '${flavor}' is not in environments.json`);
		if (!process.env.CLOUDFLARE_ACCOUNT_ID) throw new Error('CLOUDFLARE_ACCOUNT_ID is not set');
		const out = render(readFileSync('wrangler.toml', 'utf8'), entry, env);
		writeFileSync('wrangler.generated.toml', out);
		console.log(`[render-wrangler] ${flavor}/${env} -> wrangler.generated.toml`);
	} catch (e) {
		console.error(`[render-wrangler] REFUSED: ${e.message}`);
		process.exit(1);
	}
}
