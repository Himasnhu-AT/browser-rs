import { Callout, Code, DocsPage, H2, H3, P, Pre, Table } from "@/components/docs";

const HEADINGS = [
  { id: "one-tree", text: "One tree, two engines" },
  { id: "css", text: "CSS" },
  { id: "extensions", text: "The extensions" },
  { id: "xpath", text: "XPath" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/selectors"
      title="CSS and XPath"
      lede="Both engines run over the same arena, so node identity is shared and nothing is parsed twice."
      headings={HEADINGS}
    >
      <H2 id="one-tree">One tree, two engines</H2>
      <P>
        A node found by CSS and the same node found by XPath are the same node.
        You can start a query in one engine and continue it in the other.
      </P>
      <Pre
        lang="rust"
        code={`let rows = doc.css("table.results tr")?;
let second_cells = rows.xpath("./td[2]")?.texts();`}
      />

      <H2 id="css">CSS</H2>
      <P>
        Matching runs on Servo&apos;s selector engine, the same one Firefox
        ships, so Selectors Level 4 syntax works.
      </P>
      <Pre
        lang="rust"
        code={`doc.css("article:has(> img)")?;
doc.css("li:nth-child(2 of .featured)")?;
doc.css(":not(.draft, .archived)")?;`}
      />

      <H2 id="extensions">The extensions</H2>
      <P>
        Three additions are not standard CSS. They exist because the alternative
        is a second call for something you almost always want.
      </P>
      <Table
        head={["Syntax", "Returns"]}
        rows={[
          [<Code key="a">::text</Code>, "The text of the matched node."],
          [<Code key="b">::attr(name)</Code>, "One attribute value."],
          [<Code key="c">:contains(word)</Code>, "Nodes whose text contains the word."],
        ]}
      />
      <Pre
        lang="rust"
        code={`doc.css("h2.title::text")?.texts();
doc.css("a.download::attr(href)")?.texts();
doc.css("button:contains(Accept)")?;`}
      />
      <Callout kind="note" title="They are optional">
        <Code>Nodes::texts()</Code> and <Code>Nodes::attrs()</Code> do the same
        job from the Rust side. Use whichever reads better where you are.
      </Callout>

      <H2 id="xpath">XPath</H2>
      <P>
        The XPath engine is written from scratch: thirteen axes, twenty-seven
        functions and the full type system, with no C library underneath.
      </P>
      <H3>Typed results</H3>
      <P>
        XPath can return four kinds of value. <Code>xpath</Code> gives you a
        node set; <Code>xpath_value</Code> gives you whichever type the
        expression actually produced.
      </P>
      <Pre
        lang="rust"
        code={`let nodes = doc.xpath("//div[@class='item']")?;
let count = doc.xpath_value("count(//a)", &Variables::new())?;
let name  = doc.xpath_value("name(//*[1])", &Variables::new())?;`}
      />
    </DocsPage>
  );
}
