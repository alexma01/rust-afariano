// rust-afariano: small progressive enhancements, no framework.
(function () {
  "use strict";

  // Mobile sidebar drawer.
  document.addEventListener("click", function (e) {
    if (e.target.closest("[data-nav-toggle]")) {
      document.body.classList.toggle("nav-open");
    } else if (e.target.closest(".scrim")) {
      document.body.classList.remove("nav-open");
    }
  });

  // Copy buttons: data-copy="element id".
  document.addEventListener("click", function (e) {
    var btn = e.target.closest("[data-copy]");
    if (!btn) return;
    var el = document.getElementById(btn.getAttribute("data-copy"));
    if (!el) return;
    var text = el.value !== undefined ? el.value : el.textContent;
    navigator.clipboard.writeText(text.trim()).then(function () {
      var label = btn.querySelector("[data-label]");
      if (label) {
        var old = label.textContent;
        label.textContent = "Copied";
        setTimeout(function () { label.textContent = old; }, 1200);
      } else {
        btn.classList.add("ok");
        setTimeout(function () { btn.classList.remove("ok"); }, 1200);
      }
    });
  });

  // Close an open dropdown when clicking elsewhere or pressing Escape.
  document.addEventListener("click", function (e) {
    document.querySelectorAll("details.dropdown[open]").forEach(function (d) {
      if (!d.contains(e.target)) d.removeAttribute("open");
    });
  });
  document.addEventListener("keydown", function (e) {
    if (e.key !== "Escape") return;
    document.querySelectorAll("details.dropdown[open]").forEach(function (d) { d.removeAttribute("open"); });
    document.body.classList.remove("nav-open");
  });

  // Confirm destructive forms.
  document.addEventListener("submit", function (e) {
    var msg = e.target.getAttribute("data-confirm");
    if (msg && !window.confirm(msg)) e.preventDefault();
  });

  // ------------------------------------------------------------- job log
  // ANSI SGR colours/bold into spans, one <div class="line"> per log line
  // (numbered by CSS), "\r" redraws the current line like a terminal.
  var log = document.getElementById("log");
  if (log && log.dataset.stream) {
    var sgr = { bold: false, fg: 0 };
    var line = null;
    var pendingCR = false;
    var follow = true;
    var waiting = log.querySelector(".waiting");

    function newLine() {
      line = document.createElement("div");
      line.className = "line";
      log.appendChild(line);
    }
    function addText(text) {
      if (!text) return;
      if (!line) newLine();
      if (pendingCR) { line.textContent = ""; pendingCR = false; }
      var span = document.createElement("span");
      var cls = [];
      if (sgr.bold) cls.push("b");
      if (sgr.fg) cls.push("f" + sgr.fg);
      if (cls.length) span.className = cls.join(" ");
      span.textContent = text;
      line.appendChild(span);
    }
    function addPlain(text) {
      var parts = text.split(/(\r\n|\n|\r)/);
      for (var i = 0; i < parts.length; i++) {
        var p = parts[i];
        if (p === "\n" || p === "\r\n") { if (!line) newLine(); line = null; pendingCR = false; }
        else if (p === "\r") { pendingCR = true; }
        else addText(p);
      }
    }
    function feed(text) {
      var re = /\x1b\[([0-9;]*)m|\x1b\[[0-9;?]*[A-Za-z]/g;
      var last = 0, m;
      while ((m = re.exec(text))) {
        addPlain(text.slice(last, m.index));
        last = re.lastIndex;
        if (m[1] === undefined) continue;
        var codes = m[1] === "" ? ["0"] : m[1].split(";");
        for (var i = 0; i < codes.length; i++) {
          var c = parseInt(codes[i], 10);
          if (c === 0 || isNaN(c)) { if (codes[i] === "" && i > 0) continue; sgr.bold = false; sgr.fg = 0; }
          else if (c === 1) sgr.bold = true;
          else if (c === 22) sgr.bold = false;
          else if ((c >= 30 && c <= 37) || (c >= 90 && c <= 97)) sgr.fg = c;
          else if (c === 39) sgr.fg = 0;
          else if (c === 38) i += codes[i + 1] === "5" ? 2 : 4;
        }
      }
      addPlain(text.slice(last));
    }

    log.addEventListener("scroll", function () {
      follow = log.scrollTop + log.clientHeight >= log.scrollHeight - 30;
    });
    var bottom = document.getElementById("log-bottom");
    if (bottom) bottom.addEventListener("click", function () { follow = true; log.scrollTop = log.scrollHeight; });

    var es = new EventSource(log.dataset.stream);
    es.addEventListener("log", function (ev) {
      if (waiting) { waiting.remove(); waiting = null; }
      feed(JSON.parse(ev.data).t);
      if (follow) log.scrollTop = log.scrollHeight;
    });
    es.addEventListener("end", function (ev) {
      es.close();
      var data = JSON.parse(ev.data);
      if (data.reload) setTimeout(function () { window.location.reload(); }, 600);
    });
  }

  // Pages with running pipelines refresh themselves while visible.
  var refresh = document.querySelector("[data-refresh]");
  if (refresh) {
    setTimeout(function () {
      if (!document.hidden) window.location.reload();
    }, parseInt(refresh.getAttribute("data-refresh"), 10) * 1000);
  }
})();
