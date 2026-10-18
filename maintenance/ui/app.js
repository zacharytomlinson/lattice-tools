'use strict';
const $ = id => document.getElementById(id);
const token = document.querySelector('meta[name="lattice-session"]').content;
let view = null, selected = null, firmware = null, connected = false;
const stages = { idle:'Ready when you are', discovering:'Following network links…', discovered:'Network discovered', offering:'Offering release…', header:'Checking release…', preparing:'Preparing inactive storage…', receiving:'Transferring image…', verifying:'Verifying image…', ready:'Verified at the vertex', selected:'Selected for activation', error:'Operation stopped' };
function controls() {
  $('discover').disabled = !connected || !!view?.busy;
  $('file').disabled = !!view?.busy;
  $('send').disabled = !connected || !selected || !firmware || !!view?.busy;
  $('target').textContent = selected ? `Vertex · ${selected.slice(-4)}` : 'No vertex selected';
}
function error(message) { $('stage').textContent = 'Operation stopped'; $('message').textContent = message; $('progress-panel').classList.add('error'); }
async function request(path, body) {
  const response = await fetch(path, {method:'POST', headers:{'x-lattice-session':token,'content-type':'application/octet-stream'}, body});
  if (!response.ok) { let message = `Request failed (${response.status})`; try { message = (await response.json()).error || message; } catch {} throw Error(message); }
}
function render(next) {
  view = next;
  if (!view.contacts.some(c => c.address === selected && c.responded)) selected = null;
  $('radio').textContent = view.local ? `Radio ${view.local} · channel ${view.channel}` : 'Waiting for discovery';
  $('vertices').replaceChildren();
  if (!view.contacts.length) { const empty=document.createElement('div'); empty.className='empty'; empty.textContent=view.stage==='discovering'?'Looking for resident peers…':'Your reachable vertices will appear here.'; $('vertices').append(empty); }
  for (const c of view.contacts) {
    const button=document.createElement('button'); button.className=`vertex${c.address===selected?' selected':''}`; button.dataset.address=c.address; button.disabled=view.busy||!c.responded; button.setAttribute('aria-pressed',String(c.address===selected));
    const icon=document.createElement('span'); icon.className='symbol'; icon.textContent='◇';
    const label=document.createElement('span'), name=document.createElement('strong'), address=document.createElement('small');
    name.textContent=`Vertex · ${c.address.slice(-4)}`; address.textContent=c.address; label.append(name,address);
    const status=document.createElement('span'); status.className='status'; status.textContent=c.responded?(c.via===view.local?'Direct':'Via peer'):'Advertised';
    button.append(icon,label,status); button.onclick=()=>{selected=c.address;render(view);}; $('vertices').append(button);
  }
  $('discovery-note').textContent = view.incomplete.length ? 'Discovery is partial. Some referenced peers did not respond; you can discover again.' : '';
  $('stage').textContent = stages[view.stage] || view.stage;
  $('message').textContent = view.message || (view.stage==='verifying'?'The receiver is checking the image in bounded steps.':view.busy?'Ordinary appliance work takes precedence during transfer.':'');
  $('progress-panel').classList.toggle('error',view.stage==='error');
  const percent=view.total?Math.floor(view.accepted/view.total*100):0;
  $('progress').value=percent; $('percent').textContent=view.total?`${percent}% transferred`:'';
  $('counts').textContent=view.total?`${view.accepted.toLocaleString()} / ${view.total.toLocaleString()} bytes · ${view.committed.toLocaleString()} durable${view.changes?` · ${view.changes} state changes received`:''}${view.lost_changes?` · ${view.lost_changes} buffered changes lost`:''}`:'';
  controls();
}
async function choose(file) {
  if (!file || view?.busy) return;
  firmware=null;controls();
  try {
    if (file.size<193 || file.size>16*1024*1024+192) throw Error('Choose a complete .lvup firmware package (up to 16 MiB).');
    const bytes=await file.slice(0,192).arrayBuffer(), data=new DataView(bytes);
    if (new TextDecoder().decode(bytes.slice(0,4))!=='LVUP'||data.getUint16(4,true)!==1||data.getUint32(68,true)!==file.size-192) throw Error('This file is not a valid Lattice firmware package.');
    firmware=file; $('filename').textContent=file.name; $('file-detail').textContent='Package selected · choose Send update to begin';
    $('release').textContent=data.getBigUint64(60,true).toString(); $('platform').textContent=({1:'nRF52840',2:'ESP32-C6'})[data.getUint16(6,true)]||`Platform ${data.getUint16(6,true)}`;
    $('size').textContent=`${(file.size-192).toLocaleString()} bytes`; $('package-info').hidden=false;controls();
  } catch(e) { $('filename').textContent='Choose another firmware package'; $('package-info').hidden=true;error(e.message); }
}
$('discover').onclick=async()=>{try{await request('/discover');}catch(e){error(e.message);}};
$('file').onchange=e=>choose(e.target.files[0]);
$('drop').onkeydown=e=>{if(e.key==='Enter'||e.key===' '){e.preventDefault();if(!view?.busy)$('file').click();}};
for(const name of ['dragenter','dragover']) $('drop').addEventListener(name,e=>{e.preventDefault();if(!view?.busy)$('drop').classList.add('over');});
for(const name of ['dragleave','drop']) $('drop').addEventListener(name,e=>{e.preventDefault();$('drop').classList.remove('over');});
$('drop').addEventListener('drop',e=>choose(e.dataTransfer.files[0]));
$('send').onclick=async()=>{if(!selected||!firmware)return;$('send').disabled=true;try{await request(`/upload/${selected}`,firmware);}catch(e){error(e.message);controls();}};
const events=new EventSource('/events');
events.onopen=()=>{connected=true;$('connection').textContent='Local application connected';$('connection').classList.remove('offline');controls();};
events.onmessage=e=>render(JSON.parse(e.data));
events.onerror=()=>{connected=false;$('connection').textContent='Local application disconnected';$('connection').classList.add('offline');controls();};
