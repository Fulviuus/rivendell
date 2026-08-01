/**
 * Small markdown renderer for message bodies.
 *
 * Agent output is untrusted text, so every character is HTML-escaped before any
 * markup is added and only our own tags are ever inserted. Link hrefs are
 * restricted to http/https/mailto so a crafted reply cannot smuggle in a
 * javascript: URL.
 */

const escapeMap: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
  "'": "&#39;",
};

function esc(s: string): string {
  return s.replace(/[&<>"']/g, (c) => escapeMap[c]);
}

function safeHref(url: string): string | null {
  const trimmed = url.trim();
  if (/^(https?:|mailto:)/i.test(trimmed)) return esc(trimmed);
  return null;
}

/** An agent that can actually be summoned, and the colour it owns. */
export interface Mentionable {
  name: string;
  color: string;
}

const RE_META = /[.*+?^${}()|[\]\\]/g;

/**
 * Highlight `@name`, but only for names that are really agents in this room.
 *
 * The backend ignores an `@word` that matches nobody, so styling it would
 * promise a summons that never happened. Same word-boundary rule as the parser
 * there, which is what keeps email addresses out of it.
 */
function highlightMentions(text: string, people: Mentionable[]): string {
  if (people.length === 0) return text;
  // Longest first, so @ana cannot shadow @anabel.
  const ordered = [...people].sort((a, b) => b.name.length - a.name.length);
  const alt = ordered.map((p) => p.name.replace(RE_META, "\\$&")).join("|");
  const re = new RegExp(`(^|[^a-zA-Z0-9@])@(${alt})\\b`, "gi");
  return text.replace(re, (_m, before: string, name: string) => {
    const person = ordered.find((p) => p.name.toLowerCase() === name.toLowerCase())!;
    return `${before}<span class="mention mention-${person.color || "slate"}">@${name}</span>`;
  });
}

/** Inline spans. Runs on already-escaped text. */
function inline(text: string, people: Mentionable[]): string {
  const codes: string[] = [];
  // Pull inline code out first so its contents are never treated as markup.
  let out = text.replace(/`([^`]+)`/g, (_, code) => {
    codes.push(code);
    return `\u0000${codes.length - 1}\u0000`;
  });

  out = out.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (whole, label, url) => {
    const href = safeHref(url);
    return href ? `<a href="${href}" target="_blank" rel="noreferrer">${label}</a>` : whole;
  });
  out = out.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  out = out.replace(/(^|[\s(])\*([^*\n]+)\*/g, "$1<em>$2</em>");
  out = out.replace(/(^|[\s(])_([^_\n]+)_/g, "$1<em>$2</em>");
  out = out.replace(/~~([^~]+)~~/g, "<del>$1</del>");

  out = highlightMentions(out, people);

  return out.replace(/\u0000(\d+)\u0000/g, (_, i) => `<code>${codes[Number(i)]}</code>`);
}

export function renderMarkdown(src: string, people: Mentionable[] = []): string {
  const lines = esc(src ?? "").split("\n");
  const out: string[] = [];
  let i = 0;
  let listType: "ul" | "ol" | null = null;

  const closeList = () => {
    if (listType) {
      out.push(`</${listType}>`);
      listType = null;
    }
  };

  while (i < lines.length) {
    const line = lines[i];

    // fenced code
    const fence = line.match(/^\s*```(\w*)\s*$/);
    if (fence) {
      closeList();
      const body: string[] = [];
      i++;
      while (i < lines.length && !/^\s*```\s*$/.test(lines[i])) body.push(lines[i++]);
      i++; // consume the closing fence
      const lang = fence[1] ? ` class="language-${fence[1]}"` : "";
      out.push(`<pre><code${lang}>${body.join("\n")}</code></pre>`);
      continue;
    }

    if (!line.trim()) {
      closeList();
      i++;
      continue;
    }

    const heading = line.match(/^(#{1,6})\s+(.*)$/);
    if (heading) {
      closeList();
      const level = Math.min(heading[1].length + 1, 6);
      out.push(`<h${level}>${inline(heading[2], people)}</h${level}>`);
      i++;
      continue;
    }

    if (/^\s*(---|\*\*\*|___)\s*$/.test(line)) {
      closeList();
      out.push("<hr />");
      i++;
      continue;
    }

    const quote = line.match(/^\s*&gt;\s?(.*)$/);
    if (quote) {
      closeList();
      const body: string[] = [quote[1]];
      i++;
      while (i < lines.length) {
        const m = lines[i].match(/^\s*&gt;\s?(.*)$/);
        if (!m) break;
        body.push(m[1]);
        i++;
      }
      out.push(`<blockquote>${inline(body.join(" "), people)}</blockquote>`);
      continue;
    }

    const ul = line.match(/^\s*[-*+]\s+(.*)$/);
    const ol = line.match(/^\s*\d+[.)]\s+(.*)$/);
    if (ul || ol) {
      const want = ul ? "ul" : "ol";
      if (listType !== want) {
        closeList();
        out.push(`<${want}>`);
        listType = want;
      }
      out.push(`<li>${inline((ul ?? ol)![1], people)}</li>`);
      i++;
      continue;
    }

    // paragraph: absorb following non-structural lines
    closeList();
    const para: string[] = [line];
    i++;
    while (
      i < lines.length &&
      lines[i].trim() &&
      !/^\s*(```|#{1,6}\s|[-*+]\s|\d+[.)]\s|&gt;|---|\*\*\*|___)/.test(lines[i])
    ) {
      para.push(lines[i++]);
    }
    out.push(`<p>${inline(para.join("\n"), people)}</p>`);
  }

  closeList();
  return out.join("\n");
}
