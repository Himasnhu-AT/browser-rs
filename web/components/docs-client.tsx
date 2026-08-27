"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import {
  CaretDown,
  Check,
  Copy,
  List,
  MagnifyingGlass,
  X,
} from "@phosphor-icons/react";

import { FLAT, NAV, type NavLeaf } from "@/lib/nav";

/* -------------------------------------------------------------------------- */
/* Search. A real filter over the nav registry, not a decorative input.        */
/* Motion budget is 2, so this animates nothing; it opens and closes.          */
/* -------------------------------------------------------------------------- */

export function Search() {
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && e.key === "k") {
        e.preventDefault();
        setOpen(true);
      }
      if (e.key === "Escape") setOpen(false);
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  useEffect(() => {
    if (open) inputRef.current?.focus();
  }, [open]);

  const results = useMemo<NavLeaf[]>(() => {
    const needle = q.trim().toLowerCase();
    if (!needle) return [];
    return FLAT.filter(
      (p) =>
        p.title.toLowerCase().includes(needle) ||
        p.blurb.toLowerCase().includes(needle),
    ).slice(0, 8);
  }, [q]);

  return (
    <>
      <button
        onClick={() => setOpen(true)}
        className="flex h-9 w-full max-w-[260px] items-center gap-2 rounded-[10px] border px-3 text-left text-[13.5px] transition-colors"
        style={{
          borderColor: "var(--border-strong)",
          color: "var(--fg-faint)",
          background: "var(--bg-elev)",
        }}
        aria-label="Search the documentation"
      >
        <MagnifyingGlass size={15} />
        <span className="flex-1">Search</span>
        <kbd
          className="hidden font-mono text-[11px] sm:inline"
          style={{ color: "var(--fg-faint)" }}
        >
          &#8984;K
        </kbd>
      </button>

      {open && (
        <div
          className="fixed inset-0 z-[70] flex items-start justify-center px-4 pt-[12vh]"
          style={{ background: "color-mix(in oklab, var(--bg-sunk) 76%, transparent)" }}
          onClick={() => setOpen(false)}
        >
          <div
            className="w-full max-w-xl overflow-hidden rounded-[16px] border shadow-2xl"
            style={{ borderColor: "var(--border-strong)", background: "var(--bg-elev)" }}
            onClick={(e) => e.stopPropagation()}
          >
            <div
              className="flex items-center gap-3 border-b px-4"
              style={{ borderColor: "var(--border)" }}
            >
              <MagnifyingGlass size={17} style={{ color: "var(--fg-faint)" }} />
              <input
                ref={inputRef}
                value={q}
                onChange={(e) => setQ(e.target.value)}
                placeholder="Search pages"
                aria-label="Search pages"
                className="h-12 flex-1 bg-transparent text-[15px] outline-none"
                style={{ color: "var(--fg)" }}
              />
              <button onClick={() => setOpen(false)} aria-label="Close search">
                <X size={16} style={{ color: "var(--fg-faint)" }} />
              </button>
            </div>

            {/* Empty state, not a blank panel. */}
            {q.trim() === "" ? (
              <p
                className="px-4 py-6 text-[13.5px]"
                style={{ color: "var(--fg-faint)" }}
              >
                Type to search {FLAT.length} pages by title or summary.
              </p>
            ) : results.length === 0 ? (
              <p
                className="px-4 py-6 text-[13.5px]"
                style={{ color: "var(--fg-faint)" }}
              >
                Nothing matches &ldquo;{q}&rdquo;. Try a shorter word.
              </p>
            ) : (
              <ul className="max-h-[52vh] overflow-y-auto p-2">
                {results.map((r) => (
                  <li key={r.href}>
                    <a
                      href={r.href}
                      className="block rounded-[10px] px-3 py-2.5 transition-colors hover:bg-[var(--bg-sunk)]"
                    >
                      <span className="text-[14.5px] font-semibold">{r.title}</span>
                      <span
                        className="mt-0.5 block text-[13px] leading-snug"
                        style={{ color: "var(--fg-muted)" }}
                      >
                        {r.blurb}
                      </span>
                    </a>
                  </li>
                ))}
              </ul>
            )}
          </div>
        </div>
      )}
    </>
  );
}

/* -------------------------------------------------------------------------- */
/* Sidebar. Collapsible groups, current page marked. Mobile: a drawer.         */
/* -------------------------------------------------------------------------- */

export function Sidebar({ current }: { current: string }) {
  const [openDrawer, setOpenDrawer] = useState(false);

  const tree = (
    <nav className="text-[14px]">
      {NAV.map((group) => (
        <Group key={group.title} group={group} current={current} />
      ))}
    </nav>
  );

  return (
    <>
      {/* Mobile trigger */}
      <button
        onClick={() => setOpenDrawer(true)}
        className="inline-flex h-9 items-center gap-2 rounded-[10px] border px-3 text-[13.5px] lg:hidden"
        style={{ borderColor: "var(--border-strong)" }}
      >
        <List size={16} />
        Menu
      </button>

      <div className="hidden lg:block">{tree}</div>

      {openDrawer && (
        <div
          className="fixed inset-0 z-[70] lg:hidden"
          style={{ background: "color-mix(in oklab, var(--bg-sunk) 76%, transparent)" }}
          onClick={() => setOpenDrawer(false)}
        >
          <div
            className="h-full w-[300px] max-w-[86vw] overflow-y-auto border-r p-5"
            style={{ borderColor: "var(--border)", background: "var(--bg)" }}
            onClick={(e) => e.stopPropagation()}
          >
            <div className="mb-5 flex justify-end">
              <button onClick={() => setOpenDrawer(false)} aria-label="Close menu">
                <X size={18} />
              </button>
            </div>
            {tree}
          </div>
        </div>
      )}
    </>
  );
}

function Group({
  group,
  current,
}: {
  group: (typeof NAV)[number];
  current: string;
}) {
  const holdsCurrent = group.items.some((i) => i.href === current);
  const [open, setOpen] = useState(holdsCurrent);

  return (
    <div className="mb-1">
      <button
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center justify-between rounded-[10px] px-2 py-1.5 text-left text-[12px] font-semibold tracking-[0.06em] uppercase transition-colors"
        style={{ color: "var(--fg-faint)" }}
        aria-expanded={open}
      >
        {group.title}
        <CaretDown
          size={12}
          className="transition-transform duration-200"
          style={{ transform: open ? "rotate(0deg)" : "rotate(-90deg)" }}
        />
      </button>

      {open && (
        <ul
          className="mt-1 ml-2 border-l"
          style={{ borderColor: "var(--border)" }}
        >
          {group.items.map((item) => {
            const active = item.href === current;
            return (
              <li key={item.href}>
                <a
                  href={item.href}
                  aria-current={active ? "page" : undefined}
                  className="-ml-px block border-l py-1.5 pl-3.5 transition-colors"
                  style={{
                    borderColor: active ? "var(--accent)" : "transparent",
                    color: active ? "var(--accent-ink)" : "var(--fg-muted)",
                    fontWeight: active ? 600 : 400,
                  }}
                >
                  {item.title}
                </a>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}

/* -------------------------------------------------------------------------- */
/* Right-hand table of contents, with scrollspy via IntersectionObserver.      */
/* No scroll listener: that is a hard ban and also the janky way to do it.     */
/* -------------------------------------------------------------------------- */

export function Toc({ headings }: { headings: { id: string; text: string }[] }) {
  const [active, setActive] = useState(headings[0]?.id ?? "");

  useEffect(() => {
    if (headings.length === 0) return;
    const seen = new Map<string, boolean>();
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) seen.set(e.target.id, e.isIntersecting);
        const first = headings.find((h) => seen.get(h.id));
        if (first) setActive(first.id);
      },
      { rootMargin: "-88px 0px -68% 0px", threshold: 0 },
    );
    for (const h of headings) {
      const el = document.getElementById(h.id);
      if (el) io.observe(el);
    }
    return () => io.disconnect();
  }, [headings]);

  if (headings.length === 0) return null;

  return (
    <nav className="text-[13.5px]">
      <p
        className="mb-3 text-[12px] font-semibold tracking-[0.06em] uppercase"
        style={{ color: "var(--fg-faint)" }}
      >
        On this page
      </p>
      <ul className="border-l" style={{ borderColor: "var(--border)" }}>
        {headings.map((h) => {
          const on = h.id === active;
          return (
            <li key={h.id}>
              <a
                href={`#${h.id}`}
                className="-ml-px block border-l py-1.5 pl-3.5 leading-snug transition-colors"
                style={{
                  borderColor: on ? "var(--accent)" : "transparent",
                  color: on ? "var(--accent-ink)" : "var(--fg-muted)",
                }}
              >
                {h.text}
              </a>
            </li>
          );
        })}
      </ul>
    </nav>
  );
}

/* -------------------------------------------------------------------------- */
/* Code block with a copy button.                                              */
/* Motivation for the only animation here: feedback. The tick confirms the     */
/* clipboard write actually happened.                                          */
/* -------------------------------------------------------------------------- */

export function CopyButton({ text }: { text: string }) {
  const [done, setDone] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(text);
      setDone(true);
      setTimeout(() => setDone(false), 1600);
    } catch {
      // Clipboard is blocked in some embeds. Leave the button unchanged
      // rather than claiming a copy that did not happen.
      setDone(false);
    }
  }

  return (
    <button
      onClick={copy}
      aria-label={done ? "Copied" : "Copy code"}
      className="absolute top-2.5 right-2.5 inline-flex h-8 w-8 items-center justify-center rounded-[10px] border transition-colors"
      style={{
        borderColor: "var(--border)",
        background: "var(--bg-elev)",
        color: done ? "var(--accent-ink)" : "var(--fg-faint)",
      }}
    >
      {done ? <Check size={14} /> : <Copy size={14} />}
    </button>
  );
}
