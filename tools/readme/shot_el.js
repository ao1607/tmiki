// node shot_el.js <file-or-url> <out.png> <selector> [viewportW] [scale] [clickText]
const { chromium } = require(process.env.PW_DIR + '/node_modules/playwright');
(async () => {
  const [src, out, sel, w = '1280', scale = '2', click = ''] = process.argv.slice(2);
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: +w, height: 900 }, deviceScaleFactor: +scale });
  const url = /^https?:|^file:/.test(src) ? src : 'file:///' + src.replace(/\\/g, '/');
  await page.goto(url);
  if (click) { await page.getByText(click, { exact: true }).first().click(); }
  await page.waitForTimeout(400);
  const el = page.locator(sel).first();
  await el.screenshot({ path: out, omitBackground: true });
  const box = await el.boundingBox();
  console.log('shot', out, Math.round(box.width) + 'x' + Math.round(box.height), 'scale', scale);
  await browser.close();
})();
