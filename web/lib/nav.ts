/**
 * One registry drives the sidebar, the search index and the prev/next footer.
 * Adding a page in three places is how docs navigation rots, so it is declared
 * once here and consumed everywhere.
 */

export type NavLeaf = {
  title: string;
  href: string;
  /** Plain-language summary. Shown in search results, so it is not decoration. */
  blurb: string;
};

export type NavGroup = {
  title: string;
  items: NavLeaf[];
};

export const NAV: NavGroup[] = [
  {
    title: "Overview",
    items: [
      {
        title: "Introduction",
        href: "/",
        blurb: "What browser-rs is, and which entry point to reach for first.",
      },
      {
        title: "Installation",
        href: "/docs/installation",
        blurb: "Add the crate, install the CLI, or run the server in Docker.",
      },
      {
        title: "Quick start",
        href: "/docs/quick-start",
        blurb: "Fetch a page and pull text out of it in about ten lines.",
      },
    ],
  },
  {
    title: "Parsing",
    items: [
      {
        title: "Documents and nodes",
        href: "/docs/documents",
        blurb: "Document, Node and Nodes, and how the three relate.",
      },
      {
        title: "CSS and XPath",
        href: "/docs/selectors",
        blurb: "Both engines over one tree, plus the ::text and ::attr extensions.",
      },
      {
        title: "Adaptive selectors",
        href: "/docs/adaptive",
        blurb: "Relocating an element by structure after a site redesign.",
      },
    ],
  },
  {
    title: "Fetching",
    items: [
      {
        title: "HTTP fetchers",
        href: "/docs/http",
        blurb: "One-shot, session and client fetchers, with TLS impersonation.",
      },
      {
        title: "Real browsers",
        href: "/docs/browser",
        blurb: "Driving Chrome over CDP for pages that need JavaScript.",
      },
      {
        title: "Crawling",
        href: "/docs/spiders",
        blurb: "The crawl engine, link extraction and the ready-made spiders.",
      },
    ],
  },
  {
    title: "Server",
    items: [
      {
        title: "REST API",
        href: "/docs/server",
        blurb: "brs-serve, the endpoint table, sessions and authentication.",
      },
      {
        title: "MCP for agents",
        href: "/docs/mcp",
        blurb: "Eight tools over stdio or HTTP, for any agent runtime.",
      },
      {
        title: "Dashboard",
        href: "/docs/dashboard",
        blurb: "Watching a live browser, and signing in by hand once.",
      },
    ],
  },
  {
    title: "Reference",
    items: [
      {
        title: "Feature flags",
        href: "/docs/features",
        blurb: "Which cargo features pull in which crates.",
      },
      {
        title: "Divergences",
        href: "/docs/divergences",
        blurb: "Documented behavioural decisions, and what is not verified.",
      },
    ],
  },
];

export const FLAT: NavLeaf[] = NAV.flatMap((g) => g.items);

export function neighbours(href: string) {
  const i = FLAT.findIndex((p) => p.href === href);
  return {
    prev: i > 0 ? FLAT[i - 1] : null,
    next: i >= 0 && i < FLAT.length - 1 ? FLAT[i + 1] : null,
  };
}
