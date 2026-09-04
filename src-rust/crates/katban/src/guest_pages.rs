// The two guest-server pages: the Clawde TUI's design language (ACCENT_BUILD
// green, square corners, JetBrains Mono, transcript rows) framed in the
// retro-terminal structure of the original Clawde landing page — 1px section
// lines with crosshair `+` marks at the intersections, a terminal-window
// header with traffic dots, staggered fade-in entrances, a blinking block
// cursor in the prompt, and a subtle CRT scanline overlay.
//
// The chat page additionally mirrors the TUI *startup screen* (crates/tui
// render.rs + rustail.rs): the two-column welcome box with the animated
// Rustail mascot, the BUILD modeline, full-width accent rules framing the
// prompt, and the bottom status bar (cwd / guest pill / ctx / branch). The
// mascot frames and timings match rustail.rs (frame 0 rest 3000 ms, frame 2
// extend 2000 ms, frame 3 stairs 2000 ms) so the web cat breathes like the
// terminal one.
//
// The chat page is served through `chat_page_html()` — a LazyLock that
// injects `clawde_core::constants::APP_VERSION` into the welcome title, the
// same `vX.Y.Z` the TUI prints. A `const &str` cannot interpolate it because
// the version lives in clawde-core, not this crate. LOGIN_PAGE stays a plain
// const for the router.
//
// Constants rather than functions: axum serves them through `Html(...)` and
// the guest-server tests assert on their content.
pub(crate) const LOGIN_PAGE: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Katban Guest</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@300;400;500;700&display=swap" rel="stylesheet">
<style>
  /* Base palette: the TUI's colors (crates/tui constants) on the landing
     page's deep #030303; --line/crosshair are the landing's section lines. */
  :root{
    --bg:#030303;
    --panel:#14141c;
    --line:rgba(255,255,255,.12);
    --crosshair:rgba(255,255,255,.4);
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
  /* CRT scanline overlay — the retro terminal layer. */
  .scanlines{position:fixed;inset:0;z-index:40;pointer-events:none;
    background:repeating-linear-gradient(to bottom,rgba(255,255,255,.02) 0 1px,transparent 1px 3px)}
  /* The landing page's framed container: side rails + divided sections. */
  .frame{width:400px;max-width:100vw;border-left:1px solid var(--line);border-right:1px solid var(--line);background:rgba(0,0,0,.25);margin:0 1rem}
  .section{position:relative;border-bottom:1px solid var(--line)}
  .section.top{border-top:1px solid var(--line)}
  .section::before,.section::after{content:'';position:absolute;width:9px;height:9px;
    background:
      linear-gradient(var(--crosshair),var(--crosshair)) center/1px 100% no-repeat,
      linear-gradient(var(--crosshair),var(--crosshair)) center/100% 1px no-repeat;
    z-index:10}
  .section::before{bottom:-5px;left:-5px}
  .section::after{bottom:-5px;right:-5px}
  .cross-tl,.cross-tr{position:absolute;width:9px;height:9px;
    background:
      linear-gradient(var(--crosshair),var(--crosshair)) center/1px 100% no-repeat,
      linear-gradient(var(--crosshair),var(--crosshair)) center/100% 1px no-repeat;
    z-index:10}
  .cross-tl{top:-5px;left:-5px}
  .cross-tr{top:-5px;right:-5px}
  .bar{display:flex;justify-content:space-between;align-items:center;padding:14px 20px;gap:12px}
  .dots{display:flex;align-items:center;gap:8px}
  .dots span{width:11px;height:11px;border-radius:50%}
  .dots span:nth-child(1){background:#ff5f56}
  .dots span:nth-child(2){background:#ffbd2e}
  .dots span:nth-child(3){background:#27c93f}
  .brand{display:flex;align-items:center;gap:.8rem;font-weight:700;letter-spacing:.05em;font-size:1rem}
  .brand .glyph{color:var(--accent)}
  .card-tag{font-size:.7rem;color:var(--subtle);text-transform:uppercase;letter-spacing:.1em}
  .body{padding:28px 24px}
  h1{font-size:15px;margin:0 0 4px;font-weight:500;letter-spacing:-.01em}
  p{color:var(--muted);margin:0 0 20px;font-size:13px}
  /* Rustail mascot — drawn as an inline SVG (block glyphs mapped to
     quadrant rects) so alignment never depends on webfont fallback for
     the Block Elements range, which Google Fonts' unicode-range subsets
     don't cover. Cell aspect 1:2 matches the TUI's terminal grid. */
  :root{--mascot:#00ff00}
  .mascot{width:100%;max-width:232px;height:128px;margin:0 auto 4px;color:var(--mascot);user-select:none}
  .mascot svg{display:block;width:100%;height:100%}
  .tagline{font-size:11px;color:var(--accent);text-align:center;margin:0 0 18px;letter-spacing:.02em}
  .field{display:flex;align-items:center;border:1px solid var(--border);background:#0f0f16;margin-bottom:14px}
  .field:focus-within{border-color:var(--accent)}
  .field .glyph{padding:0 0 0 12px;color:var(--accent);font-weight:700;user-select:none}
  /* Block cursor: near-white like the TUI's input caret, not the old green. */
  .cursor{display:inline-block;width:9px;height:1.05em;background:var(--text);margin-left:8px;vertical-align:text-bottom;animation:blink 1.1s steps(1) infinite}
  .field:focus-within .cursor{animation:none;opacity:1}
  @keyframes blink{50%{opacity:0}}
  input{flex:1;padding:10px 12px;border:0;background:transparent;color:var(--text);font-family:var(--mono);font-size:14px;outline:none;min-width:0}
  input::placeholder{color:var(--subtle)}
  button{width:100%;padding:10px;border:0;background:var(--accent);color:#030303;font-family:var(--mono);font-weight:700;cursor:pointer;letter-spacing:.02em}
  button:hover{background:#4ce06a}
  button:disabled{opacity:.6;cursor:wait}
  #err{color:var(--error);display:none;font-size:12px;margin:12px 0 0}
  .foot{border-bottom:none}
  .hint{margin:0;padding:12px 20px;font-size:11px;color:var(--subtle);text-align:center}
  .fade-in{opacity:0;animation:fadeIn .8s ease forwards}
  .d-1{animation-delay:.1s}
  .d-2{animation-delay:.25s}
  .d-3{animation-delay:.4s}
  @keyframes fadeIn{to{opacity:1}}
  @media (max-width:480px){.frame{margin:0;border-left:0;border-right:0}}
</style></head>
<body>
<div class="bg-grid"></div>
<div class="bg-glow"></div>
<div class="scanlines"></div>
<div class="frame fade-in d-1">
  <div class="section top">
    <div class="cross-tl"></div><div class="cross-tr"></div>
    <div class="bar">
      <div class="dots"><span></span><span></span><span></span></div>
      <div class="brand"><span class="glyph">&#9656;</span> Katban Guest</div>
      <div class="card-tag">guest access</div>
    </div>
  </div>
  <div class="section">
    <div class="body fade-in d-2">
      <div class="mascot" id="mascot" aria-hidden="true"></div>
      <div class="tagline">All borders Are porous to cats</div>
      <h1>Enter the password your host shared with you.</h1>
      <p>Authenticate to open the guest chat.</p>
      <form id="f">
        <div class="field"><span class="glyph">&#10095;</span><span class="cursor"></span><input type="password" id="pw" placeholder="password" autofocus autocomplete="current-password"></div>
        <button type="submit" id="join">Join</button>
      </form>
      <p id="err"></p>
    </div>
  </div>
  <div class="section foot fade-in d-3">
    <div class="hint">protected surface &#183; attempts are logged</div>
  </div>
</div>
<script>
/* Rustail rest pose, verbatim from crates/tui/rustail.rs FRAMES[0]
   (8 rows × 29 cols). Drawn as SVG by the shared mascotSVG() below. */
const REST=[
"                             ",
"                             ",
"                   ▄▛▜▛▜▄    ",
"                 ▟▛▜▙▟▙▟▛▜▙  ",
"                 █▙▟▛▔▔▜▙▟█  ",
"                 ██▘    ▝██  ",
"                 ▜█▖ ▗▖ ▗█▛  ",
"                  ████████   "
];
/* Block-element glyph → filled quadrant rects within one 1×1 cell. */
const QUAD={'█':[0,0,1,1],'▀':[0,0,1,.5],'▄':[0,.5,1,.5],'▌':[0,0,.5,1],'▐':[.5,0,.5,1],'▖':[0,.5,.5,.5],'▗':[.5,.5,.5,.5],'▘':[0,0,.5,.5],'▝':[.5,0,.5,.5],'▚':[0,0,.5,.5,.5,.5,.5,.5],'▞':[.5,0,.5,.5,0,.5,.5,.5],'▛':[0,0,.5,.5,.5,0,.5,.5,0,.5,.5,.5],'▜':[0,0,.5,.5,.5,0,.5,.5,.5,.5,.5,.5],'▙':[0,0,.5,.5,0,.5,.5,.5,.5,.5,.5,.5],'▟':[.5,0,.5,.5,0,.5,.5,.5,.5,.5,.5,.5],'▔':[0,0,1,.25]};
function mascotSVG(rows){
  let r='';
  rows.forEach((row,y)=>{[...row].forEach((ch,x)=>{const q=QUAD[ch];if(!q)return;
    for(let i=0;i<q.length;i+=4)r+=`<rect x="${x+q[i]}" y="${y+q[i+1]}" width="${q[i+2]}" height="${q[i+3]}"/>`;});});
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 29 8" preserveAspectRatio="none" shape-rendering="crispEdges" fill="currentColor" aria-hidden="true">${r}</svg>`;
}
document.getElementById('mascot').innerHTML=mascotSVG(REST);
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

/// Chat page up to the version-dependent welcome title; `{VERSION}` is
/// replaced once at first render by `chat_page_html()`.
const CHAT_PAGE_HEAD: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Katban Guest Chat</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@300;400;500;700&display=swap" rel="stylesheet">
<style>
  /* Transcript rows keep the TUI language: bold green "&#8250; " user prefix
     on a raised #17171f block, plain "&#9656; " assistant rows. The frame,
     section lines, crosshairs, dots, scanlines and fades are the landing
     page's retro-terminal structure; the welcome box, modeline, accent rules
     and status bar mirror the TUI startup screen (render.rs + rustail.rs). */
  :root{
    --bg:#030303;
    --panel:#14141c;
    --line:rgba(255,255,255,.12);
    --crosshair:rgba(255,255,255,.4);
    --border:#484850;
    --text:#ebebf0;
    --muted:#8b8b99;
    --subtle:#6e6e76;
    --accent:#39d353;
    --accent-shy:rgba(57,211,83,.12);
    --user-bg:#17171f;
    --effort:#b48cff;
    --hint-blue:#64b4ff;
    --error:#ff5f56;
    --mono:'JetBrains Mono',ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;
    color-scheme:dark;
  }
  *{box-sizing:border-box;margin:0;padding:0;-webkit-font-smoothing:antialiased;-moz-osx-font-smoothing:grayscale}
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
  .scanlines{position:fixed;inset:0;z-index:40;pointer-events:none;
    background:repeating-linear-gradient(to bottom,rgba(255,255,255,.02) 0 1px,transparent 1px 3px)}
  .frame{flex:1;display:flex;flex-direction:column;max-width:960px;width:100%;margin:0 auto;border-left:1px solid var(--line);border-right:1px solid var(--line);background:rgba(0,0,0,.25)}
  .section{position:relative;border-bottom:1px solid var(--line)}
  .section.top{border-top:1px solid var(--line)}
  .section::before,.section::after{content:'';position:absolute;width:9px;height:9px;
    background:
      linear-gradient(var(--crosshair),var(--crosshair)) center/1px 100% no-repeat,
      linear-gradient(var(--crosshair),var(--crosshair)) center/100% 1px no-repeat;
    z-index:10}
  .section::before{bottom:-5px;left:-5px}
  .section::after{bottom:-5px;right:-5px}
  .cross-tl,.cross-tr{position:absolute;width:9px;height:9px;
    background:
      linear-gradient(var(--crosshair),var(--crosshair)) center/1px 100% no-repeat,
      linear-gradient(var(--crosshair),var(--crosshair)) center/100% 1px no-repeat;
    z-index:10}
  .cross-tl{top:-5px;left:-5px}
  .cross-tr{top:-5px;right:-5px}
  header{display:flex;justify-content:space-between;align-items:center;padding:12px 20px;background:var(--panel);gap:12px}
  .dots{display:flex;align-items:center;gap:8px}
  .dots span{width:11px;height:11px;border-radius:50%}
  .dots span:nth-child(1){background:#ff5f56}
  .dots span:nth-child(2){background:#ffbd2e}
  .dots span:nth-child(3){background:#27c93f}
  .brand{display:flex;align-items:center;gap:.8rem;font-weight:700;letter-spacing:.05em;font-size:1rem}
  .brand .glyph{color:var(--accent)}
  header button{background:transparent;border:1px solid var(--border);color:var(--muted);font-family:var(--mono);font-size:12px;border-radius:0;padding:6px 12px;cursor:pointer;transition:border-color .15s,color .15s}
  header button:hover{border-color:rgba(57,211,83,.4);color:var(--text)}

  /* ── Welcome box ── the TUI's two-column startup box ─────────────── */
  .welcome{display:flex;background:var(--panel);padding:10px 0 8px}
  .w-left{width:250px;min-width:220px;padding:0 12px}
  .w-div{width:1px;background:var(--accent)}
  .w-right{flex:1;min-width:0;padding:0 14px;font-size:12.5px}
  .welcome .hi{font-weight:700;color:#ffffff;font-size:14px;margin:0 0 2px}
  .welcome .hi .ver{color:#8b8b99;font-weight:400;font-size:12px}
  /* Rustail mascot — drawn as an inline SVG (block glyphs mapped to
     quadrant rects) so alignment never depends on webfont fallback for
     the Block Elements range, which Google Fonts' unicode-range subsets
     don't cover. Cell aspect 1:2 matches the TUI's terminal grid. */
  :root{--mascot:#00ff00}
  .mascot{width:100%;max-width:232px;height:128px;margin:0 0 4px;color:var(--mascot);user-select:none}
  .mascot svg{display:block;width:100%;height:100%}
  .tagline{font-size:11.5px;color:var(--accent)}
  .r-h{font-weight:700;color:var(--accent);font-size:13px}
  .r-t{color:var(--text)}
  .r-sec{margin-top:12px}
  .r-act{color:#8b8b99;font-size:12px;font-style:italic}
  .r-act::before{content:'\25B8  ';color:var(--accent);font-style:normal}

  /* ── Transcript ── */
  #log{flex:1;overflow-y:auto;padding:20px;display:flex;flex-direction:column;gap:14px}
  .msg{white-space:pre-wrap;line-height:1.65;font-size:14px;animation:fadeIn .25s ease}
  .user{background:var(--user-bg);padding:10px 14px}
  .user::before{content:'\203A  ';color:var(--accent);font-weight:700}
  .bot::before{content:'\25B8  ';color:var(--subtle);font-weight:700}
  .note{color:var(--subtle);font-size:12px;font-style:italic}
  .note::before{content:'\25B8  ';color:var(--accent);font-style:normal}
  .note.error{color:var(--error)}
  .summary-row{display:flex;justify-content:center;padding:10px 0 0;background:var(--bg)}
  #summary{display:none;white-space:pre-wrap;padding:14px 20px;background:var(--panel);border:1px solid var(--border);font-size:13px;max-width:820px}
  .summary-head{display:flex;justify-content:space-between;align-items:center;border-bottom:1px solid var(--border);margin:-14px -20px 12px;padding:8px 20px}
  .summary-head span{font-size:.7rem;color:var(--subtle);text-transform:uppercase;letter-spacing:.1em}
  .summary-head button{background:none;border:none;color:var(--subtle);cursor:pointer;font-family:var(--mono);font-size:14px;padding:0}
  .summary-head button:hover{color:var(--text)}

  /* ── Modeline + prompt + status bar ── the TUI's bottom trio ─────── */
  .modeline{display:flex;justify-content:space-between;align-items:center;gap:12px;padding:7px 14px 5px;background:var(--bg)}
  .pill{background:var(--accent);color:#030303;font-weight:700;font-size:11.5px;padding:1px 7px;letter-spacing:.03em}
  .ml-model{font-weight:700;color:var(--text)}
  .ml-dim{color:#6e6e76}
  .ml-effort{color:var(--effort)}
  .ml-right{color:#6e6e76;font-size:12px;white-space:nowrap}
  .ml-right b{color:var(--hint-blue);font-weight:400}
  .rule{height:1px;background:var(--accent)}
  form{display:flex;gap:0;background:var(--panel)}
  .field{flex:1;display:flex;align-items:center}
  .field .glyph{padding:0 0 0 20px;color:var(--accent);font-weight:700;user-select:none}
  /* Block cursor: near-white like the TUI's input caret. */
  .cursor{display:inline-block;width:9px;height:1.05em;background:var(--text);margin-left:10px;vertical-align:text-bottom;animation:blink 1.1s steps(1) infinite}
  .field:focus-within .cursor{animation:none;opacity:1}
  @keyframes blink{50%{opacity:0}}
  input{flex:1;padding:14px 12px;border:0;background:transparent;color:var(--text);font-family:var(--mono);font-size:14px;outline:none;min-width:0}
  input::placeholder{color:var(--subtle)}
  form button{padding:14px 24px;border:0;border-left:1px solid var(--border);background:transparent;color:var(--accent);font-family:var(--mono);font-weight:700;cursor:pointer;letter-spacing:.02em}
  form button:hover{background:var(--accent-shy)}
  form button:disabled{opacity:.5;cursor:wait}
  .statusbar{display:flex;justify-content:space-between;align-items:center;gap:12px;padding:9px 14px 11px;font-size:12px;background:var(--bg)}
  .sb-cwd{color:#6e6e76;display:flex;align-items:center;gap:14px}
  .sb-pill{color:#508c50;border:1px solid rgba(80,140,80,.35);padding:0 6px;font-size:11px}
  .sb-pill.on{color:#50c850;font-weight:700;border-color:rgba(80,200,80,.45)}
  .sb-right{display:flex;align-items:center;gap:14px;white-space:nowrap}
  .sb-ctx{color:#50c850}
  .sb-branch{color:#3ddbd9}
  .fade-in{opacity:0;animation:fadeIn .8s ease forwards}
  .d-1{animation-delay:.1s}
  .d-2{animation-delay:.25s}
  .d-3{animation-delay:.4s}
  @keyframes fadeIn{to{opacity:1}}
  @media (max-width:640px){
    #log{padding:14px}
    header{padding:10px 14px}
    form .field .glyph{padding-left:14px}
    .frame{border-left:0;border-right:0}
    .w-left{min-width:0;width:44%}
    .sb-cwd{display:none}
  }
</style></head>
<body>
<div class="bg-grid"></div>
<div class="bg-glow"></div>
<div class="scanlines"></div>
<div class="frame">
  <header class="section top fade-in d-1">
    <div class="cross-tl"></div><div class="cross-tr"></div>
    <div class="dots"><span></span><span></span><span></span></div>
    <div class="brand"><span class="glyph">&#9656;</span> Katban Guest</div>
    <button id="sumBtn">Download session summary</button>
  </header>
  <div class="welcome section fade-in d-2">
    <div class="w-left">
      <p class="hi">Welcome back! <span class="ver">v{VERSION}</span></p>
      <div class="mascot" id="mascot" aria-hidden="true"></div>
      <div class="tagline">All borders Are porous to cats</div>
    </div>
    <div class="w-div"></div>
    <div class="w-right">
      <div class="r-h">Tips for getting started</div>
      <div class="r-t">Edit AGENTS.md to add instructions for Clawde &#8212; or just ask below.</div>
      <div class="r-sec">
        <div class="r-h">Recent activity</div>
        <div class="r-act">No recent activity</div>
      </div>
    </div>
  </div>
  <div id="log" class="section fade-in d-3"><div class="note">Chat with Clawde &#8212; web search available, nothing else.</div></div>
  <div class="summary-row"><div id="summary"><div class="summary-head"><span>session summary</span><button id="sumClose" title="Close">&times;</button></div><span id="sumBody"></span></div></div>
  <div class="modeline">
    <span><span class="pill">BUILD</span> <span class="ml-model">auto</span> <span class="ml-dim">&#183; guest</span> <span class="ml-effort">&#183; &#9680; medium</span></span>
    <span class="ml-right"><b>Auto</b> &#183; Type a message</span>
  </div>
  <div class="rule"></div>
  <form id="f" class="section"><div class="field"><span class="glyph">&#10095;</span><span class="cursor"></span><input id="msg" placeholder="Ask anything&#8230;" autocomplete="off"></div><button id="send" type="submit">Send</button></form>
  <div class="rule"></div>
  <div class="statusbar">
    <div class="sb-cwd"><span>~/katban</span><span class="sb-pill on">guest:online</span></div>
    <div class="sb-right"><span class="sb-ctx">ctx: 0%</span><span class="sb-branch">&#8806; katban</span></div>
  </div>
</div>
<script>
/* Rustail mascot frames — verbatim from crates/tui/rustail.rs FRAMES
   (frame 0 rest, frame 2 claw extend, frame 3 stairs; each 8 rows × 29
   columns). Drawn as SVG by mascotSVG() below with the TUI's frame
   timings: rest 3000 ms, extend 2000 ms, stairs 2000 ms. */
const M=[
[
"                             ",
"                             ",
"                   ▄▛▜▛▜▄    ",
"                 ▟▛▜▙▟▙▟▛▜▙  ",
"                 █▙▟▛▔▔▜▙▟█  ",
"                 ██▘    ▝██  ",
"                 ▜█▖ ▗▖ ▗█▛  ",
"                  ████████   "
],
[
"                  ▗ ▐  ▌ ▖   ",
"                  ▐▄▛▜▛▜▄▌   ",
"                 ▟▛▜▙▟▙▟▛▜▙  ",
"                 █▙▟▛▔▔▜▙▟█  ",
"                 ██▘    ▝██  ",
"                 ▜█▖ ▗▖ ▗█▛  ",
"                  ████████   ",
"                  ████████   "
],
[
"                  ▗ ▐  ▌ ▖   ",
"    ▚  ▚          ▐▄▛▜▛▜▄▌   ",
"  ▚  ▚  ▚        ▟▛▜▙▟▙▟▛▜▙  ",
"   ▚  ▚  ▚       █▙▟▛▔▔▜▙▟█  ",
" ▚  ▚  ▚         ██▘    ▝██  ",
"  ▚  ▚  ▚        ▜█▖ ▗▖ ▗█▛  ",
"   ▚  ▚           ████████   ",
"                  ████████   "
]];
/* Block-element glyph → filled quadrant rects within one 1×1 cell. */
const QUAD={'█':[0,0,1,1],'▀':[0,0,1,.5],'▄':[0,.5,1,.5],'▌':[0,0,.5,1],'▐':[.5,0,.5,1],'▖':[0,.5,.5,.5],'▗':[.5,.5,.5,.5],'▘':[0,0,.5,.5],'▝':[.5,0,.5,.5],'▚':[0,0,.5,.5,.5,.5,.5,.5],'▞':[.5,0,.5,.5,0,.5,.5,.5],'▛':[0,0,.5,.5,.5,0,.5,.5,0,.5,.5,.5],'▜':[0,0,.5,.5,.5,0,.5,.5,.5,.5,.5,.5],'▙':[0,0,.5,.5,0,.5,.5,.5,.5,.5,.5,.5],'▟':[.5,0,.5,.5,0,.5,.5,.5,.5,.5,.5,.5],'▔':[0,0,1,.25]};
function mascotSVG(rows){
  let r='';
  rows.forEach((row,y)=>{[...row].forEach((ch,x)=>{const q=QUAD[ch];if(!q)return;
    for(let i=0;i<q.length;i+=4)r+=`<rect x="${x+q[i]}" y="${y+q[i+1]}" width="${q[i+2]}" height="${q[i+3]}"/>`;});});
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 29 8" preserveAspectRatio="none" shape-rendering="crispEdges" fill="currentColor" aria-hidden="true">${r}</svg>`;
}
const MC=document.getElementById('mascot');
const T0=performance.now();
function mascotTick(now){
  const el=(now-T0)%7000;
  const f=(el>=3000&&el<5000)?1:(el>=5000)?2:0;
  MC.innerHTML=mascotSVG(M[f]);
  requestAnimationFrame(mascotTick);
}
requestAnimationFrame(mascotTick);
const log=document.getElementById('log'),form=document.getElementById('f'),msg=document.getElementById('msg'),send=document.getElementById('send');
/* Same cat-verb pool the TUI spinner uses (clawde-core/src/spinner.rs). */
const CAT_VERBS=['Basking','Batting','Chasing','Clawing','Crouching','Darting','Fidgeting','Gliding','Grooming','Growling','Hissing','Hunting','Kneading','Leaping','Licking','Loafing','Mauling','Meowing','Napping','Pawing','Playing','Preening','Prowling','Purring','Scratching','Slinking','Sleeping','Sniffing','Snuggling','Stalking','Stretching','Swatting','Yowling'];
const SPIN_GLYPHS=['\u25DC','\u25DD','\u25DE','\u25DF'];
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

/// (`🐾 Clawde v0.3.0` in the terminal becomes `Welcome back! v0.3.0` here).
/// Built once per process; a `const &str` cannot interpolate the version
/// because it lives in clawde-core, not this crate's env.
pub(crate) fn chat_page_html() -> &'static str {
    static PAGE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        CHAT_PAGE_HEAD.replace("{VERSION}", clawde_core::constants::APP_VERSION)
    });
    &PAGE
}
