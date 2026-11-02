// A small WASI preview1 implementation with an in-memory file system: exactly
// the calls eiketsu_web.wasm imports. Works in browsers and in Node (tests).
//
// The game folder is one preopened directory ("/game", fd 3). Files are
// { data: Uint8Array, size }, directories { dir: Map<name, node> }.

const E = { SUCCESS: 0, BADF: 8, EXIST: 20, INVAL: 28, ISDIR: 31, NOENT: 44, NOSYS: 52, NOTDIR: 54, NOTEMPTY: 55 };
const FT = { CHAR: 2, DIR: 3, FILE: 4 };

export class MemFS {
  constructor() {
    this.root = { dir: new Map() };
    this.dirty = new Set(); // paths written since the last takeDirty()
  }
  static file(bytes) {
    return { data: bytes, size: bytes.length };
  }
  // path: "A/B/C" relative to the root
  lookup(path) {
    let node = this.root;
    for (const part of path.split("/").filter((p) => p && p !== ".")) {
      if (!node.dir) return null;
      node = node.dir.get(part);
      if (!node) return null;
    }
    return node;
  }
  parentOf(path) {
    const parts = path.split("/").filter((p) => p && p !== ".");
    const name = parts.pop();
    const parent = this.lookup(parts.join("/"));
    return { parent, name };
  }
  writeFile(path, bytes) {
    const parts = path.split("/").filter(Boolean);
    let node = this.root;
    for (const part of parts.slice(0, -1)) {
      if (!node.dir.has(part)) node.dir.set(part, { dir: new Map() });
      node = node.dir.get(part);
    }
    node.dir.set(parts[parts.length - 1], MemFS.file(bytes));
  }
  // Every file as [path, bytes].
  files(node = this.root, prefix = "") {
    const out = [];
    for (const [name, n] of node.dir) {
      const p = prefix ? prefix + "/" + name : name;
      if (n.dir) out.push(...this.files(n, p));
      else out.push([p, n.data.subarray(0, n.size)]);
    }
    return out;
  }
  takeDirty() {
    const d = [...this.dirty];
    this.dirty.clear();
    return d;
  }
}

function grow(f, need) {
  if (need <= f.data.length) return;
  const n = new Uint8Array(Math.max(need, f.data.length * 2, 4096));
  n.set(f.data.subarray(0, f.size));
  f.data = n;
}

export class WASI {
  constructor(fs, { stdout = console.log, stderr = console.warn, preopen = "/game" } = {}) {
    this.fs = fs;
    this.preopen = preopen;
    this.memory = null;
    this.out = { 1: stdout, 2: stderr };
    this.lineBuf = { 1: "", 2: "" };
    // fd -> { node, path, pos, cookie? }
    this.fds = new Map([[3, { node: fs.root, path: "", pos: 0 }]]);
    this.nextFd = 4;
    this.imports = this.makeImports();
  }

  view() {
    return new DataView(this.memory.buffer);
  }
  bytes(ptr, len) {
    return new Uint8Array(this.memory.buffer, ptr, len);
  }
  str(ptr, len) {
    return new TextDecoder().decode(this.bytes(ptr, len));
  }
  // Resolve "a/b/../c" against a directory fd; returns the fs-root-relative path.
  resolve(fd, rel) {
    const base = this.fds.get(fd);
    if (!base || !base.node.dir) return null;
    const parts = base.path ? base.path.split("/") : [];
    for (const p of rel.split("/")) {
      if (!p || p === ".") continue;
      if (p === "..") parts.pop();
      else parts.push(p);
    }
    return parts.join("/");
  }
  stat(dv, ptr, node) {
    for (let i = 0; i < 64; i += 8) dv.setBigUint64(ptr + i, 0n, true);
    dv.setUint8(ptr + 16, node.dir ? FT.DIR : FT.FILE);
    dv.setBigUint64(ptr + 24, 1n, true);
    dv.setBigUint64(ptr + 32, BigInt(node.dir ? 0 : node.size), true);
  }

  makeImports() {
    const w = this;
    return {
      environ_get: () => E.SUCCESS,
      environ_sizes_get(countPtr, sizePtr) {
        const dv = w.view();
        dv.setUint32(countPtr, 0, true);
        dv.setUint32(sizePtr, 0, true);
        return E.SUCCESS;
      },
      clock_time_get(id, _precision, timePtr) {
        const ms = id === 0 ? Date.now() : performance.now();
        w.view().setBigUint64(timePtr, BigInt(Math.round(ms * 1e6)), true);
        return E.SUCCESS;
      },
      random_get(buf, len) {
        crypto.getRandomValues(w.bytes(buf, len));
        return E.SUCCESS;
      },
      proc_exit(code) {
        throw new Error("wasm exited with code " + code);
      },
      poll_oneoff: () => E.NOSYS,
      sock_accept: () => E.NOSYS,

      fd_prestat_get(fd, ptr) {
        if (fd !== 3) return E.BADF;
        const dv = w.view();
        dv.setUint8(ptr, 0);
        dv.setUint32(ptr + 4, new TextEncoder().encode(w.preopen).length, true);
        return E.SUCCESS;
      },
      fd_prestat_dir_name(fd, ptr, len) {
        if (fd !== 3) return E.BADF;
        w.bytes(ptr, len).set(new TextEncoder().encode(w.preopen).subarray(0, len));
        return E.SUCCESS;
      },
      fd_fdstat_get(fd, ptr) {
        const dv = w.view();
        let ft;
        if (fd <= 2) ft = FT.CHAR;
        else {
          const f = w.fds.get(fd);
          if (!f) return E.BADF;
          ft = f.node.dir ? FT.DIR : FT.FILE;
        }
        dv.setUint8(ptr, ft);
        dv.setUint16(ptr + 2, 0, true);
        dv.setBigUint64(ptr + 8, 0xffffffffffffffffn, true);
        dv.setBigUint64(ptr + 16, 0xffffffffffffffffn, true);
        return E.SUCCESS;
      },
      fd_fdstat_set_flags: () => E.SUCCESS,
      fd_close(fd) {
        if (fd <= 3) return E.SUCCESS;
        return w.fds.delete(fd) ? E.SUCCESS : E.BADF;
      },
      fd_filestat_get(fd, ptr) {
        const f = w.fds.get(fd);
        if (!f) return E.BADF;
        w.stat(w.view(), ptr, f.node);
        return E.SUCCESS;
      },
      fd_filestat_set_size(fd, size) {
        const f = w.fds.get(fd);
        if (!f || f.node.dir) return E.BADF;
        const n = Number(size);
        grow(f.node, n);
        if (n > f.node.size) f.node.data.fill(0, f.node.size, n);
        f.node.size = n;
        w.fs.dirty.add(f.path);
        return E.SUCCESS;
      },
      fd_seek(fd, offset, whence, newPtr) {
        const f = w.fds.get(fd);
        if (!f || f.node.dir) return E.BADF;
        const base = whence === 0 ? 0 : whence === 1 ? f.pos : f.node.size;
        const pos = base + Number(offset);
        if (pos < 0) return E.INVAL;
        f.pos = pos;
        w.view().setBigUint64(newPtr, BigInt(pos), true);
        return E.SUCCESS;
      },
      fd_read(fd, iovs, iovsLen, nreadPtr) {
        const f = w.fds.get(fd);
        const dv = w.view();
        if (fd === 0) {
          dv.setUint32(nreadPtr, 0, true);
          return E.SUCCESS;
        }
        if (!f || f.node.dir) return E.BADF;
        let total = 0;
        for (let i = 0; i < iovsLen; i++) {
          const buf = dv.getUint32(iovs + i * 8, true);
          const len = dv.getUint32(iovs + i * 8 + 4, true);
          const n = Math.max(0, Math.min(len, f.node.size - f.pos));
          w.bytes(buf, n).set(f.node.data.subarray(f.pos, f.pos + n));
          f.pos += n;
          total += n;
          if (n < len) break;
        }
        dv.setUint32(nreadPtr, total, true);
        return E.SUCCESS;
      },
      fd_write(fd, iovs, iovsLen, nwrittenPtr) {
        const dv = w.view();
        let total = 0;
        const chunks = [];
        for (let i = 0; i < iovsLen; i++) {
          const buf = dv.getUint32(iovs + i * 8, true);
          const len = dv.getUint32(iovs + i * 8 + 4, true);
          chunks.push(w.bytes(buf, len).slice());
          total += len;
        }
        if (fd === 1 || fd === 2) {
          let s = w.lineBuf[fd] + chunks.map((c) => new TextDecoder().decode(c)).join("");
          const lines = s.split("\n");
          w.lineBuf[fd] = lines.pop();
          for (const l of lines) w.out[fd](l);
        } else {
          const f = w.fds.get(fd);
          if (!f || f.node.dir) return E.BADF;
          for (const c of chunks) {
            grow(f.node, f.pos + c.length);
            f.node.data.set(c, f.pos);
            f.pos += c.length;
            f.node.size = Math.max(f.node.size, f.pos);
          }
          w.fs.dirty.add(f.path);
        }
        dv.setUint32(nwrittenPtr, total, true);
        return E.SUCCESS;
      },
      fd_readdir(fd, buf, bufLen, cookie, usedPtr) {
        const f = w.fds.get(fd);
        if (!f || !f.node.dir) return E.BADF;
        const dv = w.view();
        const names = [...f.node.dir.keys()];
        let used = 0;
        for (let i = Number(cookie); i < names.length; i++) {
          const name = new TextEncoder().encode(names[i]);
          const node = f.node.dir.get(names[i]);
          const ent = new Uint8Array(24 + name.length);
          const ev = new DataView(ent.buffer);
          ev.setBigUint64(0, BigInt(i + 1), true);
          ev.setBigUint64(8, BigInt(i + 1), true);
          ev.setUint32(16, name.length, true);
          ev.setUint8(20, node.dir ? FT.DIR : FT.FILE);
          ent.set(name, 24);
          const n = Math.min(ent.length, bufLen - used);
          w.bytes(buf + used, n).set(ent.subarray(0, n));
          used += n;
          if (used >= bufLen) break;
        }
        dv.setUint32(usedPtr, used, true);
        return E.SUCCESS;
      },
      path_open(dirfd, _dirflags, pathPtr, pathLen, oflags, _rb, _ri, fdflags, fdPtr) {
        const path = w.resolve(dirfd, w.str(pathPtr, pathLen));
        if (path === null) return E.BADF;
        let node = w.fs.lookup(path);
        const CREAT = 1, DIRECTORY = 2, EXCL = 4, TRUNC = 8;
        if (node && oflags & EXCL && oflags & CREAT) return E.EXIST;
        if (!node) {
          if (!(oflags & CREAT)) return E.NOENT;
          const { parent, name } = w.fs.parentOf(path);
          if (!parent || !parent.dir) return E.NOENT;
          node = MemFS.file(new Uint8Array(0));
          parent.dir.set(name, node);
          w.fs.dirty.add(path);
        }
        if (oflags & DIRECTORY && !node.dir) return E.NOTDIR;
        if (oflags & TRUNC && !node.dir) {
          node.size = 0;
          w.fs.dirty.add(path);
        }
        const fd = w.nextFd++;
        w.fds.set(fd, { node, path, pos: fdflags & 1 && !node.dir ? node.size : 0 });
        w.view().setUint32(fdPtr, fd, true);
        return E.SUCCESS;
      },
      path_filestat_get(fd, _flags, pathPtr, pathLen, ptr) {
        const path = w.resolve(fd, w.str(pathPtr, pathLen));
        const node = path === null ? null : w.fs.lookup(path);
        if (!node) return E.NOENT;
        w.stat(w.view(), ptr, node);
        return E.SUCCESS;
      },
      path_create_directory(fd, pathPtr, pathLen) {
        const path = w.resolve(fd, w.str(pathPtr, pathLen));
        if (path === null) return E.BADF;
        if (w.fs.lookup(path)) return E.EXIST;
        const { parent, name } = w.fs.parentOf(path);
        if (!parent || !parent.dir) return E.NOENT;
        parent.dir.set(name, { dir: new Map() });
        return E.SUCCESS;
      },
      path_remove_directory(fd, pathPtr, pathLen) {
        const path = w.resolve(fd, w.str(pathPtr, pathLen));
        const node = path === null ? null : w.fs.lookup(path);
        if (!node) return E.NOENT;
        if (!node.dir) return E.NOTDIR;
        if (node.dir.size) return E.NOTEMPTY;
        const { parent, name } = w.fs.parentOf(path);
        parent.dir.delete(name);
        return E.SUCCESS;
      },
      path_unlink_file(fd, pathPtr, pathLen) {
        const path = w.resolve(fd, w.str(pathPtr, pathLen));
        const node = path === null ? null : w.fs.lookup(path);
        if (!node) return E.NOENT;
        if (node.dir) return E.ISDIR;
        const { parent, name } = w.fs.parentOf(path);
        parent.dir.delete(name);
        w.fs.dirty.add(path);
        return E.SUCCESS;
      },
      path_rename(fd, oldPtr, oldLen, newFd, newPtr, newLen) {
        const from = w.resolve(fd, w.str(oldPtr, oldLen));
        const to = w.resolve(newFd, w.str(newPtr, newLen));
        const node = from === null ? null : w.fs.lookup(from);
        if (!node || to === null) return E.NOENT;
        const a = w.fs.parentOf(from);
        const b = w.fs.parentOf(to);
        if (!b.parent || !b.parent.dir) return E.NOENT;
        a.parent.dir.delete(a.name);
        b.parent.dir.set(b.name, node);
        w.fs.dirty.add(from);
        w.fs.dirty.add(to);
        return E.SUCCESS;
      },
    };
  }
}
