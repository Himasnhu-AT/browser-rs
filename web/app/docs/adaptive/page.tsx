import { Callout, Code, DocsPage, H2, P, Pre } from "@/components/docs";

const HEADINGS = [
  { id: "problem", text: "The problem" },
  { id: "saving", text: "Saving a match" },
  { id: "relocating", text: "Relocating it" },
  { id: "storage", text: "Storage" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/adaptive"
      title="Adaptive selectors"
      lede="A selector is a guess about someone else's markup. When the guess breaks, relocate the element by structure instead of returning nothing."
      headings={HEADINGS}
    >
      <H2 id="problem">The problem</H2>
      <P>
        A scraper that matched <Code>.product-title</Code> yesterday returns an
        empty set today because the class became <Code>.product__title</Code>.
        Nothing errored. The job ran, found nothing, and wrote an empty file.
      </P>

      <H2 id="saving">Saving a match</H2>
      <P>
        When a selector works, store a fingerprint of what it matched: the
        element&apos;s tag, its attributes, its depth, its position among
        siblings and the shape of its subtree.
      </P>
      <Pre
        lang="rust"
        code={`let config = DocumentConfig::default()
    .adaptive(true)
    .storage(SqliteStorage::open("selectors.db")?);

let doc = Document::parse_with(html, &config);
if let Some(node) = doc.css_first(".product-title")? {
    doc.save(&node, "product-title")?;
}`}
      />

      <H2 id="relocating">Relocating it</H2>
      <P>
        On the next run, if the selector misses, score every candidate in the
        new tree against the stored fingerprint and take the closest match above
        a threshold.
      </P>
      <Pre
        lang="rust"
        code={`let found = match doc.css_first(".product-title")? {
    Some(node) => Some(node),
    None => {
        let print = doc.retrieve("product-title")?;
        print.and_then(|p| doc.relocate(&p, 80).first().cloned())
    }
};`}
      />
      <P>
        The percentage is a similarity floor. Below it, relocation returns
        nothing rather than guessing, because a confidently wrong element is
        worse than an empty result.
      </P>

      <Callout kind="warning" title="Relocation is a fallback, not a strategy">
        It buys time after a redesign. It does not replace fixing the selector.
        Log when it fires so you know the markup moved.
      </Callout>

      <H2 id="storage">Storage</H2>
      <P>
        The bundled backend is SQLite. The <Code>StorageSystem</Code> trait is
        public, so a different store is a matter of implementing three methods.
      </P>
    </DocsPage>
  );
}
