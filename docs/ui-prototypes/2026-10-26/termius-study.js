state.mode='a';
state.theme='dark';
state.location='catalog';
state.inspector=false;
state.selectedHost=3;
state.inventoryLayout='grid';
modes.a={title:'Vault / 资源卡片库',desc:'功能导航在左，分组和主机只在主区域出现；点选卡片展开详情，再打开连接。',recommend:false};
modes.b={title:'Focus / 专注终端',desc:'12 个打开会话集中在左侧，主区域只保留当前终端，顶部没有逐会话标签。',recommend:true};
modes.c={title:'Mission / 任务分屏',desc:'以任务组织多台主机，顶部只保留全局导航；任务内可在分屏与专注之间切换。',recommend:false};

function titleBar(){
 return `<header class="titlebar"><div class="brand-wrap"><span class="logo">${icon('logo')}</span><span class="brand">adit</span></div><nav class="global-nav" aria-label="应用导航"><button class="${state.location==='catalog'?'active':''}" data-action="catalog">${icon('library')}<span class="nav-label">资源库</span></button><button class="${state.location==='workspace'?'active':''}" data-action="workspace">${icon('grid')}<span class="nav-label">工作区</span></button><button data-action="switcher">${icon('terminal')}<span class="nav-label">打开的连接</span><span class="count nav-count">${state.open.length}</span></button></nav><span class="spacer"></span><button class="quick-search" data-action="switcher" aria-label="搜索并切换打开的会话">${icon('search')}<span>搜索连接</span><kbd>Ctrl K</kbd></button>${appButton('plus','新建连接','new')}${appButton('collapse',state.sidebar?'隐藏侧栏':'显示侧栏','sidebar')}<span class="window-chrome" aria-hidden="true"><span>−</span><span>□</span><span>×</span></span></header>`;
}
function activity(){return ''}
function tabStrip(){return ''}
function vaultSidebar(){
 const items=[['desktop','主机','hosts'],['key','密钥与凭据','credentials'],['split','端口转发','tunnels'],['terminal','命令片段','snippets'],['lock','已知主机','knownhosts'],['file','连接日志','logs']];
 return `<aside class="sidebar" aria-label="资源功能导航"><div class="vault-name"><span>${icon('lock')}</span><span><b>本地资源库</b><small>个人连接与凭据</small></span><span class="spacer"></span>${icon('down')}</div><div class="section-label">资源管理</div><nav class="vault-links">${items.map(([ic,label,panel])=>`<button class="vault-link ${panel==='hosts'?'active':''}" ${panel==='hosts'?'data-action="catalog"':`data-info="${panel}"`}>${icon(ic)}<span>${label}</span>${panel==='hosts'?`<span class="count">${hosts.length}</span>`:''}</button>`).join('')}</nav><div class="vault-line"></div><div class="section-label">继续工作</div><nav class="vault-links"><button class="vault-link" data-action="workspace">${icon('terminal')}<span>打开的连接</span><span class="count">${state.open.length}</span></button></nav><span class="spacer"></span><div class="vault-bottom">${icon('lock')}凭据库已保护<span class="spacer"></span>${icon('check')}</div></aside>`;
}
function renderSidebar(){
 if(state.location==='catalog')return vaultSidebar();
 if(state.mode==='c')return legacyRenderSidebar();
 const source=opened().filter(h=>(h.name+' '+h.host).toLowerCase().includes(state.sideQuery.toLowerCase()));
 return `<aside class="sidebar" aria-label="打开的会话"><div class="side-head"><h3>已打开连接</h3><span class="count">${state.open.length}</span><span class="spacer"></span>${appButton('plus','新建连接','new')}</div><label class="side-search">${icon('search')}<span class="sr">筛选侧栏</span><input id="side-filter" placeholder="查找已打开的连接" value="${esc(state.sideQuery)}"></label><div class="side-scroll">${groupNames.map((name,g)=>{const rows=source.filter(h=>h.group===g);return rows.length?`<div class="section-label">${name}<span class="spacer"></span><span class="count">${rows.length}</span></div>${rows.map(h=>sessionRow(h)).join('')}`:''}).join('')||'<div class="no-results">没有匹配的打开连接</div>'}</div><div class="focus-side-foot"><button class="secondary" data-action="catalog">${icon('plus')}选择主机</button><div class="side-foot"><span class="dot"></span>${state.open.length} 个连接保持打开</div></div></aside>`;
}
function footer(){return `<div class="statusbar"><span><span class="dot"></span>${state.open.length} 个打开会话</span><span class="optional">${state.location==='catalog'?'资源库':state.mode==='c'?'任务分屏':'专注模式 · 无顶部会话标签'}</span><span class="spacer"></span><span class="bottom-badge">${state.demo==='readonly'?'只读模式':'凭据已保护'}</span><span class="optional mono">UTF-8</span><span class="mono">ADIT</span></div>`}
function catalogStage(){
 const filtered=hosts.filter(h=>(state.group<0||h.group===state.group)&&(h.name+' '+h.host+' '+h.user).toLowerCase().includes(state.catalogQuery.toLowerCase()));
 if(filtered.length&&!filtered.some(h=>h.id===state.selectedHost))state.selectedHost=filtered[0].id;
 const selected=filtered.find(h=>h.id===state.selectedHost);
 const grid=state.inventoryLayout==='grid'?`<div class="host-grid">${filtered.map(h=>`<button class="host-tile ${state.inspector&&h.id===state.selectedHost?'selected':''}" data-inspect="${h.id}" aria-label="查看 ${esc(h.name)} 的连接详情">${proto(h)}<span class="session-copy"><b>${esc(h.name)}</b><small>${h.protocol.toLowerCase()} · ${esc(h.user)}</small></span>${state.open.includes(h.id)?'<span class="dot" aria-label="已有打开会话"></span>':''}</button>`).join('')}</div>`:`<table class="host-table"><thead><tr><th>主机名称</th><th class="host-address">地址</th><th class="host-protocol">协议</th><th>操作</th></tr></thead><tbody>${filtered.map(h=>`<tr><td><span class="host-cell">${proto(h)}<span class="truncate">${esc(h.name)}<small>${esc(h.user)}</small></span></span></td><td class="mono host-address">${esc(h.host)}</td><td class="host-protocol">${h.protocol}</td><td><button class="secondary" data-inspect="${h.id}">详情</button></td></tr>`).join('')}</tbody></table>`;
 const inspector=state.inspector&&selected?`<aside class="inspector" aria-label="主机详情"><div class="inspector-top"><span>主机详情</span>${appButton('close','关闭主机详情','inspector-close')}</div>${proto(selected)}<h4>${esc(selected.name)}</h4><span class="env">${groupNames[selected.group]}</span><dl><dt>连接地址</dt><dd class="mono">${esc(selected.host)}</dd><dt>协议与端口</dt><dd class="mono">${selected.protocol} · ${selected.protocol==='SSH'?22:selected.protocol==='VNC'?5901:3389}</dd><dt>用户名</dt><dd class="mono">${esc(selected.user)}</dd><dt>凭据来源</dt><dd>本地凭据库 · 已保护</dd><dt>连接状态</dt><dd>${state.open.includes(selected.id)?'已有打开会话':'尚未打开'}</dd></dl><button class="primary" data-open="${selected.id}">${state.open.includes(selected.id)?'切换到连接':'打开连接'}</button><p class="inspector-note">连接凭据不会显示在此面板中</p></aside>`:'';
 return `<section class="stage" aria-label="资源卡片库"><div class="inventory-layout"><div class="inventory-main"><header class="inventory-header"><div><h3>主机</h3><p>本地资源库 / ${state.group<0?'全部主机':groupNames[state.group]}</p></div><span class="spacer"></span><button class="primary" data-action="new">＋ 新建主机</button></header><label class="inventory-search">${icon('search')}<span class="sr">搜索主机资源</span><input id="catalog-search" placeholder="搜索名称、地址或用户名" value="${esc(state.catalogQuery)}"></label><div class="inventory-label">分组 <small>3</small>${state.group>=0?'<button class="muted" data-group="-1">查看全部</button>':''}</div><div class="folder-grid">${groupNames.map((name,g)=>`<button class="folder-tile ${state.group===g?'active':''}" data-group="${g}"><span class="folder-mark">${icon('folder')}</span><span class="truncate"><b>${name}</b><small>${hosts.filter(h=>h.group===g).length} 台主机</small></span></button>`).join('')}</div><div class="inventory-label">${state.group<0?'全部主机':groupNames[state.group]} <small>${filtered.length}</small><div class="grid-control" aria-label="主机显示方式"><button data-layout="grid" class="${state.inventoryLayout==='grid'?'active':''}" aria-label="卡片视图">${icon('grid')}</button><button data-layout="list" class="${state.inventoryLayout==='list'?'active':''}" aria-label="列表视图">${icon('file')}</button></div></div>${filtered.length?grid:'<div class="no-results">没有匹配的主机，换个名称或地址试试</div>'}</div>${inspector}</div></section>`;
}
function contentHead(h){
 return `<header class="content-head"><div class="focus-heading">${proto(h)}<div class="truncate"><div class="content-title truncate">${esc(h.name)}</div><div class="content-sub mono">${esc(h.user)}@${esc(h.host)}</div></div><span class="connection-chip"><span class="dot ${state.demo==='error'?'error':state.demo==='loading'?'wait':''}"></span><span>${state.demo==='error'?'失败':state.demo==='loading'?'连接中':h.protocol}</span></span></div><div class="head-actions"><span class="env">${groupNames[h.group].replace('环境','')}</span>${appButton('folder','打开文件入口','files')}${appButton('lock',state.demo==='readonly'?'启用会话控制':'切换为只读','readonly')}${appButton('close','关闭当前会话','close-session')}</div></header>`;
}
function tools(){return ''}
function renderModal(){
 if(state.modal!=='info'){legacyRenderModal();return}
 document.querySelector('.modal-scrim')?.remove();
 const details={credentials:['密钥与凭据','保护连接使用的用户名、密码和密钥。','凭据库状态：已启用保护','本轮仅展示导航与组织，不读写真实凭据。'],tunnels:['端口转发','按会话管理本地、远程与动态转发。','已保存规则：3 项','转发列表和编辑器留待下一轮界面设计。'],snippets:['命令片段','将常用命令放在随手可取的位置。','示例片段：服务检查、磁盘用量、进程诊断','这里只展示入口层级，不发送任何命令。'],knownhosts:['已知主机','查看已确认的服务器身份与指纹。','来源：本地主机信任记录','原型没有读取或修改本机 known_hosts。'],logs:['连接日志','按连接和时间查找会话记录。','记录类型：连接、断开与错误','显示内容为合成预览。']};
 const [title,desc,stat,note]=details[state.infoPanel]||details.credentials;
 const el=document.createElement('div');el.className='modal-scrim';el.innerHTML=`<section class="dialog" role="dialog" aria-modal="true" aria-label="${title}"><div class="dialog-head"><h3>${title}</h3>${appButton('close','关闭导航预览','modal-close')}</div><div class="form-body"><div class="info-preview"><p>${desc}</p><div class="info-stat">${icon('lock')}<span>${stat}</span></div><p>${note}</p></div></div><div class="form-actions"><button class="secondary" data-action="modal-close">返回主机</button></div></section>`;el.addEventListener('click',e=>{if(e.target===el)closeModal()});app.append(el);
}
function render(){
 legacyRender();
 app.dataset.study=state.mode;
 document.querySelector('#variant-title').innerHTML=`${({a:'D',b:'E',c:'F'})[state.mode]} / ${modes[state.mode].title}${modes[state.mode].recommend?'<span class="recommend">优先推荐</span>':''}`;
}
actions['inspector-close']=()=>{state.inspector=false;render();requestAnimationFrame(()=>document.querySelector(`[data-inspect="${state.selectedHost}"]`)?.focus())};
document.addEventListener('click',e=>{
 const host=e.target.closest('[data-inspect]');
 if(host){state.selectedHost=Number(host.dataset.inspect);state.inspector=true;render();requestAnimationFrame(()=>document.querySelector('.inspector [data-action="inspector-close"]')?.focus());return}
 const layout=e.target.closest('[data-layout]');
 if(layout){state.inventoryLayout=layout.dataset.layout;render();return}
 const info=e.target.closest('[data-info]');
 if(info){state.infoPanel=info.dataset.info;openModal('info');return}
});
document.addEventListener('dblclick',e=>{const host=e.target.closest('[data-inspect]');if(host)openHost(Number(host.dataset.inspect))});
document.addEventListener('keydown',e=>{if(e.key==='Escape'&&state.inspector&&state.location==='catalog'&&!state.modal){e.preventDefault();actions['inspector-close']()}});
render();
