import { Callout, Code, DocsPage, H2, P, Table } from "@/components/docs";

const HEADINGS = [
  { id: "why", text: "Why this page exists" },
  { id: "behaviour", text: "Behavioural decisions" },
  { id: "unverified", text: "What is not verified" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/divergences"
      title="Divergences"
      lede="Documented decisions where behaviour is deliberate rather than obvious, and an honest list of what has not been proven."
      headings={HEADINGS}
    >
      <H2 id="why">Why this page exists</H2>
      <P>
        Every port makes choices. Writing them down is cheaper than having
        someone rediscover them from a failing test at two in the morning.
      </P>

      <H2 id="behaviour">Behavioural decisions</H2>
      <Table
        head={["Area", "Decision"]}
        rows={[
          [
            "Tree shape",
            "html5ever is the parser, so implied tbody and the HTML5 adoption agency apply. This is what a browser produces, which is what a scraper wants.",
          ],
          [
            "Selectors Level 4",
            "Accepted where older CSS-to-XPath translators reject them. No selector that worked before stops working.",
          ],
          [
            "Boolean attributes",
            "A bare checked attribute has the value empty string, and cannot be distinguished from checked equals empty string.",
          ],
          [
            <Code key="a">wait_until: commit</Code>,
            "Behaves as load, because the underlying wait always resolves load first. Slower than asked, never wrong.",
          ],
          [
            "Crawl budget",
            "Spent on enqueue, not on fetch, so a duplicate that gets rejected has already been counted.",
          ],
          [
            "robots.txt timeout",
            "A fetch that times out is cached as allow-everything. Under heavy load a crawl can stop honouring robots without any counter showing it.",
          ],
        ]}
      />

      <Callout kind="warning" title="The robots case is worth reading twice">
        Every statistic is identical whether that fetch happened or not. Only a
        fixture hit count can observe it, which is why it is documented here
        rather than left to be discovered.
      </Callout>

      <H2 id="unverified">What is not verified</H2>
      <P>
        Proxies and TLS impersonation are exercised at the configuration level
        but never against a real proxy or a real fingerprinting service, because
        a loopback fixture cannot reach one. The Cloudflare solver is verified
        against a structural fixture that reproduces the challenge DOM, not
        against Cloudflare.
      </P>
    </DocsPage>
  );
}
