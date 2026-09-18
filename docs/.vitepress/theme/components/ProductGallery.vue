<script setup lang="ts">
import { withBase } from 'vitepress'

const props = defineProps<{ locale: 'en' | 'zh' }>()

const content = props.locale === 'zh'
  ? [
      {
        title: '运行总览',
        description: '查看请求趋势、用量、费用与主要上游额度。',
        image: '/images/overview.png',
        link: '/zh/guide/upstreams'
      },
      {
        title: '实时请求',
        description: '对照模型、凭据、词元、费用与响应表现。',
        image: '/images/requests.png',
        link: '/zh/guide/requests'
      },
      {
        title: '上游服务',
        description: '集中管理账号连接、模型目录与路由关系。',
        image: '/images/providers.png',
        link: '/zh/guide/routing'
      }
    ]
  : [
      {
        title: 'Operations overview',
        description: 'Track request trends, usage, cost, and key upstream quotas.',
        image: '/images/overview.png',
        link: '/en/guide/upstreams'
      },
      {
        title: 'Live requests',
        description: 'Compare models, credentials, tokens, cost, and response behavior.',
        image: '/images/requests.png',
        link: '/en/guide/requests'
      },
      {
        title: 'Upstream services',
        description: 'Manage account connections, model catalogs, and route relationships.',
        image: '/images/providers.png',
        link: '/en/guide/routing'
      }
    ]
</script>

<template>
  <div class="product-gallery">
    <a
      v-for="item in content"
      :key="item.title"
      class="product-gallery__item"
      :href="withBase(item.link)"
    >
      <img :src="withBase(item.image)" :alt="item.title" loading="lazy">
      <span class="product-gallery__copy">
        <strong>{{ item.title }}</strong>
        <span>{{ item.description }}</span>
      </span>
    </a>
  </div>
</template>
