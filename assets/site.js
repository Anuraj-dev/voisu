// Voisu site: install tabs, copy buttons and OS hints. Every page works
// without this file: tab panels render stacked with their own headings and
// the copy buttons stay hidden.
(function () {
  "use strict";

  var root = document.documentElement;
  root.classList.add("js");

  var ua = navigator.userAgent || "";
  var platform = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || "";
  var isWindows = /Win/i.test(platform) || /Windows/i.test(ua);
  var isLinux = !isWindows && /Linux|X11/i.test(platform + " " + ua) && !/Android/i.test(ua);
  var isMac = !isWindows && /Mac/i.test(platform) && !/iPhone|iPad/i.test(ua);
  var distro = /Ubuntu|Debian/i.test(ua) ? "debian" : /Fedora/i.test(ua) ? "fedora" : /Arch/i.test(ua) ? "arch" : "";

  // ---- Tabs (WAI-ARIA tabs pattern, automatic activation) ----
  document.querySelectorAll("[data-tabs]").forEach(function (box) {
    var list = box.querySelector("[role=tablist]");
    var tabs = Array.prototype.slice.call(box.querySelectorAll("[role=tab]"));
    var panels = tabs.map(function (t) {
      return document.getElementById(t.getAttribute("aria-controls"));
    });

    function select(i, focus) {
      tabs.forEach(function (t, j) {
        var on = i === j;
        t.setAttribute("aria-selected", on ? "true" : "false");
        t.tabIndex = on ? 0 : -1;
        panels[j].hidden = !on;
      });
      if (focus) tabs[i].focus();
    }

    tabs.forEach(function (t, i) {
      t.addEventListener("click", function () {
        select(i, false);
      });
    });

    list.addEventListener("keydown", function (e) {
      var i = tabs.indexOf(document.activeElement);
      if (i < 0) return;
      var n = tabs.length;
      var next = null;
      if (e.key === "ArrowRight") next = (i + 1) % n;
      else if (e.key === "ArrowLeft") next = (i - 1 + n) % n;
      else if (e.key === "Home") next = 0;
      else if (e.key === "End") next = n - 1;
      if (next === null) return;
      e.preventDefault();
      select(next, true);
    });

    // Default tab: a deep link (#install-windows) wins, then the visitor's OS.
    var want = -1;
    var hash = location.hash.slice(1);
    tabs.forEach(function (t, i) {
      if (hash && t.getAttribute("aria-controls") === hash) want = i;
    });
    if (want < 0) {
      var key = isWindows ? "windows" : isMac ? "macos" : distro;
      tabs.forEach(function (t, i) {
        if (key && t.dataset.os === key) want = i;
      });
    }
    select(want < 0 ? 0 : want, false);
  });

  // In-page links to a tab panel (e.g. the hero's "Download for Windows"
  // when it points at #install-windows) open that tab.
  document.addEventListener("click", function (e) {
    var a = e.target.closest && e.target.closest("a[href^='#install-']");
    if (!a) return;
    var tab = document.querySelector("[role=tab][aria-controls='" + a.getAttribute("href").slice(1) + "']");
    if (tab) tab.click();
  });

  // ---- Lead with the visitor's platform on the landing page ----
  var ctaWin = document.querySelector("[data-cta=windows]");
  var lead = isLinux ? document.querySelector("[data-cta=linux]")
    : isMac ? document.querySelector("[data-cta=mac]") : null;
  if (ctaWin && lead) {
    ctaWin.classList.remove("primary");
    lead.classList.add("primary");
    lead.parentNode.insertBefore(lead, ctaWin);
  }
  // The demo's keycaps show the Mac chord to Mac visitors.
  var demoKeys = document.querySelector("[data-keys]");
  if (demoKeys && isMac) demoKeys.innerHTML = "<kbd>Control</kbd><kbd>Option</kbd>";

  // ---- Copy buttons ----
  document.querySelectorAll(".code").forEach(function (block) {
    var pre = block.querySelector("pre");
    if (!pre) return;
    var btn = document.createElement("button");
    btn.type = "button";
    btn.className = "copy";
    btn.textContent = "Copy";
    btn.setAttribute("aria-label", "Copy to clipboard");
    block.appendChild(btn);

    btn.addEventListener("click", function () {
      // Copy what a person would type: skip prompt markers and comments.
      var clone = pre.cloneNode(true);
      clone.querySelectorAll(".p, .c").forEach(function (n) {
        n.remove();
      });
      var text = clone.textContent
        .split("\n")
        .filter(function (line) {
          return line.trim() !== "";
        })
        .join("\n");

      function done(ok) {
        btn.textContent = ok ? "Copied" : "Press Ctrl+C";
        btn.classList.toggle("ok", ok);
        setTimeout(function () {
          btn.textContent = "Copy";
          btn.classList.remove("ok");
        }, 1800);
      }

      if (navigator.clipboard && window.isSecureContext) {
        navigator.clipboard.writeText(text).then(
          function () {
            done(true);
          },
          function () {
            done(false);
          }
        );
      } else {
        var sel = window.getSelection();
        var range = document.createRange();
        range.selectNodeContents(pre);
        sel.removeAllRanges();
        sel.addRange(range);
        done(false);
      }
    });
  });
})();
