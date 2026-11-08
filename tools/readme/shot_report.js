// node shot_report.js <index.html> <out.png> [tabText] [width]
// Screenshot of the diff report from the top down to the end of the first file card.
const { chromium } = require(process.env.PW_DIR + '/node_modules/playwright');
(async () => {
  const [src, out, tab = '', w = '1200'] = process.argv.slice(2);
  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: +w, height: 1000 }, deviceScaleFactor: 2 });
  await page.goto('file:///' + src.replace(/\\/g, '/'));
  if (tab) { await page.locator('.tabs').getByText(tab, { exact: true }).click(); }
  await page.waitForTimeout(500);
  const box = await page.locator('section.file').first().boundingBox();
  const h = Math.ceil(box.y + box.height + 24);
  await page.screenshot({ path: out, clip: { x: 0, y: 0, width: +w, height: h } });
  console.log('shot', out, w + 'x' + h);
  await browser.close();
})();
