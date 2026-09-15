export type ArchiveItemFormat = 'input' | 'messages' | 'output' | 'choices' | 'output_text';
export interface ArchiveStreamItem { format: ArchiveItemFormat; value: unknown }
type Frame = { kind: 'object' | 'array'; key?: string; expectKey?: boolean; ready?: boolean; format?: ArchiveItemFormat };

/** Validates discarded fields too, without retaining their string contents. */
class JsonStructure {
  private stack: { kind: 'object' | 'array'; state: 'keyOrEnd' | 'key' | 'colon' | 'value' | 'valueOrEnd' | 'commaOrEnd' }[] = [];
  private started = false;
  private quoted = false;
  private key = false;
  private escaped = false;
  private unicode = 0;
  private literal = '';

  append(text: string) {
    for (const char of text) {
      if (this.quoted) {
        if (this.unicode) {
          if (!/[0-9a-f]/i.test(char)) throw new Error('Invalid archive JSON escape');
          this.unicode -= 1;
        } else if (this.escaped) {
          this.escaped = false;
          if (char === 'u') this.unicode = 4;
          else if (!'"\\/bfnrt'.includes(char)) throw new Error('Invalid archive JSON escape');
        } else if (char === '\\') this.escaped = true;
        else if (char === '"') {
          this.quoted = false;
          if (this.key) this.stack.at(-1)!.state = 'colon';
        } else if (char.charCodeAt(0) < 32) throw new Error('Invalid archive JSON string');
        continue;
      }
      if (this.literal) {
        if (!/[\x20\t\r\n,\]}]/.test(char)) { this.literal += char; continue; }
        JSON.parse(this.literal); this.literal = '';
      }
      if (/[\x20\t\r\n]/.test(char)) continue;
      const top = this.stack.at(-1);
      if (char === '}' || char === ']') {
        if (!top || top.kind !== (char === '}' ? 'object' : 'array')
          || !['keyOrEnd', 'valueOrEnd', 'commaOrEnd'].includes(top.state)) throw new Error('Invalid archive JSON closing delimiter');
        this.stack.pop(); continue;
      }
      if (top?.state === 'commaOrEnd') {
        if (char !== ',') throw new Error('Missing archive JSON comma');
        top.state = top.kind === 'object' ? 'key' : 'value'; continue;
      }
      if (top?.state === 'colon') {
        if (char !== ':') throw new Error('Missing archive JSON colon');
        top.state = 'value'; continue;
      }
      if (top?.state === 'key' || top?.state === 'keyOrEnd') {
        if (char !== '"') throw new Error('Invalid archive JSON key');
        this.quoted = true; this.key = true; continue;
      }
      if (!top && this.started) throw new Error('Unexpected data after archive JSON');
      this.started = true;
      if (top) top.state = 'commaOrEnd';
      if (char === '{') this.stack.push({ kind: 'object', state: 'keyOrEnd' });
      else if (char === '[') this.stack.push({ kind: 'array', state: 'valueOrEnd' });
      else if (char === '"') { this.quoted = true; this.key = false; }
      else if (/[\dtfn-]/.test(char)) this.literal = char;
      else throw new Error('Invalid archive JSON value');
    }
  }

  finish() {
    if (this.literal) { JSON.parse(this.literal); this.literal = ''; }
    if (!this.started || this.stack.length || this.quoted) throw new Error('Archive JSON ended inside a value');
  }
}

/**
 * Incremental archive framing. Keeps one unfinished JSON item/SSE event and
 * the unread part of the current byte page, not the entire response archive.
 * Byte decoding belongs to the range reader so UTF-8 can cross network pages.
 */
export class ArchiveItemStream {
  private buffer = '';
  private mode?: 'json' | 'sse';
  private position = 0;
  private frames: Frame[] = [];
  private inString = false;
  private escaped = false;
  private stringStart = 0;
  private keyString = false;
  private capture?: { start: number; depth: number; kind: 'compound' | 'string' | 'literal'; format: ArchiveItemFormat };
  private seenOutput = new Set<number>();
  private waitingOutput = new Map<number, unknown>();
  private nextOutput = 0;
  private pendingSse: ArchiveStreamItem[] = [];
  private ended = false;
  private recognized = false;
  private validation?: JsonStructure;
  private incomplete = false;

  constructor(private readonly side: 'request' | 'response') {}

  append(text: string) {
    const priorMode = this.mode;
    this.buffer += text;
    if (!this.mode) {
      const start = this.buffer.trimStart();
      if (/^[{["]/.test(start)) this.mode = 'json';
      else if (/^(?:event:|data:|id:|retry:|:)/.test(start)) this.mode = 'sse';
      else if (start.length >= 8) throw new Error('Unsupported archive framing');
    }
    if (this.mode === 'json') {
      this.validation ??= new JsonStructure();
      this.validation.append(priorMode ? text : this.buffer);
    }
  }

  take(count: number): ArchiveStreamItem[] {
    if (!Number.isSafeInteger(count) || count <= 0) return [];
    return this.mode === 'sse' ? this.takeSse(count) : this.mode === 'json' ? this.takeJson(count) : [];
  }

  finish() {
    this.validation?.finish();
    if (this.mode === 'json' && (this.frames.length || this.inString || this.capture)) throw new Error('Archive JSON ended inside an item');
    if (this.mode === 'sse' && !this.ended) throw new Error('Archive stream ended before its terminal event');
    if (this.incomplete) throw new Error('Archive stream did not complete');
    if (!this.recognized) throw new Error('Unsupported archive content');
  }

  private takeSse(count: number) {
    const items: ArchiveStreamItem[] = [];
    while (items.length < count) {
      if (this.pendingSse.length) { items.push(this.pendingSse.shift()!); continue; }
      const boundary = /\r?\n\r?\n/.exec(this.buffer);
      if (!boundary) break;
      const block = this.buffer.slice(0, boundary.index);
      this.buffer = this.buffer.slice(boundary.index + boundary[0].length);
      const data = block.split(/\r?\n/).filter(line => line.startsWith('data:')).map(line => line.slice(5).replace(/^ /, '')).join('\n');
      if (!data || data === '[DONE]') continue;
      const event = JSON.parse(data) as { type?: string; output_index?: number; item?: unknown; response?: { output?: unknown[] } };
      if (event.type === 'response.output_item.done' && Number.isSafeInteger(event.output_index) && event.output_index! >= 0 && event.item !== undefined) {
        if (!this.seenOutput.has(event.output_index!)) {
          this.seenOutput.add(event.output_index!);
          this.waitingOutput.set(event.output_index!, event.item);
          this.recognized = true;
          while (this.waitingOutput.has(this.nextOutput)) {
            this.pendingSse.push({ format: 'output', value: this.waitingOutput.get(this.nextOutput) });
            this.waitingOutput.delete(this.nextOutput++);
          }
        }
      } else if (event.type === 'response.completed' || event.type === 'response.incomplete' || event.type === 'response.failed') {
        for (const [index, value] of (event.response?.output ?? []).entries()) {
          if (!this.seenOutput.has(index)) this.waitingOutput.set(index, value);
        }
        for (const [, value] of [...this.waitingOutput].sort(([left], [right]) => left - right)) this.pendingSse.push({ format: 'output', value });
        this.waitingOutput.clear();
        this.recognized = true;
        this.incomplete = event.type !== 'response.completed';
        this.ended = true;
      }
    }
    return items;
  }

  private takeJson(count: number) {
    const items: ArchiveStreamItem[] = [];
    const emit = (end: number) => {
      const capture = this.capture!;
      items.push({ format: capture.format, value: JSON.parse(this.buffer.slice(capture.start, end)) });
      this.capture = undefined;
      if (!this.frames.length) this.ended = true;
    };
    while (this.position < this.buffer.length && items.length < count) {
      const char = this.buffer[this.position];
      const top = this.frames.at(-1);
      if (this.ended && !/\s/.test(char)) throw new Error('Unexpected data after archive JSON');
      if (this.inString) {
        if (this.escaped) this.escaped = false;
        else if (char === '\\') this.escaped = true;
        else if (char === '"') {
          this.inString = false;
          if (this.keyString && top?.kind === 'object') top.key = JSON.parse(this.buffer.slice(this.stringStart, this.position + 1)) as string;
          if (this.capture?.kind === 'string') emit(this.position + 1);
        }
        this.position += 1;
        continue;
      }
      if (this.capture?.kind === 'literal' && /[\s,\]}]/.test(char)) {
        emit(this.position);
        if (items.length >= count) break;
      }
      if (!this.capture && !/\s/.test(char)) {
        let format: ArchiveItemFormat | undefined;
        if (top?.kind === 'array' && top.ready && top.format && char !== ']') { format = top.format; top.ready = false; }
        else if (char === '"' && top?.kind === 'object' && !top.expectKey
          && (this.frames.length === 1 || (this.side === 'response' && this.frames.length === 2 && this.frames[0].key === 'response'))
          && ((this.side === 'request' && top.key === 'input') || (this.side === 'response' && top.key === 'output_text'))) format = top.key as ArchiveItemFormat;
        else if (!top && char === '"') format = this.side === 'request' ? 'input' : 'output_text';
        if (format) { this.recognized = true; this.capture = { start: this.position, depth: this.frames.length, kind: char === '{' || char === '[' ? 'compound' : char === '"' ? 'string' : 'literal', format }; }
      }
      if (char === '"') {
        this.inString = true;
        this.stringStart = this.position;
        this.keyString = top?.kind === 'object' && Boolean(top.expectKey);
      } else if (char === '{') this.frames.push({ kind: 'object', expectKey: true });
      else if (char === '[') {
        const key = top?.kind === 'object' ? top.key : undefined;
        const inEnvelope = this.frames.length === 1 || (this.frames.length === 2 && this.frames[0].key === 'response');
        const format = !top ? (this.side === 'request' ? 'input' : 'output')
          : inEnvelope && (this.side === 'request' ? ['input', 'messages'] : ['output', 'choices']).includes(key ?? '') ? key as ArchiveItemFormat : undefined;
        if (format) this.recognized = true;
        this.frames.push({ kind: 'array', ready: true, format });
      } else if (char === '}' || char === ']') {
        const closed = this.frames.pop();
        if (!closed || closed.kind !== (char === '}' ? 'object' : 'array')) throw new Error('Invalid archive JSON nesting');
        if (this.capture?.kind === 'compound' && this.frames.length === this.capture.depth) emit(this.position + 1);
        if (!this.frames.length) this.ended = true;
      } else if (char === ',' && top?.kind === 'object') { top.expectKey = true; top.key = undefined; }
      else if (char === ',' && top?.kind === 'array') top.ready = true;
      else if (char === ':' && top?.kind === 'object') top.expectKey = false;
      this.position += 1;
    }
    const keep = Math.min(this.position, this.capture?.start ?? this.position, this.inString && this.keyString ? this.stringStart : this.position);
    this.buffer = this.buffer.slice(keep);
    this.position -= keep;
    if (this.capture) this.capture.start -= keep;
    if (this.inString) this.stringStart -= keep;
    return items;
  }
}
