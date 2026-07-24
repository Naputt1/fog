export default {
  title: 'fog',
  description: 'Terminal-based service orchestrator & reverse-proxy dashboard',
  base: '/fog/',
  themeConfig: {
    nav: [
      { text: 'Guide', link: '/getting-started' },
      { text: 'Configuration', link: '/configuration' },
      { text: 'Architecture', link: '/architecture' },
      { text: 'GitHub', link: 'https://github.com/Naputt1/fog' },
    ],
    sidebar: [
      {
        text: 'Getting Started',
        items: [
          { text: 'Overview', link: '/' },
          { text: 'Installation & Usage', link: '/getting-started' },
          { text: 'Keybindings', link: '/keybindings' },
        ],
      },
      {
        text: 'Reference',
        items: [
          { text: 'Configuration', link: '/configuration' },
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
  },
  cleanUrls: true,
}
