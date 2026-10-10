import assert from 'node:assert/strict';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { SafeMarkdown, parseSafeMarkdown, safeLinkHref } from '../src/safeMarkdown.js';
import {
  normalizeUpstreamNotes, upstreamNotesDirty, upstreamNotesMaxScalars,
  upstreamNotesMaxUtf8Bytes, upstreamNotesScalarCount, upstreamNotesUpdateBody, validateUpstreamNotes,
} from '../src/operator/upstreamNotes.js';

function render(source: string): string {
  return renderToStaticMarkup(createElement(SafeMarkdown, { source }));
}

test('https links render as safe external anchors', () => {
  const html = render('See [API docs](https://example.com/docs) for details.');
  assert.ok(html.includes('href="https://example.com/docs"'));
  assert.ok(html.includes('target="_blank"'));
  assert.ok(html.includes('rel="noopener noreferrer"'));
  assert.ok(html.includes('>API docs</a>'));
});

test('script, data, file and protocol-relative links never become anchors', () => {
  for (const url of ['javascript:alert(1)', 'data:text/html,<script>1</script>', 'file:///etc/passwd', 'vbscript:x', '//evil.example/x']) {
    assert.equal(safeLinkHref(url), undefined, url);
    const html = render(`[click](${url})`);
    assert.ok(!html.includes('<a'), url);
    assert.ok(html.includes('click'), url);
  }
  assert.equal(safeLinkHref('https://example.com/a?b=c'), 'https://example.com/a?b=c');
  assert.equal(safeLinkHref(' https://example.com '), undefined);
});

test('links with embedded credentials never become anchors', () => {
  for (const url of ['https://user:password@example.com/docs', 'https://user@example.com/docs', 'http://token@evil.example/']) {
    assert.equal(safeLinkHref(url), undefined, url);
    assert.ok(!render(`[docs](${url})`).includes('<a'), url);
  }
});

test('unclosed emphasis delimiters stay literal and parse linearly', () => {
  const unclosed = render('**not bold and *not italic');
  assert.ok(!unclosed.includes('<strong') && !unclosed.includes('<em'));
  assert.ok(unclosed.includes('**not bold and *not italic'));
  const pathological = '*'.repeat(20_000);
  const blocks = parseSafeMarkdown(pathological);
  assert.equal(blocks.length, 1);
  assert.deepEqual(blocks[0], { kind: 'paragraph', children: [{ kind: 'text', text: pathological }] });
  const boldPathological = '**' + 'a'.repeat(30_000);
  const rendered = render(boldPathological);
  assert.ok(!rendered.includes('<strong'));
  assert.ok(render('**a'.repeat(10_000)).includes('<strong>a</strong>'));
});

test('notes write payload carries tenant identity, explicit notes and the revision fence', () => {
  assert.deepEqual(upstreamNotesUpdateBody('**docs** https://example.com', 'tenant-a', 42), {
    tenant_external_id: 'tenant-a', notes: '**docs** https://example.com', expected_updated_at: 42,
  });
  assert.deepEqual(upstreamNotesUpdateBody('   ', 'tenant-a', 7), {
    tenant_external_id: 'tenant-a', notes: null, expected_updated_at: 7,
  });
});

test('raw HTML is escaped as text and never rendered', () => {
  const html = render('<script>alert(1)</script><img src=x onerror=alert(1)>');
  assert.ok(!html.includes('<script'));
  assert.ok(!html.includes('<img'));
  assert.ok(html.includes('&lt;script&gt;'));
});

test('remote images render as alt-text links and are never embedded', () => {
  const html = render('![diagram](https://example.com/diagram.png)');
  assert.ok(!html.includes('<img'));
  assert.ok(html.includes('<a href="https://example.com/diagram.png"'));
  assert.ok(html.includes('>diagram</a>'));
  const unsafe = render('![tracking](javascript:alert(1))');
  assert.ok(!unsafe.includes('<a'));
});

test('headings, lists, emphasis and code blocks render structure', () => {
  const html = render('# Title\n\n**bold** and *italic* and `code`\n\n- one\n- two\n\n```\n<raw> & text\n```');
  assert.ok(html.includes('<h1>Title</h1>'));
  assert.ok(html.includes('<strong>bold</strong>'));
  assert.ok(html.includes('<em>italic</em>'));
  assert.ok(html.includes('<code>code</code>'));
  assert.ok(html.includes('<ul><li>one</li><li>two</li></ul>'));
  assert.ok(html.includes('&lt;raw&gt; &amp; text'));
  assert.ok(!html.includes('<raw>'));
});

test('parser keeps plain paragraphs and single line breaks', () => {
  const blocks = parseSafeMarkdown('first\nsecond\n\nthird');
  assert.equal(blocks.length, 2);
  assert.deepEqual(blocks[0], { kind: 'paragraph', children: [{ kind: 'text', text: 'first\nsecond' }] });
});

test('validation enforces scalar and byte limits', () => {
  assert.equal(validateUpstreamNotes('a'.repeat(upstreamNotesMaxScalars)), undefined);
  assert.equal(validateUpstreamNotes('a'.repeat(upstreamNotesMaxScalars + 1)), 'tooLong');
  const wide = '\u{1F600}'; // 1 Unicode scalar, 4 UTF-8 bytes
  const atBothLimits = wide.repeat(upstreamNotesMaxScalars);
  assert.equal(new TextEncoder().encode(atBothLimits).length, upstreamNotesMaxUtf8Bytes);
  assert.equal(validateUpstreamNotes(atBothLimits), undefined);
  assert.equal(validateUpstreamNotes(`${atBothLimits}a`), 'tooLong');
  assert.ok(upstreamNotesScalarCount(`${wide}b`) < `${wide}b`.length);
});

test('validation rejects control characters but allows tab and newlines', () => {
  assert.equal(validateUpstreamNotes('line\twith\ttabs\nand\r\nbreaks'), undefined);
  assert.equal(validateUpstreamNotes(`bell${String.fromCharCode(7)}here`), 'controlChars');
  assert.equal(validateUpstreamNotes(`null${String.fromCharCode(0)}byte`), 'controlChars');
  assert.equal(validateUpstreamNotes(`escape${String.fromCharCode(27)}sequence`), 'controlChars');
});

test('clearing semantics: whitespace clears, other content is preserved verbatim', () => {
  assert.equal(normalizeUpstreamNotes(''), null);
  assert.equal(normalizeUpstreamNotes('  \n\t '), null);
  const source = '  keep **exact** spacing  \n';
  assert.equal(normalizeUpstreamNotes(source), source);
  assert.equal(upstreamNotesDirty(null, ''), false);
  assert.equal(upstreamNotesDirty(null, 'note'), true);
  assert.equal(upstreamNotesDirty('note', 'note'), false);
  assert.equal(upstreamNotesDirty(undefined, '   '), false);
  assert.equal(upstreamNotesDirty('note', ''), true);
});
