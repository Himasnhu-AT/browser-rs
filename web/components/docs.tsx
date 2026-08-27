import { ArrowRight, ArrowLeft, Info, Warning } from "@phosphor-icons/react/dist/ssr";

import { Mark } from "@/components/mark";
import { CopyButton, Search, Sidebar, Toc } from "@/components/docs-client";
import { neighbours } from "@/lib/nav";

export type Heading = { id: string; text: string };

/* -------------------------------------------------------------------------- */
/* The three-column shell. Sidebar, content, table of contents.                */
/* -------------------------------------------------------------------------- */

export function DocsPage({
  href,
  title,
  lede,
  headings,
  children,
}: {
  href: string;
  title: string;
  lede: string;
  headings: Heading[];
  children: React.ReactNode;
}) {
  const { prev, next } = neighbours(href);

  return (
    <div style={{ background: "var(--bg)", color: "var(--fg)" }}>
      <TopBar current={href} />

      <div className="mx-auto flex w-full max-w-[1560px] gap-0 px-4 md:px-6">
        {/* Left rail */}
        <aside
          className="sticky top-14 hidden h-[calc(100dvh-3.5rem)] w-[262px] shrink-0 overflow-y-auto border-r py-8 pr-5 lg:block"
          style={{ borderColor: "var(--border)" }}
        >
          <Sidebar current={href} />
        </aside>

        {/* Content */}
        <main className="min-w-0 flex-1 px-0 py-10 lg:px-10 xl:px-14">
          <article className="prose-doc mx-auto max-w-[46rem]">
            <h1 className="text-3xl font-extrabold tracking-tighter md:text-4xl">
              {title}
            </h1>
            <p
              className="mt-3 mb-10 text-[17px] leading-relaxed"
              style={{ color: "var(--fg-muted)" }}
            >
              {lede}
            </p>
            {children}
          </article>

          <nav className="mx-auto mt-16 flex max-w-[46rem] gap-4 border-t pt-8"
               style={{ borderColor: "var(--border)" }}>
            {prev ? (
              <a
                href={prev.href}
                className="group flex flex-1 flex-col gap-1 rounded-[16px] border p-4 transition-colors"
                style={{ borderColor: "var(--border)" }}
              >
                <span
                  className="inline-flex items-center gap-1.5 text-[12.5px]"
                  style={{ color: "var(--fg-faint)" }}
                >
                  <ArrowLeft size={12} />
                  Previous
                </span>
                <span className="text-[15px] font-semibold">{prev.title}</span>
              </a>
            ) : (
              <span className="flex-1" />
            )}
            {next ? (
              <a
                href={next.href}
                className="group flex flex-1 flex-col items-end gap-1 rounded-[16px] border p-4 text-right transition-colors"
                style={{ borderColor: "var(--border)" }}
              >
                <span
                  className="inline-flex items-center gap-1.5 text-[12.5px]"
                  style={{ color: "var(--fg-faint)" }}
                >
                  Next
                  <ArrowRight size={12} />
                </span>
                <span className="text-[15px] font-semibold">{next.title}</span>
              </a>
            ) : (
              <span className="flex-1" />
            )}
          </nav>
        </main>

        {/* Right rail */}
        <aside
          className="sticky top-14 hidden h-[calc(100dvh-3.5rem)] w-[224px] shrink-0 overflow-y-auto py-10 xl:block"
        >
          <Toc headings={headings} />
        </aside>
      </div>
    </div>
  );
}

function TopBar({ current }: { current: string }) {
  return (
    <header
      className="sticky top-0 z-50 border-b backdrop-blur-xl"
      style={{
        borderColor: "var(--border)",
        background: "color-mix(in oklab, var(--bg) 84%, transparent)",
      }}
    >
      <div className="mx-auto flex h-14 w-full max-w-[1560px] items-center gap-4 px-4 md:px-6">
        <a href="/" className="flex shrink-0 items-center gap-2.5">
          <Mark className="h-5 w-5" />
          <span className="font-mono text-[14.5px] font-semibold tracking-tight">
            browser-rs
          </span>
        </a>

        <span
          className="hidden font-mono text-[12px] sm:inline"
          style={{ color: "var(--fg-faint)" }}
        >
          0.1.0
        </span>

        <div className="ml-auto flex items-center gap-3">
          <Search />
          <div className="lg:hidden">
            <Sidebar current={current} />
          </div>
          <a
            href="https://github.com/himasnhu-AT/browser-rs"
            className="hidden h-9 items-center rounded-[10px] border px-3 text-[13.5px] sm:inline-flex"
            style={{ borderColor: "var(--border-strong)" }}
          >
            GitHub
          </a>
        </div>
      </div>
    </header>
  );
}

/* -------------------------------------------------------------------------- */
/* Content components                                                          */
/* -------------------------------------------------------------------------- */

export function H2({ id, children }: { id: string; children: React.ReactNode }) {
  return (
    <h2
      id={id}
      className="mt-14 scroll-mt-24 border-t pt-10 text-[26px] font-bold tracking-tight"
      style={{ borderColor: "var(--border)" }}
    >
      {children}
    </h2>
  );
}

export function H3({ children }: { children: React.ReactNode }) {
  return <h3 className="mt-9 text-[19px] font-bold tracking-tight">{children}</h3>;
}

export function P({ children }: { children: React.ReactNode }) {
  return (
    <p className="mt-4 leading-[1.75]" style={{ color: "var(--fg-muted)" }}>
      {children}
    </p>
  );
}

export function Code({ children }: { children: React.ReactNode }) {
  return (
    <code
      className="rounded-[6px] px-[0.36em] py-[0.12em] font-mono text-[0.88em]"
      style={{ background: "var(--bg-sunk)", color: "var(--fg)" }}
    >
      {children}
    </code>
  );
}

export function Pre({ code, lang = "rust" }: { code: string; lang?: string }) {
  return (
    <div
      className="relative mt-6 overflow-hidden rounded-[16px] border"
      style={{ borderColor: "var(--border)", background: "var(--bg-elev)" }}
    >
      <div
        className="flex items-center border-b px-4 py-2"
        style={{ borderColor: "var(--border)" }}
      >
        <span className="font-mono text-[11.5px]" style={{ color: "var(--fg-faint)" }}>
          {lang}
        </span>
      </div>
      <CopyButton text={code} />
      <pre className="overflow-x-auto p-5 font-mono text-[13px] leading-[1.7]">
        <code>{code}</code>
      </pre>
    </div>
  );
}

export function Callout({
  kind = "note",
  title,
  children,
}: {
  kind?: "note" | "warning";
  title: string;
  children: React.ReactNode;
}) {
  const warn = kind === "warning";
  return (
    <div
      className="mt-6 rounded-[16px] border p-5"
      style={{
        borderColor: warn ? "var(--accent)" : "var(--border-strong)",
        background: warn
          ? "color-mix(in oklab, var(--accent) 8%, var(--bg-elev))"
          : "var(--bg-elev)",
      }}
    >
      <p className="flex items-center gap-2 text-[14.5px] font-semibold">
        {warn ? (
          <Warning size={16} style={{ color: "var(--accent-ink)" }} />
        ) : (
          <Info size={16} style={{ color: "var(--fg-faint)" }} />
        )}
        {title}
      </p>
      <div
        className="mt-2 text-[14.5px] leading-[1.7]"
        style={{ color: "var(--fg-muted)" }}
      >
        {children}
      </div>
    </div>
  );
}

export function Table({
  head,
  rows,
}: {
  head: string[];
  rows: React.ReactNode[][];
}) {
  return (
    <div
      className="mt-6 overflow-x-auto rounded-[16px] border"
      style={{ borderColor: "var(--border)" }}
    >
      <table className="w-full text-left text-[14px]">
        <thead>
          <tr style={{ background: "var(--bg-sunk)" }}>
            {head.map((h) => (
              <th key={h} className="px-4 py-3 font-semibold whitespace-nowrap">
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody style={{ color: "var(--fg-muted)" }}>
          {rows.map((r, i) => (
            <tr
              key={i}
              className="border-t"
              style={{ borderColor: "var(--border)" }}
            >
              {r.map((cell, j) => (
                <td key={j} className="px-4 py-3 align-top">
                  {cell}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
