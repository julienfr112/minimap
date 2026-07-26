// minimap viewer -- no dependencies.
//
// Three parts, in order:
//   1. a minimal protobuf reader          (~50 lines)
//   2. a Mapbox Vector Tile decoder       (~80 lines)
//   3. a canvas renderer + pan/zoom       (the rest)
//
// The backend hands us tiles whose coordinates are already tile-local integers
// on a 0..EXTENT grid, so drawing is just an affine map to screen pixels.

'use strict';

// ---------------------------------------------------------------- protobuf

class Reader {
  constructor(bytes) { this.b = bytes; this.p = 0; }
  get done() { return this.p >= this.b.length; }

  varint() {
    let v = 0, shift = 0, byte;
    do {
      byte = this.b[this.p++];
      // 2**shift rather than << : shifts past bit 31 would wrap.
      v += (byte & 0x7f) * 2 ** shift;
      shift += 7;
    } while (byte & 0x80);
    return v;
  }

  // zigzag: even -> v/2, odd -> -(v+1)/2
  svarint() { const v = this.varint(); return v % 2 ? -(v + 1) / 2 : v / 2; }

  bytes() {
    const n = this.varint(), start = this.p;
    this.p += n;
    return this.b.subarray(start, start + n);
  }

  string() { return new TextDecoder().decode(this.bytes()); }

  field() { const key = this.varint(); return [key >>> 3, key & 7]; }

  skip(wire) {
    if (wire === 0) this.varint();
    else if (wire === 2) this.p += this.varint();
    else if (wire === 5) this.p += 4;
    else if (wire === 1) this.p += 8;
    else throw new Error('unknown wire type ' + wire);
  }
}

// ------------------------------------------------------------- MVT decode

// Tile { repeated Layer layers = 3 }
function decodeTile(buffer) {
  const r = new Reader(new Uint8Array(buffer)), layers = [];
  while (!r.done) {
    const [f, w] = r.field();
    if (f === 3 && w === 2) layers.push(decodeLayer(new Reader(r.bytes())));
    else r.skip(w);
  }
  return layers;
}

// Layer { name=1, features=2, keys=3, values=4, extent=5, version=15 }
function decodeLayer(r) {
  const layer = { name: '', extent: 4096, keys: [], values: [], features: [] };
  const raw = [];
  while (!r.done) {
    const [f, w] = r.field();
    if (f === 1 && w === 2) layer.name = r.string();
    // Features are buffered: keys/values may appear after them in the stream.
    else if (f === 2 && w === 2) raw.push(r.bytes());
    else if (f === 3 && w === 2) layer.keys.push(r.string());
    else if (f === 4 && w === 2) layer.values.push(decodeValue(new Reader(r.bytes())));
    else if (f === 5 && w === 0) layer.extent = r.varint();
    else r.skip(w);
  }
  for (const b of raw) layer.features.push(decodeFeature(new Reader(b), layer));
  return layer;
}

// Value { string=1, float=2, double=3, int64=4, uint64=5, sint64=6, bool=7 }
function decodeValue(r) {
  let out = null;
  while (!r.done) {
    const [f, w] = r.field();
    if (f === 1 && w === 2) out = r.string();
    else if (f === 4 && w === 0) out = r.varint();
    else if (f === 5 && w === 0) out = r.varint();
    else if (f === 6 && w === 0) out = r.svarint();
    else if (f === 7 && w === 0) out = !!r.varint();
    else r.skip(w);
  }
  return out;
}

// Feature { id=1, tags=2 (packed), type=3, geometry=4 (packed) }
function decodeFeature(r, layer) {
  let type = 0, geometry = null;
  const tags = [];
  while (!r.done) {
    const [f, w] = r.field();
    if (f === 2 && w === 2) {
      const sub = new Reader(r.bytes());
      while (!sub.done) tags.push(sub.varint());
    } else if (f === 3 && w === 0) type = r.varint();
    else if (f === 4 && w === 2) geometry = r.bytes();
    else r.skip(w);
  }
  const props = {};
  for (let i = 0; i + 1 < tags.length; i += 2) props[layer.keys[tags[i]]] = layer.values[tags[i + 1]];
  return { type, props, rings: decodeGeometry(geometry) };
}

// Geometry is a command stream: (id | count<<3), then count zigzag delta pairs.
// MoveTo=1 starts a ring, LineTo=2 extends it, ClosePath=7 needs no parameters.
// Each ring is a flat [x0,y0,x1,y1,...] array for speed.
function decodeGeometry(bytes) {
  if (!bytes) return [];
  const r = new Reader(bytes), rings = [];
  let ring = null, x = 0, y = 0;
  while (!r.done) {
    const cmd = r.varint(), id = cmd & 7, count = cmd >> 3;
    if (id === 1) {
      for (let i = 0; i < count; i++) {
        x += r.svarint(); y += r.svarint();
        ring = [x, y];
        rings.push(ring);
      }
    } else if (id === 2) {
      for (let i = 0; i < count; i++) {
        x += r.svarint(); y += r.svarint();
        ring.push(x, y);
      }
    } // ClosePath: canvas closes the path for us
  }
  return rings;
}

// ------------------------------------------------------------------ style

const BACKGROUND = '#f6f4ef';

// Ordered, not keyed: MVT does not define feature order within a layer, so
// drawing in arrival order makes overlapping polygons paint nondeterministically
// (a park inside an urban area would flip colour depending on how the tile
// happened to be encoded). Painting class by class in this order is stable, and
// puts the smaller, more specific areas on top.
const FILLS = {
  landuse: [
    ['urban', '#eae7e1'],
    ['farmland', '#f1efe2'],
    ['wood', '#d7e4c6'],
    ['park', '#d5e8ba'],
  ],
  water: [['water', '#a8cee2']],
  buildings: [['building', '#e2ddd4']],
};

// Drawn bottom-to-top, so motorways end up above residential streets.
const ROADS = [
  ['path',        { color: '#cdc2b2', casing: null,      width: 0.4 }],
  ['service',     { color: '#fcfbf8', casing: '#ded9d1', width: 0.7 }],
  ['other',       { color: '#f3f0ea', casing: '#ded9d1', width: 0.7 }],
  ['residential', { color: '#ffffff', casing: '#d7d2c8', width: 1.0 }],
  ['tertiary',    { color: '#ffffff', casing: '#cdc8bd', width: 1.3 }],
  ['secondary',   { color: '#fdf6b2', casing: '#dcd486', width: 1.5 }],
  ['primary',     { color: '#fbd86f', casing: '#d9b63a', width: 1.8 }],
  ['trunk',       { color: '#f7c260', casing: '#d9a02c', width: 2.0 }],
  ['motorway',    { color: '#f3a259', casing: '#d8842c', width: 2.4 }],
];
const ROAD_STYLE = new Map(ROADS);

const WATER_LINES = { river: { color: '#a8cee2', width: 1.4 }, stream: { color: '#bcd9e6', width: 0.7 } };

// Roads get thicker as you zoom in, but sub-linearly.
function widthScale(zoom) { return 1.6 * 2 ** ((zoom - 10) / 2.4); }

// ---------------------------------------------------------------- mercator

// CSS pixels per tile at integer zoom. The pipeline derives its size thresholds and
// simplification tolerances from MPP0 = 2 * WORLD / 512, so changing this
// without changing that there would make the pipeline drop detail the renderer
// is big enough to show (or keep detail it cannot).
const TILE = 512;

function project(lon, lat) {
  const s = Math.sin(lat * Math.PI / 180);
  return [(lon + 180) / 360, 0.5 - Math.log((1 + s) / (1 - s)) / (4 * Math.PI)];
}

function unproject(x, y) {
  const n = Math.PI - 2 * Math.PI * y;
  return [x * 360 - 180, 180 / Math.PI * Math.atan(0.5 * (Math.exp(n) - Math.exp(-n)))];
}

// -------------------------------------------------------------------- map

class Minimap {
  constructor(canvas, meta) {
    this.canvas = canvas;
    this.ctx = canvas.getContext('2d');
    this.meta = meta;
    this.minzoom = meta.minzoom;
    this.maxzoom = meta.maxzoom;
    this.zoom = meta.center[2];
    this.center = { lon: meta.center[0], lat: meta.center[1] };

    // #zoom/lat/lon in the URL wins, so a view can be linked to directly.
    const hash = location.hash.match(/^#(\d+(?:\.\d+)?)\/(-?\d+\.?\d*)\/(-?\d+\.?\d*)/);
    if (hash) {
      this.zoom = Math.max(this.minzoom, Math.min(this.maxzoom + 3, +hash[1]));
      this.center = { lon: +hash[3], lat: +hash[2] };
    }
    this.tiles = new Map(); // "z/x/y" -> layers | 'loading' | 'empty'
    this.pending = 0;
    this.dirty = true;

    this.#bindEvents();
    this.resize();
    const frame = () => { if (this.dirty) { this.dirty = false; this.render(); } requestAnimationFrame(frame); };
    requestAnimationFrame(frame);
  }

  resize() {
    const dpr = window.devicePixelRatio || 1;
    const { clientWidth: w, clientHeight: h } = this.canvas;
    this.canvas.width = w * dpr;
    this.canvas.height = h * dpr;
    this.w = w;
    this.h = h;
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    this.dirty = true;
  }

  #bindEvents() {
    window.addEventListener('resize', () => this.resize());

    let dragging = false, lastX = 0, lastY = 0;
    this.canvas.addEventListener('pointerdown', (e) => {
      dragging = true; lastX = e.clientX; lastY = e.clientY;
      this.canvas.setPointerCapture(e.pointerId);
      this.canvas.style.cursor = 'grabbing';
    });
    this.canvas.addEventListener('pointermove', (e) => {
      if (!dragging) return;
      this.#panPixels(lastX - e.clientX, lastY - e.clientY);
      lastX = e.clientX; lastY = e.clientY;
    });
    const stop = () => { dragging = false; this.canvas.style.cursor = 'grab'; };
    this.canvas.addEventListener('pointerup', stop);
    this.canvas.addEventListener('pointercancel', stop);

    this.canvas.addEventListener('wheel', (e) => {
      e.preventDefault();
      // Zoom about the cursor: keep the geographic point under it fixed.
      const rect = this.canvas.getBoundingClientRect();
      const px = e.clientX - rect.left - this.w / 2;
      const py = e.clientY - rect.top - this.h / 2;
      const before = this.#screenToWorld(px, py);
      this.zoom = Math.max(this.minzoom, Math.min(this.maxzoom + 3, this.zoom - e.deltaY * 0.002));
      const after = this.#screenToWorld(px, py);
      const c = project(this.center.lon, this.center.lat);
      const [lon, lat] = unproject(c[0] + before[0] - after[0], c[1] + before[1] - after[1]);
      this.center = { lon, lat };
      this.dirty = true;
    }, { passive: false });
  }

  // Offset in world (normalized 0..1) units for a pixel offset from centre.
  #screenToWorld(px, py) {
    const world = TILE * 2 ** this.zoom;
    return [px / world, py / world];
  }

  #panPixels(dx, dy) {
    const world = TILE * 2 ** this.zoom;
    const c = project(this.center.lon, this.center.lat);
    const [lon, lat] = unproject(c[0] + dx / world, c[1] + dy / world);
    this.center = { lon, lat: Math.max(-85, Math.min(85, lat)) };
    this.dirty = true;
  }

  #tileKey(z, x, y) { return `${z}/${x}/${y}`; }

  #want(z, x, y) {
    const key = this.#tileKey(z, x, y);
    const hit = this.tiles.get(key);
    if (hit !== undefined) return hit === 'loading' || hit === 'empty' ? null : hit;

    this.tiles.set(key, 'loading');
    this.pending++;
    fetch(`/tiles/${z}/${x}/${y}`)
      .then((r) => {
        if (r.status === 204) return null;       // tile legitimately empty
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.arrayBuffer();
      })
      .then((buf) => {
        this.tiles.set(key, buf ? decodeTile(buf) : 'empty');
      })
      .catch((err) => {
        console.warn('tile', key, err);
        this.tiles.set(key, 'empty');
      })
      .finally(() => { this.pending--; this.dirty = true; onStatus(this); });
    return null;
  }

  render() {
    const ctx = this.ctx;
    ctx.fillStyle = BACKGROUND;
    ctx.fillRect(0, 0, this.w, this.h);

    const world = TILE * 2 ** this.zoom;
    const c = project(this.center.lon, this.center.lat);
    // Viewport top-left in world pixels.
    const originX = c[0] * world - this.w / 2;
    const originY = c[1] * world - this.h / 2;

    // Beyond maxzoom we overzoom: reuse the deepest tiles, drawn larger.
    const z = Math.max(this.minzoom, Math.min(this.maxzoom, Math.floor(this.zoom)));
    const n = 2 ** z;
    const span = world / n; // one tile's size on screen

    const x0 = Math.max(0, Math.floor(originX / span));
    const x1 = Math.min(n - 1, Math.floor((originX + this.w) / span));
    const y0 = Math.max(0, Math.floor(originY / span));
    const y1 = Math.min(n - 1, Math.floor((originY + this.h) / span));

    // Collect visible tiles once; the road passes below iterate them repeatedly.
    const visible = [];
    for (let x = x0; x <= x1; x++) {
      for (let y = y0; y <= y1; y++) {
        const layers = this.#want(z, x, y);
        if (layers) visible.push({ layers, ox: x * span - originX, oy: y * span - originY, span });
      }
    }

    for (const t of visible) this.#drawFills(t, 'landuse');
    for (const t of visible) this.#drawFills(t, 'water');
    for (const t of visible) this.#drawWaterLines(t);

    // Casings for every tile first, then bodies, so a road crossing a tile
    // boundary does not get another road's casing painted over it.
    const k = widthScale(this.zoom);
    for (const [cls, style] of ROADS) {
      if (style.casing) {
        for (const t of visible) this.#drawRoads(t, cls, style.casing, (style.width + 0.6) * k);
      }
      for (const t of visible) this.#drawRoads(t, cls, style.color, style.width * k);
    }

    for (const t of visible) this.#drawFills(t, 'buildings');

    onStatus(this);
  }

  #layer(tile, name) { return tile.layers.find((l) => l.name === name); }

  #path(ctx, tile, feature, extent) {
    const s = tile.span / extent;
    ctx.beginPath();
    for (const ring of feature.rings) {
      ctx.moveTo(tile.ox + ring[0] * s, tile.oy + ring[1] * s);
      for (let i = 2; i < ring.length; i += 2) {
        ctx.lineTo(tile.ox + ring[i] * s, tile.oy + ring[i + 1] * s);
      }
    }
  }

  #drawFills(tile, name) {
    const layer = this.#layer(tile, name);
    if (!layer) return;
    const ctx = this.ctx;
    for (const [cls, color] of FILLS[name] || []) {
      ctx.fillStyle = color;
      for (const f of layer.features) {
        if (f.type !== 3 || f.props.cls !== cls) continue; // polygons of this class
        this.#path(ctx, tile, f, layer.extent);
        ctx.closePath();
        // nonzero matches MVT winding: exteriors and holes wind oppositely.
        ctx.fill();
      }
    }
  }

  #drawWaterLines(tile) {
    const layer = this.#layer(tile, 'water');
    if (!layer) return;
    const ctx = this.ctx;
    const k = widthScale(this.zoom);
    for (const f of layer.features) {
      if (f.type !== 2) continue;
      const style = WATER_LINES[f.props.cls];
      if (!style) continue;
      this.#path(ctx, tile, f, layer.extent);
      ctx.strokeStyle = style.color;
      ctx.lineWidth = Math.max(0.5, style.width * k);
      ctx.lineCap = 'round';
      ctx.stroke();
    }
  }

  #drawRoads(tile, cls, color, width) {
    const layer = this.#layer(tile, 'roads');
    if (!layer) return;
    const ctx = this.ctx;
    ctx.strokeStyle = color;
    ctx.lineWidth = Math.max(0.4, width);
    ctx.lineCap = 'round';
    ctx.lineJoin = 'round';
    for (const f of layer.features) {
      if (f.type !== 2 || f.props.cls !== cls) continue;
      this.#path(ctx, tile, f, layer.extent);
      ctx.stroke();
    }
  }
}

// ------------------------------------------------------------------- boot

let onStatus = () => {};

async function main() {
  const meta = await (await fetch('/meta.json')).json();
  const canvas = document.getElementById('map');
  const map = new Minimap(canvas, meta);

  const hud = document.getElementById('hud');
  let hashTimer = null;
  onStatus = (m) => {
    hud.textContent =
      `z${m.zoom.toFixed(1)}  ${m.center.lat.toFixed(4)}, ${m.center.lon.toFixed(4)}` +
      (m.pending ? `  loading ${m.pending}` : '');
    // Keep the URL shareable, but do not spam history while dragging.
    clearTimeout(hashTimer);
    hashTimer = setTimeout(() => {
      const h = `#${m.zoom.toFixed(1)}/${m.center.lat.toFixed(5)}/${m.center.lon.toFixed(5)}`;
      if (location.hash !== h) history.replaceState(null, '', h);
    }, 300);
  };
  onStatus(map);
  document.getElementById('attribution').innerHTML = meta.attribution;
  window.map = map; // handy in the console
}

main().catch((e) => {
  document.getElementById('hud').textContent = 'error: ' + e.message;
  console.error(e);
});
