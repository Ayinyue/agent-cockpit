/* 用量看板图表重建守卫（PERFORMANCE_PLAN T10 / P10）。
 *
 * 验三件事：
 *   ① 同一份数据反复 paint 不重建图表（省得下来）
 *   ② 数据变了 / force=true / 换区间，必须重建（该刷的没漏）
 *   ③ 切走再切回（重新 mount）后必须重新渲染，不能留白屏
 * 判据用真实 DOM 节点身份（图表节点是否被换掉），不用「看起来一样」。
 */
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const { launchBrowser } = require('./browser-launch.cjs');
const root = path.resolve(__dirname, '../ui');
const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.svg': 'image/svg+xml', '.png': 'image/png' };
const server = http.createServer((req, res) => {
  const pathname = decodeURIComponent(new URL(req.url, 'http://localhost').pathname);
  const file = path.resolve(root, '.' + (pathname === '/' ? '/index.html' : pathname));
  if (!file.startsWith(root + path.sep)) { res.writeHead(403).end(); return; }
  fs.readFile(file, (error, bytes) => {
    if (error) { res.writeHead(404).end(); return; }
    res.setHeader('Content-Type', mime[path.extname(file)] || 'application/octet-stream');
    res.end(bytes);
  });
});

(async () => {
  await new Promise(r => server.listen(0, '127.0.0.1', r));
  const browser = await launchBrowser({});
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 1000 } });
    const errors = []; page.on('pageerror', e => errors.push(e.message));
    await page.addInitScript(() => localStorage.setItem('cockpit-toured', '1'));
    await page.goto(`http://127.0.0.1:${server.address().port}/`);
    await page.waitForFunction(() => typeof S !== 'undefined' && S.data && !S.busy);
    await page.evaluate(() => { clearInterval(pollTimer); S.view = 'usage'; render(); });
    await page.waitForFunction(() => document.getElementById('ud-body') && document.getElementById('ud-body').children.length > 0);

    // 给图表节点打标记：重建会换成新节点，标记随之消失
    const mark = () => page.evaluate(() => {
      const grid = document.querySelector('#ud-body .ud-chart-grid');
      if (grid) grid.dataset.probe = 'kept';
      return !!grid;
    });
    const kept = () => page.evaluate(() => document.querySelector('#ud-body .ud-chart-grid')?.dataset.probe === 'kept');

    assert.ok(await mark(), '用量页应已渲染出图表');

    // ① 同一份数据再 paint：图表节点必须还是原来那个
    await page.evaluate(() => UsageDashboard.setVisible(true, invoke));
    assert.ok(await kept(), '同一份数据重复 paint 不该重建图表');

    // ② force=true 必须重建
    await page.evaluate(() => {
      const body = document.getElementById('ud-body');
      // 通过公开 API 触发一次强制重绘：换区间再换回来
      const sel = document.getElementById('ud-range');
      sel.value = '7'; sel.dispatchEvent(new Event('change'));
    });
    assert.equal(await kept(), false, '换区间（force paint）必须重建图表');

    // ③ 数据变了必须重建
    await mark();
    // refresh() 走 state.bridge（setVisible 时捕获），不是全局 invoke；且 mock.js
    // 没有 usage_dashboard 分支（兜底返回 {ok:true,msg:''}），所以这里自备两份快照：
    // 先喂 A 建立基线，再喂 B（同一来源、数字不同），验「数据变了必须重建」。
    const feed = (tokens) => page.evaluate(async (t) => {
      // 字段名按 normalizeSources 的真实契约：exact_tokens / estimated_tokens。
      // 写成 exact/estimated 会被忽略，聚合恒为 0，"数据变了"就永远测不出来。
      const snapshot = {
        sources: [{ id: 'probe|one', app_id: 'codex', app_name: 'Codex', instance_id: 'one',
          instance_name: '探针账号', unit: 'tokens', status: 'ready', scope: 'device',
          count_in_total: true,
          days: [{ date: '2026-11-17', exact_tokens: t, estimated_tokens: 0, requests: 3, sessions: 1, models: [] }],
          total: t, peak: t, peak_date: '2026-11-17', files: 1, requests: 3 }],
        // 固定时间戳：用 Date.now() 会让每次快照都"不同"，
        // 那就测不出守卫到底拦没拦——期望值必须只来自注入的固定输入。
        totals: {}, generated_at: 1763300000,
      };
      // refresh() 期望 bridge 直接给出带 sources 数组的快照（usage-dashboard.js:363），
      // {ok,msg} 的解包是 app.js 那层 invoke 干的，不是这一层。
      UsageDashboard.setVisible(true, async (cmd) => cmd === 'usage_dashboard' ? snapshot : { ok: true });
      await UsageDashboard.refresh(true);
    }, tokens);

    await feed(1000);
    await mark();
    await feed(1000);                       // 同一份数据：不该重建
    assert.ok(await kept(), '喂同一份快照不该重建图表');
    await feed(9999);                       // 数字变了：必须重建
    assert.equal(await kept(), false, '数据变了必须重建图表');

    // ④ 切走再切回：重新 mount 后不能白屏
    await page.evaluate(() => { S.view = 'home'; render(); UsageDashboard.setVisible(false, invoke); });
    await page.evaluate(() => { S.view = 'usage'; render(); UsageDashboard.setVisible(true, invoke); });
    await page.waitForFunction(() => document.getElementById('ud-body')?.children.length > 0);
    const children = await page.evaluate(() => document.getElementById('ud-body').children.length);
    assert.ok(children > 0, '切回用量页必须重新渲染出内容，不能白屏');

    assert.deepEqual(errors, [], '页面不应有 JS 错误');
    console.log('PASS usage-paint-guard: 同数据不重建 / force 重建 / 数据变更重建 / 切回不白屏');
  } finally {
    await browser.close();
    await new Promise(r => server.close(r));
  }
})().catch(e => { console.error(e); process.exitCode = 1; });