import { Code, DocsPage, H2, P, Pre } from "@/components/docs";

const HEADINGS = [
  { id: "parse", text: "Parse a string" },
  { id: "fetch", text: "Fetch a page" },
  { id: "session", text: "Keep a session" },
  { id: "markdown", text: "Convert to markdown" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/quick-start"
      title="Quick start"
      lede="Four short programs that cover most of what people reach for on the first day."
      headings={HEADINGS}
    >
      <H2 id="parse">Parse a string</H2>
      <P>No network involved. This is the cheapest thing the library does.</P>
      <Pre
        lang="rust"
        code={`use browser::Document;

let doc = Document::parse("<ul><li>one</li><li>two</li></ul>");
let items = doc.css("li::text")?.texts();
assert_eq!(items, ["one", "two"]);`}
      />

      <H2 id="fetch">Fetch a page</H2>
      <P>
        The one-shot fetcher builds no session and holds no state, which is what
        you want for a single request.
      </P>
      <Pre
        lang="rust"
        code={`use browser::AsyncFetcher;

let page = AsyncFetcher::get("https://example.com/").send().await?;
println!("{}", page.css_first("h1::text")?.unwrap_or_default());`}
      />

      <H2 id="session">Keep a session</H2>
      <P>
        A session carries a cookie jar across requests, so a login on one call
        is still in effect on the next.
      </P>
      <Pre
        lang="rust"
        code={`use browser::AsyncFetcherSession;

let session = AsyncFetcherSession::new()?;
session.open().await?;

session.post("https://example.com/login")
    .form([("user", "me"), ("pass", "hunter2")])
    .send()
    .await?;

let me = session.get("https://example.com/account").send().await?;`}
      />

      <H2 id="markdown">Convert to markdown</H2>
      <P>
        Useful when the consumer is a language model rather than a person.{" "}
        <Code>clean_for_ai</Code> strips navigation, scripts and other chrome
        first.
      </P>
      <Pre
        lang="rust"
        code={`let page = AsyncFetcher::get(url).send().await?;
let md = page.document()?.clean_for_ai().markdown();`}
      />
    </DocsPage>
  );
}
