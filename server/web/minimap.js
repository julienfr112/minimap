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

// Which background is correct depends on what the archive contains, so both
// are named here. The sea has to be the background rather than a drawn layer:
// the archive carries polygons for land only, so everywhere no tile covers is
// water by construction, and that is what keeps the Atlantic free (see
// config::LAND_URL). The land layer then paints paper back over the continent.
//
// Against an archive with no `land` layer, SEA would instead paint the whole of
// Europe blue and leave the roads floating on it, so this follows the archive.
const PAPER = '#f6f4ef';
const SEA = '#c3d9e8';
const BACKGROUND = SEA;

// Which levels exist is an archive fact, not a viewer preference, so it arrives
// per layer in /meta.json. Guessing means requesting tiles that were never
// baked. This is only the fallback for a server that says nothing.
const FALLBACK_LEVELS = [10, 12, 14];

// Draw order. The server sends its own, but this is what the styling tables
// below are keyed by, and a layer the server offers that is not in here has no
// style and is skipped.
const LAYER_ORDER = ['land', 'landuse', 'water', 'roads', 'buildings', 'places'];

// How far past the deepest rung the viewer will go. One doubling: the geometry
// there was simplified to one pixel at the rung's own zoom, so a 2x stretch
// costs about two pixels of error and is what turns a z17 rung into the 192 m
// view. Two doublings would be 4x, which is the point where it reads as blurry.
const OVERZOOM = 1;

// One mouse-wheel click, in the units `deltaY` reports. A trackpad sends a
// stream of small deltas instead, so they are accumulated and spent a stop at a
// time -- otherwise a single flick crosses the whole range.
const WHEEL_STEP = 100;

// How many decoded tiles to keep. Unbounded, this grows for as long as someone
// keeps panning -- and each entry now also holds the Path2D objects built from
// it, which is the point of keeping them but also most of their weight. A few
// hundred is many screenfuls, so nothing visible is ever evicted; insertion
// order is a good enough proxy for least-recently-used when the cap is that far
// above the working set.
const TILE_CACHE = 400;

// Panning at a fixed zoom redraws identical pixels every frame, so each tile is
// rasterised once at exactly the size it will be shown and then blitted. Keyed
// by display zoom, so the bitmap is always 1:1 with the screen and costs no
// sharpness -- rasterising at the tile's native size and letting drawImage
// stretch it would add raster blur on top of geometry that is already one rung
// coarse.
//
// The price is memory, so two limits: no single bitmap above RASTER_MAX pixels
// on a side (a 4x-stretched tile at dpr 2 would be 4096, or 64 MB alone -- those
// fall back to drawing vectors), and a byte budget across all of them.
// 1024 admits a native-size tile at any device pixel ratio, and a 2x-stretched
// one at dpr 1. It deliberately excludes a 2x tile at dpr 2 (2048 px, 16 MB
// each) and a 4x tile at any ratio: those are the stretched stops, where a tile
// covers four or sixteen times the screen so there are correspondingly few of
// them, and the cached Path2D geometry already draws them cheaply.
const RASTER_MAX = 1024;
const RASTER_BUDGET = 64 << 20;

// Ordered, not keyed: MVT does not define feature order within a layer, so
// drawing in arrival order makes overlapping polygons paint nondeterministically
// (a park inside an urban area would flip colour depending on how the tile
// happened to be encoded). Painting class by class in this order is stable, and
// puts the smaller, more specific areas on top.
const FILLS = {
  land: [['land', PAPER]],
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

// Labels. `places` is the one layer whose features carry a name, and the only
// one drawn as text rather than geometry. Cities are set larger and heavier so
// that when two labels collide the more important one is the one that survives.
const LABELS = {
  city: { size: 13, weight: 600 },
  town: { size: 11, weight: 400 },
};
const LABEL_COLOR = '#3a3733';
const LABEL_HALO = 'rgba(255, 255, 255, 0.9)';

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
    // `?maxzoom=N` pretends the archive stops at N, overzooming from there.
    // Worth having because the deepest zoom is most of the archive -- z15 is
    // 44% of France -- so this answers "what would I lose by not baking it"
    // without rebuilding anything.
    const cap = +new URLSearchParams(location.search).get('maxzoom');
    this.maxzoom = cap ? Math.min(meta.maxzoom, cap) : meta.maxzoom;
    // Each layer is its own archive with its own zoom rungs, and the server says
    // which. `land` stops where the background cap put it; `buildings` start
    // where they first become worth drawing. Nothing here needs to know why --
    // it just picks, per layer, the deepest rung at or below the zoom shown.
    //
    // Spacing rungs two apart bounds overzoom at 4x: a rung stretched further
    // reads as blurry, because its geometry was simplified to one pixel at *its*
    // zoom, not at the one being displayed.
    this.layerRungs = new Map();
    for (const l of meta.layers ?? []) {
      // `?maxzoom=N` applies here, per layer: pretending the build stopped at N
      // has to mean no layer offers a rung past it.
      const rungs = (l.rungs ?? [])
        .filter((z) => z <= this.maxzoom)
        .sort((a, b) => a - b);
      if (rungs.length) this.layerRungs.set(l.name, rungs);
    }
    if (!this.layerRungs.size) {
      // A server that said nothing useful, or a cap below every rung there is.
      for (const name of LAYER_ORDER) {
        this.layerRungs.set(name, FALLBACK_LEVELS.filter((z) => z <= this.maxzoom));
      }
    }
    // Every rung any layer has, which is what the zoom stops span.
    this.levels = [...new Set([...this.layerRungs.values()].flat())].sort((a, b) => a - b);
    // Every zoom the viewer will settle on, shallowest first. Zoom is discrete
    // because there is nothing in between: the archive holds a handful of
    // rungs, and a fractional zoom only stretches one of them by a non-integer
    // factor, which costs sharpness and buys nothing but a smoother-feeling
    // gesture. The bounds are the archive's own -- below the shallowest rung
    // there are no tiles to widen out to, and above the deepest plus OVERZOOM
    // the stretch stops being honest.
    //
    // Integers give a stop per zoom level; `this.stops = [...this.levels]`
    // instead would give one stop per rung, every one pixel-native and never
    // stretched, at the price of jumping 4x at a time.
    this.stops = [];
    for (let z = this.levels[0]; z <= this.maxzoom + OVERZOOM; z++) this.stops.push(z);

    this.zoom = this.#snap(meta.center[2]);
    this.center = { lon: meta.center[0], lat: meta.center[1] };

    // #zoom/lat/lon in the URL wins, so a view can be linked to directly.
    const hash = location.hash.match(/^#(\d+(?:\.\d+)?)\/(-?\d+\.?\d*)\/(-?\d+\.?\d*)/);
    if (hash) {
      this.zoom = this.#snap(+hash[1]);
      this.center = { lon: +hash[3], lat: +hash[2] };
    }
    this.tiles = new Map(); // "z/x/y" -> layers | 'loading' | 'empty'  (see #keep)
    this.rasters = new Map(); // "z/x/y|layers@zoom" -> canvas  (see #paint)
    this.rasterBytes = 0;
    this.dragging = false;
    this.pending = 0;
    this.dirty = true;

    this.#bindEvents();
    this.resize();
    const frame = () => { if (this.dirty) { this.dirty = false; this.render(); } requestAnimationFrame(frame); };
    requestAnimationFrame(frame);
  }

  // The nearest allowed stop to `z`. Used for whatever arrives from outside --
  // the archive's centre zoom, a hand-written URL -- so nothing can put the
  // viewer at a zoom it has no rung for.
  #snap(z) {
    return this.stops.reduce((a, b) => (Math.abs(b - z) < Math.abs(a - z) ? b : a));
  }

  // Move `steps` stops, keeping the point under (px, py) fixed. Screen
  // coordinates are relative to the canvas centre.
  #zoomBy(steps, px = 0, py = 0) {
    const i = this.stops.indexOf(this.zoom);
    const j = Math.max(0, Math.min(this.stops.length - 1, i + steps));
    if (j === i) return;
    const before = this.#screenToWorld(px, py);
    this.zoom = this.stops[j];
    const after = this.#screenToWorld(px, py);
    const c = project(this.center.lon, this.center.lat);
    const [lon, lat] = unproject(c[0] + before[0] - after[0], c[1] + before[1] - after[1]);
    this.center = { lon, lat };
    this.dirty = true;
  }

  resize() {
    const dpr = window.devicePixelRatio || 1;
    const { clientWidth: w, clientHeight: h } = this.canvas;
    this.canvas.width = w * dpr;
    this.canvas.height = h * dpr;
    this.w = w;
    this.h = h;
    this.dpr = dpr;
    this.#identity();
    this.dirty = true;
  }

  // CSS pixels, no tile placement. Everything that measures in screen terms --
  // the background, the labels -- has to be drawn under this.
  #identity() {
    this.ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
  }

  #bindEvents() {
    window.addEventListener('resize', () => this.resize());

    let lastX = 0, lastY = 0;
    this.canvas.addEventListener('pointerdown', (e) => {
      this.dragging = true; lastX = e.clientX; lastY = e.clientY;
      this.canvas.setPointerCapture(e.pointerId);
      this.canvas.style.cursor = 'grabbing';
    });
    this.canvas.addEventListener('pointermove', (e) => {
      if (!this.dragging) return;
      this.#panPixels(lastX - e.clientX, lastY - e.clientY);
      lastX = e.clientX; lastY = e.clientY;
    });
    // Coming up ends cheap mode and asks for one more frame, which is the one
    // that rasterises whatever the drag exposed and puts the casings back.
    const stop = () => {
      this.dragging = false;
      this.canvas.style.cursor = 'grab';
      this.dirty = true;
    };
    this.canvas.addEventListener('pointerup', stop);
    this.canvas.addEventListener('pointercancel', stop);

    // Wheel deltas are reported in three different units depending on the
    // browser and the device; normalise to pixels before counting them.
    let wheelAcc = 0;
    this.canvas.addEventListener('wheel', (e) => {
      e.preventDefault();
      const dy = e.deltaMode === 1 ? e.deltaY * 33 : e.deltaMode === 2 ? e.deltaY * 400 : e.deltaY;
      wheelAcc += dy;
      const steps = Math.trunc(wheelAcc / WHEEL_STEP);
      if (!steps) return;
      wheelAcc -= steps * WHEEL_STEP;
      // Zoom about the cursor: keep the geographic point under it fixed.
      const rect = this.canvas.getBoundingClientRect();
      this.#zoomBy(-steps, e.clientX - rect.left - this.w / 2, e.clientY - rect.top - this.h / 2);
    }, { passive: false });

    // With only a handful of stops, clicking through them is as natural as
    // scrolling. Shift reverses, which is the convention every map uses.
    this.canvas.addEventListener('dblclick', (e) => {
      e.preventDefault();
      const rect = this.canvas.getBoundingClientRect();
      this.#zoomBy(e.shiftKey ? -1 : 1,
        e.clientX - rect.left - this.w / 2, e.clientY - rect.top - this.h / 2);
    });

    window.addEventListener('keydown', (e) => {
      if (e.key === '+' || e.key === '=') this.#zoomBy(1);
      else if (e.key === '-' || e.key === '_') this.#zoomBy(-1);
      else return;
      e.preventDefault();
    });
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

  // One layer's tile, fetched if this is the first time it has been asked for.
  //
  // Returns the decoded layer, or null while it is in flight and for good once
  // it comes back empty. A 204 means the tile is legitimately absent -- a layer
  // does not fill its bounding box -- and a 404 means the server has no such
  // layer at all, which is equally final and equally not an error.
  #want(name, z, x, y) {
    const key = `${name}/${z}/${x}/${y}`;
    const hit = this.tiles.get(key);
    if (hit !== undefined) return hit === 'loading' || hit === 'empty' ? null : hit;
    this.tiles.set(key, 'loading');
    this.pending++;
    fetch(`/tiles/${name}/${z}/${x}/${y}`)
      .then((r) => {
        if (r.status === 204 || r.status === 404) return null;
        if (!r.ok) throw new Error(`${r.status}`);
        return r.arrayBuffer();
      })
      .then((buf) => {
        // Each archive's tile holds exactly the one layer it is an archive of.
        const layers = buf ? decodeTile(buf) : [];
        this.#keep(key, layers.length ? layers[0] : 'empty');
      })
      .catch((err) => {
        console.warn('tile', key, err);
        this.#keep(key, 'empty');
      })
      .finally(() => { this.pending--; this.dirty = true; onStatus(this); });
    return null;
  }

  // The deepest rung of `layer` at or below the zoom being displayed, or null
  // if the layer has nothing that shallow -- which is how a layer that only
  // exists deep (buildings) stays absent from a wide view without anything
  // having to special-case it.
  #rungFor(layer) {
    const rungs = this.layerRungs.get(layer);
    if (!rungs) return null;
    const want = Math.floor(this.zoom);
    let best = null;
    for (const z of rungs) if (z <= want) best = z;
    return best;
  }

  // Store a decoded tile, dropping the oldest once the cache is over its cap.
  #keep(key, value) {
    this.tiles.set(key, value);
    while (this.tiles.size > TILE_CACHE) {
      const oldest = this.tiles.keys().next().value;
      if (oldest === key) break;
      this.tiles.delete(oldest);
    }
  }

  // Every position on rung `z` that the viewport touches, with the requested
  // layers' data for it and where to draw it.
  //
  // A position may have some layers and not others -- each is a separate
  // archive and a separate request -- so `layers` is a Map of what actually
  // arrived. A position with nothing at all is skipped.
  #collect(z, names, world, originX, originY) {
    const n = 2 ** z;
    const span = world / n;
    const x0 = Math.max(0, Math.floor(originX / span));
    const x1 = Math.min(n - 1, Math.floor((originX + this.w) / span));
    const y0 = Math.max(0, Math.floor(originY / span));
    const y1 = Math.min(n - 1, Math.floor((originY + this.h) / span));
    const out = [];
    for (let x = x0; x <= x1; x++) {
      for (let y = y0; y <= y1; y++) {
        const layers = new Map();
        for (const name of names) {
          const layer = this.#want(name, z, x, y);
          if (layer) layers.set(name, layer);
        }
        if (!layers.size) continue;
        out.push({
          key: `${z}/${x}/${y}`,
          layers,
          ox: x * span - originX,
          oy: y * span - originY,
          span,
        });
      }
    }
    return out;
  }

  render() {
    const ctx = this.ctx;
    // The passes below leave the canvas in some tile's coordinate space, so the
    // background and the labels each reset it first.
    this.#identity();
    ctx.fillStyle = BACKGROUND;
    ctx.fillRect(0, 0, this.w, this.h);

    const world = TILE * 2 ** this.zoom;
    const c = project(this.center.lon, this.center.lat);
    // Viewport top-left in world pixels.
    const originX = c[0] * world - this.w / 2;
    const originY = c[1] * world - this.h / 2;

    // Group the layers into runs of consecutive ones drawing from the same
    // rung. Runs, not a map keyed by rung, because draw order has to survive:
    // if `places` and `roads` share a rung but `buildings` between them does
    // not, collecting by rung would paint the labels under the buildings.
    const runs = [];
    for (const name of LAYER_ORDER) {
      const rung = this.#rungFor(name);
      if (rung == null) continue;
      const last = runs[runs.length - 1];
      if (last && last.rung === rung) last.names.push(name);
      else runs.push({ rung, names: [name] });
    }

    let labelled = [];
    for (const run of runs) {
      const tiles = this.#collect(run.rung, run.names, world, originX, originY);
      for (const t of tiles) this.#paint(t, run.names);
      if (run.names.includes('places')) labelled = tiles;
    }

    // Labels are not part of any tile: they are laid out in screen space and
    // collide across tile boundaries, so they cannot be baked into a bitmap.
    this.#identity();
    this.#drawLabels(labelled);

    onStatus(this);
  }

  #layer(tile, name) { return tile.layers.get(name); }

  // Draw one tile, from its bitmap if there is one and by making one if not.
  //
  // Mid-drag a tile with no bitmap yet is drawn straight to the screen in cheap
  // mode instead: rasterising newly exposed ground is exactly the hitch a drag
  // must not have, and the frame after the pointer comes up redraws it properly.
  #paint(tile, names) {
    const rk = `${tile.key}|${[...tile.layers.keys()].join('+')}@${this.zoom}`;
    let bmp = this.rasters.get(rk);
    if (!bmp) {
      if (this.dragging) {
        this.#drawDirect(tile, names, true);
        return;
      }
      bmp = this.#rasterise(tile, names);
      if (!bmp) {
        // Too large to be worth caching; draw it straight, at full quality.
        this.#drawDirect(tile, names, false);
        return;
      }
      this.rasters.set(rk, bmp);
      this.rasterBytes += bmp.width * bmp.height * 4;
      this.#trimRasters();
    }
    this.#identity();
    // The bitmap is `span * dpr` pixels for a `span` CSS-pixel box, so this is
    // a 1:1 device-pixel blit.
    this.ctx.drawImage(bmp, tile.ox, tile.oy, tile.span, tile.span);
  }

  // Draw a tile onto the screen without going through a bitmap, clipped to its
  // own box.
  //
  // The clip is what makes this equivalent to the bitmap path rather than
  // merely similar: an offscreen canvas clips the 64-unit buffer for free, and
  // without the same clip here a tile's buffered geometry would paint over its
  // neighbour -- which for road casings is visible, and is the thing the old
  // layer-at-a-time ordering existed to prevent.
  #drawDirect(tile, names, cheap) {
    const ctx = this.ctx;
    ctx.save();
    this.#identity();
    ctx.beginPath();
    ctx.rect(tile.ox, tile.oy, tile.span, tile.span);
    ctx.clip();
    this.#place(tile, this.#extent(tile));
    this.#drawTile(tile, names, cheap);
    ctx.restore();
  }

  // Render a tile into an offscreen canvas of exactly its on-screen size.
  //
  // The drawing helpers all target `this.ctx` and place themselves from the
  // tile's own offsets, so the cheapest correct way to reuse them is to point
  // `this.ctx` at the offscreen canvas and hand them a tile sitting at the
  // origin. One drawing implementation, used for both the screen and the cache.
  #rasterise(tile, names) {
    const px = Math.round(tile.span * this.dpr);
    if (!(px >= 1 && px <= RASTER_MAX)) return null;
    const c = document.createElement('canvas');
    c.width = px;
    c.height = px;
    const target = c.getContext('2d');
    if (!target) return null;
    const saved = this.ctx;
    this.ctx = target;
    try {
      const at = { key: tile.key, layers: tile.layers, ox: 0, oy: 0, span: tile.span };
      this.#place(at, this.#extent(at));
      this.#drawTile(at, names, false);
    } finally {
      this.ctx = saved;
    }
    return c;
  }

  // What one tile contributes, for the layers it was asked for. `names` arrives
  // in draw order, so this just follows it. `cheap` drops the road casings --
  // half the road passes -- for frames that have to be fast rather than final.
  #drawTile(tile, names, cheap) {
    for (const name of names) {
      switch (name) {
        case 'land':
        case 'landuse':
        case 'buildings':
          this.#drawFills(tile, name);
          break;
        case 'water':
          this.#drawFills(tile, 'water');
          this.#drawWaterLines(tile);
          break;
        case 'roads': {
          const k = widthScale(this.zoom);
          for (const [cls, style] of ROADS) {
            if (style.casing && !cheap) {
              this.#drawRoads(tile, cls, style.casing, (style.width + 0.6) * k);
            }
            this.#drawRoads(tile, cls, style.color, style.width * k);
          }
          break;
        }
        // Labels are laid out in screen space and collide across tile
        // boundaries, so they are never part of a tile's pixels.
        case 'places':
          break;
      }
    }
  }

  // Every layer shares one extent in practice, and the placement has to be
  // chosen before any particular layer is looked up.
  #extent(tile) {
    for (const layer of tile.layers.values()) return layer.extent ?? 4096;
    return 4096;
  }

  #trimRasters() {
    while (this.rasterBytes > RASTER_BUDGET && this.rasters.size > 1) {
      const oldest = this.rasters.keys().next().value;
      const bmp = this.rasters.get(oldest);
      this.rasterBytes -= bmp.width * bmp.height * 4;
      this.rasters.delete(oldest);
    }
  }

  // One Path2D per class per layer, in the tile's own 0..extent coordinates.
  //
  // Two things are going on. The first is batching: OSM splits ways at every
  // junction and tag change, so a z8 tile holds ~25k road features averaging
  // 2.0 points each, and the cost of drawing them is per-call overhead, not
  // geometry. One path per class turns ~300k canvas calls into a few dozen, and
  // since every feature in a class shares one opaque colour and one width the
  // result is pixel-for-pixel what per-feature stroking produced.
  //
  // The second is why the coordinates are tile-local rather than screen. A
  // tile's geometry in its own space never changes -- not when you pan, not
  // when you zoom -- so this is built once and then *placed* with a transform.
  // Building it in screen coordinates instead meant every frame of a drag
  // walked every point of every feature, and walked them once per style pass:
  // roads alone are nine classes with a casing and a body each, and each of
  // those eighteen passes re-scanned the whole layer looking for its own class.
  // At z10, where a tile carries tens of thousands of features, that is the
  // whole reason panning was slow.
  #paths(layer) {
    if (layer.paths) return layer.paths;
    const paths = new Map();
    for (const f of layer.features) {
      const key = `${f.type}:${f.props.cls}`;
      let p = paths.get(key);
      if (!p) paths.set(key, (p = new Path2D()));
      for (const ring of f.rings) {
        p.moveTo(ring[0], ring[1]);
        for (let i = 2; i < ring.length; i += 2) p.lineTo(ring[i], ring[i + 1]);
      }
    }
    layer.paths = paths;
    return paths;
  }

  // Put the canvas into `tile`'s coordinate space so a cached path lands in the
  // right place at the right size. Returns the scale, because line widths are
  // scaled by the transform too and have to be divided back out.
  #place(tile, extent) {
    const s = tile.span / extent;
    const d = this.dpr;
    this.ctx.setTransform(d * s, 0, 0, d * s, d * tile.ox, d * tile.oy);
    return s;
  }

  #drawFills(tile, name) {
    const layer = this.#layer(tile, name);
    if (!layer) return;
    const ctx = this.ctx;
    const paths = this.#paths(layer);
    for (const [cls, color] of FILLS[name] || []) {
      const p = paths.get(`3:${cls}`);
      if (!p) continue;
      ctx.fillStyle = color;
      // nonzero (the default) matches MVT winding: exteriors and holes wind
      // oppositely, and it survives batching -- where one polygon's hole is
      // covered by another's exterior the windings sum to 1, which is filled,
      // which is correct. `fill` closes each subpath implicitly.
      ctx.fill(p);
    }
  }

  #drawWaterLines(tile) {
    const layer = this.#layer(tile, 'water');
    if (!layer) return;
    const ctx = this.ctx;
    const paths = this.#paths(layer);
    const k = widthScale(this.zoom);
    const s = tile.span / layer.extent;
    ctx.lineCap = 'round';
    for (const [cls, style] of Object.entries(WATER_LINES)) {
      const p = paths.get(`2:${cls}`);
      if (!p) continue;
      ctx.strokeStyle = style.color;
      ctx.lineWidth = Math.max(0.5, style.width * k) / s;
      ctx.stroke(p);
    }
  }

  // Labels, across all visible tiles at once rather than tile by tile: two
  // things here are viewport-wide, not tile-wide.
  //
  // Collision. Text that overlaps other text is unreadable, so a label is only
  // drawn if its box is clear, and boxes do not stop at a tile edge. Cities are
  // placed before towns so the loser of a collision is the smaller place.
  //
  // Duplicates. Tiles carry a 64-unit buffer, so a city near an edge is in both
  // tiles and would be drawn twice, very slightly offset -- which reads as
  // blurred text, not as two labels. Both copies land on the same screen point,
  // so keying on rounded position removes one.
  #drawLabels(visible) {
    const ctx = this.ctx;
    const items = [];
    const seen = new Set();
    for (const t of visible) {
      const layer = this.#layer(t, 'places');
      if (!layer) continue;
      const s = t.span / layer.extent;
      for (const f of layer.features) {
        if (f.type !== 1 || !f.rings.length || !f.props.name) continue;
        const x = t.ox + f.rings[0][0] * s;
        const y = t.oy + f.rings[0][1] * s;
        const key = `${f.props.name}@${Math.round(x / 4)},${Math.round(y / 4)}`;
        if (seen.has(key)) continue;
        seen.add(key);
        items.push({ name: f.props.name, cls: f.props.cls, x, y });
      }
    }
    items.sort((a, b) => (LABELS[b.cls]?.size ?? 0) - (LABELS[a.cls]?.size ?? 0));

    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    ctx.lineJoin = 'round';
    const placed = [];
    for (const it of items) {
      const style = LABELS[it.cls] ?? LABELS.town;
      ctx.font = `${style.weight} ${style.size}px system-ui, -apple-system, sans-serif`;
      const half = ctx.measureText(it.name).width / 2 + 3;
      const box = [it.x - half, it.y - style.size / 2 - 2, it.x + half, it.y + style.size / 2 + 2];
      if (box[2] < 0 || box[0] > this.w || box[3] < 0 || box[1] > this.h) continue;
      if (placed.some((p) => box[0] < p[2] && box[2] > p[0] && box[1] < p[3] && box[3] > p[1])) continue;
      placed.push(box);
      ctx.strokeStyle = LABEL_HALO;
      ctx.lineWidth = 3;
      ctx.strokeText(it.name, it.x, it.y);
      ctx.fillStyle = LABEL_COLOR;
      ctx.fillText(it.name, it.x, it.y);
    }
  }

  #drawRoads(tile, cls, color, width) {
    const layer = this.#layer(tile, 'roads');
    if (!layer) return;
    const p = this.#paths(layer).get(`2:${cls}`);
    if (!p) return;
    const ctx = this.ctx;
    const s = tile.span / layer.extent;
    ctx.strokeStyle = color;
    ctx.lineWidth = Math.max(0.4, width) / s;
    ctx.lineCap = 'round';
    ctx.lineJoin = 'round';
    ctx.stroke(p);
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
      `z${m.zoom}  ${m.center.lat.toFixed(4)}, ${m.center.lon.toFixed(4)}` +
      (m.pending ? `  loading ${m.pending}` : '');
    // Keep the URL shareable, but do not spam history while dragging.
    clearTimeout(hashTimer);
    hashTimer = setTimeout(() => {
      const h = `#${m.zoom}/${m.center.lat.toFixed(5)}/${m.center.lon.toFixed(5)}`;
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
