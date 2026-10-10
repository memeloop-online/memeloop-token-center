import { Fragment, createElement, type ReactNode } from 'react';

export type SafeMarkdownInline =
  | { kind: 'text'; text: string }
  | { kind: 'bold'; children: SafeMarkdownInline[] }
  | { kind: 'italic'; children: SafeMarkdownInline[] }
  | { kind: 'code'; text: string }
  | { kind: 'link'; label: string; href: string };

export type SafeMarkdownBlock =
  | { kind: 'paragraph'; children: SafeMarkdownInline[] }
  | { kind: 'heading'; level: number; children: SafeMarkdownInline[] }
  | { kind: 'list'; ordered: boolean; items: SafeMarkdownInline[][] }
  | { kind: 'codeBlock'; text: string };

/** Only absolute http(s) URLs may become clickable links. */
export function safeLinkHref(url: string): string | undefined {
  if (!url || url.trim() !== url || /\s/.test(url)) return undefined;
  let parsed: URL;
  try { parsed = new URL(url); } catch { return undefined; }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return undefined;
  // Documentation links never need embedded credentials.
  if (parsed.username || parsed.password) return undefined;
  return parsed.href;
}

const MAX_INLINE_DEPTH = 4;
const LINK_PATTERN = /^\[([^\]]*)\]\(([^)\s]*)\)/;
const IMAGE_PATTERN = /^!\[([^\]]*)\]\(([^)\s]*)\)/;

export function parseSafeMarkdownInlines(source: string, depth = 0): SafeMarkdownInline[] {
  const nodes: SafeMarkdownInline[] = [];
  let text = '';
  const flush = () => { if (text) { nodes.push({ kind: 'text', text }); text = ''; } };
  let index = 0;
  // A failed delimiter search proves none exists later, so each delimiter is
  // scanned at most once and parsing stays linear even without closers.
  let noCode = false;
  let noBold = false;
  let noItalic = false;
  while (index < source.length) {
    const rest = source.slice(index);
    if (!noCode && rest.startsWith('`')) {
      const end = source.indexOf('`', index + 1);
      if (end > index) {
        flush();
        nodes.push({ kind: 'code', text: source.slice(index + 1, end) });
        index = end + 1;
        continue;
      }
      noCode = true;
    }
    const image = rest.match(IMAGE_PATTERN);
    if (image) {
      flush();
      const href = safeLinkHref(image[2]);
      if (href) nodes.push({ kind: 'link', label: image[1] || href, href });
      else text += image[0];
      index += image[0].length;
      continue;
    }
    const link = rest.match(LINK_PATTERN);
    if (link) {
      const href = safeLinkHref(link[2]);
      if (href) {
        flush();
        nodes.push({ kind: 'link', label: link[1] || href, href });
      } else {
        text += link[0];
      }
      index += link[0].length;
      continue;
    }
    if (!noBold && depth < MAX_INLINE_DEPTH && rest.startsWith('**')) {
      const end = source.indexOf('**', index + 2);
      if (end > index + 2) {
        flush();
        nodes.push({ kind: 'bold', children: parseSafeMarkdownInlines(source.slice(index + 2, end), depth + 1) });
        index = end + 2;
        continue;
      }
      if (end === -1) noBold = true;
    }
    if (!noItalic && depth < MAX_INLINE_DEPTH && rest.startsWith('*') && !rest.startsWith('**')) {
      const end = source.indexOf('*', index + 1);
      if (end > index + 1) {
        flush();
        nodes.push({ kind: 'italic', children: parseSafeMarkdownInlines(source.slice(index + 1, end), depth + 1) });
        index = end + 1;
        continue;
      }
      if (end === -1) noItalic = true;
    }
    text += source[index];
    index += 1;
  }
  flush();
  return nodes;
}

const BLOCK_START = /^(?:```|#{1,6}\s|\s*(?:[-*+]|\d{1,9}[.)])\s)/;
const LIST_ITEM = /^\s*(?:[-*+]|\d{1,9}[.)])\s+(.*)$/;
const ORDERED_ITEM = /^\s*\d{1,9}[.)]\s/;

export function parseSafeMarkdown(source: string): SafeMarkdownBlock[] {
  const blocks: SafeMarkdownBlock[] = [];
  const lines = source.split('\n');
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    if (!line.trim()) { index += 1; continue; }
    if (line.startsWith('```')) {
      const body: string[] = [];
      index += 1;
      while (index < lines.length && !lines[index].startsWith('```')) { body.push(lines[index]); index += 1; }
      if (index < lines.length) index += 1;
      blocks.push({ kind: 'codeBlock', text: body.join('\n') });
      continue;
    }
    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      blocks.push({ kind: 'heading', level: heading[1].length, children: parseSafeMarkdownInlines(heading[2]) });
      index += 1;
      continue;
    }
    const item = line.match(LIST_ITEM);
    if (item) {
      const ordered = ORDERED_ITEM.test(line);
      const items: SafeMarkdownInline[][] = [];
      while (index < lines.length) {
        const entry = lines[index].match(LIST_ITEM);
        if (!entry) break;
        items.push(parseSafeMarkdownInlines(entry[1]));
        index += 1;
      }
      blocks.push({ kind: 'list', ordered, items });
      continue;
    }
    const paragraph = [line];
    index += 1;
    while (index < lines.length && lines[index].trim() && !BLOCK_START.test(lines[index])) {
      paragraph.push(lines[index]);
      index += 1;
    }
    blocks.push({ kind: 'paragraph', children: parseSafeMarkdownInlines(paragraph.join('\n')) });
  }
  return blocks;
}

function renderText(text: string, key: string): ReactNode {
  const parts = text.split('\n');
  return <Fragment key={key}>{parts.map((part, index) => <Fragment key={index}>{index > 0 && <br />}{part}</Fragment>)}</Fragment>;
}

function renderInline(node: SafeMarkdownInline, key: string): ReactNode {
  switch (node.kind) {
    case 'text': return renderText(node.text, key);
    case 'bold': return <strong key={key}>{node.children.map((child, index) => renderInline(child, String(index)))}</strong>;
    case 'italic': return <em key={key}>{node.children.map((child, index) => renderInline(child, String(index)))}</em>;
    case 'code': return <code key={key}>{node.text}</code>;
    case 'link': return <a key={key} href={node.href} target="_blank" rel="noopener noreferrer">{node.label}</a>;
  }
}

function renderBlock(block: SafeMarkdownBlock, key: string): ReactNode {
  switch (block.kind) {
    case 'heading':
      return createElement(`h${Math.min(6, Math.max(1, block.level))}`, { key }, block.children.map((child, index) => renderInline(child, String(index))));
    case 'paragraph':
      return <p key={key}>{block.children.map((child, index) => renderInline(child, String(index)))}</p>;
    case 'list': {
      const items = block.items.map((entry, index) => <li key={index}>{entry.map((child, childIndex) => renderInline(child, String(childIndex)))}</li>);
      return block.ordered ? <ol key={key}>{items}</ol> : <ul key={key}>{items}</ul>;
    }
    case 'codeBlock':
      return <pre key={key}><code>{block.text}</code></pre>;
  }
}

/** Renders a safe Markdown subset as React elements; raw HTML is always escaped text. */
export function SafeMarkdown({ source, className }: { source: string; className?: string }) {
  return <div className={className}>{parseSafeMarkdown(source).map((block, index) => renderBlock(block, String(index)))}</div>;
}
