// Voisu landing hero: a Recording plays out in a mock window.
// The capsule is drawn the way the real Windows overlay draws it
// (apps/windows overlay): 44 bars, newest level at the centre mirrored
// outward, edge fade 0.45 + 0.55·sin, attack 0.65 / release 0.16, and a
// brightness sweep while Processing. Colours follow the logo (green
// outline, blue bars) rather than the overlay's white bars. The voice is a
// synthetic envelope.
(function () {
  "use strict";

  var stage = document.querySelector("[data-hero]");
  if (!stage) return;

  var fig = stage.closest("figure") || stage.parentNode;
  var capsule = stage.querySelector(".capsule");
  var canvas = capsule.querySelector("canvas");
  var ctx = canvas.getContext("2d");
  var titleEl = stage.querySelector(".win-title");
  var bodyEl = stage.querySelector(".win-body");
  var ctxEl = stage.querySelector(".ctx");
  var typedEl = stage.querySelector(".typed");
  var keysEl = fig.querySelector(".keys");
  var rail = fig.querySelectorAll(".rail li");
  var pauseBtn = fig.querySelector(".stage-pause");

  var SCENES = [
    {
      title: "Re: Thursday · Mail",
      term: false,
      ctx: "Hi Sam,",
      text: "Thursday works for me. I'll bring the draft and we can go through the numbers together."
    },
    {
      title: "notes.md · Editor",
      term: false,
      ctx: "## Standup",
      text: "Shipped the paste fallback. Next up: tidy the settings window and finish the docs."
    },
    {
      title: "Terminal",
      term: true,
      ctx: '<span class="ps">~/app $</span> git commit -m "',
      text: 'Fix the overlay flicker after resume"'
    }
  ];

  // Overlay constants (apps/windows overlay).
  var BARS = 44;
  var GAP = 2;
  var HALF = BARS / 2;
  var ATTACK = 0.65;
  var RELEASE = 0.16;
  var SWEEP_MS = 1200;
  var SIGMA = 4.4;
  var BASE = 0.35;

  // Phase lengths (ms).
  var T_IDLE = 900;
  var T_REC = 3300;
  var T_PROC = 1000;
  var T_CHAR = 17;
  var T_HOLD = 2600;

  var reduce = window.matchMedia("(prefers-reduced-motion: reduce)");

  var history = new Array(HALF).fill(0);
  var smooth = 0;
  var lastPush = 0;
  var scene = 0;
  var phase = "idle";
  var phaseStart = 0;
  var envelope = [];
  var typed = 0;
  var paused = false;
  var visible = true;
  var raf = 0;
  var clock = 0; // animation time, frozen while paused
  var lastFrame = 0;

  // ---- Canvas sizing ----
  var W = 0;
  var H = 0;
  function resize() {
    var r = canvas.getBoundingClientRect();
    var dpr = Math.min(window.devicePixelRatio || 1, 3);
    W = Math.max(1, Math.round(r.width));
    H = Math.max(1, Math.round(r.height));
    canvas.width = Math.round(W * dpr);
    canvas.height = Math.round(H * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  }

  // ---- A plausible voice: words of syllables with pauses ----
  function seeded(seed) {
    var s = seed >>> 0;
    return function () {
      s = (s * 1664525 + 1013904223) >>> 0;
      return s / 4294967296;
    };
  }

  function makeEnvelope(seed) {
    var rnd = seeded(seed);
    var out = [];
    var t = 160; // breath before the first word
    while (t < T_REC - 220) {
      var len = 140 + rnd() * 320;
      var peak = 0.55 + rnd() * 0.45;
      out.push({ a: t, b: t + len, peak: peak, rate: 4 + rnd() * 3, ph: rnd() * 6.28 });
      t += len + 40 + rnd() * (rnd() < 0.2 ? 320 : 140);
    }
    return out;
  }

  function voice(t) {
    for (var i = 0; i < envelope.length; i++) {
      var w = envelope[i];
      if (t >= w.a && t <= w.b) {
        var x = (t - w.a) / (w.b - w.a);
        var shape = Math.sin(Math.PI * x);
        var syl = 0.55 + 0.45 * Math.abs(Math.sin(w.ph + (t / 1000) * w.rate * Math.PI));
        return Math.min(1, w.peak * shape * syl * (0.85 + Math.random() * 0.3));
      }
    }
    return 0.02 + Math.random() * 0.03; // room noise
  }

  // ---- Drawing ----
  function roundBar(x, y, w, h) {
    var r = Math.min(w / 2, h / 2);
    ctx.beginPath();
    if (ctx.roundRect) ctx.roundRect(x, y, w, h, r);
    else ctx.rect(x, y, w, h);
    ctx.fill();
  }

  function draw(mode, now) {
    ctx.clearRect(0, 0, W, H);
    var bw = (W - GAP * (BARS - 1)) / BARS;
    // Mid-resize the canvas can measure narrower than its gaps; skip that frame.
    if (bw <= 0) return;
    var minH = Math.max(2, bw * 0.9);
    var sweepPos = ((now % SWEEP_MS) / SWEEP_MS) * (BARS + 8) - 4;
    for (var i = 0; i < BARS; i++) {
      // Newest level sits at the centre and flows outward on both sides.
      var d = i < HALF ? HALF - 1 - i : i - HALF;
      var level;
      var bright;
      if (mode === "proc") {
        level = 0.16;
        var g = Math.exp(-((i - sweepPos) * (i - sweepPos)) / (2 * SIGMA * SIGMA));
        bright = BASE + (1 - BASE) * g;
      } else if (mode === "static") {
        level = history[d];
        bright = 0.6;
      } else {
        level = history[d];
        bright = 1;
      }
      var edge = 0.45 + 0.55 * Math.sin((Math.PI * (i + 0.5)) / BARS);
      var h = Math.max(minH, level * H);
      ctx.fillStyle = "rgba(143,180,255," + (edge * bright).toFixed(3) + ")";
      roundBar(i * (bw + GAP), (H - h) / 2, bw, h);
    }
  }

  // ---- Phases ----
  function setRail(s) {
    rail.forEach(function (li) {
      li.classList.toggle("on", li.dataset.s === s);
    });
  }

  function loadScene(i) {
    var s = SCENES[i];
    titleEl.textContent = s.title;
    bodyEl.classList.toggle("term", s.term);
    ctxEl.innerHTML = s.ctx;
    typedEl.textContent = "";
    typed = 0;
  }

  function enter(p, now) {
    phase = p;
    phaseStart = now;
    if (p === "idle") {
      loadScene(scene);
      envelope = makeEnvelope(scene * 7919 + 17);
      history.fill(0);
      smooth = 0;
      keysEl.classList.remove("down");
      capsule.classList.remove("on");
      setRail("");
    } else if (p === "rec") {
      keysEl.classList.add("down");
      capsule.classList.add("on");
      setRail("rec");
    } else if (p === "proc") {
      keysEl.classList.remove("down");
      setRail("proc");
    } else if (p === "type") {
      capsule.classList.remove("on");
      setRail("ok");
    }
  }

  function tick(now) {
    var t = now - phaseStart;
    if (phase === "idle") {
      if (t > T_IDLE) enter("rec", now);
    } else if (phase === "rec") {
      if (now - lastPush > 42) {
        lastPush = now;
        var target = voice(t);
        smooth += (target - smooth) * (target > smooth ? ATTACK : RELEASE);
        history.unshift(smooth);
        history.length = HALF;
      }
      draw("rec", now);
      if (t > T_REC) enter("proc", now);
    } else if (phase === "proc") {
      draw("proc", now);
      if (t > T_PROC) enter("type", now);
    } else if (phase === "type") {
      var text = SCENES[scene].text;
      var n = Math.min(text.length, Math.floor(t / T_CHAR));
      if (n !== typed) {
        typed = n;
        typedEl.textContent = text.slice(0, n);
      }
      if (n >= text.length && t > text.length * T_CHAR + T_HOLD) {
        scene = (scene + 1) % SCENES.length;
        enter("idle", now);
      }
    }
  }

  function frame(ts) {
    raf = 0;
    if (lastFrame) clock += Math.min(64, ts - lastFrame);
    lastFrame = ts;
    tick(clock);
    schedule();
  }

  function running() {
    return !paused && visible && !document.hidden && !reduce.matches;
  }

  function schedule() {
    if (running() && !raf) raf = requestAnimationFrame(frame);
    if (!running()) lastFrame = 0;
  }

  // Reduced motion: one still frame with the transcript already delivered.
  function still() {
    loadScene(0);
    typedEl.textContent = SCENES[0].text;
    envelope = makeEnvelope(17);
    history = [];
    for (var i = 0; i < HALF; i++) {
      var v = 0.25 + 0.6 * Math.abs(Math.sin(i * 0.9 + 0.4)) * (1 - i / (HALF * 1.4));
      history.push(v);
    }
    capsule.classList.add("on");
    setRail("ok");
    keysEl.classList.remove("down");
    draw("static", 0);
  }

  function start() {
    resize();
    if (reduce.matches) {
      if (pauseBtn) pauseBtn.hidden = true;
      still();
      return;
    }
    if (pauseBtn) pauseBtn.hidden = false;
    enter("idle", clock);
    schedule();
  }

  // ---- Controls (WCAG 2.2.2: anything that moves for more than 5 s can pause) ----
  var PAUSE_ICON = '<svg viewBox="0 0 16 16" aria-hidden="true"><rect x="3.5" y="2.5" width="3" height="11" rx="1" fill="currentColor"/><rect x="9.5" y="2.5" width="3" height="11" rx="1" fill="currentColor"/></svg>';
  var PLAY_ICON = '<svg viewBox="0 0 16 16" aria-hidden="true"><path d="M4.5 2.8v10.4a.8.8 0 0 0 1.2.7l8.3-5.2a.8.8 0 0 0 0-1.4L5.7 2.1a.8.8 0 0 0-1.2.7z" fill="currentColor"/></svg>';

  if (pauseBtn) {
    pauseBtn.addEventListener("click", function () {
      paused = !paused;
      pauseBtn.innerHTML = paused ? PLAY_ICON : PAUSE_ICON;
      pauseBtn.setAttribute("aria-label", paused ? "Play the demo" : "Pause the demo");
      pauseBtn.setAttribute("aria-pressed", paused ? "true" : "false");
      schedule();
    });
  }

  if ("IntersectionObserver" in window) {
    new IntersectionObserver(function (entries) {
      visible = entries[0].isIntersecting;
      schedule();
    }).observe(stage);
  }

  document.addEventListener("visibilitychange", schedule);
  window.addEventListener("resize", function () {
    resize();
    if (reduce.matches) draw("static", 0);
  });
  if (reduce.addEventListener) {
    reduce.addEventListener("change", function () {
      if (raf) cancelAnimationFrame(raf);
      raf = 0;
      start();
    });
  }

  start();
})();
