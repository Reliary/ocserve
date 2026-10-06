/** K5b dual-runtime sqlite shim (spike variant — NOT refine's shipped shim).
 *
 *  One normalized file must serve bun, node, and deno:
 *    - bun 1.3.14 has NO node:sqlite (file context); its native bun:sqlite works;
 *    - node/deno have node:sqlite; any STATIC node:sqlite import fails bun's
 *      graph link before any code runs;
 *    - specifiers are COMPUTED ("bun:" + "sqlite") so no bundler can
 *      analyze/alias them (same trick magic-context uses for transformers)
 *      and off-runtime rejections are runtime-catchable promises.
 *
 *  Exports: Database (the only name magic-context imports: 4 aliases),
 *  Statement + default mirror the shipped shim's surface for safety.
 *  - bun path: native bun:sqlite Database (definitionally compatible;
 *    production under bun has exercised this surface for years/months).
 *  - node/deno path: the shipped shim's classes VERBATIM (byte-parity for
 *    K3 — including its duplicate `query` definition where the second wins).
 */
const BUN_SPEC = "bun:" + "sqlite";
const NODE_SPEC = "node:" + "sqlite";

const bunMod = await import(BUN_SPEC).catch(() => null);

/** @type {any} */
export let Database;
/** @type {any} */
export let Statement;

if (bunMod) {
  Database = bunMod.Database;
} else {
  const { DatabaseSync } = await import(NODE_SPEC);

  class ShippedStatement {
    constructor(db, sql) { this._db = db; this._sql = sql; }
    _prep() { return this._db.prepare(this._sql); }
    all(...args) { return this._prep().all(...args); }
    get(...args) { return this._prep().get(...args); }
    run(...args) { const r = this._prep().run(...args); return { changes: Number(r.changes), lastInsertRowid: r.lastInsertRowid }; }
    iterate(...args) { return this._prep().iterate(...args); }
  }

  class ShippedDatabase {
    constructor(path, _opts) { this._db = new DatabaseSync(path); this.path = path; }
    prepare(sql) { return new ShippedStatement(this._db, sql); }
    exec(sql) { this._db.exec(sql); return this; }
    close() { this._db.close(); }
    // bun:sqlite direct-query API (magic-context uses db.run)
    run(sql, ...args) {
      const r = this._db.prepare(sql).run(...args);
      return { changes: Number(r.changes), lastInsertRowid: r.lastInsertRowid };
    }
    get(sql, ...args) { return this._db.prepare(sql).get(...args); }
    all(sql, ...args) { return this._db.prepare(sql).all(...args); }
    query(sql) { return { all: (...a) => this._db.prepare(sql).all(...a), get: (...a) => this._db.prepare(sql).get(...a), run: (...a) => this._db.prepare(sql).run(...a) }; }
    function() { throw new Error("bun:sqlite db.function() unsupported in shim"); }
    aggregate() { throw new Error("bun:sqlite db.aggregate() unsupported in shim"); }
    loadExtension() { throw new Error("bun:sqlite loadExtension unsupported in shim"); }
    query(sql) { return { all: (...a) => this._db.prepare(sql).all(...a), get: (...a) => this._db.prepare(sql).get(...a), run: (...a) => this._db.prepare(sql).run(...a) }; }
    transaction(fn) { return (...args) => { this._db.exec("BEGIN"); try { const r = fn(...args); this._db.exec("COMMIT"); return r; } catch (e) { this._db.exec("ROLLBACK"); throw e; } }; }
  }

  Database = ShippedDatabase;
  Statement = ShippedStatement;
}
