import { Callout, Code, DocsPage, H2, H3, P, Pre, Table } from "@/components/docs";

const HEADINGS = [
  { id: "engine", text: "The engine" },
  { id: "rules", text: "Link extraction and rules" },
  { id: "ready", text: "Ready-made spiders" },
  { id: "limits", text: "Limits" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/spiders"
      title="Crawling"
      lede="A bounded crawl engine with robots support, throttling, checkpoints and six spiders you can use as they are."
      headings={HEADINGS}
    >
      <H2 id="engine">The engine</H2>
      <P>
        Implement <Code>Spider</Code>, hand it to <Code>SpiderRunner</Code>. The
        engine owns the frontier, the per-domain concurrency and the retry
        ladder.
      </P>
      <Pre
        lang="rust"
        code={`use browser::{RunOptions, SpiderRunner, Spider};
use std::sync::Arc;

let spider = Arc::new(MySpider::default()) as Arc<dyn Spider>;
let result = SpiderRunner::new(spider, RunOptions::default())?
    .run()
    .await?;

println!("{} pages, {} items", result.stats.pages, result.items.len());`}
      />

      <H2 id="rules">Link extraction and rules</H2>
      <P>
        <Code>LinkExtractor</Code> pulls candidate links out of a page and
        filters them. <Code>CrawlRule</Code> pairs an extractor with the
        callback that should handle whatever it finds.
      </P>
      <Pre
        lang="rust"
        code={`let extractor = LinkExtractor::builder()
    .allow([r"/products/\\d+$"])
    .deny([r"/admin/"])
    .build()?;

let rule = CrawlRule::new(extractor).callback(PARSE_PRODUCT);`}
      />
      <Callout kind="warning" title="Set the callback explicitly">
        A rule that leaves the callback unset inherits the originating
        request&apos;s callback, so every page reached from a product page gets
        parsed as a product. Name it, even when it is the default.
      </Callout>
      <H3>Rule dispatch is a free function</H3>
      <P>
        <Code>requests_to_follow</Code> is callable from any spider you write,
        so composing rules into your own crawler does not require inheriting
        from anything.
      </P>

      <H2 id="ready">Ready-made spiders</H2>
      <Table
        head={["Spider", "For"]}
        rows={[
          [<Code key="a">CrawlSpider</Code>, "Rule-driven crawling of a site."],
          [<Code key="b">SitemapSpider</Code>, "Walking sitemap indexes."],
          [<Code key="c">ShopifySpider</Code>, "Shopify product JSON."],
          [<Code key="d">XMLFeedSpider</Code>, "XML feeds, node by node."],
          [<Code key="e">CSVFeedSpider</Code>, "CSV feeds, row by row."],
          [<Code key="f">SiteToMarkdownSpider</Code>, "Turning a site into markdown."],
        ]}
      />

      <H2 id="limits">Limits</H2>
      <P>
        Every crawl is bounded. <Code>max_pages</Code> is spent on enqueue
        rather than on fetch, so a link that turns out to be a duplicate has
        already been paid for. A ceiling that can be exceeded is not a ceiling.
      </P>
    </DocsPage>
  );
}
