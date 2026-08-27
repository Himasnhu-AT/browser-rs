import { Callout, Code, DocsPage, H2, P, Pre, Table } from "@/components/docs";

const HEADINGS = [
  { id: "flags", text: "The flags" },
  { id: "defaults", text: "Defaults" },
  { id: "gap", text: "One sharp edge" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/features"
      title="Feature flags"
      lede="Parsing and HTTP are on by default. Everything with a real cost is opt-in."
      headings={HEADINGS}
    >
      <H2 id="flags">The flags</H2>
      <Table
        head={["Flag", "Pulls in"]}
        rows={[
          [<Code key="a">parser</Code>, "The document API. On by default."],
          [<Code key="b">http</Code>, "The HTTP fetchers. On by default."],
          [<Code key="c">browser</Code>, "Headless Chromium over CDP."],
          [<Code key="d">spider</Code>, "The crawl engine and the ready-made spiders."],
          [<Code key="e">markdown</Code>, "HTML to markdown, and the AI sanitiser."],
          [<Code key="f">adaptive</Code>, "Selector storage, with bundled SQLite."],
          [<Code key="g">blocking</Code>, "The synchronous fetcher façade."],
          [<Code key="h">full</Code>, "All of the above."],
        ]}
      />

      <H2 id="defaults">Defaults</H2>
      <Pre
        lang="toml"
        code={`[dependencies]
browser-rs = "0.1"

# or, everything
browser-rs = { version = "0.1", features = ["full"] }

# or, parse only, no network at all
browser-rs = { version = "0.1", default-features = false, features = ["parser"] }`}
      />

      <H2 id="gap">One sharp edge</H2>
      <Callout kind="warning" title="markdown alone does not give you Document">
        <Code>markdown</Code> forwards to the parser crate without turning on
        this crate&apos;s own <Code>parser</Code> flag, so{" "}
        <Code>--no-default-features --features markdown</Code> builds and then
        exposes no <Code>Document</Code>. Add <Code>parser</Code> explicitly. The
        smaller surface is deliberate; a broken one would not be.
      </Callout>
    </DocsPage>
  );
}
