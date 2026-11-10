const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { createRelay } = require('./relay.cjs');
const TOKEN = 'filmcraft-public-test-token-not-a-secret';

async function fixture(t, subfolder = '') {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'filmcraft-relay-'));
  await fs.mkdir(path.join(root, subfolder), { recursive: true });
  const project = path.join(root, subfolder, 'cut.fcproj');
  await fs.writeFile(path.join(root, 'take.wav'), Buffer.from('0123456789'));
  const document = { format: 'filmcraft.project', schema_version: 17, project: {
    name: 'Network edit', items: { 1: { kind: { Media: { media: { File: { path: path.join(root, 'take.wav') } }, proxy: null } } } }, settings: { scratch: { video_previews: 'host-only-folder' } }
  } };
  await fs.writeFile(project, JSON.stringify(document));
  const app = await createRelay({ root, project, token: TOKEN, port: 0, bind: '127.0.0.1' });
  await app.run();
  const origin = `http://127.0.0.1:${app.server.address().port}`;
  t.after(async () => { await app.shutdown(); await fs.rm(root, { recursive: true, force: true }); });
  const request = (url, options = {}) => fetch(origin + url, { ...options, signal: AbortSignal.timeout(5000), headers: { Authorization: 'Bearer ' + TOKEN, ...options.headers } });
  return { root, project, request, origin };
}

test('authentication applies to every endpoint without CORS disclosure', async t => {
  const { origin } = await fixture(t);
  const response = await fetch(origin + '/v1/project');
  assert.equal(response.status, 401);
  assert.equal(response.headers.get('access-control-allow-origin'), null);
  assert.equal((await fetch(origin + '/v1/list')).status, 401);
  assert.equal((await fetch(origin + '/v1/project', { headers: { Authorization: TOKEN } })).status, 401);
});
test('snapshot uses portable paths and bounded byte reads preserve exact bytes', async t => {
  const { request } = await fixture(t);
  const snapshot = await (await request('/v1/project')).json();
  assert.equal(snapshot.document.project.items[1].kind.Media.media.File.path, 'take.wav');
  const head = await request('/v1/file?path=take.wav', { method: 'HEAD' });
  assert.equal(head.status, 200);
  assert.equal(head.headers.get('content-length'), '10');
  const response = await request('/v1/file?path=take.wav', { headers: { Range: 'bytes=3-6', 'If-Match': head.headers.get('etag') } });
  assert.equal(response.status, 206);
  assert.equal(response.headers.get('content-range'), 'bytes 3-6/10');
  assert.equal(await response.text(), '3456');
  for (const range of ['bytes=4-3', 'bytes=0-10', 'bytes=0-', 'bytes=-1-3', 'bytes=9007199254740992-9007199254740993']) {
    assert.equal((await request('/v1/file?path=take.wav', { headers: { Range: range } })).status, 416, range);
  }
});
test('stale project versions cannot overwrite another editor and scratch paths stay on the host', async t => {
  const { request, project } = await fixture(t);
  const snapshot = await (await request('/v1/project')).json();
  const doc = snapshot.document;
  doc.project.name = 'First editor';
  doc.project.settings.scratch = { video_previews: 'client-only-folder' };
  const options = { method: 'PUT', headers: { 'If-Match': snapshot.version, 'Content-Type': 'application/octet-stream' }, body: JSON.stringify(doc) };
  assert.equal((await request('/v1/project', options)).status, 200);
  doc.project.name = 'Stale editor';
  assert.equal((await request('/v1/project', { ...options, body: JSON.stringify(doc) })).status, 409);
  const stored = JSON.parse(await fs.readFile(project));
  assert.equal(stored.project.name, 'First editor');
  assert.equal(stored.project.settings.scratch.video_previews, 'host-only-folder');
});
test('two simultaneous saves have exactly one winner', async t => {
  const { request } = await fixture(t);
  const snapshot = await (await request('/v1/project')).json();
  const send = name => request('/v1/project', { method: 'PUT', headers: { 'If-Match': snapshot.version }, body: JSON.stringify({ ...snapshot.document, marker: name }) });
  const responses = await Promise.all([send('a'), send('b')]);
  assert.deepEqual(responses.map(r => r.status).sort(), [200, 409]);
});

test('a host without scratch settings never acquires the editing computers paths', async t => {
  const { request, project } = await fixture(t);
  const original = JSON.parse(await fs.readFile(project));
  delete original.project.settings.scratch;
  await fs.writeFile(project, JSON.stringify(original));
  const snapshot = await (await request('/v1/project')).json();
  snapshot.document.project.settings.scratch = { video_previews: 'client-only-folder' };
  const response = await request('/v1/project', { method: 'PUT', headers: { 'If-Match': snapshot.version }, body: JSON.stringify(snapshot.document) });
  assert.equal(response.status, 200);
  assert.deepEqual(JSON.parse(await fs.readFile(project)).project.settings.scratch, {});
});

test('snapshot and save preserve exact integer ticks and numeric literals', async t => {
  const { request, project } = await fixture(t);
  const literals = ['9007199254740993', '9223372036854775806', '-9223372036854775807', '-0.0', '1.234567890123456789e-25'];
  const document = JSON.parse(await fs.readFile(project));
  document.project.items[1].kind.Media.media.File.path = 'take.wav';
  const source = JSON.stringify(document).replace('"name":"Network edit"', '"name":"Network edit","exact":[' + literals.join(',') + ']');
  await fs.writeFile(project, source);
  const snapshotText = await (await request('/v1/project')).text();
  for (const literal of literals) assert.ok(snapshotText.includes(literal), 'snapshot preserves ' + literal);
  const version = JSON.parse(snapshotText).version;
  const saved = await request('/v1/project', { method: 'PUT', headers: { 'If-Match': version }, body: source.replace('Network edit', 'Exact edit') });
  assert.equal(saved.status, 200);
  const stored = await fs.readFile(project, 'utf8');
  for (const literal of literals) assert.ok(stored.includes(literal), 'save preserves ' + literal);
});
test('traversal, foreign roots and symlinks cannot escape the shared folder', async t => {
  const { request, root } = await fixture(t);
  for (const name of ['../private', '/etc/passwd', 'C:\\private', 'a\\..\\private', 'file:///etc/passwd', 'a:stream', 'a\0b']) {
    assert.equal((await request('/v1/file?path=' + encodeURIComponent(name), { method: 'HEAD' })).status, 400, name);
  }
  try {
    await fs.symlink(path.dirname(root), path.join(root, 'outside'), process.platform === 'win32' ? 'junction' : 'dir');
    assert.equal((await request('/v1/list?path=outside')).status, 403);
  } catch (error) { if (!['EPERM', 'EACCES'].includes(error.code)) throw error; }
  const snapshot = await (await request('/v1/project')).json();
  snapshot.document.project.items[1].kind.Media.media.File.path = '../outside.wav';
  assert.equal((await request('/v1/project', { method: 'PUT', headers: { 'If-Match': snapshot.version }, body: JSON.stringify(snapshot.document) })).status, 400);
});
test('media version changes refuse later reads rather than mixing two files', async t => {
  const { request, root } = await fixture(t);
  const head = await request('/v1/file?path=take.wav', { method: 'HEAD' });
  await fs.writeFile(path.join(root, 'take.wav'), 'replacement');
  assert.equal((await request('/v1/file?path=take.wav', { headers: { Range: 'bytes=0-1', 'If-Match': head.headers.get('etag') } })).status, 409);
});
test('uploads stream to unique complete files and become available to other operating systems', async t => {
  const { request, root } = await fixture(t);
  const response = await request('/v1/upload?name=audio%20take.wav', { method: 'PUT', body: Buffer.from('uploaded media') });
  assert.equal(response.status, 200);
  const result = await response.json();
  assert.equal(await fs.readFile(path.join(root, result.path), 'utf8'), 'uploaded media');
  assert.ok(!result.path.includes('\\'));
  assert.equal((await request('/v1/upload?name=..%2Foutside', { method: 'PUT', body: 'bad' })).status, 400);
});
test('invalid projects and weak tokens are rejected before opening a listener', async t => {
  const { project, root } = await fixture(t);
  await assert.rejects(createRelay({ project, root, token: 'short' }), /32/);
  await fs.writeFile(project, '{invalid');
  await assert.rejects(createRelay({ project, root, token: TOKEN }), /Invalid project/);
});

test('oversized host documents fail while streaming and remain untouched', async t => {
  const { request, project } = await fixture(t);
  const size = 32 * 1024 * 1024 + 1;
  await fs.truncate(project, size);
  const response = await request('/v1/project');
  assert.equal(response.status, 413);
  assert.match((await response.json()).error, /32 MiB/);
  assert.equal((await fs.stat(project)).size, size);
});

test('directory enumeration stops at the limit and closes the iterator', async t => {
  const { request } = await fixture(t);
  let scanned = 0;
  let closed = false;
  t.mock.method(fs, 'opendir', async () => (async function* () {
    try {
      while (true) {
        scanned++;
        assert.ok(scanned <= 20001, 'enumeration must stop before an unbounded directory fills memory');
        yield { name: 'take-' + scanned, isSymbolicLink: () => false, isDirectory: () => false };
      }
    } finally { closed = true; }
  })());
  assert.equal((await request('/v1/list')).status, 413);
  assert.equal(scanned, 20001);
  assert.equal(closed, true);
});
test('a nested project shares sibling media through portable parent-relative references', async t => {
  const { request } = await fixture(t, 'edits');
  const snapshot = await (await request('/v1/project')).json();
  assert.equal(snapshot.project, 'edits/cut.fcproj');
  assert.equal(snapshot.document.project.items[1].kind.Media.media.File.path, '../take.wav');
  assert.equal((await request('/v1/project', { method: 'PUT', headers: { 'If-Match': snapshot.version }, body: JSON.stringify(snapshot.document) })).status, 200);
});
test('file URLs from the host become portable media paths', async t => {
  const { root, project, request } = await fixture(t);
  const document = JSON.parse(await fs.readFile(project));
  document.project.items[1].kind.Media.media.File.path = require('node:url').pathToFileURL(path.join(root, 'take.wav')).href;
  await fs.writeFile(project, JSON.stringify(document));
  const snapshot = await (await request('/v1/project')).json();
  assert.equal(snapshot.document.project.items[1].kind.Media.media.File.path, 'take.wav');
});
test('upgrading a schema keeps an exact original backup without replacing an existing one', async t => {
  const { root, project, request } = await fixture(t);
  const old = JSON.parse(await fs.readFile(project));
  old.schema_version = 1;
  const original = JSON.stringify(old);
  await fs.writeFile(project, original);
  const snapshot = await (await request('/v1/project')).json();
  snapshot.document.schema_version = 17;
  assert.equal((await request('/v1/project', { method: 'PUT', headers: { 'If-Match': snapshot.version }, body: JSON.stringify(snapshot.document) })).status, 200);
  assert.equal(await fs.readFile(path.join(root, 'cut (schema v1 backup).fcproj'), 'utf8'), original);
});
test('an interrupted upload leaves no published partial media and the relay keeps serving', async t => {
  const { root, request, origin } = await fixture(t);
  const upload = require('node:http').request(origin + '/v1/upload?name=interrupted.wav', {
    method: 'PUT', headers: { Authorization: 'Bearer ' + TOKEN, 'Content-Length': 4096 }
  });
  upload.on('error', () => {});
  upload.write('partial');
  await new Promise(resolve => setTimeout(resolve, 30));
  upload.destroy(new Error('synthetic interrupted connection'));
  await new Promise(resolve => setTimeout(resolve, 100));
  const files = await fs.readdir(path.join(root, 'FilmCraft Incoming')).catch(error => { if (error.code === 'ENOENT') return []; throw error; });
  assert.deepEqual(files, []);
  assert.equal((await request('/v1/project')).status, 200);
});
