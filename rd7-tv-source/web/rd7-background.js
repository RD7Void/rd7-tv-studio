/* JavaScript / rd7-background.js / ES5 LG webOS / RD7 TV Studio backgrounds */
(function () {
  'use strict';
  var KEY = 'rd7.background.v1', config = null, index = 0, timer = 0, generation = 0;
  var host = document.createElement('div');
  host.id = 'rd7-custom-background';
  var a = document.createElement('div'), b = document.createElement('div'), shade = document.createElement('div');
  a.className = b.className = 'rd7-wallpaper-image';
  shade.className = 'rd7-wallpaper-shade';
  host.appendChild(a); host.appendChild(b); host.appendChild(shade);
  document.getElementById('sky').appendChild(host);
  var front = a;
  function schedule() {
    clearTimeout(timer);
    if (!config || config.mode !== 'slideshow' || config.images.length < 2 || document.hidden) return;
    timer = setTimeout(function () { show((index + 1) % config.images.length); }, config.interval * 1000);
  }
  function show(next) {
    var token = generation, image = new Image();
    image.onload = function () {
      if (token !== generation) return;
      var back = front === a ? b : a;
      back.style.backgroundImage = 'url("' + config.images[next].data + '")';
      back.style.opacity = '1'; front.style.opacity = '0'; front = back; index = next; schedule();
    };
    image.onerror = schedule;
    image.src = config.images[next].data;
  }
  function apply(value) {
    clearTimeout(timer); generation++;
    config = value;
    var enabled = config && config.mode !== 'solid' && config.images.length;
    host.style.display = enabled ? 'block' : 'none';
    if (!enabled) { a.style.backgroundImage = b.style.backgroundImage = ''; return; }
    shade.style.background = 'rgba(0,0,0,' + config.dim / 100 + ')';
    var blur = config.blur ? 'blur(' + config.blur + 'px)' : 'none';
    a.style.webkitFilter = a.style.filter = b.style.webkitFilter = b.style.filter = blur;
    a.style.opacity = b.style.opacity = '0'; front = b; index = 0; show(0);
  }
  window.RD7Background = {
    set: function (value) {
      if (!value || !value.images || value.images.length > 8 || ['solid', 'image', 'slideshow'].indexOf(value.mode) < 0) return {error: 'Configuração de fundo inválida'};
      if (value.mode !== 'solid' && !value.images.length) return {error: 'Selecione uma imagem'};
      var safe = {mode: value.mode, images: [], dim: Math.max(0, Math.min(85, Number(value.dim) || 0)), blur: Math.max(0, Math.min(12, Number(value.blur) || 0)), interval: Math.max(5, Math.min(120, Number(value.interval) || 20))};
      for (var i = 0; i < value.images.length; i++) {
        if (!/^data:image\/jpeg;base64,[A-Za-z0-9+/=]+$/.test(value.images[i].data || '')) return {error: 'Imagem inválida'};
        safe.images.push({name: String(value.images[i].name || ''), data: value.images[i].data});
      }
      try { localStorage.setItem(KEY, JSON.stringify(safe)); } catch (e) { return {error: 'Armazenamento da TV cheio. Use menos imagens.'}; }
      apply(safe); return {saved: true, images: safe.images.length, mode: safe.mode};
    },
    get: function () { return config; }
  };
  document.addEventListener('visibilitychange', function () { if (document.hidden) clearTimeout(timer); else schedule(); });
  try { var saved = JSON.parse(localStorage.getItem(KEY)); if (saved) apply(saved); } catch (e) {}
})();
