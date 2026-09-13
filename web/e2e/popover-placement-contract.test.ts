import assert from 'node:assert/strict';
import test from 'node:test';
import { popoverPlacement } from '../src/popoverPlacement.js';

test('short menus stay below the field; tall menus flip into available space', () => {
  const anchor = { left: 80, top: 600, bottom: 644, width: 320 };
  const viewport = { left: 0, top: 0, width: 1440, height: 900 };
  assert.equal(popoverPlacement(anchor, { width: 320, height: 80 }, viewport).top, 650);
  const tall = popoverPlacement(anchor, { width: 320, height: 400 }, viewport);
  assert.equal(tall.top, 194);
  assert.equal(tall.maxHeight, 586);
});

test('a mobile menu matches the field without overflowing the visual viewport', () => {
  const result = popoverPlacement({ left: 24, top: 200, bottom: 244, width: 600 }, { width: 600, height: 280 }, { left: 0, top: 0, width: 390, height: 400 }, true);
  assert.equal(result.width, 374);
  assert.equal(result.left, 8);
  assert.ok(result.top >= 8);
  assert.ok(result.top + result.maxHeight <= 392);
});

test('soft-keyboard and zoom offsets keep the menu inside the visible area', () => {
  const viewport = { left: 50, top: 200, width: 320, height: 250 };
  for (const top of [0, 210, 300, 600]) {
    const result = popoverPlacement({ left: 900, top, bottom: top + 44, width: 280 }, { width: 480, height: 300 }, viewport);
    assert.ok(result.left >= 58);
    assert.ok(result.left + result.maxWidth <= 362);
    assert.ok(result.top >= 208);
    assert.ok(result.top + result.maxHeight <= 442);
  }
});
