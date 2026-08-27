import { Callout, Code, DocsPage, H2, P, Pre, Table } from "@/components/docs";

const HEADINGS = [
  { id: "library", text: "As a library" },
  { id: "cli", text: "The command line" },
  { id: "server", text: "The server" },
  { id: "chrome", text: "Chrome" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/installation"
      title="Installation"
      lede="Add the crate to a project, install the binaries, or run the whole server in a container."
      headings={HEADINGS}
    >
      <H2 id="library">As a library</H2>
      <P>
        The workspace publishes one façade crate. Its library name is{" "}
        <Code>browser</Code>, so imports read <Code>use browser::Document;</Code>{" "}
        rather than repeating the suffix.
      </P>
      <Pre lang="shell" code={`cargo add browser-rs --features markdown,adaptive`} />
      <P>
        The minimum supported Rust version is 1.97.0, pinned by{" "}
        <Code>rust-toolchain.toml</Code>.
      </P>

      <H2 id="cli">The command line</H2>
      <P>
        The <Code>brs</Code> binary fetches a page and writes it out as HTML,
        markdown or plain text, picking the mode from the file suffix.
      </P>
      <Pre
        lang="shell"
        code={`cargo install browser-cli

brs extract get https://example.com/ page.md
brs uncurl "curl https://example.com -H 'Accept: text/html'"`}
      />

      <H2 id="server">The server</H2>
      <P>
        <Code>brs-serve</Code> hosts the REST API, the MCP endpoint and the
        dashboard on a single port.
      </P>
      <Pre
        lang="shell"
        code={`docker run -d --name brs -p 127.0.0.1:8080:8080 \\
  -e BROWSER_RS_API_TOKEN=$(openssl rand -hex 32) \\
  -e BROWSER_RS_BIND=0.0.0.0:8080 \\
  -v brs-profiles:/var/lib/browser-rs/profiles \\
  --security-opt seccomp=unconfined --shm-size=1g \\
  browser-rs:latest`}
      />
      <P>
        The volume matters. Session profiles live there, so a login you perform
        once survives a restart.
      </P>

      <Callout kind="warning" title="Do not expose this to the internet">
        The server fetches arbitrary URLs and runs a real browser on their
        content. Publish it to loopback, keep the token set, and put a reverse
        proxy in front of it if it has to travel further.
      </Callout>

      <H2 id="chrome">Chrome</H2>
      <P>
        Browser control needs a Chrome or Chromium binary. The container ships
        one. Outside the container, detection honours the <Code>CHROME</Code>{" "}
        environment variable and then falls back to the usual install paths.
      </P>
      <Table
        head={["Variable", "Meaning"]}
        rows={[
          [<Code key="a">CHROME</Code>, "Explicit path to the browser binary."],
          [
            <Code key="b">BROWSER_RS_SKIP_BROWSER_TESTS</Code>,
            "Skip every test that would launch a browser.",
          ],
        ]}
      />
    </DocsPage>
  );
}
