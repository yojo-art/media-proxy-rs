import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { test } from 'node:test';

const require = createRequire(import.meta.url);
const { MediaProcessor, ABI_VERSION } = require('../index.js');
const dummyPng = readFileSync(new URL('../../asset/dummy.png', import.meta.url));

test('ABI_VERSION', () => {
	assert.equal(ABI_VERSION, 1);
});

test('avatar は webp を返す', async () => {
	const p = new MediaProcessor();
	const r = await p.process(dummyPng, { avatar: true });
	assert.equal(r.contentType, 'image/webp');
	assert.equal(r.ext, '.webp');
	assert.ok(r.data.length > 0);
});

test('SVG の文字をシステムフォント無しで描ける', async () => {
	const p = new MediaProcessor({ loadSystemFonts: false });
	const withText = Buffer.from('<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32"><text x="0" y="20" font-family="sans-serif">Ab</text></svg>');
	const withoutText = Buffer.from('<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32"></svg>');
	const r1 = await p.process(withText, { static: true });
	const r2 = await p.process(withoutText, { static: true });
	assert.equal(r1.contentType, 'image/webp');
	assert.equal(r2.contentType, 'image/webp');
	// 文字が描かれていれば、テキストあり/無しで結果が異なる
	assert.notDeepEqual(r1.data, r2.data);
});

test('未知の形式は code=Unsupported', async () => {
	const p = new MediaProcessor();
	await assert.rejects(p.process(Buffer.from('not an image'), {}), { code: 'Unsupported' });
});

test('範囲外のオプションは構築時にエラー', () => {
	assert.throws(() => new MediaProcessor({ webpQuality: 101 }));
	assert.throws(() => new MediaProcessor({ fontDirs: ['/nonexistent-font-dir'] }));
});
