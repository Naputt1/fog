export default {
  title: 'fog',
  description: 'Terminal-based service orchestrator & reverse-proxy dashboard',
  base: '/fog/',
  markdown: {
    // wrap wide tables and code blocks via custom CSS; mermaid via code block
    lineNumbers: false,
  },
  themeConfig: {
    nav: [
      { text: 'Guide', link: '/getting-started' },
      { text: 'Configuration', link: '/configuration' },
      { text: 'Router & DNS', link: '/router' },
      { text: 'Architecture', link: '/architecture' },
      { text: 'GitHub', link: 'https://github.com/Naputt1/fog' },
    ],
    sidebar: [
      {
        text: 'Getting Started',
        items: [
          { text: 'Overview', link: '/' },
          { text: 'Installation & Usage', link: '/getting-started' },
          { text: 'Agentic Worktrees', link: '/agentic' },
          { text: 'Keybindings', link: '/keybindings' },
        ],
      },
      {
        text: 'Reference',
        items: [
          { text: 'Configuration', link: '/configuration' },
          { text: 'Router & DNS', link: '/router' },
          { text: 'Index Server', link: '/index-server' },
          { text: 'Proxy', link: '/proxy' },
          { text: 'Themes', link: '/themes' },
          { text: 'Architecture', link: '/architecture' },
        ],
      },
      {
        text: 'Support',
        items: [
          { text: 'Troubleshooting', link: '/troubleshooting' },
        ],
      },
    ],
    footer: {
      message: 'Released under the MIT License.',
    },
    outline: [2, 3],
    socialLinks: [
      { icon: 'github', link: 'https://github.com/Naputt1/fog' },
    ],
    search: { provider: 'local' },
  },
  cleanUrls: true,
}
