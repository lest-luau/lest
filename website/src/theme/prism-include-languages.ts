import siteConfig from '@generated/docusaurus.config';
import type * as PrismNamespace from 'prismjs';

// Swizzled from @docusaurus/theme-classic to add language aliases: Prism has
// no grammar named `luau` or `console`, which is how the guides fence Luau
// code and shell sessions. Alias them onto the closest real grammars after
// the configured additionalLanguages load.
export default function prismIncludeLanguages(
  PrismObject: typeof PrismNamespace,
): void {
  const {
    themeConfig: {prism},
  } = siteConfig;
  const {additionalLanguages} = prism as {additionalLanguages: string[]};

  // Prism components read the global at require time.
  (globalThis as {Prism?: unknown}).Prism = PrismObject;

  additionalLanguages.forEach((lang) => {
    // eslint-disable-next-line global-require, import/no-dynamic-require
    require(`prismjs/components/prism-${lang}`);
  });

  PrismObject.languages.luau = PrismObject.languages.lua;
  PrismObject.languages.console = PrismObject.languages['shell-session'];

  delete (globalThis as {Prism?: unknown}).Prism;
}
