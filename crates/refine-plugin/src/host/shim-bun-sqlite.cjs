/** bun:sqlite → node:sqlite (CJS path: context-mode does require("bun:sqlite")). */
const { DatabaseSync } = require("node:sqlite");

class Statement {
  constructor(db, sql) { this._db = db; this._sql = sql; }
  _prep() { return this._db.prepare(this._sql); }
  all(...a) { return this._prep().all(...a); }
  get(...a) { return this._prep().get(...a); }
  run(...a) { const r = this._prep().run(...a); return { changes: Number(r.changes), lastInsertRowid: r.lastInsertRowid }; }
  iterate(...a) { return this._prep().iterate(...a); }
}

class Database {
  constructor(path, opts = {}) {
    this._db = new DatabaseSync(path, opts.readonly ? { readOnly: true } : {});
    this.path = path;
    this.readonly = !!opts.readonly;
  }
  prepare(sql) { return new Statement(this._db, sql); }
  exec(sql) { this._db.exec(sql); return this; }
  close() { this._db.close(); }
  run(sql, ...args) { const r = this._db.prepare(sql).run(...args); return { changes: Number(r.changes), lastInsertRowid: r.lastInsertRowid }; }
  get(sql, ...args) { return this._db.prepare(sql).get(...args); }
  all(sql, ...args) { return this._db.prepare(sql).all(...args); }
  query(sql) { const stmt = this._db.prepare(sql); return {
    all: (...a) => stmt.all(...a), get: (...a) => stmt.get(...a),
    run: (...a) => { const r = stmt.run(...a); return { changes: Number(r.changes), lastInsertRowid: r.lastInsertRowid }; },
  }; }
  pragma(s) { try { return this._db.prepare(`PRAGMA ${s}`).all(); } catch { return this._db.prepare(`PRAGMA ${s}`).run(); } }
  transaction(fn) {
    const db = this._db;
    return (...args) => {
      db.exec("BEGIN");
      try { const r = fn(...args); db.exec("COMMIT"); return r; }
      catch (e) { db.exec("ROLLBACK"); throw e; }
    };
  }
  function() { throw new Error("bun:sqlite db.function() unsupported in shim"); }
  aggregate() { throw new Error("bun:sqlite db.aggregate() unsupported in shim"); }
}

module.exports = { Database, Statement };
