import type { ReactNode } from "react";
import { forwardRef, useEffect, useRef, useState } from "react";
import { BRAND_ICONS } from "./brand-icons";
import type { ThreadStatus } from "./types";

// ------------------------------------------------------------------ icons ---
// Inline so the app stays self-contained and offline. Brand marks and the more
// intricate UI glyphs come from src/brand-icons.ts (see that file for sources);
// the rest are drawn here.

const paths: Record<string, ReactNode> = {
  robot: (
    <>
      <rect x="4" y="8" width="16" height="11" rx="3" fill="none" strokeWidth="1.8" />
      <circle cx="9" cy="13.5" r="1.4" />
      <circle cx="15" cy="13.5" r="1.4" />
      <path d="M12 8V4.5M9.6 4.5h4.8" fill="none" strokeWidth="1.8" />
    </>
  ),
  user: (
    <>
      <circle cx="12" cy="8" r="3.6" fill="none" strokeWidth="1.8" />
      <path d="M4.8 20a7.2 7.2 0 0 1 14.4 0" fill="none" strokeWidth="1.8" />
    </>
  ),
  terminal: (
    <>
      <rect x="3" y="4.5" width="18" height="15" rx="2.5" fill="none" strokeWidth="1.7" />
      <path d="m7.5 9.5 3 2.5-3 2.5M12.8 15h4" fill="none" strokeWidth="1.7" />
    </>
  ),
  hash: <path d="M9.2 3.5 7.8 20.5M16.2 3.5l-1.4 17M4 8.8h16M3.4 15.2h16" fill="none" strokeWidth="1.7" />,
  folder: (
    <path
      d="M3.5 6.8A2 2 0 0 1 5.5 5h3.3l1.8 2.2h8A2 2 0 0 1 20.5 9v8.2a2 2 0 0 1-2 2h-13a2 2 0 0 1-2-2V6.8Z"
      fill="none"
      strokeWidth="1.7"
    />
  ),
  plus: <path d="M12 5v14M5 12h14" fill="none" strokeWidth="2" />,
  check: <path d="m5 12.5 4.5 4.5L19 7" fill="none" strokeWidth="2.1" />,
  play: <path d="M7 4.5 19 12 7 19.5V4.5Z" />,
  pause: <path d="M8 4.5h3.2v15H8zM12.8 4.5H16v15h-3.2z" />,
  chevron: <path d="m8 4.5 7.5 7.5L8 19.5" fill="none" strokeWidth="1.9" />,
  copy: (
    <>
      <rect x="8.5" y="8.5" width="11" height="11" rx="2" fill="none" strokeWidth="1.7" />
      <path d="M15.5 5.5h-11v11" fill="none" strokeWidth="1.7" />
    </>
  ),
  x: <path d="M6 6 18 18M18 6 6 18" fill="none" strokeWidth="1.9" />,
  search: (
    <>
      <circle cx="11" cy="11" r="6.4" fill="none" strokeWidth="1.8" />
      <path d="m16 16 4.4 4.4" fill="none" strokeWidth="1.8" />
    </>
  ),
  key: (
    <>
      <circle cx="7.8" cy="16.2" r="3.6" fill="none" strokeWidth="1.7" />
      <path d="m10.4 13.6 8-8M16.2 7.8l2 2M14 10l1.6 1.6" fill="none" strokeWidth="1.7" />
    </>
  ),
  spark: <path d="M12 3.5 13.6 9 19 10.6 13.6 12.2 12 17.6 10.4 12.2 5 10.6 10.4 9 12 3.5Z" />,
  file: (
    <path
      d="M13.5 3.5H7a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V9l-5.5-5.5Zm0 0V9H19"
      fill="none"
      strokeWidth="1.7"
    />
  ),
  git: (
    <>
      <circle cx="7" cy="6.5" r="2.4" fill="none" strokeWidth="1.7" />
      <circle cx="7" cy="17.5" r="2.4" fill="none" strokeWidth="1.7" />
      <circle cx="17" cy="12" r="2.4" fill="none" strokeWidth="1.7" />
      <path d="M7 9v6M9.4 12H14.6" fill="none" strokeWidth="1.7" />
    </>
  ),
  trash: (
    <path
      d="M4.8 6.8h14.4M9.5 6.8V5a1.2 1.2 0 0 1 1.2-1.2h2.6A1.2 1.2 0 0 1 14.5 5v1.8M6.6 6.8l.8 12a1.6 1.6 0 0 0 1.6 1.5h6a1.6 1.6 0 0 0 1.6-1.5l.8-12"
      fill="none"
      strokeWidth="1.6"
    />
  ),
};

export function Icon({
  name,
  size = 16,
  className = "",
}: {
  name: string;
  size?: number;
  className?: string;
}) {
  const brand = BRAND_ICONS[name];
  // A filled brand mark stroked as well as filled comes out visibly bolder, so
  // the two families are rendered with different paint settings.
  const solid = brand?.solid ?? false;

  return (
    <svg
      viewBox="0 0 24 24"
      width={size}
      height={size}
      fill={solid ? "currentColor" : "none"}
      stroke={solid ? "none" : "currentColor"}
      strokeWidth={brand && !solid ? 2 : 1.8}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={`shrink-0 ${className}`}
      aria-hidden
    >
      {brand ? brand.node : (paths[name] ?? paths.robot)}
    </svg>
  );
}

// ---------------------------------------------------------------- avatars ---

/**
 * Each agent owns a colour: its avatar, and the edge and tint of everything it
 * says. Two assistants in a room read as two different voices at a glance.
 * Set explicitly in agent settings; agents with no choice fall back to a stable
 * hash of their name so a fresh room is still legible.
 */
export const AGENT_COLORS = [
  "indigo",
  "teal",
  "fuchsia",
  "amber",
  "sky",
  "lime",
  "orange",
  "violet",
  "rose",
  "emerald",
  "slate",
] as const;

export type AgentColor = (typeof AGENT_COLORS)[number];

interface Tone {
  avatar: string;
  edge: string;
  tint: string;
  swatch: string;
}

// Written out rather than interpolated: Tailwind only emits classes it can see
// as complete literals in the source.
const TONES: Record<AgentColor, Tone> = {
  indigo: {
    avatar: "bg-indigo-100 text-indigo-700 ring-indigo-300/60 dark:bg-indigo-500/20 dark:text-indigo-200 dark:ring-indigo-400/25",
    edge: "border-l-indigo-400 dark:border-l-indigo-500",
    tint: "bg-indigo-50/60 dark:bg-indigo-500/[0.06]",
    swatch: "bg-indigo-400",
  },
  teal: {
    avatar: "bg-teal-100 text-teal-700 ring-teal-300/60 dark:bg-teal-500/20 dark:text-teal-200 dark:ring-teal-400/25",
    edge: "border-l-teal-400 dark:border-l-teal-500",
    tint: "bg-teal-50/60 dark:bg-teal-500/[0.06]",
    swatch: "bg-teal-400",
  },
  fuchsia: {
    avatar: "bg-fuchsia-100 text-fuchsia-700 ring-fuchsia-300/60 dark:bg-fuchsia-500/20 dark:text-fuchsia-200 dark:ring-fuchsia-400/25",
    edge: "border-l-fuchsia-400 dark:border-l-fuchsia-500",
    tint: "bg-fuchsia-50/60 dark:bg-fuchsia-500/[0.06]",
    swatch: "bg-fuchsia-400",
  },
  amber: {
    avatar: "bg-amber-100 text-amber-700 ring-amber-300/60 dark:bg-amber-500/20 dark:text-amber-200 dark:ring-amber-400/25",
    edge: "border-l-amber-400 dark:border-l-amber-500",
    tint: "bg-amber-50/60 dark:bg-amber-500/[0.06]",
    swatch: "bg-amber-400",
  },
  sky: {
    avatar: "bg-sky-100 text-sky-700 ring-sky-300/60 dark:bg-sky-500/20 dark:text-sky-200 dark:ring-sky-400/25",
    edge: "border-l-sky-400 dark:border-l-sky-500",
    tint: "bg-sky-50/60 dark:bg-sky-500/[0.06]",
    swatch: "bg-sky-400",
  },
  lime: {
    avatar: "bg-lime-100 text-lime-700 ring-lime-300/60 dark:bg-lime-500/20 dark:text-lime-200 dark:ring-lime-400/25",
    edge: "border-l-lime-400 dark:border-l-lime-500",
    tint: "bg-lime-50/60 dark:bg-lime-500/[0.06]",
    swatch: "bg-lime-400",
  },
  orange: {
    avatar: "bg-orange-100 text-orange-700 ring-orange-300/60 dark:bg-orange-500/20 dark:text-orange-200 dark:ring-orange-400/25",
    edge: "border-l-orange-400 dark:border-l-orange-500",
    tint: "bg-orange-50/60 dark:bg-orange-500/[0.06]",
    swatch: "bg-orange-400",
  },
  violet: {
    avatar: "bg-violet-100 text-violet-700 ring-violet-300/60 dark:bg-violet-500/20 dark:text-violet-200 dark:ring-violet-400/25",
    edge: "border-l-violet-400 dark:border-l-violet-500",
    tint: "bg-violet-50/60 dark:bg-violet-500/[0.06]",
    swatch: "bg-violet-400",
  },
  rose: {
    avatar: "bg-rose-100 text-rose-700 ring-rose-300/60 dark:bg-rose-500/20 dark:text-rose-200 dark:ring-rose-400/25",
    edge: "border-l-rose-400 dark:border-l-rose-500",
    tint: "bg-rose-50/60 dark:bg-rose-500/[0.06]",
    swatch: "bg-rose-400",
  },
  emerald: {
    avatar: "bg-emerald-100 text-emerald-700 ring-emerald-300/60 dark:bg-emerald-500/20 dark:text-emerald-200 dark:ring-emerald-400/25",
    edge: "border-l-emerald-400 dark:border-l-emerald-500",
    tint: "bg-emerald-50/60 dark:bg-emerald-500/[0.06]",
    swatch: "bg-emerald-400",
  },
  slate: {
    avatar: "bg-chip text-soft ring-line",
    edge: "border-l-line",
    tint: "bg-transparent",
    swatch: "bg-faint",
  },
};

/** An explicit choice wins; otherwise derive one from the name. */
export function agentTone(name: string, color?: string | null): Tone {
  if (color && color in TONES) return TONES[color as AgentColor];
  let h = 0;
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) >>> 0;
  const auto = AGENT_COLORS.slice(0, -1); // never auto-assign the neutral
  return TONES[auto[h % auto.length]];
}

export function swatchFor(color: AgentColor): string {
  return TONES[color].swatch;
}

export function Avatar({
  name,
  icon,
  color,
  size = 26,
}: {
  name: string;
  icon?: string;
  color?: string | null;
  size?: number;
}) {
  return (
    <span
      className={`inline-flex shrink-0 items-center justify-center rounded-lg ring-1 ring-inset ${agentTone(name, color).avatar}`}
      style={{ width: size, height: size }}
      title={name}
    >
      {icon ? (
        <Icon name={icon} size={Math.round(size * 0.58)} />
      ) : (
        <span className="text-[11px] font-semibold uppercase">{name.slice(0, 2)}</span>
      )}
    </span>
  );
}

/** Row of selectable colour swatches for the agent settings form. */
export function ColorPicker({
  value,
  onChange,
}: {
  value: string;
  onChange: (c: string) => void;
}) {
  return (
    <div className="flex flex-wrap gap-1.5">
      {AGENT_COLORS.map((c) => (
        <button
          key={c}
          type="button"
          title={c}
          onClick={() => onChange(c)}
          className={`h-6 w-6 rounded-lg transition ${swatchFor(c)} ${
            value === c
              ? "ring-2 ring-accent ring-offset-2 ring-offset-modal"
              : "ring-1 ring-black/10 hover:scale-110 dark:ring-white/15"
          }`}
        />
      ))}
    </div>
  );
}

// ----------------------------------------------------------------- badges ---

const TAG_TONE: Record<string, string> = {
  sky: "bg-sky-100 text-sky-800 ring-sky-300/60 dark:bg-sky-500/12 dark:text-sky-300 dark:ring-sky-400/25",
  rose: "bg-rose-100 text-rose-800 ring-rose-300/60 dark:bg-rose-500/12 dark:text-rose-300 dark:ring-rose-400/25",
  violet:
    "bg-violet-100 text-violet-800 ring-violet-300/60 dark:bg-violet-500/12 dark:text-violet-300 dark:ring-violet-400/25",
  amber:
    "bg-amber-100 text-amber-800 ring-amber-300/60 dark:bg-amber-500/12 dark:text-amber-300 dark:ring-amber-400/25",
  emerald:
    "bg-emerald-100 text-emerald-800 ring-emerald-300/60 dark:bg-emerald-500/12 dark:text-emerald-300 dark:ring-emerald-400/25",
  cyan: "bg-cyan-100 text-cyan-800 ring-cyan-300/60 dark:bg-cyan-500/12 dark:text-cyan-300 dark:ring-cyan-400/25",
  orange:
    "bg-orange-100 text-orange-800 ring-orange-300/60 dark:bg-orange-500/12 dark:text-orange-300 dark:ring-orange-400/25",
  slate: "bg-chip text-soft ring-line dark:bg-chip dark:text-soft dark:ring-line",
};

/** Solid version of a tag colour, for the stripe down the side of a list row. */
const TAG_STRIPE: Record<string, string> = {
  sky: "bg-sky-400 dark:bg-sky-500",
  rose: "bg-rose-400 dark:bg-rose-500",
  violet: "bg-violet-400 dark:bg-violet-500",
  amber: "bg-amber-400 dark:bg-amber-500",
  emerald: "bg-emerald-400 dark:bg-emerald-500",
  cyan: "bg-cyan-400 dark:bg-cyan-500",
  orange: "bg-orange-400 dark:bg-orange-500",
  slate: "bg-faint",
};

export function tagStripe(color: string): string {
  return TAG_STRIPE[color] ?? TAG_STRIPE.slate;
}

export function TagChip({ color, label }: { color: string; label: string }) {
  return (
    <span
      className={`inline-flex items-center rounded-md px-1.5 py-px text-[10.5px] font-semibold tracking-wide uppercase ring-1 ring-inset ${
        TAG_TONE[color] ?? TAG_TONE.slate
      }`}
    >
      {label}
    </span>
  );
}

const STATUS_TONE: Record<ThreadStatus, string> = {
  OPEN: "bg-chip text-soft",
  AWAITING_REPLIES:
    "bg-indigo-100 text-indigo-800 dark:bg-indigo-500/12 dark:text-indigo-300",
  NEEDS_CODER: "bg-amber-200/70 text-amber-900 dark:bg-amber-500/15 dark:text-amber-300",
  RESOLVED: "bg-emerald-100 text-emerald-800 dark:bg-emerald-500/12 dark:text-emerald-300",
  BLOCKED: "bg-rose-100 text-rose-800 dark:bg-rose-500/12 dark:text-rose-300",
  WONTFIX: "bg-chip text-muted",
};

const STATUS_LABEL: Record<ThreadStatus, string> = {
  OPEN: "Open",
  AWAITING_REPLIES: "Awaiting replies",
  NEEDS_CODER: "Replied",
  RESOLVED: "Resolved",
  BLOCKED: "Blocked",
  WONTFIX: "Won't fix",
};

export function StatusChip({ status }: { status: ThreadStatus }) {
  return (
    <span
      className={`inline-flex items-center rounded-md px-1.5 py-px text-[10.5px] font-medium ${STATUS_TONE[status]}`}
    >
      {STATUS_LABEL[status]}
    </span>
  );
}

/**
 * REFUTED was renamed to CLEARED — "refuted" names a move in an argument, not
 * the conclusion a reviewer reached. Both are still styled and both still read
 * as CLEARED, because messages written before the rename keep the old value.
 */
export function verdictLabel(verdict: string): string {
  return (verdict === "REFUTED" ? "CLEARED" : verdict).replace(/_/g, " ");
}

const VERDICT_TONE: Record<string, string> = {
  CONFIRMED: "bg-rose-100 text-rose-800 ring-rose-300/60 dark:bg-rose-500/15 dark:text-rose-300 dark:ring-rose-400/30",
  CLEARED:
    "bg-emerald-100 text-emerald-800 ring-emerald-300/60 dark:bg-emerald-500/15 dark:text-emerald-300 dark:ring-emerald-400/30",
  REFUTED:
    "bg-emerald-100 text-emerald-800 ring-emerald-300/60 dark:bg-emerald-500/15 dark:text-emerald-300 dark:ring-emerald-400/30",
  UNCERTAIN:
    "bg-amber-100 text-amber-800 ring-amber-300/60 dark:bg-amber-500/15 dark:text-amber-300 dark:ring-amber-400/30",
  ANSWERED:
    "bg-emerald-100 text-emerald-800 ring-emerald-300/60 dark:bg-emerald-500/15 dark:text-emerald-300 dark:ring-emerald-400/30",
  NEEDS_INFO:
    "bg-amber-100 text-amber-800 ring-amber-300/60 dark:bg-amber-500/15 dark:text-amber-300 dark:ring-amber-400/30",
  APPROVED:
    "bg-emerald-100 text-emerald-800 ring-emerald-300/60 dark:bg-emerald-500/15 dark:text-emerald-300 dark:ring-emerald-400/30",
  CONCERNS:
    "bg-amber-100 text-amber-800 ring-amber-300/60 dark:bg-amber-500/15 dark:text-amber-300 dark:ring-amber-400/30",
  REJECTED:
    "bg-rose-100 text-rose-800 ring-rose-300/60 dark:bg-rose-500/15 dark:text-rose-300 dark:ring-rose-400/30",
};

export function VerdictChip({ verdict, severity }: { verdict: string; severity?: string | null }) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <span
        className={`rounded-md px-1.5 py-px text-[10.5px] font-semibold tracking-wide ring-1 ring-inset ${
          VERDICT_TONE[verdict] ?? "bg-chip text-body ring-line"
        }`}
      >
        {verdictLabel(verdict)}
      </span>
      {severity && (
        <span className="text-[10.5px] font-semibold tracking-wide text-muted">{severity}</span>
      )}
    </span>
  );
}

// -------------------------------------------------------------- controls ---

export function Button({
  children,
  onClick,
  variant = "ghost",
  size = "md",
  disabled,
  title,
  type = "button",
  className = "",
}: {
  children: ReactNode;
  onClick?: () => void;
  variant?: "primary" | "ghost" | "danger" | "subtle";
  size?: "sm" | "md";
  disabled?: boolean;
  title?: string;
  type?: "button" | "submit";
  className?: string;
}) {
  const variants = {
    primary:
      "bg-accent text-on-accent hover:brightness-110 font-medium shadow-card disabled:bg-chip disabled:text-faint disabled:shadow-none",
    ghost: "bg-card text-body hover:bg-hover ring-1 ring-inset ring-line shadow-card",
    subtle: "text-muted hover:text-strong hover:bg-hover",
    danger:
      "bg-rose-50 text-rose-700 hover:bg-rose-100 ring-1 ring-inset ring-rose-300/60 dark:bg-rose-500/12 dark:text-rose-300 dark:hover:bg-rose-500/20 dark:ring-rose-500/25",
  };
  return (
    <button
      type={type}
      title={title}
      disabled={disabled}
      onClick={onClick}
      className={`inline-flex items-center justify-center gap-1.5 rounded-lg transition disabled:cursor-not-allowed disabled:opacity-60 ${
        size === "sm" ? "px-2 py-1 text-[12px]" : "px-3 py-1.5"
      } ${variants[variant]} ${className}`}
    >
      {children}
    </button>
  );
}

export function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: ReactNode;
}) {
  return (
    <label className="block">
      <div className="mb-1 flex items-baseline gap-2">
        <span className="text-[11.5px] font-semibold tracking-wide text-soft uppercase">
          {label}
        </span>
        {hint && <span className="text-[11.5px] text-muted">{hint}</span>}
      </div>
      {children}
    </label>
  );
}

const inputBase =
  "rounded-lg bg-field text-body px-2.5 py-1.5 ring-1 ring-inset ring-line outline-none placeholder:text-faint focus:ring-2 focus:ring-accent/50";

/**
 * Tailwind decides which of two conflicting utilities wins by their order in
 * the generated stylesheet, not by the order they appear in the class
 * attribute. So a base `w-full` beats a caller's `w-20` or `flex-1` and the
 * caller silently gets full width — which collapsed the sibling fields in the
 * context row to nothing. Only apply the default width when the caller has not
 * asked for one.
 */
function sized(className = ""): string {
  const callerSetsWidth = /(^|\s)(w-|min-w-|max-w-|flex-1|flex-\[|basis-|grow)/.test(className);
  return `${inputBase} ${callerSetsWidth ? "" : "w-full"} ${className}`;
}

export function Input(props: React.InputHTMLAttributes<HTMLInputElement>) {
  return <input {...props} className={sized(props.className)} />;
}

export const Textarea = forwardRef<
  HTMLTextAreaElement,
  React.TextareaHTMLAttributes<HTMLTextAreaElement>
>(function Textarea(props, ref) {
  return <textarea {...props} ref={ref} className={sized(props.className)} />;
});

export function Select(props: React.SelectHTMLAttributes<HTMLSelectElement>) {
  return (
    <select
      {...props}
      className={`${sized(props.className)} appearance-none bg-[length:12px] bg-[right_0.6rem_center] bg-no-repeat pr-8`}
      style={{
        backgroundImage:
          "url(\"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 12 12'%3E%3Cpath d='M2 4.5 6 8.5 10 4.5' fill='none' stroke='%236c7488' stroke-width='1.6' stroke-linecap='round'/%3E%3C/svg%3E\")",
      }}
    />
  );
}

export function Modal({
  title,
  subtitle,
  onClose,
  children,
  wide,
}: {
  title: string;
  subtitle?: string;
  onClose: () => void;
  children: ReactNode;
  wide?: boolean;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-slate-900/25 p-8 backdrop-blur-sm dark:bg-black/55"
      onMouseDown={(e) => e.target === e.currentTarget && onClose()}
    >
      <div
        className={`my-auto w-full rounded-2xl bg-modal shadow-pop ring-1 ring-line ${
          wide ? "max-w-3xl" : "max-w-lg"
        }`}
      >
        <div className="flex items-start justify-between gap-4 border-b border-line px-5 py-3.5">
          <div>
            <h2 className="font-semibold text-strong">{title}</h2>
            {subtitle && <p className="mt-0.5 text-[12.5px] text-muted">{subtitle}</p>}
          </div>
          <Button variant="subtle" size="sm" onClick={onClose} title="Close">
            <Icon name="x" size={15} />
          </Button>
        </div>
        <div className="px-5 py-4">{children}</div>
      </div>
    </div>
  );
}

export function CopyButton({ text, label = "Copy" }: { text: string; label?: string }) {
  const [done, setDone] = useState(false);
  const timer = useRef<number | undefined>(undefined);
  useEffect(() => () => window.clearTimeout(timer.current), []);

  return (
    <Button
      size="sm"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(text);
          setDone(true);
          timer.current = window.setTimeout(() => setDone(false), 1600);
        } catch {
          setDone(false);
        }
      }}
    >
      <Icon name={done ? "check" : "copy"} size={13} />
      {done ? "Copied" : label}
    </Button>
  );
}

export function Empty({
  icon,
  title,
  children,
}: {
  icon: string;
  title: string;
  children?: ReactNode;
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 p-10 text-center">
      <div className="rounded-2xl bg-card p-3.5 text-faint shadow-card ring-1 ring-line">
        <Icon name={icon} size={22} />
      </div>
      <div>
        <p className="font-medium text-body">{title}</p>
        {children && <div className="mt-1 max-w-sm text-[12.5px] text-muted">{children}</div>}
      </div>
    </div>
  );
}

/** Relative time, because absolute timestamps read as noise in a chat log. */
export function ago(iso: string): string {
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return "";
  const s = Math.max(0, (Date.now() - then) / 1000);
  if (s < 45) return "just now";
  if (s < 3600) return `${Math.round(s / 60)}m ago`;
  if (s < 86400) return `${Math.round(s / 3600)}h ago`;
  if (s < 604800) return `${Math.round(s / 86400)}d ago`;
  return new Date(iso).toLocaleDateString();
}
