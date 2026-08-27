import { Callout, Code, DocsPage, H2, H3, P, Pre, Table } from "@/components/docs";

const HEADINGS = [
  { id: "three", text: "Three shapes" },
  { id: "impersonation", text: "TLS impersonation" },
  { id: "proxies", text: "Proxies" },
  { id: "redirects", text: "Redirects" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/http"
      title="HTTP fetchers"
      lede="Plain HTTP with a browser-shaped TLS handshake, in three shapes depending on how much state you need."
      headings={HEADINGS}
    >
      <H2 id="three">Three shapes</H2>
      <Table
        head={["Type", "Keeps cookies", "Use when"]}
        rows={[
          [<Code key="a">AsyncFetcher</Code>, "No", "One request, no follow-up."],
          [<Code key="b">AsyncFetcherSession</Code>, "Yes", "A login, then more requests."],
          [<Code key="c">AsyncFetcherClient</Code>, "No", "Many unrelated requests, pooled."],
        ]}
      />
      <P>
        Each exposes <Code>get</Code>, <Code>post</Code>, <Code>put</Code>,{" "}
        <Code>delete</Code> and <Code>request</Code>, all returning a builder.
        With the <Code>blocking</Code> feature the same three exist without{" "}
        <Code>await</Code>.
      </P>

      <H2 id="impersonation">TLS impersonation</H2>
      <P>
        Many sites fingerprint the TLS handshake before they ever look at your
        headers. The transport presents a real browser&apos;s JA3 and JA4
        signature, and sends headers in that browser&apos;s order.
      </P>
      <Pre
        lang="rust"
        code={`use browser::{AsyncFetcher, Impersonate};

let page = AsyncFetcher::get(url)
    .impersonate(Impersonate::Chrome131)
    .send()
    .await?;`}
      />
      <Callout kind="note" title="Header order is part of the fingerprint">
        Sending the right headers in the wrong order is itself a signal. The
        sampler reproduces the order the chosen browser actually uses.
      </Callout>

      <H2 id="proxies">Proxies</H2>
      <P>
        A proxy can be set per request, per session, or rotated by a trait
        implementation you supply.
      </P>
      <Pre
        lang="rust"
        code={`let page = AsyncFetcher::get(url)
    .proxy(Proxy::parse("http://user:pass@host:3128")?)
    .send()
    .await?;`}
      />
      <P>
        This is what people mean by controlling the exit address. Nothing here
        operates below the transport layer.
      </P>

      <H2 id="redirects">Redirects</H2>
      <H3>The safe mode</H3>
      <P>
        <Code>follow_redirects</Code> accepts <Code>true</Code>,{" "}
        <Code>false</Code> or <Code>&quot;safe&quot;</Code>. The safe mode
        refuses a hop that would reach a private address, which is the usual
        server-side request forgery route.
      </P>
      <Pre
        lang="rust"
        code={`AsyncFetcher::get(url).follow_redirects(FollowRedirects::Safe)`}
      />
    </DocsPage>
  );
}
