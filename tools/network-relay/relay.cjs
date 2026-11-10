// A separate RedWeb service: FilmCraft's desktop application remains entirely Rust.
const { defineApp } = require('redweb');
const fs = require('node:fs/promises');
const { createReadStream, createWriteStream } = require('node:fs');
const path = require('node:path');
const { createHash, randomUUID, timingSafeEqual } = require('node:crypto');
const { Transform } = require('node:stream');
const { pipeline } = require('node:stream/promises');
const { fileURLToPath } = require('node:url');

const PROJECT_LIMIT = 32 * 1024 * 1024;
const RANGE_LIMIT = 8 * 1024 * 1024;
const UPLOAD_LIMIT = 32 * 1024 * 1024 * 1024;
const fail = (status, message) => Object.assign(new Error(message), { status });
const hash = bytes => '"' + createHash('sha256').update(bytes).digest('hex') + '"';
async function atomicWrite(file, bytes, replace = true) {
  const directory = path.dirname(file);
  const temp = path.join(directory, '.filmcraft-' + randomUUID() + '.tmp');
  try {
    const handle = await fs.open(temp, 'wx');
    try { await handle.writeFile(bytes); await handle.sync(); } finally { await handle.close(); }
    if (replace) await fs.rename(temp, file);
    else await fs.link(temp, file); // Publish a complete backup only if its name is unoccupied.
    if (process.platform !== 'win32') {
      const dir = await fs.open(directory, 'r');
      try { await dir.sync(); } finally { await dir.close(); }
    }
  } finally { await fs.rm(temp, { force: true }).catch(() => {}); }
}
function logical(value, allowEmpty = false) {
  if (typeof value !== 'string' || value.length > 4096 || /[\\\x00-\x1f:]/.test(value) || value.startsWith('/')) throw fail(400, 'Invalid file path');
  const parts = value.split('/');
  if ((!allowEmpty || value !== '') && parts.some(p => !p || p === '.' || p === '..')) throw fail(400, 'Invalid file path');
  return value;
}
function within(root, file) {
  const rel = path.relative(root, file);
  return !path.isAbsolute(rel) && rel !== '..' && !rel.startsWith('..' + path.sep);
}
function refs(doc) {
  const project = doc.project ?? doc;
  if (!project || typeof project !== 'object' || !project.items || typeof project.items !== 'object') throw fail(400, 'Not a FilmCraft project');
  return Object.values(project.items).flatMap(item => {
    const media = item?.kind?.Media;
    return media ? [media.media?.File, media.proxy?.File].filter(Boolean) : [];
  });
}
function parseProject(bytes) {
  if (bytes.length > PROJECT_LIMIT) throw fail(413, 'Project exceeds the 32 MiB relay limit');
  let doc;
  // Tick and identity integers exceed JavaScript's exact Number range. Keep every numeric
  // token verbatim while rewriting paths; rounding here would silently damage edit timing.
  try { doc = JSON.parse(bytes, (_key, value, context) => typeof value === 'number' ? JSON.rawJSON(context.source) : value); } catch { throw fail(400, 'Invalid project JSON'); }
  refs(doc);
  return doc;
}
async function body(request, limit, message = 'Request is too large') {
  const chunks = [];
  let size = 0;
  for await (const chunk of request) {
    size += chunk.length;
    if (size > limit) throw fail(413, message);
    chunks.push(chunk);
  }
  return Buffer.concat(chunks);
}

async function createRelay({ project, root = path.dirname(project), token, port = 9877, bind = '0.0.0.0', signals = false }) {
  if (typeof JSON.rawJSON !== 'function' || JSON.parse('1', (_key, _value, context) => context?.source) !== '1') throw new Error('Node.js 22 or newer with lossless JSON support is required');
  if (typeof token !== 'string' || token.length < 32 || token.length > 256 || /[^\x21-\x7e]/.test(token)) throw new Error('FILMCRAFT_RELAY_TOKEN must contain 32–256 printable non-space ASCII characters');
  root = await fs.realpath(root);
  project = await fs.realpath(project);
  if (!within(root, project) || path.extname(project).toLowerCase() !== '.fcproj') throw new Error('Project must be a .fcproj inside the shared root');
  const projectKey = path.relative(root, project).split(path.sep).join('/');
  const projectDir = path.dirname(project);
  const expected = Buffer.from(token);
  // Bound disk reads before parsing too: a corrupt project can be much larger than valid JSON.
  const projectBytes = () => body(createReadStream(project), PROJECT_LIMIT, 'Project exceeds the 32 MiB relay limit');
  let writes = Promise.resolve();
  const serial = fn => {
    const result = writes.then(fn, fn);
    writes = result.catch(() => {});
    return result;
  };
  async function fileFor(key) {
    logical(key);
    const file = await fs.realpath(path.join(root, key));
    // Resolve symlinks before checking containment; a shared folder cannot grant access outside it.
    if (!within(root, file)) throw fail(403, 'File is outside the shared folder');
    return file;
  }
  function portable(doc, fromClient) {
    for (const ref of refs(doc)) {
      if (typeof ref.path !== 'string' || ref.path.length > 4096 || ref.path.includes('\0')) throw fail(400, 'Invalid media reference');
      if (fromClient && (path.isAbsolute(ref.path) || /^[A-Za-z]:|^\\\\|^[a-z]+:/i.test(ref.path))) throw fail(400, 'Project media must use portable relative paths');
      let stored = ref.path;
      if (!fromClient && /^file:/i.test(stored)) {
        try { stored = fileURLToPath(stored); } catch { throw fail(400, 'Invalid media file URL'); }
      }
      const target = path.resolve(projectDir, stored);
      if (!within(root, target)) throw fail(400, 'Media is outside the shared root; choose a common parent with --root or collect the project files first');
      ref.path = path.relative(projectDir, target).split(path.sep).join('/');
    }
    return doc;
  }
  async function snapshot() {
    const bytes = await projectBytes();
    return { protocol: 1, project: projectKey, version: hash(bytes), document: portable(parseProject(bytes), false) };
  }
  await snapshot();
  async function handle(req, res) {
    res.setHeader('Cache-Control', 'no-store');
    res.setHeader('X-Content-Type-Options', 'nosniff');
    const authorization = req.headers.authorization ?? '';
    const supplied = Buffer.from(authorization.startsWith('Bearer ') ? authorization.slice(7) : '');
    if (supplied.length !== expected.length || !timingSafeEqual(supplied, expected)) throw fail(401, 'Authentication required');
    const url = new URL(req.url, 'http://relay.invalid');
    if (req.method === 'GET' && url.pathname === '/v1/project') return res.json(await snapshot());
    if (req.method === 'PUT' && url.pathname === '/v1/project') {
      const bytes = await body(req, PROJECT_LIMIT);
      const document = portable(parseProject(bytes), true);
      return serial(async () => {
        const current = await projectBytes();
        if (req.headers['if-match'] !== hash(current)) throw fail(409, 'The host project changed. Save a local recovery copy and reopen before saving.');
        // Scratch disks belong to each computer; never copy a client's local output paths to the host.
        const original = parseProject(current);
        const oldSchema = Number(original.schema_version?.rawJSON ?? original.schema_version ?? 1);
        const newSchema = Number(document.schema_version?.rawJSON ?? document.schema_version ?? 1);
        if (Number.isSafeInteger(oldSchema) && newSchema > oldSchema) {
          const backup = path.join(projectDir, path.basename(project, path.extname(project)) + ` (schema v${oldSchema} backup).fcproj`);
          try { await atomicWrite(backup, current, false); } catch (error) { if (error.code !== 'EEXIST') throw error; }
        }
        const hostProject = original.project ?? original;
        const newProject = document.project ?? document;
        if (newProject.settings != null && (typeof newProject.settings !== 'object' || Array.isArray(newProject.settings))) throw fail(400, 'Invalid project settings');
        newProject.settings ??= {};
        newProject.settings.scratch = hostProject.settings?.scratch ?? {};
        const output = Buffer.from(JSON.stringify(document, null, 2) + '\n');
        if (output.length > PROJECT_LIMIT) throw fail(413, 'Saved project exceeds the 32 MiB relay limit');
        await atomicWrite(project, output);
        return res.json({ version: hash(output) });
      });
    }
    if (['GET', 'HEAD'].includes(req.method) && url.pathname === '/v1/file') {
      const file = await fileFor(url.searchParams.get('path'));
      const stat = await fs.stat(file, { bigint: true });
      if (!stat.isFile()) throw fail(404, 'Not a file');
      const size = Number(stat.size);
      if (!Number.isSafeInteger(size)) throw fail(413, 'File exceeds the supported relay size');
      const etag = hash(`${stat.dev}:${stat.ino}:${stat.size}:${stat.mtimeNs}:${stat.ctimeNs}`);
      if (req.headers['if-match'] && req.headers['if-match'] !== etag) throw fail(409, 'Media changed on the host; reopen the project');
      res.setHeader('ETag', etag);
      res.setHeader('Accept-Ranges', 'bytes');
      res.setHeader('Content-Type', 'application/octet-stream');
      if (req.method === 'HEAD') { res.setHeader('Content-Length', size); return res.end(); }
      const range = /^bytes=(\d+)-(\d+)$/.exec(req.headers.range ?? '');
      if (!range) throw fail(416, 'A bounded byte range is required');
      const start = Number(range[1]), end = Number(range[2]);
      if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start > end || start < 0 || end >= size || end - start + 1 > RANGE_LIMIT) throw fail(416, 'Invalid or excessive byte range');
      res.status(206);
      res.setHeader('Content-Range', `bytes ${start}-${end}/${size}`);
      res.setHeader('Content-Length', end - start + 1);
      await pipeline(createReadStream(file, { start, end }), res);
      return;
    }
    if (req.method === 'GET' && url.pathname === '/v1/list') {
      const key = logical(url.searchParams.get('path') ?? '', true);
      const directory = key === '' ? root : await fileFor(key);
      const entries = [];
      let scanned = 0;
      // Stop enumeration at the cap instead of allocating the entire directory first.
      for await (const entry of await fs.opendir(directory)) {
        if (++scanned > 20000) throw fail(413, 'Directory exceeds the relay listing limit');
        if (!entry.isSymbolicLink() && !entry.name.startsWith('.filmcraft-')) entries.push({ name: entry.name, isDir: entry.isDirectory(), size: null, modified: null });
      }
      return res.json(entries);
    }
    if (req.method === 'PUT' && url.pathname === '/v1/upload') {
      const name = url.searchParams.get('name');
      if (typeof name !== 'string' || name.length > 180 || !name || /[\\/\x00-\x1f:]/.test(name) || name === '.' || name === '..') throw fail(400, 'Invalid upload name');
      const length = Number(req.headers['content-length']);
      if (!Number.isSafeInteger(length) || length < 0 || length > UPLOAD_LIMIT) throw fail(413, 'Upload needs a Content-Length of at most 32 GiB');
      const incoming = path.join(root, 'FilmCraft Incoming');
      await fs.mkdir(incoming, { recursive: true });
      if (!within(root, await fs.realpath(incoming))) throw fail(403, 'Upload directory is outside the shared root');
      const dest = path.join(incoming, randomUUID() + '-' + name);
      const temp = path.join(incoming, '.filmcraft-' + randomUUID() + '.tmp');
      let received = 0;
      const bounded = new Transform({ transform(chunk, _encoding, done) {
        received += chunk.length;
        done(received > length ? fail(413, 'Upload exceeds its declared length') : null, chunk);
      } });
      try {
        await pipeline(req, bounded, createWriteStream(temp, { flags: 'wx' }));
        if (received !== length) throw fail(400, 'Incomplete upload');
        await fs.rename(temp, dest);
      } finally { await fs.rm(temp, { force: true }).catch(() => {}); }
      return res.json({ path: path.relative(root, dest).split(path.sep).join('/') });
    }
    throw fail(404, 'Unknown relay endpoint');
  }
  class Files {
    async onShutdown() { await writes; }
    onInit(context) {
      context.app.use((req, res) => {
        handle(req, res).catch(error => {
          if (res.headersSent || res.destroyed) { res.destroy(); return; }
          const status = error.status ?? (error.code === 'ENOENT' ? 404 : error.code === 'ENOSPC' ? 507 : 500);
          res.status(status).json({ error: status === 507 ? 'The host disk is full; free space and save again' : status === 500 ? 'Relay file operation failed' : error.message });
        });
      });
    }
  }
  return defineApp({ services: [Files], port, bind, signals, publicPaths: [] });
}

module.exports = { createRelay, logical, parseProject };
if (require.main === module) {
  const args = process.argv.slice(2);
  const project = args[0];
  const rootIndex = args.indexOf('--root');
  if (!project) {
    console.error('Usage: node relay.cjs project.fcproj [--root shared-folder]; set FILMCRAFT_RELAY_TOKEN (32+ characters), PORT (9877), BIND (0.0.0.0)');
    process.exitCode = 1;
  } else {
    createRelay({ project: path.resolve(project), root: rootIndex >= 0 ? args[rootIndex + 1] : path.dirname(path.resolve(project)), token: process.env.FILMCRAFT_RELAY_TOKEN, port: Number(process.env.PORT ?? 9877), bind: process.env.BIND ?? '0.0.0.0', signals: true })
      .then(async app => { await app.run(); console.log('FilmCraft RedWeb relay is listening. Connect using this machine’s LAN address and the configured token.'); })
      .catch(error => { console.error(error.message); process.exitCode = 1; });
  }
}
