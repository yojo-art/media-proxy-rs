import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { Readable } from 'node:stream';
import { test } from 'node:test';

const require = createRequire(import.meta.url);
const { MediaProcessor, ABI_VERSION } = require('../index.js');
const dummyPng = readFileSync(new URL('../../asset/dummy.png', import.meta.url));

test('ABI_VERSION', () => {
	assert.equal(ABI_VERSION, 2);
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

test('Readable から process と同じ結果を返す', async () => {
	const p = new MediaProcessor();
	const expected = await p.process(dummyPng, { avatar: true });
	const actual = await p.processStream(Readable.from([dummyPng]), { avatar: true });
	assert.equal(actual.contentType, expected.contentType);
	assert.equal(actual.ext, expected.ext);
	assert.deepEqual(actual.data, expected.data);

	const fromTypedArray = await p.processStream(
		Readable.from([new Uint8Array(dummyPng)]),
		{ avatar: true },
	);
	assert.deepEqual(fromTypedArray.data, expected.data);
});

test('Readable の文字列チャンクを処理できる', async () => {
	const p = new MediaProcessor({ loadSystemFonts: false });
	const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32"><text x="0" y="20" font-family="sans-serif">Ab</text></svg>';
	const r = await p.processStream(Readable.from([svg]), { static: true });
	assert.equal(r.contentType, 'image/webp');
	assert.ok(r.data.length > 0);
});

test('Readable のエラーは code=Stream', async () => {
	const p = new MediaProcessor();
	const stream = new Readable({
		read() {
			this.push(dummyPng.subarray(0, 10));
			this.destroy(new Error('boom'));
		},
	});
	await assert.rejects(p.processStream(stream, {}), { code: 'Stream' });
});

test('maxInputBytes を超えると code=TooLarge', async () => {
	const p = new MediaProcessor({ maxInputBytes: 10 });
	await assert.rejects(p.processStream(Readable.from([dummyPng]), {}), {
		code: 'TooLarge',
	});
});

test('出力サイズと WebP 品質を都度指定できる', async () => {
	const p = new MediaProcessor();
	const sized = await p.process(dummyPng, { width: 64, height: 32, webpQuality: 40 });
	assert.equal(sized.contentType, 'image/webp');
	const streamed = await p.processStream(Readable.from([dummyPng]), {
		width: 64,
		height: 32,
		webpQuality: 40,
	});
	assert.deepEqual(streamed.data, sized.data);
});

test('lossless WebP を出力できる', async () => {
	const p = new MediaProcessor();
	const lossless = await p.process(dummyPng, { lossless: true });
	const lossy = await p.process(dummyPng, {});
	assert.equal(lossless.contentType, 'image/webp');
	assert.equal(lossless.ext, '.webp');
	assert.notDeepEqual(lossless.data, lossy.data);
	const streamed = await p.processStream(Readable.from([dummyPng]), {
		lossless: true,
	});
	assert.deepEqual(streamed.data, lossless.data);
});

test('範囲外の process オプションはエラー', () => {
	const p = new MediaProcessor();
	assert.throws(() => p.process(dummyPng, { width: 0 }));
	assert.throws(() => p.process(dummyPng, { height: 16384 }));
	assert.throws(() => p.process(dummyPng, { webpQuality: 101 }));
});

test('範囲外のオプションは構築時にエラー', () => {
	assert.throws(() => new MediaProcessor({ webpQuality: 101 }));
	assert.throws(() => new MediaProcessor({ maxInputBytes: 0 }));
	assert.throws(() => new MediaProcessor({ fontDirs: ['/nonexistent-font-dir'] }));
});
