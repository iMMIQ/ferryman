const { test, before, after } = require('node:test');
const assert = require('node:assert/strict');
const http = require('node:http');
const fs = require('node:fs/promises');
const path = require('node:path');
const { chromium } = require('playwright');
let browser, server, origin;
const root = path.join(__dirname, '..', 'web');
const screenshotDir = process.env.UI_SCREENSHOT_DIR;
before(async () => {
  server = http.createServer(async (req, res) => {
    const name = new URL(req.url, 'http://localhost').pathname;
    const file = {'/':'index.html','/app.js':'app.js','/styles.css':'styles.css'}[name];
    if (!file) { res.writeHead(404); res.end(); return; }
    res.setHeader('Content-Type', file.endsWith('.js') ? 'text/javascript' : file.endsWith('.css') ? 'text/css' : 'text/html');
    res.end(await fs.readFile(path.join(root, file)));
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  origin = `http://127.0.0.1:${server.address().port}`;
  browser = await chromium.launch({headless:true, ...(process.env.TEST_BROWSER_EXECUTABLE ? {executablePath:process.env.TEST_BROWSER_EXECUTABLE} : {}), args:['--no-sandbox']});
  if (screenshotDir) await fs.mkdir(screenshotDir, {recursive:true});
});
after(async () => { await browser?.close(); await new Promise(resolve => server?.close(resolve)); });
const defaultJobs = () => [
  {id:'one', filename:'The Design of Everyday Things — Revised and Expanded Edition.epub',status:'translating',total:420,completed:148,translated:148,failed_segments:0},
  {id:'two',filename:'季度财务分析报告与附录.docx',status:'completed',total:100,completed:100,translated:96,failed_segments:4,result_available:true},
  {id:'three',filename:'字幕合集 — 第三集.srt',status:'failed',total:120,completed:8,translated:8,failed_segments:1,error:'连接推理服务失败：请求超时，请检查算力舱连接后重试。完整诊断信息需要在手机上能够阅读。'},
].map(j=>({...j,preset:'7b-fp8',target:'中文',mode:'bilingual',created_at:1789990000,source_path:'研究资料/'+j.filename,source_storage:'documents'}));
async function withPage(fn, viewport={width:1440,height:1000}, initial={}) {
  const state={jobs:defaultJobs(),offline:false,modelsOffline:false,posts:[],delayUpload:false,...initial};
  const page=await browser.newPage({viewport});
  const errors=[]; page.on('pageerror',e=>errors.push(e.message));
  await page.route('**/api/**',async route=>{
    const req=route.request(), u=new URL(req.url());
    if(state.offline || (state.modelsOffline && u.pathname==='/api/models')) { await route.fulfill({status:503,json:{error:'测试连接中断'}}); return; }
    let data={};
    if(req.method()==='POST') {
      state.posts.push({path:u.pathname,body:req.postData()});
      if(u.pathname==='/api/jobs/selection/preview') data={eligible_count:1,files:[{source_path:'Books/book.txt',source_storage:'documents',save_path:'Books/book.txt',save_storage:'documents',overwrite:true,skip_reason:null},{source_path:'Books/manual.docx',save_path:'Books/manual.docx',overwrite:true,skip_reason:'DOCX 仅支持双语对照'}]};
      else if(u.pathname==='/api/jobs/selection') data={jobs:[{id:'created'}],skipped_incompatible:1};
      else if(u.pathname==='/api/jobs') { if(state.delayUpload) await new Promise(r=>setTimeout(r,700)); data={id:'uploaded'}; }
      else if(u.pathname.endsWith('/retry')) { const j=state.jobs.find(j=>u.pathname.includes(j.id)); if(j) j.status='queued'; }
    } else if(u.pathname==='/api/config') data={max_upload_bytes:512*1024*1024};
    else if(u.pathname==='/api/jobs/active') data={jobs:state.jobs.filter(j=>['queued','translating'].includes(j.status))};
    else if(u.pathname==='/api/jobs') data={jobs:state.jobs,total:state.jobs.length,next_cursor:null};
    else if(u.pathname==='/api/runtime') data={state:'ready',preset:'7b-fp8',active_requests:12,leases:2};
    else if(u.pathname==='/api/models') data={models:[{preset:'7b-fp8',state:'ready',downloaded_bytes:8e9,expected_bytes:8e9},{preset:'30b-fp8',state:'absent',expected_bytes:32e9}],available_bytes:91e9,benchmark:{state:'idle',results:[]}};
    else if(u.pathname==='/api/storage') data={model_bytes:8e9,partial_bytes:0,cache_bytes:2.4e9,available_bytes:91e9};
    else if(u.pathname==='/api/documents') data={path:'',parent:null,entries:[{name:'Books',path:'Books',kind:'directory',supported:false},{name:'manual.docx',path:'manual.docx',kind:'file',supported:true,size:1024}]};
    try { await route.fulfill({json:data}); } catch(e) { if(!state.delayUpload) throw e; }
  });
  try {
    await page.goto(origin);
    await page.waitForFunction(()=>document.querySelector('#runtime-label').textContent!=='连接中');
    await fn(page,state);
    assert.deepEqual(errors,[]);
  } finally { await page.close(); }
}
async function shot(page,name) { if(screenshotDir) await page.screenshot({path:path.join(screenshotDir,name+'.png'),fullPage:false}); }

test('partial status, stable keyboard focus, details and retry', async()=>withPage(async(page,state)=>{
  await page.locator('tr[data-id="two"] .status-chip').waitFor();
  assert.equal(await page.locator('tr[data-id="two"] .status-chip').textContent(),'部分完成');
  assert.equal(await page.locator('tr[data-id="two"] .progress-fill').evaluate(e=>e.style.width),'96%');
  const cancel=page.locator('tr[data-id="one"] [data-action="cancel"]');
  await cancel.focus();
  await page.evaluate(()=>{ window.focusedButton=document.activeElement; });
  state.jobs[0].translated=150;
  await page.evaluate(()=>refreshActiveJobs());
  assert.equal(await page.evaluate(()=>document.activeElement===window.focusedButton && focusedButton.isConnected),true);
  await shot(page,'desktop-workspace');
  await page.setViewportSize({width:1366,height:768});
  const submitRect = await page.locator('#submit-job').boundingBox();
  assert(submitRect.y + submitRect.height <= 768, 'desktop submit stays reachable');
  await page.setViewportSize({width:1440,height:1000});
  await page.locator('tr[data-id="two"] .file-title').click();
  assert.match(await page.locator('#job-detail-content').textContent(),/季度财务分析报告与附录.docx/);
  await shot(page,'task-details');
  await page.locator('#job-detail-actions [data-action="retry"]').click();
  assert(state.posts.some(p=>p.path==='/api/jobs/two/retry'));
}));

test('offline errors persist without false empty history or model failure', async()=>withPage(async(page,state)=>{
  await page.locator('tr[data-id="one"]').waitFor();
  state.offline=true;
  await page.evaluate(async()=>{await refreshJobs();await refreshRuntime();});
  assert.equal(await page.locator('#runtime-label').textContent(),'连接中断');
  assert.equal(await page.locator('#runtime-startup').isVisible(),false);
  assert.equal(await page.locator('#jobs-error').isVisible(),true);
  assert.equal(await page.locator('tr[data-id="one"]').isVisible(),true);
  await shot(page,'desktop-offline');
  await page.reload();
  await page.locator('#jobs-error').waitFor();
  assert.equal(await page.locator('#empty-state').isVisible(),false);
  assert.equal(await page.locator('.jobs-pagination').isVisible(),false);
  state.offline=false;
  await page.locator('#retry-jobs').click();
  await page.waitForFunction(()=>document.querySelector('#jobs-error').hidden);
}));

test('mobile navigation keeps submission and history reachable', async()=>withPage(async(page)=>{
  const rect=await page.locator('#submit-job').boundingBox();
  assert(rect.y>=0 && rect.y+rect.height<=844);
  assert.equal(await page.locator('.jobs-section').isVisible(),false);
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth),390);
  await shot(page,'mobile-create');
  await page.locator('button[data-workspace="jobs"]').click();
  assert.equal(await page.locator('.jobs-section').isVisible(),true);
  assert.equal(await page.locator('#submit-job').isVisible(),false);
  await shot(page,'mobile-tasks');
  await page.setViewportSize({width:360,height:740});
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth),360);
},{width:390,height:844}));

test('multi-file validation and upload preview submit each selected file', async()=>withPage(async(page,state)=>{
  await page.locator('#file-input').setInputFiles([
    {name:'one.txt',mimeType:'text/plain',buffer:Buffer.from('one')},
    {name:'two.docx',mimeType:'application/octet-stream',buffer:Buffer.from('two')},
    {name:'bad.exe',mimeType:'application/octet-stream',buffer:Buffer.from('bad')},
  ]);
  assert.equal(await page.locator('#upload-list li').count(),2);
  assert.match(await page.locator('#upload-validation').textContent(),/不支持/);
  assert.equal(await page.locator('input[name="mode"][value="replace"]').isDisabled(),true);
  assert.equal(await page.locator('#mode-hint').isVisible(),true);
  await page.locator('#submit-job').click();
  await page.locator('#submission-dialog').waitFor();
  assert.match(await page.locator('#submission-summary').textContent(),/2 个任务/);
  await shot(page,'upload-preview');
  await page.locator('#confirm-submit').click();
  await page.waitForFunction(()=>!document.querySelector('#submit-job').disabled);
  assert.equal(state.posts.filter(p=>p.path==='/api/jobs').length,2);
  assert.equal(await page.locator('#upload-list li').count(),0);
}));

test('cancel upload retains unsubmitted files and shows persistent feedback',async()=>withPage(async(page,state)=>{
  state.delayUpload=true;
  await page.locator('#file-input').setInputFiles({name:'keep.txt',mimeType:'text/plain',buffer:Buffer.from('keep')});
  await page.locator('#submit-job').click();
  await page.locator('#confirm-submit').click();
  await page.locator('#cancel-upload').click();
  await page.waitForFunction(()=>!document.querySelector('#submit-job').disabled);
  assert.equal(await page.locator('#upload-list li').count(),1);
  assert.match(await page.locator('#submission-error').textContent(),/取消|停止/);
}));

test('mounted preview shows skipped files and requires overwrite acknowledgement',async()=>withPage(async(page,state)=>{
  await page.locator('label.segment').filter({hasText:'文稿与网盘'}).click();
  await page.locator('#source-directory').click();
  await page.locator('.folder-checkbox[data-entry-select="Books"]').click();
  await shot(page,'directory-picker');
  await page.locator('#select-current-folder').click();
  await page.locator('input[name="mode"][value="replace"]').check({force:true});
  await page.locator('input[name="save_strategy"][value="sibling_overwrite"]').check({force:true});
  await page.locator('#submit-job').click();
  await page.locator('#submission-dialog').waitFor();
  assert.match(await page.locator('#submission-files').textContent(),/DOCX 仅支持/);
  assert.equal(await page.locator('#confirm-submit').isDisabled(),true);
  assert.equal(state.posts.filter(p=>p.path==='/api/jobs/selection').length,0);
  await shot(page,'overwrite-preview');
  await page.locator('#confirm-overwrite').check();
  await page.locator('#confirm-submit').click();
  await page.waitForFunction(()=>!document.querySelector('#submit-job').disabled);
  assert.equal(state.posts.filter(p=>p.path==='/api/jobs/selection').length,1);
}));

test('runtime management is secondary and does not offer redundant start',async()=>withPage(async(page,state)=>{
  assert.equal(await page.locator('#start-runtime').isVisible(),false);
  await page.locator('#manage-models').click();
  assert.equal(await page.locator('#start-runtime').isDisabled(),true);
  assert.match(await page.locator('#start-runtime').textContent(),/运行中/);
  await page.locator('#model-list button').first().focus();
  await page.evaluate(()=>{window.modelAction=document.activeElement;});
  await page.evaluate(()=>refreshRuntime());
  assert.equal(await page.evaluate(()=>document.activeElement===window.modelAction),true);
  await shot(page,'models');
  await page.setViewportSize({width:390,height:844});
  const done = await page.locator('#done-model-dialog').boundingBox();
  assert(done.y + done.height <= 844);
  await shot(page,'mobile-models');
  await page.keyboard.press('Escape');
  state.modelsOffline=true;
  await page.evaluate(()=>refreshRuntime());
  assert.equal(await page.locator('#runtime-label').textContent(),'可用');
  assert.match(await page.locator('#connection-error-text').textContent(),/模型目录/);
}));

test('empty state hides pagination and offers creation',async()=>withPage(async(page)=>{
  await page.locator('#empty-create').waitFor();
  assert.equal(await page.locator('.jobs-pagination').isVisible(),false);
  await shot(page,'desktop-empty');
},{width:1440,height:1000},{jobs:[]}));

test('drag and drop accepts multiple files and rejects oversized inputs',async()=>withPage(async(page)=>{
  await page.evaluate(()=>{
    maxUploadBytes=4;
    const transfer=new DataTransfer();
    transfer.items.add(new File(['one'],'one.txt'));
    transfer.items.add(new File(['two'],'two.md'));
    transfer.items.add(new File(['too large'],'large.txt'));
    document.querySelector('#drop-zone').dispatchEvent(new DragEvent('drop',{dataTransfer:transfer,bubbles:true}));
  });
  assert.equal(await page.locator('#upload-list li').count(),2);
  assert.match(await page.locator('#upload-validation').textContent(),/large.txt.*超过/);
  await page.locator('[data-remove-file="0"]').click();
  assert.equal(await page.locator('#upload-list li').count(),1);
  assert.match(await page.locator('#upload-list').textContent(),/two.md/);
}));
