/**
 * The browser-rs mark: a geometric crab, drawn as a dome shell with two eye
 * stalks and two claws.
 *
 * Hand-rolled SVG is normally the wrong reach, but a brand mark was explicitly
 * asked for and this is the narrow allowed case: one simple geometric glyph,
 * built from arcs and circles, no illustration. It inherits `currentColor` so
 * it works on either theme without a second asset.
 *
 * The full mascot illustration for promo use is NOT this file. See the note at
 * the bottom of `app/page.tsx`.
 */
export function Mark({
  className = "h-7 w-7",
  title = "browser-rs",
}: {
  className?: string;
  title?: string;
}) {
  return (
    <svg
      viewBox="0 0 32 32"
      fill="none"
      role="img"
      aria-label={title}
      className={className}
    >
      {/* eye stalks, drawn before the shell so the shell caps them */}
      <path
        d="M12 14V11M20 14V11"
        stroke="currentColor"
        strokeWidth="1.9"
        strokeLinecap="round"
      />
      <circle cx="12" cy="9.6" r="1.5" fill="currentColor" />
      <circle cx="20" cy="9.6" r="1.5" fill="currentColor" />

      {/* shell */}
      <path
        d="M5.5 22a10.5 10.5 0 0 1 21 0Z"
        stroke="currentColor"
        strokeWidth="1.9"
        strokeLinejoin="round"
      />

      {/* claws */}
      <path
        d="M6 16.5c-2.4-.6-4 .5-4 2.2s1.7 2.6 3.4 2M26 16.5c2.4-.6 4 .5 4 2.2s-1.7 2.6-3.4 2"
        stroke="currentColor"
        strokeWidth="1.9"
        strokeLinecap="round"
      />

      {/* legs */}
      <path
        d="M8 24.2 6 26.6M16 24.6V27M24 24.2l2 2.4"
        stroke="currentColor"
        strokeWidth="1.9"
        strokeLinecap="round"
      />
    </svg>
  );
}
