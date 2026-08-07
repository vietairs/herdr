import { defineCollection, z } from 'astro:content';
import { glob } from 'astro/loaders';
import { docsLoader } from '@astrojs/starlight/loaders';
import { docsSchema } from '@astrojs/starlight/schema';
// Single source of truth for docs slugs. A local copy here loses the `preview`
// and `_versions/<version>` prefix handling, which silently routes archived
// docs to /docs/_versions/<version>/ instead of /docs/<version>/ — it still
// builds, and docs-path.test.ts still passes, because the test covers the
// shared helper rather than whatever the loader happens to call.
import { docsPath } from './docs-path';

export const collections = {
  docs: defineCollection({ loader: docsLoader({ generateId: docsPath }), schema: docsSchema() }),
  blog: defineCollection({
    loader: glob({ pattern: '*.md', base: './src/content/blog' }),
    schema: z.object({
      title: z.string(),
      description: z.string(),
      date: z.coerce.date(),
      draft: z.boolean().default(false),
      /* description is always used for meta and OG; set false when it should
         not also print as a subtitle under the title */
      lede: z.boolean().default(true),
      ogImage: z.string().optional(),
    }),
  }),
};
