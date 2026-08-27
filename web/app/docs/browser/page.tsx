import { Callout, Code, DocsPage, H2, P, Pre, Table } from "@/components/docs";

const HEADINGS = [
  { id: "when", text: "When to reach for it" },
  { id: "fetching", text: "Fetching a page" },
  { id: "waiting", text: "Waiting" },
  { id: "profiles", text: "Persistent profiles" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/browser"
      title="Real browsers"
      lede="Chrome driven over raw CDP, for pages that only make sense once JavaScript has run."
      headings={HEADINGS}
    >
      <H2 id="when">When to reach for it</H2>
      <P>
        A browser costs a process and a second or two. Use HTTP first. Reach for
        Chrome when the markup you need is not in the initial response, or when
        the page is behind a login that HTTP alone cannot hold.
      </P>

      <H2 id="fetching">Fetching a page</H2>
      <P>
        A session owns the browser. <Code>fetch</Code> returns the same{" "}
        <Code>Response</Code> type the HTTP path returns, so downstream code does
        not care which one produced it.
      </P>
      <Pre
        lang="rust"
        code={`use browser::{BrowserConfig, BrowserSession, FetchParams};

let session = BrowserSession::new(BrowserConfig::default().headless(true))?;
session.start().await?;

let page = session.fetch("https://example.com/", FetchParams::default()).await?;
println!("{}", page.css_first("h1::text")?.unwrap_or_default());`}
      />

      <H2 id="waiting">Waiting</H2>
      <Table
        head={["Wait state", "Returns when"]}
        rows={[
          [<Code key="a">load</Code>, "The load event fires."],
          [<Code key="b">domcontentloaded</Code>, "The DOM is parsed."],
          [<Code key="c">networkidle</Code>, "Requests settle for a quiet period."],
          [<Code key="d">selector</Code>, "A given selector matches."],
        ]}
      />
      <Pre
        lang="rust"
        code={`let params = FetchParams::default()
    .wait_for_selector(".results-loaded")
    .timeout_ms(15_000.0);`}
      />
      <Callout kind="note" title="commit behaves as load">
        The underlying wait always resolves the load event before it consults
        the other conditions, so asking for <Code>commit</Code> gives you{" "}
        <Code>load</Code> semantics. Slower than requested, never wrong.
      </Callout>

      <H2 id="profiles">Persistent profiles</H2>
      <P>
        Point <Code>user_data_dir</Code> at a directory and the browser keeps
        its cookies, local storage and logins there between runs. This is the
        mechanism behind server sessions.
      </P>
      <Pre
        lang="rust"
        code={`let config = BrowserConfig::default()
    .user_data_dir("/var/lib/browser-rs/profiles/shop")
    .headless(false);`}
      />
    </DocsPage>
  );
}
