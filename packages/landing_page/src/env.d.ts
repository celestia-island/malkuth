/// <reference types="vite/client" />

declare module '*.vue' {
  import type { DefineComponent } from 'vue'
  const component: DefineComponent<{}, {}, any>
  export default component
}
declare module "*.scss" { const href: string; export default href }

// Side-effect stylesheet entry: no type surface, only its CSS matters.
declare module '@celestia-island/hikari/styles';
declare module '*.scss';
