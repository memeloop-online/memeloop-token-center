import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { defineConfig, type DefaultTheme } from 'vitepress'

const docsDir = path.dirname(path.dirname(fileURLToPath(import.meta.url)))

const LOCALES = ['zh', 'en'] as const
type Locale = (typeof LOCALES)[number]

// Directories that are part of the VitePress site rather than legacy content.
const SITE_DIRS = new Set([...LOCALES, 'public', '.vitepress', 'node_modules'])

// The published site contains only docs/index.md plus docs/zh/** and
// docs/en/**. Everything else under docs/ is legacy internal material and is
// excluded from the build by computing srcExclude from the actual tree.
function computeSrcExclude(): string[] {
  const patterns: string[] = []
  for (const entry of readdirSync(docsDir)) {
    if (SITE_DIRS.has(entry)) continue
    const full = path.join(docsDir, entry)
    if (statSync(full).isDirectory()) {
      patterns.push(`${entry}/**`)
    } else if (entry.endsWith('.md') && entry !== 'index.md') {
      patterns.push(entry)
    }
  }
  return patterns
}

function fileTitle(file: string): string {
  try {
    const match = readFileSync(file, 'utf8').match(/^#\s+(.+)$/m)
    if (match) return match[1].replace(/[#*`]/g, '').trim()
  } catch {
    // fall through to filename-based title
  }
  const base = path.basename(file, '.md')
  return nameTitle(base === 'index' ? path.basename(path.dirname(file)) : base)
}

function nameTitle(name: string): string {
  return name.replace(/[-_]+/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase())
}

function sidebarItems(dir: string, linkPrefix: string): DefaultTheme.SidebarItem[] {
  const entries = readdirSync(dir)
    .filter((entry) => !entry.startsWith('.'))
    .sort((a, b) => {
      const order = ['index.md', 'getting-started.md', 'upstreams.md', 'routing.md', 'credentials.md', 'requests.md', 'api.md', 'development.md', 'operator-ui.md']
      const rank = (name: string) => order.includes(name) ? order.indexOf(name) : order.length
      return rank(a) - rank(b) || a.localeCompare(b)
    })

  const items: DefaultTheme.SidebarItem[] = []
  for (const entry of entries) {
    const full = path.join(dir, entry)
    if (statSync(full).isDirectory()) {
      const children = sidebarItems(full, `${linkPrefix}${entry}/`)
      if (children.length === 0) continue
      const index = children.find((child) => child.link === `${linkPrefix}${entry}/`)
      const rest = children.filter((child) => child !== index)
      if (rest.length === 0) {
        items.push(index ?? { text: nameTitle(entry), link: `${linkPrefix}${entry}/` })
        continue
      }
      items.push({
        text: entry === 'guide'
          ? (linkPrefix.startsWith('/zh/') ? '使用指南' : 'User guide')
          : index?.text ?? nameTitle(entry),
        collapsed: false,
        items: rest,
        ...(index ? { link: index.link } : {})
      })
    } else if (entry.endsWith('.md')) {
      const slug = entry === 'index.md' ? '' : entry.slice(0, -3)
      items.push({ text: fileTitle(full), link: `${linkPrefix}${slug}` })
    }
  }
  return items
}

function sidebarFor(locale: Locale): DefaultTheme.SidebarItem[] {
  const root = path.join(docsDir, locale)
  return existsSync(root) ? sidebarItems(root, `/${locale}/`) : []
}

// Tentative top-level navigation. Entries are emitted only when the target
// page already exists, so partial content never produces dead links.
const NAV_CANDIDATES: { key: string; labels: Record<Locale, string> }[] = [
  { key: 'guide/getting-started', labels: { zh: '快速上手', en: 'Getting Started' } },
  { key: 'plugins/index', labels: { zh: '插件', en: 'Plugins' } }
]

function navFor(locale: Locale): DefaultTheme.NavItem[] {
  const root = path.join(docsDir, locale)
  if (!existsSync(root)) return []
  const home: Record<Locale, string> = { zh: '首页', en: 'Home' }
  const items: DefaultTheme.NavItem[] = [{ text: home[locale], link: `/${locale}/` }]
  for (const { key, labels } of NAV_CANDIDATES) {
    if (existsSync(path.join(root, `${key}.md`))) {
      const link = key.endsWith('/index') ? `/${locale}/${key.slice(0, -5)}` : `/${locale}/${key}`
      items.push({ text: labels[locale], link })
    }
  }
  return items
}

const base = process.env.DOCS_BASE || '/'

export default defineConfig({
  base,
  title: 'Memeloop Token Center',
  description: 'Memeloop Token Center product documentation',
  cleanUrls: true,
  lastUpdated: true,
  srcExclude: computeSrcExclude(),

  head: [['meta', { name: 'theme-color', content: '#0e7490' }]],

  locales: {
    zh: {
      label: '简体中文',
      lang: 'zh-CN',
      title: 'Memeloop Token Center',
      description: 'Memeloop Token Center 产品文档',
      themeConfig: {
        nav: navFor('zh'),
        sidebar: sidebarFor('zh'),
        outline: { label: '本页目录' },
        docFooter: { prev: '上一页', next: '下一页' },
        lastUpdated: { text: '最后更新' },
        darkModeSwitchLabel: '外观',
        sidebarMenuLabel: '菜单',
        returnToTopLabel: '回到顶部',
        langMenuLabel: '语言'
      }
    },
    en: {
      label: 'English',
      lang: 'en-US',
      title: 'Memeloop Token Center',
      description: 'Memeloop Token Center product documentation',
      themeConfig: {
        nav: navFor('en'),
        sidebar: sidebarFor('en')
      }
    }
  },

  themeConfig: {
    search: {
      provider: 'local',
      options: {
        locales: {
          zh: {
            translations: {
              button: { buttonText: '搜索', buttonAriaLabel: '搜索' },
              modal: {
                noResultsText: '未找到相关结果',
                resetButtonTitle: '清除查询',
                footer: { selectText: '选择', navigateText: '切换', closeText: '关闭' }
              }
            }
          }
        }
      }
    }
  }
})
