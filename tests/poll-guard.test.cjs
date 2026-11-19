/* 轮询重建守卫（PERFORMANCE_PLAN T3 / P2）。
 *
 * 两条都要验，缺一条就是自欺：
 *   ① 省得下来：连续轮询同一份 state，DOM 不再重建。
 *   ② 该刷的没漏：数据变了 / 切页了 / 交互过了，必须重新渲染。
 * 判据用「重建计数」而不是「看起来没变」——后者在 pack 布局下可能结构上永远为真。
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
    // 停掉真实轮询，改成手动 load()，让断言不受 4s 节拍干扰
    await page.evaluate(() => { clearInterval(pollTimer); });

    // 用 renderNav 的真实重建计数作判据：它每轮都会 clear + 重建侧栏
    // app.js 在 init 时就 `invoke = window.__TAURI__.core.invoke` 捕获了引用，
    // 之后替换 window.__TAURI__ 对它无效；顶层 let/function 要按 DEVELOPMENT.md 的
    // 说明直接改写绑定本身（CDP evaluate 与页面同一全局词法环境）。
    await page.evaluate(() => {
      window.__navBuilds = 0; window.__renderCalls = 0;
      const origNav = renderNav, origRender = render;
      renderNav = function (...a) { window.__navBuilds++; return origNav.apply(this, a); };
      render = function (...a) { window.__renderCalls++; return origRender.apply(this, a); };
      window.__origInvoke = invoke;
    });

    const counts = () => page.evaluate(() => ({ nav: window.__navBuilds, render: window.__renderCalls }));
    const reset = () => page.evaluate(() => { window.__navBuilds = 0; window.__renderCalls = 0; });

    // ① 同一份数据连续轮询 3 次：一次都不该重建
    await reset();
    await page.evaluate(async () => { await load(); await load(); await load(); });
    let c = await counts();
    assert.equal(c.render, 0, `同一份 state 连续轮询不该重建 DOM，实际 render ${c.render} 次`);
    assert.equal(c.nav, 0, `侧栏不该被重建，实际 ${c.nav} 次`);

    // ② 数据变了必须重建
    await reset();
    await page.evaluate(async () => {
      const orig = window.__origInvoke;
      invoke = async (cmd, args) => {
        const d = await orig(cmd, args);
        if (cmd === 'get_state') { d.__probe = (d.__probe || 0) + 1; }   // 制造一处真实变化
        return d;
      };
      await load();
    });
    c = await counts();
    assert.ok(c.render >= 1, '数据变了必须重新渲染');
    assert.ok(c.nav >= 1, '数据变了侧栏应重建');

    // ③ 数据不变但切了页：必须重建（同一份数据在不同页渲染不同内容）
    await reset();
    await page.evaluate(async () => { S.view = 'settings'; await load(); });
    c = await counts();
    assert.ok(c.render >= 1, '切页后即使数据没变也必须渲染');

    // ④ 交互调用 render() 之后，下一轮轮询不许被守卫吞掉
    await reset();
    await page.evaluate(async () => { S.view = 'home'; render(); await load(); });
    c = await counts();
    assert.ok(c.render >= 2, `交互后的首轮轮询必须再渲染一次，实际 ${c.render} 次`);

    // ⑤ 守卫之后界面仍是活的：数字确实随后端变化更新
    await page.evaluate(() => { S.view = 'home'; render(); });
    const before = await page.evaluate(() => document.querySelector('#view-home')?.innerText.length || 0);
    await page.evaluate(async () => {
      const orig = window.__origInvoke;
      invoke = async (cmd, args) => {
        const d = await orig(cmd, args);
        if (cmd === 'get_state' && d.apps?.[0]) d.apps[0].name = '守卫探针改名';
        return d;
      };
      await load();
    });
    const reflects = await page.evaluate(() => document.body.innerText.includes('守卫探针改名'));
    assert.ok(reflects, '后端改名后界面必须反映出来（守卫不能把真实更新吞掉）');
    assert.ok(before > 0, '首页应有内容');

    assert.deepEqual(errors, [], '页面不应有 JS 错误');
    console.log('PASS poll-guard: 同数据零重建 / 数据变更重建 / 切页重建 / 交互后不吞刷新 / 真实更新可见');
  } finally {
    await browser.close();
    await new Promise(r => server.close(r));
  }
})().catch(e => { console.error(e); process.exitCode = 1; });