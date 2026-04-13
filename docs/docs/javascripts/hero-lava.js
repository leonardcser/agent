(function () {
  var container = document.getElementById("hero-lava");
  if (!container) return;

  var pre = document.createElement("pre");
  pre.id = "lava-canvas";
  pre.setAttribute("aria-hidden", "true");
  container.appendChild(pre);

  var COLS = 0;
  var ROWS = 0;
  var cellWidth = 10.8;
  var cellHeight = 21.6;
  var buf = [];
  var heat = [];
  var nextHeat = [];
  var flow = [];
  var animFrame = 0;
  var lastTime = 0;
  var sparkles = [];
  var drips = [];
  var blasts = [];
  var mouse = {
    active: false,
    x: 0,
    y: 0,
    vx: 0,
    vy: 0,
    px: 0,
    py: 0,
    power: 0,
    lastMove: 0,
  };

  var LAVA_CHARS = " .,:;-~=+*ox%#@";
  var LAVA_CRUST_CHARS = ":;=~_";
  var SPARK_CHARS = ".'`^*+";
  var DRIP_CHAR = "|";
  var BASE_LEVEL = 0.86;
  var FRAME_INTERVAL = 1000 / 30;
  var coarsePointerQuery = window.matchMedia ? window.matchMedia("(pointer: coarse)") : null;

  function currentScheme() {
    return document.body.getAttribute("data-md-color-scheme") || "default";
  }

  function isLightTheme() {
    return currentScheme() === "default";
  }

  function clamp(v, min, max) {
    return Math.max(min, Math.min(max, v));
  }

  function idx(x, y) {
    return y * COLS + x;
  }

  function createCell() {
    return {
      ch: " ",
      color: "transparent",
    };
  }

  function measureCell() {
    var probe = document.createElement("span");
    probe.textContent = "M";
    probe.style.position = "absolute";
    probe.style.visibility = "hidden";
    probe.style.whiteSpace = "pre";
    probe.style.fontFamily = getComputedStyle(pre).fontFamily;
    probe.style.fontSize = getComputedStyle(pre).fontSize;
    probe.style.lineHeight = getComputedStyle(pre).lineHeight;
    probe.style.letterSpacing = getComputedStyle(pre).letterSpacing;
    container.appendChild(probe);

    var rect = probe.getBoundingClientRect();
    cellWidth = Math.max(1, rect.width);
    cellHeight = Math.max(1, rect.height);
    container.removeChild(probe);
  }

  function targetCellSize(width, height) {
    var scale = 1;
    if (width < 720 || height < 540) scale = 1.2;
    if (width < 560 || height < 420) scale = 1.35;
    if (coarsePointerQuery && coarsePointerQuery.matches) scale = Math.max(scale, 1.45);
    return scale;
  }

  function resize() {
    var w = container.clientWidth;
    var h = container.clientHeight;
    measureCell();
    var scale = targetCellSize(w, h);
    COLS = Math.max(36, Math.ceil(w / (cellWidth * scale)));
    ROWS = Math.max(18, Math.ceil(h / (cellHeight * scale)));
    buf = new Array(ROWS * COLS);
    heat = new Array(ROWS * COLS);
    nextHeat = new Array(ROWS * COLS);
    flow = new Array(ROWS * COLS);

    for (var i = 0; i < buf.length; i++) {
      buf[i] = createCell();
      heat[i] = 0;
      nextHeat[i] = 0;
      flow[i] = Math.random() * Math.PI * 2;
    }

    sparkles = [];
    drips = [];
    blasts = [];
    seedLava();
    spawnDrips();
  }

  function seedLava() {
    for (var y = 0; y < ROWS; y++) {
      for (var x = 0; x < COLS; x++) {
        var rowFrac = y / Math.max(1, ROWS - 1);
        var surface = surfaceHeight(x, 0);
        var depth = rowFrac - surface;
        heat[idx(x, y)] = depth > 0 ? clamp(depth * 4.4, 0, 1) : 0;
      }
    }
  }

  function surfaceHeight(x, time) {
    var fx = x / Math.max(1, COLS - 1);
    var waveA = Math.sin(fx * 7.5 + time * 0.9) * 0.015;
    var waveB = Math.sin(fx * 17 + time * 0.42 + 1.3) * 0.008;
    var sag = Math.pow(Math.abs(fx - 0.5) * 1.6, 2) * 0.028;
    return BASE_LEVEL + waveA + waveB + sag;
  }

  function spawnDrips() {
    var count = Math.max(3, Math.round(COLS / 30));
    for (var i = 0; i < count; i++) {
      drips.push(createDrip());
    }
  }

  function createDrip() {
    return {
      sourceX: clamp(2 + Math.floor(Math.random() * (COLS - 4)), 1, COLS - 2),
      anchorY: 0,
      length: 1,
      maxLength: 2 + Math.floor(Math.random() * 3),
      width: Math.random() > 0.7 ? 2 : 1,
      state: "forming",
      timer: 0.5 + Math.random() * 1.8,
      y: 0,
      speed: 10 + Math.random() * 7,
      headChar: "o",
    };
  }

  function spawnPop(x, y, energy) {
    var count = 2 + Math.floor(Math.random() * 4 + energy * 3);
    for (var i = 0; i < count; i++) {
      sparkles.push({
        x: x + (Math.random() - 0.5) * 1.6,
        y: y,
        vx: (Math.random() - 0.5) * (2.4 + energy * 2.5),
        vy: -(1.2 + Math.random() * (2.1 + energy * 2.8)),
        life: 0.4 + Math.random() * 0.8,
        maxLife: 0.4 + Math.random() * 0.8,
        char: SPARK_CHARS[Math.floor(Math.random() * SPARK_CHARS.length)],
      });
    }
  }

  function spawnBlast(x, y, power) {
    var count = 18 + Math.floor(power * 20);
    for (var i = 0; i < count; i++) {
      var angle = Math.random() * Math.PI * 2;
      var speed = 4 + Math.random() * (6 + power * 8);
      sparkles.push({
        x: x + (Math.random() - 0.5) * 0.8,
        y: y + (Math.random() - 0.5) * 0.8,
        vx: Math.cos(angle) * speed,
        vy: Math.sin(angle) * speed,
        life: 0.35 + Math.random() * 0.55,
        maxLife: 0.35 + Math.random() * 0.55,
        char: SPARK_CHARS[Math.floor(Math.random() * SPARK_CHARS.length)],
      });
    }

    blasts.push({
      x: x,
      y: y,
      radius: 0,
      maxRadius: 7 + power * 9,
      life: 0.24,
      maxLife: 0.24,
      heat: 0.5 + power * 0.35,
    });
  }

  function updateDrips(dt, time) {
    for (var i = 0; i < drips.length; i++) {
      var drip = drips[i];
      drip.timer -= dt;

      if (drip.state === "forming") {
        if (drip.timer <= 0) {
          if (drip.length < drip.maxLength) {
            drip.length += 1;
            drip.timer = 0.18 + Math.random() * 0.28;
          } else {
            drip.state = "hanging";
            drip.timer = 0.2 + Math.random() * 0.45;
            drip.y = drip.anchorY + drip.length;
            drip.headChar = Math.random() > 0.5 ? "o" : "O";
          }
        }
        continue;
      }

      if (drip.state === "hanging") {
        if (drip.timer <= 0) {
          drip.state = "falling";
          drip.speed = 8 + Math.random() * 7;
        }
        continue;
      }

      drip.y += drip.speed * dt;
      drip.speed += dt * 18;

      var lavaTop = Math.floor(surfaceHeight(drip.sourceX, time) * ROWS);
      if (drip.y >= lavaTop) {
        var splashY = Math.max(0, lavaTop - 1);
        var energy = 0.35 + Math.random() * 0.55;
        injectHeat(drip.sourceX, splashY + 1, 0.3 + energy * 0.25, 3);
        spawnPop(drip.sourceX, splashY, energy);
        drips[i] = createDrip();
      }
    }
  }

  function injectHeat(cx, cy, amount, radius) {
    for (var y = Math.max(0, cy - radius); y <= Math.min(ROWS - 1, cy + radius); y++) {
      for (var x = Math.max(0, cx - radius); x <= Math.min(COLS - 1, cx + radius); x++) {
        var dx = x - cx;
        var dy = y - cy;
        var dist = Math.sqrt(dx * dx + dy * dy);
        if (dist > radius) continue;
        var influence = 1 - dist / Math.max(1, radius);
        var pos = idx(x, y);
        heat[pos] = clamp(heat[pos] + amount * influence, 0, 1.4);
      }
    }
  }

  function updateMouse(time) {
    var idle = time - mouse.lastMove;
    if (idle > 1.2) {
      mouse.power *= 0.92;
    }
    mouse.active = idle < 1.6;
  }

  function updateHeat(dt, time) {
    for (var y = 0; y < ROWS; y++) {
      for (var x = 0; x < COLS; x++) {
        var pos = idx(x, y);
        var rowFrac = y / Math.max(1, ROWS - 1);
        var surface = surfaceHeight(x, time);
        var below = y < ROWS - 1 ? heat[idx(x, y + 1)] : heat[pos];
        var left = x > 0 ? heat[idx(x - 1, y)] : heat[pos];
        var right = x < COLS - 1 ? heat[idx(x + 1, y)] : heat[pos];
        var swirl = Math.sin(flow[pos] + time * 1.2 + y * 0.13) * 0.03;
        var mixed = heat[pos] * 0.58 + below * 0.28 + (left + right) * 0.07 + swirl;

        if (rowFrac >= surface) {
          var depth = rowFrac - surface;
          mixed += 0.12 + depth * 0.42;
          mixed -= Math.max(0, (surface - (rowFrac - 1 / ROWS)) * 11) * 0.025;
        } else {
          mixed *= 0.42;
        }

        if (mouse.active) {
          var mx = mouse.x * COLS;
          var my = mouse.y * ROWS;
          var dx = x - mx;
          var dy = y - my;
          var dist2 = dx * dx + dy * dy;
          var radius = 28 + mouse.power * 32;
          if (dist2 < radius) {
            var push = 1 - dist2 / radius;
            mixed += push * (0.14 + mouse.power * 0.35);
            if (dy > 0) {
              mixed += push * 0.05;
            }
          }
        }

        nextHeat[pos] = clamp(mixed - dt * 0.08, 0, 1.2);
      }
    }

    for (var i = blasts.length - 1; i >= 0; i--) {
      var blast = blasts[i];
      blast.life -= dt;
      var progress = 1 - blast.life / blast.maxLife;
      blast.radius = blast.maxRadius * progress;
      injectHeat(Math.round(blast.x), Math.round(blast.y), blast.heat * (1 - progress * 0.6), Math.max(2, Math.round(blast.radius)));
      if (blast.life <= 0) {
        blasts.splice(i, 1);
      }
    }

    var currentHeat = heat;
    heat = nextHeat;
    nextHeat = currentHeat;
  }

  function maybeSpawnSurfacePops(time) {
    var attempts = Math.max(1, Math.floor(COLS / 52));
    for (var i = 0; i < attempts; i++) {
      if (Math.random() > 0.045) continue;
      var x = Math.floor(Math.random() * COLS);
      var surface = Math.floor(surfaceHeight(x, time) * ROWS);
      var intensity = heat[idx(x, clamp(surface + 1, 0, ROWS - 1))];
      if (intensity > 0.36) {
        spawnPop(x, Math.max(0, surface - 1), intensity);
      }
    }
  }

  function updateSparkles(dt, time) {
    maybeSpawnSurfacePops(time);
    for (var i = sparkles.length - 1; i >= 0; i--) {
      var spark = sparkles[i];
      spark.life -= dt;
      spark.x += spark.vx * dt;
      spark.y += spark.vy * dt;
      spark.vy += dt * 7.2;
      spark.vx *= 0.97;
      if (spark.life <= 0 || spark.y < 0 || spark.x < 0 || spark.x >= COLS) {
        sparkles.splice(i, 1);
      }
    }
  }

  function clearBuffer() {
    for (var i = 0; i < buf.length; i++) {
      buf[i].ch = " ";
      buf[i].color = "transparent";
    }
  }

  function lavaColor(level, depth, isCrust, x, y, time) {
    var flicker = Math.sin(x * 0.81 + y * 0.37 + time * 3.4) * 0.08;
    var shimmer = Math.sin(x * 0.23 - y * 0.61 + time * 1.9) * 0.05;
    var veins = Math.sin(x * 0.14 + time * 0.9) * 0.12 + Math.cos(y * 0.48 - time * 1.2) * 0.08;
    var tone = level + flicker + shimmer;
    var depthFade = clamp(depth * 3.2, 0, 0.32);
    var lightTheme = isLightTheme();
    tone -= depthFade;

    if (isCrust) {
      if (lightTheme) {
        if (depth < 0.03) return tone > 0.34 ? "#b8653b" : "#8f4a2b";
        return tone > 0.24 ? "#7a3a25" : "#5a281b";
      }
      if (depth < 0.03) return tone > 0.34 ? "#7d3418" : "#5a2312";
      return tone > 0.24 ? "#4a1b10" : "#30110b";
    }

    if (lightTheme) {
      if (veins > 0.15 && tone > 0.45) {
        return tone > 0.82 ? "#f39b4a" : "#ea6532";
      }
      if (veins < -0.14 && tone > 0.32) {
        return tone > 0.7 ? "#e97b3b" : "#bf4727";
      }
      if (tone > 0.96) return "#f7c66a";
      if (tone > 0.84) return "#efab54";
      if (tone > 0.72) return "#ea8b3d";
      if (tone > 0.58) return "#df6a2d";
      if (tone > 0.46) return "#ca5327";
      if (tone > 0.34) return "#aa3e22";
      if (tone > 0.24) return "#873019";
      return "#662313";
    }

    if (veins > 0.15 && tone > 0.45) {
      return tone > 0.82 ? "#ffb36b" : "#ff6b2c";
    }
    if (veins < -0.14 && tone > 0.32) {
      return tone > 0.7 ? "#ff8a3d" : "#d43d1f";
    }
    if (tone > 0.96) return "#ffe08a";
    if (tone > 0.84) return "#ffbf5a";
    if (tone > 0.72) return "#ff9a3c";
    if (tone > 0.58) return "#ff7428";
    if (tone > 0.46) return "#ef5620";
    if (tone > 0.34) return "#cf3b1d";
    if (tone > 0.24) return "#a92b18";
    return "#741c12";
  }

  function lavaChar(level, isCrust, x, y, time) {
    var noise = Math.sin(x * 1.17 + time * 2.1) + Math.cos(y * 0.73 - time * 1.3);
    var veins = Math.sin(x * 0.31 + y * 0.22 + time * 1.7);
    var variant = clamp(level + noise * 0.06 + veins * 0.04, 0, 0.999);
    if (isCrust) {
      var crustIndex = Math.abs(Math.floor((x * 3 + y * 5 + time * 8) % LAVA_CRUST_CHARS.length));
      return LAVA_CRUST_CHARS[crustIndex];
    }
    if (veins > 0.55 && variant > 0.42) return "%";
    if (veins < -0.48 && variant > 0.3) return "x";
    var index = Math.floor(variant * (LAVA_CHARS.length - 1));
    return LAVA_CHARS[index];
  }

  function paintLava(time) {
    for (var x = 0; x < COLS; x++) {
      var surface = surfaceHeight(x, time);
      var surfaceRow = Math.floor(surface * ROWS);
      for (var y = Math.max(0, surfaceRow - 1); y < ROWS; y++) {
        var rowFrac = y / Math.max(1, ROWS - 1);
        if (rowFrac < surface) continue;
        var depth = rowFrac - surface;
        var pos = idx(x, y);
        var level = heat[pos];
        var crustChance = depth > 0.02 && level < 0.54;
        var crustNoise = Math.sin(x * 0.55 + y * 0.92 + time * 1.4);
        var isCrust = crustChance && crustNoise > -0.15;
        buf[pos].ch = lavaChar(level, isCrust, x, y, time);
        buf[pos].color = lavaColor(level, depth, isCrust, x, y, time);
      }

      var lipRow = clamp(surfaceRow - 1, 0, ROWS - 1);
      var lipPos = idx(x, lipRow);
      var lightTheme = isLightTheme();
      buf[lipPos].ch = depthGlowChar(x, time);
      buf[lipPos].color = lightTheme
        ? Math.sin(x * 0.33 + time * 3) > 0.25
          ? "#e89a49"
          : "#d96e34"
        : Math.sin(x * 0.33 + time * 3) > 0.25
          ? "#ffbf5a"
          : "#ff7a2a";
    }
  }

  function depthGlowChar(x, time) {
    var phase = Math.sin(x * 0.45 + time * 2.2);
    if (phase > 0.55) return "*";
    if (phase > 0.15) return "+";
    return ".";
  }

  function paintDrips() {
    for (var i = 0; i < drips.length; i++) {
      var drip = drips[i];
      var x = clamp(Math.round(drip.sourceX), 0, COLS - 1);
      var stemTop = clamp(drip.anchorY, 0, ROWS - 1);

      if (drip.state === "forming" || drip.state === "hanging") {
        var stemBottom = clamp(stemTop + drip.length, stemTop, ROWS - 1);
        var lightTheme = isLightTheme();
        for (var y = stemTop; y < stemBottom; y++) {
          var stemPos = idx(x, y);
          buf[stemPos].ch = DRIP_CHAR;
          buf[stemPos].color = lightTheme
            ? y === stemTop
              ? "#df7f3a"
              : "#c95a28"
            : y === stemTop
              ? "#ff9c3b"
              : "#ff7b26";
          if (drip.width > 1 && x + 1 < COLS) {
            buf[idx(x + 1, y)].ch = ":";
            buf[idx(x + 1, y)].color = lightTheme ? "#a33f22" : "#c7471c";
          }
        }

        var headY = clamp(stemBottom, 0, ROWS - 1);
        buf[idx(x, headY)].ch = drip.state === "forming" ? "." : drip.headChar;
        buf[idx(x, headY)].color = lightTheme
          ? drip.state === "forming"
            ? "#e8a257"
            : "#f1b56c"
          : drip.state === "forming"
            ? "#ffb85c"
            : "#ffd27a";
      }

      if (drip.state === "falling") {
        var tailTop = clamp(Math.round(drip.y) - 2, stemTop, ROWS - 1);
        var tailBottom = clamp(Math.round(drip.y), stemTop, ROWS - 1);
        var lightTheme = isLightTheme();
        for (var fy = tailTop; fy < tailBottom; fy++) {
          var tailPos = idx(x, fy);
          buf[tailPos].ch = fy === tailBottom - 1 ? ":" : "'";
          buf[tailPos].color = lightTheme
            ? fy === tailBottom - 1
              ? "#d97a39"
              : "#a33f22"
            : fy === tailBottom - 1
              ? "#ff9c3b"
              : "#c7471c";
        }
        buf[idx(x, tailBottom)].ch = drip.headChar;
        buf[idx(x, tailBottom)].color = lightTheme ? "#f1b56c" : "#ffd27a";
      }
    }
  }

  function paintSparkles() {
    for (var i = 0; i < sparkles.length; i++) {
      var spark = sparkles[i];
      var x = Math.round(spark.x);
      var y = Math.round(spark.y);
      if (x < 0 || x >= COLS || y < 0 || y >= ROWS) continue;
      var pos = idx(x, y);
      var glow = spark.life / spark.maxLife;
      buf[pos].ch = spark.char;
      buf[pos].color = glow > 0.65 ? "#fff2c9" : glow > 0.35 ? "#ffbe63" : "#ff7f32";
    }
  }

  function render(time) {
    clearBuffer();
    paintLava(time);
    paintDrips();
    paintSparkles();

    var out = "";
    for (var y = 0; y < ROWS; y++) {
      for (var x = 0; x < COLS; x++) {
        var cell = buf[idx(x, y)];
        if (cell.color === "transparent") {
          out += " ";
        } else {
          out += '<span style="color:' + cell.color + '">' + cell.ch + "</span>";
        }
      }
      if (y < ROWS - 1) out += "\n";
    }
    pre.innerHTML = out;
  }

  function frame(timestamp) {
    if (!lastTime) lastTime = timestamp;
    var elapsed = timestamp - lastTime;
    if (elapsed < FRAME_INTERVAL) {
      animFrame = requestAnimationFrame(frame);
      return;
    }

    var dt = Math.min(elapsed / 1000, 0.05);
    lastTime = timestamp;
    var time = timestamp / 1000;

    updateMouse(time);
    updateDrips(dt, time);
    updateHeat(dt, time);
    updateSparkles(dt, time);
    render(time);

    animFrame = requestAnimationFrame(frame);
  }

  function setMouse(clientX, clientY) {
    var rect = container.getBoundingClientRect();
    if (!rect.width || !rect.height) return;
    var nx = clamp((clientX - rect.left) / rect.width, 0, 1);
    var ny = clamp((clientY - rect.top) / rect.height, 0, 1);
    mouse.vx = nx - mouse.px;
    mouse.vy = ny - mouse.py;
    mouse.x = nx;
    mouse.y = ny;
    mouse.px = nx;
    mouse.py = ny;
    mouse.power = clamp(mouse.power + Math.abs(mouse.vx) * 8 + Math.abs(mouse.vy) * 6, 0, 1.4);
    mouse.lastMove = performance.now() / 1000;

    var mx = Math.round(nx * (COLS - 1));
    var my = Math.round(ny * (ROWS - 1));
    injectHeat(mx, my, 0.12 + mouse.power * 0.08, 4 + Math.round(mouse.power * 3));
    if (ny > BASE_LEVEL - 0.08 && Math.random() > 0.55) {
      spawnPop(mx, Math.max(0, my - 1), 0.3 + mouse.power * 0.4);
    }
  }

  function triggerExplosion(clientX, clientY) {
    var rect = container.getBoundingClientRect();
    if (!rect.width || !rect.height) return;

    var nx = clamp((clientX - rect.left) / rect.width, 0, 1);
    var ny = clamp((clientY - rect.top) / rect.height, 0, 1);
    var cx = Math.round(nx * (COLS - 1));
    var cy = Math.round(ny * (ROWS - 1));
    var power = 0.8 + mouse.power * 0.5;

    mouse.x = nx;
    mouse.y = ny;
    mouse.px = nx;
    mouse.py = ny;
    mouse.active = true;
    mouse.power = clamp(mouse.power + 0.35, 0, 1.4);
    mouse.lastMove = performance.now() / 1000;

    injectHeat(cx, cy, 0.45 + power * 0.18, 5 + Math.round(power * 4));
    spawnBlast(cx, cy, power);
  }

  var observer = new IntersectionObserver(
    function (entries) {
      if (entries[0].isIntersecting) {
        lastTime = 0;
        animFrame = requestAnimationFrame(frame);
      } else {
        cancelAnimationFrame(animFrame);
      }
    },
    { threshold: 0.1 },
  );

  container.addEventListener("pointermove", function (event) {
    setMouse(event.clientX, event.clientY);
  });

  container.addEventListener("click", function (event) {
    triggerExplosion(event.clientX, event.clientY);
  });

  container.addEventListener("pointerleave", function () {
    mouse.active = false;
    mouse.power *= 0.6;
  });

  window.addEventListener("resize", function () {
    resize();
  });

  resize();
  observer.observe(container);
})();
