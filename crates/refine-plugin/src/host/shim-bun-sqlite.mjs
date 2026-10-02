/** bun:sqlite → node:sqlite adapter (Database/Statement subset the plugins use). */
import { DatabaseSync } from "node:sqlite";

export class Statement {
  constructor(db, sql) { this._db = db; this._sql = sql; }
  _prep() { return this._db.prepare(this._sql); }
  all(...args) { return this._prep().all(...args); }
  get(...args) { return this._prep().get(...args); }
  run(...args) { const r = this._prep().run(...args); return { changes: Number(r.changes), lastInsertRowid: r.lastInsertRowid }; }
  iterate(...args) { return this._prep().iterate(...args); }
}

export class Database {
  constructor(path, _opts) { this._db = new DatabaseSync(path); this.path = path; }
  prepare(sql) { return new Statement(this._db, sql); }
  exec(sql) { this._db.exec(sql); return this; }
  close() { this._db.close(); }
  // bun:sqlite direct-query API (magic-context uses db.run)
  run(sql, ...args) {
    const r = this._db.prepare(sql).run(...args);
    return { changes: Number(r.changes), lastInsertRowid: r.lastInsertRowid };
  }
  get(sql, ...args) { return this._db.prepare(sql).get(...args); }
  all(sql, ...args) { return this._db.prepare(sql).all(...args); }
  query(sql) {
    const stmt = this._db.prepare(sql);
    return { all: (...a) => stmt.all(...a), get: (...a) => stmt.get(...a), run: (...a) => { const r = stmt.run(...a); return { changes: Number(r.changes), lastInsertRowid: r.lastInsertRowid }; } };
  }
  function() { throw new Error("bun:sqlite db.function() unsupported in shim"); }
  aggregate() { throw new Error("bun:sqlite db.aggregate() unsupported in shim"); }
  loadExtension() { throw new Error("bun:sqlite loadExtension unsupported in shim"); }
  query(sql) { return { all: (...a) => this._db.prepare(sql).all(...a), get: (...a) => this._db.prepare(sql).get(...a), run: (...a) => this._db.prepare(sql).run(...a) }; }
  transaction(fn) { return (...args) => { this._db.exec("BEGIN"); try { const r = fn(...args); this._db.exec("COMMIT"); return r; } catch (e) { this._db.exec("ROLLBACK"); throw e; } }; }
}

export default { Database, Statement };
