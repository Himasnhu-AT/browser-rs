import { Callout, Code, DocsPage, H2, P, Pre, Table } from "@/components/docs";

const HEADINGS = [
  { id: "install", text: "Install" },
  { id: "first", text: "Your first scrape" },
  { id: "entry-points", text: "Four entry points" },
  { id: "agents", text: "Using it from an agent" },
  { id: "where-next", text: "Where to go next" },
];

export default function Home() {
  return (
    <DocsPage
      href="/"
      title="browser-rs"
      lede="A native Rust engine for reading the web. Real Chrome when a page needs it, selectors that survive a redesign, and an MCP server your agents can call."
      headings={HEADINGS}
    >
      <P>
        browser-rs parses HTML the way a browser does, selects with both CSS and
        XPath over the same tree, and fetches either over plain HTTP or by
        driving a real Chrome. There is no C library underneath it, no Python
        runtime beside it, and no <Code>unsafe</Code> block inside it.
      </P>

      <H2 id="install">Install</H2>
      <P>
        The library is one crate. The command line and the server ship
        separately, so a program that only parses does not pull in Chrome.
      </P>
      <Pre lang="shell" code={`cargo add browser-rs`} />

      <Callout kind="note" title="Feature flags are opt-in">
        The defaults give you parsing and HTTP. Browser control, crawling,
        markdown conversion and adaptive storage each sit behind a flag, so a
        minimal build stays small. See the feature reference.
      </Callout>

      <H2 id="first">Your first scrape</H2>
      <P>
        Fetch a page and pull the headlines out of it. This is the whole
        program.
      </P>
      <Pre
        lang="rust"
        code={`use browser::{AsyncFetcher, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let page = AsyncFetcher::get("https://news.ycombinator.com/")
        .send()
        .await?;

    for title in page.css(".titleline > a::text")?.texts() {
        println!("{title}");
    }
    Ok(())
}`}
      />
      <P>
        <Code>::text</Code> is an extension to CSS, not standard syntax. It
        pulls the text of the matched node rather than the node itself, which
        saves a second call in the common case.
      </P>

      <H2 id="entry-points">Four entry points</H2>
      <P>
        Most work starts at one of four places. Reach for the cheapest one that
        answers your question.
      </P>
      <Table
        head={["Entry point", "Use it when", "Cost"]}
        rows={[
          [
            <Code key="a">Document::parse</Code>,
            "You already have the HTML and only need to read it.",
            "No network.",
          ],
          [
            <Code key="b">AsyncFetcher</Code>,
            "The page is server-rendered and plain HTTP will do.",
            "One request.",
          ],
          [
            <Code key="c">BrowserSession</Code>,
            "The page needs JavaScript, or a login, to make sense.",
            "A real Chrome process.",
          ],
          [
            <Code key="d">SpiderRunner</Code>,
            "You want many pages, bounded, with robots respected.",
            "Many requests.",
          ],
        ]}
      />

      <H2 id="agents">Using it from an agent</H2>
      <P>
        The server exposes the same operations over MCP, so an agent runtime can
        discover and call them without any glue code on your side.
      </P>
      <Pre
        lang="shell"
        code={`docker run -p 8080:8080 \\
  -e BROWSER_RS_API_TOKEN=$(openssl rand -hex 32) \\
  browser-rs:latest`}
      />
      <P>
        Point the agent at <Code>http://localhost:8080/mcp</Code>, or spawn{" "}
        <Code>brs-mcp</Code> for stdio. Eight tools, returning markdown rather
        than raw markup.
      </P>

      <H2 id="where-next">Where to go next</H2>
      <P>
        If you are here to extract data from pages you already have, read
        documents and nodes. If you are here to fetch, read the HTTP fetchers.
        If you are wiring this to an agent, skip to the MCP page.
      </P>
    </DocsPage>
  );
}
