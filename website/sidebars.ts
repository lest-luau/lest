import type {SidebarsConfig} from '@docusaurus/plugin-content-docs';

// Explicit reading order — autogeneration would sort the guides
// alphabetically, which is not the order anyone should read them in.
const sidebars: SidebarsConfig = {
  guides: [
    'getting-started',
    'writing-tests',
    'matchers',
    'backends',
    'studio',
    'configuration',
    'cli',
    'snapshots',
    'coverage',
    'continuous-integration',
    'contributing',
  ],
};

export default sidebars;
