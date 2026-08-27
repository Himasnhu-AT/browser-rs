import { Callout, Code, DocsPage, H2, P, Pre } from "@/components/docs";

const HEADINGS = [
  { id: "open", text: "Opening it" },
  { id: "watch", text: "Watching a session" },
  { id: "control", text: "Taking control" },
  { id: "security", text: "Security" },
];

export default function Page() {
  return (
    <DocsPage
      href="/docs/dashboard"
      title="Dashboard"
      lede="Watch a real browser as your agent drives it, take the controls to sign in by hand, then give them back."
      headings={HEADINGS}
    >
      <H2 id="open">Opening it</H2>
      <P>
        The dashboard is served from the same port as everything else. A page in
        a browser cannot send an authorization header, so the token is accepted
        in the query string as well.
      </P>
      <Pre lang="shell" code={`open "http://localhost:8080/?token=$BROWSER_RS_API_TOKEN"`} />

      <H2 id="watch">Watching a session</H2>
      <P>
        Selecting a session opens a websocket that streams screencast frames
        from the live tab. If the client falls behind, frames are dropped rather
        than queued, so a slow viewer never becomes a memory leak on the server.
      </P>
      <P>
        A session with no browser attached says so rather than showing a blank
        rectangle.
      </P>

      <H2 id="control">Taking control</H2>
      <P>
        Control is an explicit toggle and starts off. While it is on, mouse and
        keyboard events go back over the same socket and are dispatched into the
        real page.
      </P>
      <P>
        The flow this exists for: open the session, take control, sign in by
        hand, release control. The profile is persistent, so every later call
        that names that session is already authenticated. Your password never
        passes through the agent.
      </P>
      <Callout kind="warning" title="A stray click lands on the real page">
        Control defaults to off for a reason. While it is on, the agent and you
        are driving the same tab.
      </Callout>

      <H2 id="security">Security</H2>
      <P>
        A websocket upgrade carrying a foreign <Code>Origin</Code> is refused
        with a 403 even when the token is valid. Browsers do not apply the
        same-origin policy to websockets, and this is the one route that can
        drive an authenticated browser, so the token alone is not enough.
      </P>
      <P>
        The cost of accepting the token in a query string is that it reaches URL
        bars and history. Treat the dashboard as operator tooling, on loopback.
      </P>
    </DocsPage>
  );
}
