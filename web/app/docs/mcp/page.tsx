import { Callout, Code, DocsPage, H2, H3, P, Pre, Table } from "@/components/docs";

const HEADINGS = [
  { id: "transports", text: "Two transports" },
  { id: "tools", text: "The tools" },
  { id: "sessions", text: "Staying logged in" },
  { id: "output", text: "Output size" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/mcp"
      title="MCP for agents"
      lede="Eight tools an agent runtime can discover and call, over stdio for a desktop client or HTTP for a remote one."
      headings={HEADINGS}
    >
      <H2 id="transports">Two transports</H2>
      <H3>Standard input and output</H3>
      <P>What a desktop agent runtime spawns directly.</P>
      <Pre
        lang="json"
        code={`{
  "mcpServers": {
    "browser-rs": {
      "command": "brs-mcp",
      "env": { "BROWSER_RS_PROFILE_ROOT": "/var/lib/browser-rs/profiles" }
    }
  }
}`}
      />
      <H3>Streamable HTTP</H3>
      <P>
        Mounted by the server at <Code>/mcp</Code>, guarded by the same bearer
        token as the REST surface.
      </P>
      <Pre lang="text" code={`http://localhost:8080/mcp    Authorization: Bearer <token>`} />

      <H2 id="tools">The tools</H2>
      <Table
        head={["Tool", "What it does"]}
        rows={[
          [<Code key="a">fetch</Code>, "Plain HTTP. Fast, no browser. Try this first."],
          [<Code key="b">browse</Code>, "Real Chrome, for JavaScript or logged-in pages."],
          [<Code key="c">extract</Code>, "CSS, XPath, markdown or cleaned text."],
          [<Code key="d">crawl</Code>, "Bounded crawl, robots respected."],
          [<Code key="e">screenshot</Code>, "PNG of a page or a live session."],
          [<Code key="f">session_open</Code>, "Create or reattach a named session."],
          [<Code key="g">session_list</Code>, "What is open right now."],
          [<Code key="h">session_close</Code>, "Close one, keeping its profile."],
        ]}
      />

      <H2 id="sessions">Staying logged in</H2>
      <P>
        Pass the same <Code>session</Code> name across calls and the agent keeps
        whatever cookies that profile holds. A human can sign in once through
        the dashboard and the agent carries on authenticated, without ever being
        given the password.
      </P>
      <Callout kind="note" title="Prefer fetch, then browse">
        The tool descriptions say so too, because an agent picks a tool by
        reading them. A browser costs a process; HTTP costs a request.
      </Callout>

      <H2 id="output">Output size</H2>
      <P>
        Responses default to markdown rather than raw HTML, and are capped with
        truncation the model can see. Screenshots are refused above four
        mebibytes, naming <Code>full_page</Code> as the likely cause, rather
        than pushing several megabytes of base64 into a context window.
      </P>
    </DocsPage>
  );
}
