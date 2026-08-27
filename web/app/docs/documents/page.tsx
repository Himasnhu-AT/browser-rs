import { Code, DocsPage, H2, H3, P, Pre, Table } from "@/components/docs";

const HEADINGS = [
  { id: "three", text: "Three types" },
  { id: "document", text: "Document" },
  { id: "node", text: "Node" },
  { id: "nodes", text: "Nodes" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/documents"
      title="Documents and nodes"
      lede="An owner, a borrow and a collection. Everything in the parsing API is one of the three."
      headings={HEADINGS}
    >
      <H2 id="three">Three types</H2>
      <Table
        head={["Type", "What it is", "Lifetime"]}
        rows={[
          [<Code key="a">Document</Code>, "Owns the parsed tree.", "Owned"],
          [<Code key="b">Node</Code>, "A borrow of one element in it.", "Borrowed"],
          [<Code key="c">Nodes</Code>, "A collection of borrows.", "Borrowed"],
        ]}
      />
      <P>
        Nodes borrow from the document, so the document has to outlive them.
        That is the only lifetime rule worth remembering.
      </P>

      <H2 id="document">Document</H2>
      <P>
        Parse from a string, from bytes with a known encoding, or from a
        response body.
      </P>
      <Pre
        lang="rust"
        code={`let doc = Document::parse(html);
let doc = Document::parse_with(html, &DocumentConfig::default());
let doc = Document::parse_bytes(&bytes, &config);`}
      />
      <P>
        Queries return <Code>Result</Code> because a selector can be malformed.
        A selector that is valid but matches nothing gives you an empty{" "}
        <Code>Nodes</Code>, not an error.
      </P>

      <H2 id="node">Node</H2>
      <P>
        A node carries the navigation, extraction and selector-generation
        surface. Navigation walks the arena in constant time.
      </P>
      <H3>Navigation</H3>
      <Pre
        lang="rust"
        code={`node.parent()
node.children()
node.siblings()
node.next()
node.previous()
node.ancestors()
node.find_ancestor(|n| n.has_class("card"))`}
      />
      <H3>Extraction</H3>
      <Pre
        lang="rust"
        code={`node.text()
node.attrib("href")
node.has_class("active")
node.html_content()
node.json()?
node.re(r"\\d+")`}
      />

      <H2 id="nodes">Nodes</H2>
      <P>
        The collection mirrors what you would do in a loop, without the loop.
        Chained queries run against every member.
      </P>
      <Pre
        lang="rust"
        code={`let prices = doc
    .css(".product")?
    .css(".price::text")?
    .texts();

let first = doc.css(".product")?.first();
let all_hrefs = doc.css("a")?.attrs("href");`}
      />
    </DocsPage>
  );
}
