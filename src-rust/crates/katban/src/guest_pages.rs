// The two guest-server pages, styled after the Clawde TUI's design language.
// Constants rather than functions: axum serves them through `Html(...)` and
// the guest-server tests assert on their content.
pub(crate) const LOGIN_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Katban Guest</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@300;400;500;700&display=swap" rel="stylesheet">
<style>
  /* Clawde TUI design language (crates/tui): ACCENT_BUILD green accent,
     #14141c panels on #0a0a0e, square corners, JetBrains Mono, box-drawing
     rules instead of drop shadows or rounded cards. */
  :root{
    --bg:#0a0a0e;
    --panel:#14141c;
    --border:#484850;
    --text:#ebebf0;
    --muted:#8b8b99;
    --subtle:#6e6e76;
    --accent:#39d353;
    --accent-shy:rgba(57,211,83,.12);
    --error:#ff5f56;
    --mono:'JetBrains Mono',ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
    color-scheme:dark;
  }
  *{box-sizing:border-box;margin:0;padding:0;-webkit-font-smoothing:antialiased;-moz-osx-font-smoothing:grayscale}
  body{font-family:var(--mono);background:var(--bg);color:var(--text);display:flex;align-items:center;justify-content:center;min-height:100vh;margin:0;line-height:1.6}
  ::selection{background:var(--accent-shy);color:var(--text)}
  ::-webkit-scrollbar{width:3px}
  ::-webkit-scrollbar-track{background:transparent}
  ::-webkit-scrollbar-thumb{background:var(--accent);border-radius:99px}
  *{scrollbar-width:thin;scrollbar-color:var(--accent) transparent}
  .bg-grid{position:fixed;inset:0;z-index:-2;pointer-events:none;
    background-image:
      linear-gradient(to right,rgba(255,255,255,.02) 1px,transparent 1px),
      linear-gradient(to bottom,rgba(255,255,255,.02) 1px,transparent 1px);
    background-size:40px 40px}
  .bg-glow{position:fixed;top:0;left:20vw;width:60vw;height:60vh;z-index:-1;pointer-events:none;
    background:radial-gradient(ellipse at center,var(--accent-shy) 0%,transparent 70%);
    opacity:.15;filter:blur(100px)}
  .card{background:var(--panel);border:1px solid var(--border);border-radius:0;padding:0;width:340px}
  .card-head{display:flex;justify-content:space-between;align-items:center;padding:14px 20px;border-bottom:1px solid var(--border)}
  .brand{display:flex;align-items:center;gap:.8rem;font-weight:700;letter-spacing:.05em;font-size:1rem}
  .brand .glyph{color:var(--accent)}
  .card-tag{font-size:.7rem;color:var(--subtle);text-transform:uppercase;letter-spacing:.1em}
  .card-body{padding:20px}
  h1{font-size:15px;margin:0 0 4px;font-weight:500;letter-spacing:-.01em}
  p{color:var(--muted);margin:0 0 20px;font-size:13px}
  .field{display:flex;align-items:center;border:1px solid var(--border);background:#0f0f16;margin-bottom:14px}
  .field:focus-within{border-color:var(--accent)}
  .field .glyph{padding:0 0 0 12px;color:var(--accent);font-weight:700;user-select:none}
  input{flex:1;padding:10px 12px;border:0;background:transparent;color:var(--text);font-family:var(--mono);font-size:14px;outline:none;min-width:0}
  input::placeholder{color:var(--subtle)}
  button{width:100%;padding:10px;border:0;background:var(--accent);color:#0a0a0e;font-family:var(--mono);font-weight:700;cursor:pointer;letter-spacing:.02em}
  button:hover{background:#4ce06a}
  button:disabled{opacity:.6;cursor:wait}
  #err{color:var(--error);display:none;font-size:12px;margin:12px 0 0}
  .hint{margin:14px 0 0;font-size:11px;color:var(--subtle);text-align:center}
</style></head>
<body>
<div class="bg-grid"></div>
<div class="bg-glow"></div>
<div class="card">
  <div class="card-head">
    <div class="brand"><span class="glyph">&#9656;</span> Katban Guest</div>
    <div class="card-tag">guest access</div>
  </div>
  <div class="card-body">
    <h1>Enter the password your host shared with you.</h1>
    <p>Authenticate to open the guest chat.</p>
    <form id="f">
      <div class="field"><span class="glyph">&#10095;</span><input type="password" id="pw" placeholder="password" autofocus autocomplete="current-password"></div>
      <button type="submit" id="join">Join</button>
    </form>
    <p id="err"></p>
    <div class="hint">protected surface &#183; attempts are logged</div>
  </div>
</div>
<script>
const f=document.getElementById('f');
f.addEventListener('submit',async e=>{e.preventDefault();
  const err=document.getElementById('err');err.style.display='none';
  const btn=document.getElementById('join');btn.disabled=true;btn.textContent='Joining\u2026';
  const res=await fetch('/auth',{method:'POST',headers:{'Content-Type':'application/json'},
    body:JSON.stringify({password:document.getElementById('pw').value})});
  if(res.ok){location.href='/chat'}
  else{
    err.textContent=await res.text();err.style.display='block';
    btn.disabled=false;btn.textContent='Join';
  }});
</script></body></html>"#;

pub(crate) const CHAT_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Katban Guest Chat</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@300;400;500;700&display=swap" rel="stylesheet">
<style>
  /* Clawde TUI design language (crates/tui): messages render like the
     transcript — bold green "› " user prefix on a raised #17171f block,
     plain assistant text, box-drawing rules, ACCENT_BUILD accents. */
  :root{
    --bg:#0a0a0e;
    --panel:#14141c;
    --border:#484850;
    --text:#ebebf0;
    --muted:#8b8b99;
    --subtle:#6e6e76;
    --accent:#39d353;
    --accent-shy:rgba(57,211,83,.12);
    --user-bg:#17171f;
    --error:#ff5f56;
    --mono:'JetBrains Mono',ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
    color-scheme:dark;
  }
  *{box-sizing:border-box;margin:0;padding:0;-webkit-font-smoothing:antialiased;-moz-osx-font-smoothing:grayscale}
  html{font-size:90%}
  body{font-family:var(--mono);background:var(--bg);color:var(--text);margin:0;height:100vh;height:100dvh;display:flex;flex-direction:column;line-height:1.6}
  ::selection{background:var(--accent-shy);color:var(--text)}
  ::-webkit-scrollbar{width:3px;height:3px}
  ::-webkit-scrollbar-track{background:transparent}
  ::-webkit-scrollbar-thumb{background:var(--accent);border-radius:99px}
  *{scrollbar-width:thin;scrollbar-color:var(--accent) transparent}
  .bg-grid{position:fixed;inset:0;z-index:-2;pointer-events:none;
    background-image:
      linear-gradient(to right,rgba(255,255,255,.02) 1px,transparent 1px),
      linear-gradient(to bottom,rgba(255,255,255,.02) 1px,transparent 1px);
    background-size:40px 40px}
  .bg-glow{position:fixed;top:0;left:20vw;width:60vw;height:60vh;z-index:-1;pointer-events:none;
    background:radial-gradient(ellipse at center,var(--accent-shy) 0%,transparent 70%);
    opacity:.15;filter:blur(100px)}
  header{padding:12px 20px;border-bottom:1px solid var(--border);display:flex;justify-content:space-between;align-items:center;background:var(--panel)}
  .brand{display:flex;align-items:center;gap:.8rem;font-weight:700;letter-spacing:.05em;font-size:1rem}
  .brand .glyph{color:var(--accent)}
  header button{background:transparent;border:1px solid var(--border);color:var(--muted);font-family:var(--mono);font-size:12px;border-radius:0;padding:6px 12px;cursor:pointer;transition:border-color .15s,color .15s}
  header button:hover{border-color:rgba(57,211,83,.4);color:var(--text)}
  #log{flex:1;overflow-y:auto;padding:20px;display:flex;flex-direction:column;gap:14px;max-width:860px;width:100%;margin:0 auto}
  /* Transcript rows: user messages get the TUI's "› " prefix on a raised
     full-width block; assistant replies are plain text, like the TUI's. */
  .msg{white-space:pre-wrap;line-height:1.65;font-size:14px}
  .user{background:var(--user-bg);padding:10px 14px}
  .user::before{content:'\203A  ';color:var(--accent);font-weight:700}
  .bot::before{content:'\25B8  ';color:var(--subtle);font-weight:700}
  .note{color:var(--subtle);font-size:12px;font-style:italic}
  .note::before{content:'\25B8  ';color:var(--accent);font-style:normal}
  .note.error{color:var(--error)}
  form{display:flex;gap:0;padding:0;border-top:1px solid var(--border);background:var(--panel);max-width:860px;width:100%;margin:0 auto}
  .field{flex:1;display:flex;align-items:center}
  .field .glyph{padding:0 0 0 20px;color:var(--accent);font-weight:700;user-select:none}
  input{flex:1;padding:14px 12px;border:0;background:transparent;color:var(--text);font-family:var(--mono);font-size:14px;outline:none;min-width:0}
  input::placeholder{color:var(--subtle)}
  form button{padding:14px 24px;border:0;border-left:1px solid var(--border);background:transparent;color:var(--accent);font-family:var(--mono);font-weight:700;cursor:pointer;letter-spacing:.02em}
  form button:hover{background:var(--accent-shy)}
  form button:disabled{opacity:.5;cursor:wait}
  #summary{display:none;white-space:pre-wrap;padding:14px 20px;margin:0 20px;background:var(--panel);border:1px solid var(--border);font-size:13px;max-width:820px}
  .summary-row{display:flex;justify-content:center;padding:10px 0 0}
  .summary-head{display:flex;justify-content:space-between;align-items:center;border-bottom:1px solid var(--border);margin:-14px -20px 12px;padding:8px 20px}
  .summary-head span{font-size:.7rem;color:var(--subtle);text-transform:uppercase;letter-spacing:.1em}
  .summary-head button{background:none;border:none;color:var(--subtle);cursor:pointer;font-family:var(--mono);font-size:14px;padding:0}
  .summary-head button:hover{color:var(--text)}
  @media (max-width:640px){
    #log{padding:14px}
    header{padding:10px 14px}
    form .field .glyph{padding-left:14px}
  }
</style></head>
<body>
<div class="bg-grid"></div>
<div class="bg-glow"></div>
<header>
  <div class="brand"><span class="glyph">&#9656;</span> Katban Guest</div>
  <button id="sumBtn">Download session summary</button>
</header>
<div id="log"><div class="note">Chat with Clawde &#8212; web search available, nothing else.</div></div>
<div class="summary-row"><div id="summary"><div class="summary-head"><span>session summary</span><button id="sumClose" title="Close">&times;</button></div><span id="sumBody"></span></div></div>
<form id="f"><div class="field"><span class="glyph">&#10095;</span><input id="msg" placeholder="Ask anything&#8230;" autocomplete="off"></div><button id="send" type="submit">Send</button></form>
<script>
const log=document.getElementById('log'),form=document.getElementById('f'),msg=document.getElementById('msg'),send=document.getElementById('send');
/* Same cat-verb pool the TUI spinner uses (clawde-core/src/spinner.rs). */
const CAT_VERBS=['Basking','Batting','Chasing','Clawing','Crouching','Darting','Fidgeting','Gliding','Grooming','Growling','Hissing','Hunting','Kneading','Leaping','Licking','Loafing','Mauling','Meowing','Napping','Pawing','Playing','Preening','Prowling','Purring','Scratching','Slinking','Sleeping','Sniffing','Snuggling','Stalking','Stretching','Swatting','Yowling'];
const SPIN_GLYPHS=['\u25DC','\u25DD','\u25DE','\u25DF'];/* ◜◝◟◞ — the TUI's effort glyphs */
const SPIN_MS=120;
function add(text,cls){const d=document.createElement('div');d.className='msg '+cls;d.textContent=text;log.appendChild(d);log.scrollTop=log.scrollHeight}
function startSpinner(){
  const n=document.createElement('div');n.className='note';
  const g=document.createElement('span');g.className='spin-glyph';
  const t=document.createElement('span');
  n.appendChild(g);n.appendChild(t);log.appendChild(n);log.scrollTop=log.scrollHeight;
  const verb=CAT_VERBS[Math.floor(Math.random()*CAT_VERBS.length)];
  let i=0;const timer=setInterval(()=>{g.textContent=SPIN_GLYPHS[i++%SPIN_GLYPHS.length]+' ';},SPIN_MS);
  t.textContent=verb+'\u2026';
  return {el:n,stop(){clearInterval(timer);n.remove()}};
}
form.addEventListener('submit',async e=>{e.preventDefault();
  const text=msg.value.trim();if(!text)return;msg.value='';add(text,'user');
  send.disabled=true;const spin=startSpinner();
  try{
    const res=await fetch('/api/chat',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({message:text})});
    spin.stop();
    if(res.ok){const data=await res.json();add(data.reply,'bot')}
    else{add('Sorry \u2014 '+await res.text(),'note error')}
  }catch(err){spin.stop();add('Network error \u2014 try again.','note error')}
  send.disabled=false;msg.focus()});
document.getElementById('sumBtn').addEventListener('click',async()=>{
  const res=await fetch('/api/summary',{method:'POST'});
  if(!res.ok){alert('Summary unavailable right now.');return}
  const data=await res.json();const s=document.getElementById('summary');
  document.getElementById('sumBody').textContent=data.summary;s.style.display='block';
  const blob=new Blob([data.summary],{type:'text/markdown'});
  const a=document.createElement('a');a.href=URL.createObjectURL(blob);a.download='katban-session-summary.md';a.click()});
document.getElementById('sumClose').addEventListener('click',()=>{document.getElementById('summary').style.display='none'});
</script></body></html>"#;
