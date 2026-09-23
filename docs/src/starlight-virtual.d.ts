// Ambient type declarations for Starlight's virtual modules.
// Starlight resolves these at build time; they have no real package source,
// so `astro check` needs ambient declarations to type-check component
// overrides that import them.

declare module 'virtual:starlight/user-config' {
  const config: import('@astrojs/starlight/types').StarlightConfig;
  export default config;
}

declare module 'virtual:starlight/user-images' {
  type ImageMetadata = import('astro').ImageMetadata;
  export const logos: {
    dark?: ImageMetadata;
    light?: ImageMetadata;
  };
}

declare module 'virtual:starlight/components/*' {
  const Component: import('astro/runtime/server').AstroComponentFactory;
  export default Component;
}
