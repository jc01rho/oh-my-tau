// Extension claiming a link scheme with `pi.registerLinkScheme`, recording each
// `link_activate` it receives through `appendEntry` so the core test can read it.
export default function linkFixture(pi: any): void {
  pi.registerLinkScheme("cc-stretch");
  pi.on("link_activate", (e: any) => {
    pi.appendEntry("link-activated", { url: e.url, scheme: e.scheme, type: e.type });
  });
}
