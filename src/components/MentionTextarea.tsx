import { useMemo, useRef, useState } from "react";
import { Avatar, Textarea } from "../ui";
import type { Agent } from "../types";

/**
 * A textarea that completes `@name` against the room's agents.
 *
 * The popup is anchored to the field rather than the caret. Caret-following
 * requires mirroring the textarea into a hidden div to measure where the text
 * lands, which breaks on wrapping, scrolling and font changes; anchoring to the
 * field is stable and reads fine at this size.
 */
export function MentionTextarea({
  value,
  onChange,
  agents,
  onKeyDown,
  ...rest
}: {
  value: string;
  onChange: (next: string) => void;
  agents: Agent[];
  onKeyDown?: (e: React.KeyboardEvent<HTMLTextAreaElement>) => void;
} & Omit<
  React.TextareaHTMLAttributes<HTMLTextAreaElement>,
  "value" | "onChange" | "onKeyDown"
>) {
  const ref = useRef<HTMLTextAreaElement>(null);
  // null when the caret is not in an @word; otherwise the partial name.
  const [query, setQuery] = useState<string | null>(null);
  const [active, setActive] = useState(0);

  const candidates = useMemo(() => {
    if (query === null) return [];
    const q = query.toLowerCase();
    return agents
      .filter((a) => !a.revoked_at && a.role !== "HUMAN")
      .filter((a) => a.name.toLowerCase().startsWith(q))
      .slice(0, 6);
  }, [agents, query]);

  const open = query !== null && candidates.length > 0;

  /** The @word the caret currently sits inside, if any. */
  function queryAt(text: string, caret: number): string | null {
    const upto = text.slice(0, caret);
    const m = upto.match(/(?:^|[^a-zA-Z0-9@])@([a-zA-Z0-9_-]*)$/);
    return m ? m[1] : null;
  }

  function complete(name: string) {
    const el = ref.current;
    if (!el) return;
    // Read the text and the caret from the same place. Slicing the React
    // `value` prop with a DOM caret offset corrupts the insertion whenever the
    // two are momentarily out of step, which they are while typing quickly.
    const text = el.value;
    const caret = el.selectionStart ?? text.length;
    const upto = text.slice(0, caret);
    // Replace the partial @word, not the whole line.
    const replaced = upto.replace(/@([a-zA-Z0-9_-]*)$/, `@${name} `);
    const next = replaced + text.slice(caret);
    onChange(next);
    setQuery(null);
    // Put the caret after the inserted name rather than at the end of the text.
    // Write through to the DOM as well, so the caret lands correctly even
    // before React has re-rendered with the new value.
    el.value = next;
    const at = replaced.length;
    el.setSelectionRange(at, at);
    requestAnimationFrame(() => {
      el.focus();
      el.setSelectionRange(at, at);
    });
  }

  return (
    <div className="relative">
      {open && (
        <div className="absolute bottom-full left-0 z-20 mb-1 w-60 overflow-hidden rounded-xl bg-modal shadow-pop ring-1 ring-line">
          {candidates.map((a, i) => (
            <button
              key={a.id}
              // mousedown, not click: click fires after blur and the popup is
              // already gone by then.
              onMouseDown={(e) => {
                e.preventDefault();
                complete(a.name);
              }}
              onMouseEnter={() => setActive(i)}
              className={`flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-[12.5px] transition ${
                i === active ? "bg-accent-soft text-accent-text" : "text-base hover:bg-hover"
              }`}
            >
              <Avatar name={a.name} icon={a.icon} color={a.color} size={18} />
              <span className="font-medium">{a.name}</span>
              <span className="ml-auto text-[11px] text-faint">{a.profile_label ?? a.role}</span>
            </button>
          ))}
        </div>
      )}

      <Textarea
        {...rest}
        ref={ref}
        value={value}
        onChange={(e) => {
          onChange(e.target.value);
          setQuery(queryAt(e.target.value, e.target.selectionStart ?? 0));
          setActive(0);
        }}
        onBlur={() => setQuery(null)}
        onKeyUp={(e) => {
          // Arrow keys and clicks move the caret without changing the text.
          const el = e.currentTarget;
          if (["ArrowLeft", "ArrowRight", "Home", "End"].includes(e.key)) {
            setQuery(queryAt(el.value, el.selectionStart ?? 0));
          }
        }}
        onKeyDown={(e) => {
          if (open) {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setActive((i) => (i + 1) % candidates.length);
              return;
            }
            if (e.key === "ArrowUp") {
              e.preventDefault();
              setActive((i) => (i - 1 + candidates.length) % candidates.length);
              return;
            }
            if (e.key === "Escape") {
              e.preventDefault();
              setQuery(null);
              return;
            }
            // ⌘↵ still submits; a bare Enter picks the highlighted name.
            if ((e.key === "Enter" && !e.metaKey && !e.ctrlKey) || e.key === "Tab") {
              e.preventDefault();
              complete(candidates[active].name);
              return;
            }
          }
          onKeyDown?.(e);
        }}
      />
    </div>
  );
}
