/** Buckets remain absolute server epochs. Localize their labels, never shift data. */
export function displayTimeZone() {
  return Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC';
}

export function bucketTimeZoneNote(locale: string, bucketZone: string) {
  return locale === 'zh-CN'
    ? `时间显示：${displayTimeZone()}；统计按 ${bucketZone} 分桶，热力图保留 ${bucketZone}。`
    : `Times shown in ${displayTimeZone()}; buckets and heatmap use ${bucketZone}.`;
}
