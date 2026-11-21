'use strict';
const $ = id => document.getElementById(id);
const invoke = (command, args) => window.__TAURI__.core.invoke(command, args);
const colors = ['#007b63','#bd5124','#426aca','#9d4287','#987513','#377a8a'];
let fields=[], controls=[], history=[], latest=[], groups=[], layout=null;
let robotGraphs=null, cameraPort=null;
let session=0, active=false, tab='graphs', selected='', recordOn=false, docked=false;
let lastSample=0, found=new Set(), settingsKey='', runPath='', remembered=false;
function element(tag,text){const node=document.createElement(tag); node.textContent=text;return node;}
function saved(key){try{return localStorage.getItem(key)}catch{return null}}
function save(key,value){try{localStorage.setItem(key,value)}catch{}}
$('host').value=saved('robotIP') || '192.168.43.1';
$('port').value=saved('port') || '5599';
function label(name){return name.replaceAll('_',' ');}
function value(name){const i=fields.findIndex(f=>f[0]===name);return latest[i]??'';}
function notify(e){$('notice').textContent=String(e);}
function chooseTab(next){
 tab=next;
 $('dock').hidden=!(docked&&next==='graphs');
 (next==='graphs'&&docked?$('dock'):$('live')).insertBefore($('camera'),next==='graphs'&&docked?null:document.querySelector('footer'));
 document.querySelectorAll('[data-tab]').forEach(b=>{
   $(b.dataset.tab).hidden=b.dataset.tab!==next;
   b.classList.toggle('selected',b.dataset.tab===next);
 });
 if(next==='graphs'&&docked)$('camera').hidden=false;
 $('dockCamera').textContent=docked?'Hide camera':'Show camera alongside graph';
 if(next==='camera'||docked) showCamera();
 if(next==='graphs') draw();
 if(next==='signals') showSignals();
}
$('dockCamera').onclick=()=>{docked=!docked;chooseTab('graphs');};
document.querySelectorAll('[data-tab]').forEach(b=>b.onclick=()=>chooseTab(b.dataset.tab));
function setup(message=''){
 active=false;session++; $('live').hidden=true;$('setup').hidden=false;
 $('failure').textContent=message;$('status').textContent='Offline';
 $('video').removeAttribute('src');docked=false;notify('');
}
$('disconnect').onclick=async()=>{await invoke('disconnect');setup();};
$('connection').onsubmit=async e=>{
 e.preventDefault(); const id=++session;const host=$('host').value.trim(),port=Number($('port').value);
 try{
 await invoke('connect',{host,port});if(id!==session)return;
 active=true;remembered=false;fields=[];history=[];latest=[];groups=[];layout=null;selected='';controls=[];
 lastSample=0;found=new Set();settingsKey='';recordOn=false;runPath='';
 $('group').replaceChildren();$('resetLayout').hidden=true;$('layoutInfo').textContent='';
 $('setup').hidden=true;$('live').hidden=false;$('address').textContent=host+' · port '+port;
 $('status').textContent='Connecting';$('csv').textContent='Waiting for a run';notify('');chooseTab('graphs');
 robotGraphs=null;cameraPort=null;$('video').hidden=true;$('retry').disabled=true;$('record').disabled=true;
 await poll(id);
 }catch(err){if(id===session)setup(String(err));}
};
function autoGroups(){
 const map=new Map();
 for(const [i,[name,type,unit]] of fields.entries()){
   if(!found.has(i)||type==='string'||['run_id','robot_ts_ms'].includes(name))continue;
   let title=label(name);
   if(name.includes('rpm')) title='Motor speed · RPM';
   else if(/^ll_t[xy]$/.test(name)) title='Limelight target offset';
   else if(/^pinpoint_[xy]_mm$/.test(name)) title='Pinpoint position';
   else if(/^(front|back)_(left|right)_/.test(name)) title=label(name.split('_').slice(2).join('_'));
   const key=title+'|'+unit;
   if(!map.has(key))map.set(key,{title,series:[]});
   map.get(key).series.push(name);
 }
 return [...map.values()];
}
function refreshGroups(){
 groups=layout || (robotGraphs===null?autoGroups():robotGraphs.filter(g=>g.series.some(name=>found.has(fields.findIndex(f=>f[0]===name)))));
 const names=new Set(fields.map(f=>f[0]));
 groups=groups.filter(g=>g.series.some(s=>names.has(s)));
 const previous=selected;
 selected=groups.some(g=>g.title===previous)?previous:(groups[0]?.title||'');
 $('group').replaceChildren(...groups.map(g=>element('option',g.title)));
 $('group').value=selected;
}
$('group').onchange=()=>{selected=$('group').value;draw();};
$('file').onchange=async e=>{
 const file=e.target.files[0];if(!file)return;
 try{
 const data=JSON.parse(await file.text());
 const graphs=data.groups?Object.entries(data.groups).filter(([,g])=>g.graph!==false).map(([id,g])=>({title:g.title||id,series:Object.keys(g.fields)})):data.graphs;
 if(!Array.isArray(graphs)||!graphs.length||graphs.some(g=>typeof g.title!=='string'||!Array.isArray(g.series)||!g.series.every(n=>typeof n==='string')))throw Error('This file has no valid graph layout.');
 layout=graphs;refreshGroups();draw();$('resetLayout').hidden=false;
 $('layoutInfo').textContent=file.name+(groups.length?'':' · No matching robot fields');notify('');
 }catch(err){notify(err.message);}finally{e.target.value='';}
};
$('resetLayout').onclick=()=>{layout=null;refreshGroups();draw();$('layoutInfo').textContent='';$('resetLayout').hidden=true;};
function draw(){
 if(tab!=='graphs')return;
 const graph=groups.find(g=>g.title===selected);
 $('chartTitle').textContent=graph?.title||'Waiting for numeric signals';
 const canvas=$('plot'),w=canvas.clientWidth,h=canvas.clientHeight;if(!w||!h)return;
 const ratio=devicePixelRatio||1;canvas.width=w*ratio;canvas.height=h*ratio;
 const ctx=canvas.getContext('2d');ctx.scale(ratio,ratio);
 const series=(graph?.series||[]).map(n=>fields.findIndex(f=>f[0]===n)).filter(i=>i>=0);
 $('legend').replaceChildren(...series.map((i,j)=>{
 const item=element('span',label(fields[i][0])+'  '+(latest[i]||'—')+' '+fields[i][2]);
 item.style.borderColor=colors[j%colors.length];return item;}));
 const end=history.at(-1)?.[0]||0,start=end-15000;
 let lo=Infinity,hi=-Infinity;
 for(const [,row] of history)for(const i of series)if(row[i]!==''&&Number.isFinite(+row[i])){lo=Math.min(lo,+row[i]);hi=Math.max(hi,+row[i]);}
 if(!Number.isFinite(lo))return;
 if(lo===hi){lo-=0.5;hi+=0.5;}
 const x=t=>64+(t-start)/15000*(w-78),y=v=>12+(hi-v)/(hi-lo)*(h-40);
 ctx.font='12px system-ui';ctx.fillStyle='#516569';
 for(let j=0;j<=4;j++){const v=lo+(hi-lo)*j/4;ctx.fillText(Number(v.toPrecision(4)),1,y(v)+4);ctx.strokeStyle='#dde5df';ctx.beginPath();ctx.moveTo(62,y(v));ctx.lineTo(w-12,y(v));ctx.stroke();}
 ctx.fillText('15 s ago',62,h-3);ctx.fillText('Now',w-34,h-3);
 series.forEach((i,j)=>{ctx.beginPath();ctx.strokeStyle=colors[j%colors.length];ctx.lineWidth=2;let pen=false;
 for(const [t,row] of history){if(row[i]===''||!Number.isFinite(+row[i])){pen=false;continue;}
 if(pen)ctx.lineTo(x(t),y(+row[i]));else ctx.moveTo(x(t),y(+row[i]));pen=true;}ctx.stroke();});
}
function showSignals(){
 const q=$('search').value.toLowerCase();
 $('values').replaceChildren(...fields.flatMap(([n,,u],i)=>{
 if(!n.toLowerCase().includes(q))return [];
 const tr=element('tr','');tr.append(...[n,latest[i]||'—',u].map(v=>element('td',v)));return [tr];}));
}
$('search').oninput=showSignals;
function showSettings(){
 const key=JSON.stringify(controls);if(key===settingsKey)return;settingsKey=key;
 $('settings').replaceChildren(...controls.map(([name,,min,max])=>{
 const row=element('form','');row.className='setting';
 const l=element('label',label(name)+' ('+min+' to '+max+')');const input=document.createElement('input');
 input.type='number';input.step='any';input.min=min;input.max=max;input.required=true;l.append(input);
 const button=element('button','Apply'),msg=element('span','');row.append(l,button,msg);
 row.onsubmit=async e=>{e.preventDefault();button.disabled=true;try{await invoke('apply_setting',{name,value:Number(input.value)});msg.textContent='Applied';}catch(err){msg.textContent=String(err);}finally{button.disabled=false;}};
 return row;}));
 if(!controls.length)$('settings').textContent='This OpMode has no writable settings.';
}
function showCamera(){
 const ready=$('video').naturalWidth>0;$('video').hidden=!ready;$('cameraMsg').hidden=ready;
 $('cameraMsg').textContent=cameraPort?'Waiting for camera stream · initialize a Limelight-enabled OpMode':'This OpMode has no advertised camera';
 const valid=value('ll_valid'),age=value('ll_staleness_ms');
 $('vision').textContent=[value('ll_connected')==='1'?'Camera connected':'',valid==='1'?'Target detected':valid==='0'?'No valid target':'',valid==='1'&&value('ll_fiducial_id')?'Tag '+value('ll_fiducial_id'):'',valid==='1'?'Offset X '+(value('ll_tx')||'—')+' · Y '+(value('ll_ty')||'—'):'',age!==''&&+age>=0?'Result age '+age+' ms':''].filter(Boolean).join('  ·  ');
}
$('retry').onclick=()=>{$('video').src='http://'+$('host').value.trim() + ':' + cameraPort + '/?retry='+Date.now();showCamera();};
$('record').onclick=async()=>{try{await invoke('record',{enabled:!recordOn});recordOn=!recordOn;notify('');}catch(e){notify(e);}};
$('logs').onclick=()=>invoke('open_runs').catch(notify);
async function poll(id){
 while(active&&id===session){
 const f=await invoke('poll');if(id!==session)return;
 if(f.status.startsWith('Connection ended')){setup(f.status+'. Check robot Wi-Fi and initialize the diagnostics OpMode, then reconnect.');return;}
 fields=f.fields;controls=f.controls;showSettings();
 if(fields.length&&!remembered){robotGraphs=f.graphs;
 cameraPort=f.camera?.preview_port;
 $('retry').disabled=!cameraPort;$('record').disabled=!f.camera?.record_port;
 if(cameraPort)$('retry').click();
 save('robotIP',$('host').value.trim());save('port',$('port').value);remembered=true;}
 const oldSize=found.size;
 if(f.log!==runPath){runPath=f.log;history=[];found.clear();selected='';}
 for(const row of f.rows){latest=row[1];history.push(row);latest.forEach((v,i)=>{if(v!=='')found.add(i);});lastSample=row[0];}
 while(history.length&&history[0][0]<lastSample-15000)history.shift();
 if(oldSize!==found.size||!selected)refreshGroups();
 $('status').textContent=lastSample&&Date.now()-lastSample>2000?'Last sample '+Math.floor((Date.now()-lastSample)/1000)+' s ago':f.status;
 $('csv').textContent=f.log?f.log.split(/[\\/]/).pop():'Waiting for a run';
 $('recording').textContent=f.recording;if(f.recording.includes('ended'))recordOn=false;
 $('record').textContent=recordOn?'Stop recording':'Record raw video';
 if(tab==='graphs')draw();if(tab==='signals')showSignals();if(tab==='camera'||docked)showCamera();
 await new Promise(resolve=>setTimeout(resolve,200));
 }
}
window.onresize=draw;
