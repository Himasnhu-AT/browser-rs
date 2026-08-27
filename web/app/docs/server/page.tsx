import { Callout, Code, DocsPage, H2, H3, P, Pre, Table } from "@/components/docs";

const HEADINGS = [
  { id: "run", text: "Running it" },
  { id: "endpoints", text: "Endpoints" },
  { id: "sessions", text: "Sessions" },
  { id: "auth", text: "Authentication" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/server"
      title="REST API"
      lede="One process, one port. The same engine answers REST, MCP and the dashboard, so all three behave identically."
      headings={HEADINGS}
    >
      <H2 id="run">Running it</H2>
      <Pre
        lang="shell"
        code={`export BROWSER_RS_API_TOKEN=$(openssl rand -hex 32)
brs-serve

curl -s localhost:8080/api/health`}
      />

      <H2 id="endpoints">Endpoints</H2>
      <Table
        head={["Method", "Path", "Body"]}
        rows={[
          ["GET", <Code key="a">/api/health</Code>, "None. Unauthenticated."],
          ["GET", <Code key="b">/api/openapi.json</Code>, "None."],
          ["POST", <Code key="c">/api/fetch</Code>, "url, session, proxy, impersonate, format"],
          ["POST", <Code key="d">/api/browse</Code>, "url, session, wait_until, timeout_ms"],
          ["POST", <Code key="e">/api/extract</Code>, "url or html, selector, kind"],
          ["POST", <Code key="f">/api/crawl</Code>, "seed, max_pages, max_depth"],
          ["POST", <Code key="g">/api/screenshot</Code>, "url or session, full_page"],
          ["GET", <Code key="h">/api/sessions</Code>, "None."],
          ["DELETE", <Code key="i">/api/sessions/{"{name}"}</Code>, "None."],
        ]}
      />
      <Pre
        lang="shell"
        code={`curl -s localhost:8080/api/fetch \\
  -H "authorization: Bearer $BROWSER_RS_API_TOKEN" \\
  -H 'content-type: application/json' \\
  -d '{"url":"https://example.com/","format":"markdown"}'`}
      />
      <Callout kind="note" title="Screenshots come back at 2x">
        The device scale factor is 2, so a 1280 by 720 viewport produces a 2560
        by 1440 image. Halve the coordinates if you are mapping them back.
      </Callout>

      <H2 id="sessions">Sessions</H2>
      <P>
        A session is a name plus a profile directory. Naming a session that does
        not exist opens it, because with a persistent profile creating and
        reattaching are the same act.
      </P>
      <Pre
        lang="shell"
        code={`curl -s localhost:8080/api/sessions -X POST \\
  -H "authorization: Bearer $TOKEN" \\
  -H 'content-type: application/json' \\
  -d '{"name":"shop"}'`}
      />
      <P>
        Pass <Code>session</Code> on any later call and it runs inside that
        profile, carrying whatever cookies it holds.
      </P>

      <H2 id="auth">Authentication</H2>
      <H3>The token</H3>
      <P>
        Every route except <Code>/api/health</Code> requires a bearer token from{" "}
        <Code>BROWSER_RS_API_TOKEN</Code>. Health stays open so container health
        checks work.
      </P>
      <H3>The unset case</H3>
      <P>
        With no token configured the server rewrites its bind address to
        loopback and warns at startup. It will not silently listen on a public
        interface without a credential.
      </P>
    </DocsPage>
  );
}
