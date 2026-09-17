// The decoder half of minimap.js, checked against tiles encoded here by hand.
//
//   node --test server/web/minimap.test.js   (`make test` runs it if node is there)
//
// No framework and no dependencies, like the file under test: node's own
// `node:test` and `node:assert`. The encoder below is the Mapbox Vector Tile
// spec written out -- protobuf varints, zigzag deltas, the MoveTo / LineTo /
// ClosePath command stream -- so what is asserted is that the ~130 lines of
// reader and decoder in minimap.js invert it exactly, including the cases a
// real tile from the bake exercises: keys and values arriving *after* the
// features that use them, negative deltas, ids past 2^31, and holes.

'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { Reader, decodeTile, decodeGeometry, project, unproject } = require('./minimap.js');

// ------------------------------------------------------------ an encoder

function varint(v) {
  const out = [];
  // BigInt so values past 2^53 encode exactly; everything else is small.
  let n = BigInt(v);
  while (n >= 0x80n) { out.push(Number(n & 0x7fn) | 0x80); n >>= 7n; }
  out.push(Number(n));
  return out;
}

function zigzag(n) { return n < 0 ? -2 * n - 1 : 2 * n; }

function key(field, wire) { return varint((field << 3) | wire); }

function bytesField(field, bytes) { return [...key(field, 2), ...varint(bytes.length), ...bytes]; }

function varintField(field, v) { return [...key(field, 0), ...varint(v)]; }

function stringValue(s) { return bytesField(1, [...Buffer.from(s, 'utf8')]); }

// A geometry command stream from rings given as absolute [x0,y0,x1,y1,...].
// `closed` adds ClosePath after each ring, as polygons carry it.
function geometry(rings, closed) {
  const out = [];
  let x = 0, y = 0;
  for (const ring of rings) {
    const push = (nx, ny) => { out.push(...varint(zigzag(nx - x)), ...varint(zigzag(ny - y))); x = nx; y = ny; };
    out.push(...varint((1 << 3) | 1)); // MoveTo, count 1
    push(ring[0], ring[1]);
    const rest = ring.length / 2 - 1;
    if (rest) out.push(...varint((rest << 3) | 2)); // LineTo, count rest
    for (let i = 2; i < ring.length; i += 2) push(ring[i], ring[i + 1]);
    if (closed) out.push(...varint((1 << 3) | 7)); // ClosePath
  }
  return out;
}

function feature({ id, type, tags, rings, closed }) {
  const body = [];
  if (id != null) body.push(...varintField(1, id));
  const packed = tags.flatMap((t) => varint(t));
  body.push(...bytesField(2, packed));
  body.push(...varintField(3, type));
  body.push(...bytesField(4, geometry(rings, closed)));
  return body;
}

// `late` puts keys and values after the features, which the spec allows and
// real encoders do.
function layer({ name, extent = 4096, keys, values, features, late = false }) {
  const dict = [
    ...keys.flatMap((k) => bytesField(3, [...Buffer.from(k)])),
    ...values.flatMap((v) => bytesField(4, v)),
  ];
  const body = [
    ...varintField(15, 2),
    ...bytesField(1, [...Buffer.from(name)]),
    ...(late ? [] : dict),
    ...features.flatMap((f) => bytesField(2, feature(f))),
    ...(late ? dict : []),
    ...varintField(5, extent),
  ];
  return bytesField(3, body);
}

function tile(...layers) { return new Uint8Array(layers.flat()); }

// ------------------------------------------------------------------ tests

test('varints, zigzag and wide values', () => {
  const r = new Reader(new Uint8Array([...varint(300), ...varint(2 ** 32 + 5), ...varint(zigzag(-7))]));
  assert.equal(r.varint(), 300);
  assert.equal(r.varint(), 2 ** 32 + 5, 'past 32 bits without wrapping');
  assert.equal(r.svarint(), -7);
  assert.ok(r.done);
});

test('a line feature with a class, keys arriving after the features', () => {
  const t = tile(layer({
    name: 'roads',
    keys: ['cls'],
    values: [stringValue('motorway')],
    late: true,
    features: [{ id: 2 ** 40, type: 2, tags: [0, 0], rings: [[10, 20, 30, 15, -5, 40]] }],
  }));
  const layers = decodeTile(t.buffer);
  assert.equal(layers.length, 1);
  const l = layers[0];
  assert.equal(l.name, 'roads');
  assert.equal(l.extent, 4096);
  assert.deepEqual(l.keys, ['cls']);
  assert.deepEqual(l.values, ['motorway']);
  assert.equal(l.features.length, 1);
  const f = l.features[0];
  assert.equal(f.type, 2);
  assert.deepEqual(f.props, { cls: 'motorway' });
  assert.deepEqual(f.rings, [[10, 20, 30, 15, -5, 40]], 'deltas resolve to absolute coordinates');
});

test('a polygon with a hole is two rings, cursor carried across them', () => {
  const outer = [0, 0, 100, 0, 100, 100, 0, 100];
  const hole = [40, 40, 40, 60, 60, 60, 60, 40];
  const t = tile(layer({
    name: 'buildings',
    keys: ['cls'],
    values: [stringValue('building')],
    features: [{ type: 3, tags: [0, 0], rings: [outer, hole], closed: true }],
  }));
  const [l] = decodeTile(t.buffer);
  assert.equal(l.features[0].type, 3);
  assert.deepEqual(l.features[0].rings, [outer, hole]);
});

test('every value type decodes, and unknown fields are skipped', () => {
  const values = [
    stringValue('x'),
    varintField(4, 12345),            // int64
    varintField(5, 7),                // uint64
    varintField(6, zigzag(-3)),       // sint64
    varintField(7, 1),                // bool
    [...key(9, 0), ...varint(99)],    // an unknown field inside a Value
  ];
  const t = tile(layer({
    name: 'places',
    keys: ['a', 'b', 'c', 'd', 'e', 'f'],
    values,
    features: [{ type: 1, tags: [0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5], rings: [[7, 8]] }],
  }));
  const [l] = decodeTile(t.buffer);
  assert.deepEqual(l.values, ['x', 12345, 7, -3, true, null]);
  assert.deepEqual(l.features[0].props, { a: 'x', b: 12345, c: 7, d: -3, e: true, f: null });
  assert.deepEqual(l.features[0].rings, [[7, 8]], 'a point is a one-vertex ring');
});

test('several layers come back in stream order; an empty tile is no layers', () => {
  const t = tile(
    layer({ name: 'land', keys: [], values: [], features: [] }),
    layer({ name: 'water', keys: [], values: [], features: [], extent: 512 }),
  );
  const layers = decodeTile(t.buffer);
  assert.deepEqual(layers.map((l) => [l.name, l.extent]), [['land', 4096], ['water', 512]]);
  assert.deepEqual(decodeTile(new Uint8Array(0).buffer), []);
  assert.deepEqual(decodeGeometry(null), []);
});

test('mercator projection round-trips and puts the origin top-left', () => {
  assert.deepEqual(project(-180, 85.0511287798).map((v) => Math.round(v * 1e9) / 1e9), [0, 0]);
  const [x, y] = project(0, 0);
  assert.equal(x, 0.5);
  assert.equal(y, 0.5);
  for (const [lon, lat] of [[2.3, 49.9], [-73.9, 40.7], [151.2, -33.9]]) {
    const [bx, by] = unproject(...project(lon, lat));
    assert.ok(Math.abs(bx - lon) < 1e-9 && Math.abs(by - lat) < 1e-9, `${lon},${lat} -> ${bx},${by}`);
  }
});
