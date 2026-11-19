/* 浏览器测试的统一启动口径。
 *
 * 为什么要有这个文件：原先 17 个用例各自写死 `chromium.launch({channel:'chrome'})`，
 * 没装 Chrome 的机器（只有 Edge，或只装了 playwright 自带 chromium）全部起不来——
 * 「换台机器就跑不了」是综合评估报告列的短板 #1。
 *
 * 规则（顺序固定，别调）：
 *   1. COCKPIT_BROWSER_CHANNEL 显式指定 → 只用它，失败就报错（显式指定不该被悄悄兜底）
 *   2. 未指定 → 先试本机 Chrome（与历史行为一致，字体/渲染最接近真实用户）
 *   3. Chrome 不在 → 退回 playwright 自带 chromium，并打印一行说明用了哪个
 *
 * 注意：channel 与自带 chromium 的字体度量可能有细微差别，涉及像素级断言的用例
 * 若在意一致性，请显式设 COCKPIT_BROWSER_CHANNEL=chrome。
 */
const { chromium } = require('playwright');

async function launchBrowser(options = {}) {
  const explicit = process.env.COCKPIT_BROWSER_CHANNEL;
  const base = { headless: true, ...options };
  if (explicit) {
    return chromium.launch({ ...base, channel: explicit });
  }
  try {
    return await chromium.launch({ ...base, channel: 'chrome' });
  } catch (error) {
    const missing = /not found|is not installed|Executable doesn't exist/i.test(String(error && error.message));
    if (!missing) throw error;
    console.log('[browser-launch] 本机没有 Chrome，改用 playwright 自带 chromium（设 COCKPIT_BROWSER_CHANNEL=chrome 可强制）');
    return chromium.launch(base);
  }
}

module.exports = { launchBrowser };